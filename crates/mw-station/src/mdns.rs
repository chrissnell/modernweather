//! An in-process mDNS client, because `getaddrinfo` cannot resolve `.local` in this
//! binary.
//!
//! **This is a constraint from another work item, not from W2a.** **D-43** links every
//! Linux artefact statically against musl, and musl's resolver supports neither
//! `nsswitch.conf` nor NSS modules — so `nss-mdns`, which is how a glibc system resolves
//! `wll.local`, is not reachable from the shipped binary at all (W8 **OQ-4**). W8's
//! recommendation, carried into this item as `docs/reconciliation.md` §1 row 16, is that
//! WeatherLink Live resolution "uses an in-process mDNS client, not `getaddrinfo` of a
//! `.local` name". So a `.local` host is resolved by sending the multicast query here, and
//! every other host goes to the system resolver, where it belongs.
//!
//! **Why this is not a crate dependency.** No mDNS crate is in **D-32**'s table, so adding
//! one would be an amendment rather than a manifest edit (the manifest's own rule). What is
//! needed is one question — "what is the A record for this name" — asked once, answered
//! once, and discarded: no service browsing, no `_services._dns-sd._udp` enumeration, no
//! cache, no responder. That is a 12-byte header and a label sequence, and the encode and
//! the decode below are pure functions with golden tests over byte strings, which is more
//! test than a dependency would have come with.
//!
//! **Unicast-response, and why it matters here.** RFC 6762 §5.4 gives the top bit of
//! `QCLASS` to "please answer me directly" (QU). Setting it means the responder replies to
//! this socket's ephemeral port, so the client needs no multicast group membership, no
//! `SO_REUSEADDR` on port 5353 and no interference with a `systemd-resolved` or `avahi`
//! that already holds it — which is the difference between a resolver that works on the
//! owner's homelab and one that fights the host for a socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tracing::debug;

/// The IPv4 mDNS group and port (RFC 6762 §3).
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// `QTYPE`/`TYPE` for an IPv4 address record.
const TYPE_A: u16 = 1;
/// `QCLASS`/`CLASS` for the Internet class, and the two bits layered onto it: the QU bit of
/// RFC 6762 §5.4 on a question, and the cache-flush bit of §10.2 on an answer. Both occupy
/// the same position, which is why the answer's class is masked before it is compared.
const CLASS_IN: u16 = 1;
const UNICAST_RESPONSE: u16 = 0x8000;
const CLASS_MASK: u16 = 0x7fff;

/// The suffix this resolver is for.
pub const LOCAL_SUFFIX: &str = ".local";

/// How long to wait for a responder before giving up.
///
/// RFC 6762 §5.2 retransmits after one second; a device on the same LAN answers in
/// milliseconds, and the caller's own occurrence deadline is what actually bounds this. One
/// second buys a retransmit's worth of patience without making a missing device look like a
/// slow one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);

/// Why a `.local` name did not resolve.
#[derive(Debug, thiserror::Error)]
pub enum MdnsError {
    /// Not a name this resolver can ask about.
    #[error("`{name}` is not a name mDNS can resolve: {why}")]
    BadName {
        /// What was configured.
        name: String,
        /// Which rule it broke.
        why: &'static str,
    },

    /// The socket could not be opened or used.
    #[error("the mDNS query could not be sent")]
    Io {
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// Nobody answered inside the window.
    ///
    /// The ordinary failure, and the one whose message has to be actionable: on a homelab
    /// the usual cause is that the query did not leave the host's network namespace, not
    /// that the device is off.
    #[error(
        "no mDNS responder answered for `{name}` within {}ms. A WeatherLink Live answers \
         on the LAN it is attached to; if this daemon is in a container, multicast has to \
         reach that LAN. Configuring the unit's IP address instead needs no multicast at \
         all.",
        timeout.as_millis()
    )]
    NoAnswer {
        /// What was asked for.
        name: String,
        /// How long was waited.
        timeout: Duration,
    },
}

/// Resolve a `.local` name to one address.
///
/// The first A record that answers wins: a WeatherLink Live has one interface, and a
/// resolver that collected every answer would have to decide between them, which is a
/// policy no caller here has.
pub async fn resolve(name: &str, timeout: Duration) -> Result<IpAddr, MdnsError> {
    let query = query(name, 0)?;
    // Bound to every interface on an ephemeral port: the answer comes back unicast because
    // the question set the QU bit, so this socket joins no group and contends for nothing.
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .await
        .map_err(|source| MdnsError::Io { source })?;
    socket
        .set_broadcast(true)
        .map_err(|source| MdnsError::Io { source })?;
    socket
        .send_to(&query, SocketAddr::from((MDNS_GROUP, MDNS_PORT)))
        .await
        .map_err(|source| MdnsError::Io { source })?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(MdnsError::NoAnswer {
                name: name.to_string(),
                timeout,
            });
        }
        let received = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        let Ok(received) = received else {
            return Err(MdnsError::NoAnswer {
                name: name.to_string(),
                timeout,
            });
        };
        let (n, from) = received.map_err(|source| MdnsError::Io { source })?;
        match answer(&buf[..n], name) {
            Some(addr) => {
                debug!(name, %addr, %from, "resolved a .local name by mDNS");
                return Ok(IpAddr::V4(addr));
            }
            // Another responder's announcement, or an answer for a different name. The
            // group is busy on a homelab, so this is the common case rather than an error.
            None => continue,
        }
    }
}

/// Encode a one-question mDNS query for `name`'s A record.
///
/// Pure, so the wire format is golden-testable (the tests below).
fn query(name: &str, id: u16) -> Result<Vec<u8>, MdnsError> {
    let labels = labels(name)?;
    let mut out = Vec::with_capacity(12 + labels.len() + 4);
    out.extend_from_slice(&id.to_be_bytes()); // ID
    out.extend_from_slice(&0u16.to_be_bytes()); // flags: a standard query
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&labels);
    out.extend_from_slice(&TYPE_A.to_be_bytes());
    out.extend_from_slice(&(CLASS_IN | UNICAST_RESPONSE).to_be_bytes());
    Ok(out)
}

/// Encode a name as a DNS label sequence.
fn labels(name: &str) -> Result<Vec<u8>, MdnsError> {
    let name = name.trim_end_matches('.');
    let bad = |why: &'static str| MdnsError::BadName {
        name: name.to_string(),
        why,
    };
    if name.is_empty() {
        return Err(bad("it is empty"));
    }
    if name.len() > 255 {
        return Err(bad("it is longer than 255 bytes"));
    }
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        if label.is_empty() {
            return Err(bad("it has an empty label"));
        }
        let len = u8::try_from(label.len())
            .ok()
            .filter(|n| *n <= 63)
            .ok_or_else(|| bad("one of its labels is longer than 63 bytes"))?;
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(bad(
                "a label may hold only ASCII letters, digits, `-` and `_`",
            ));
        }
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(out)
}

/// Find `name`'s A record in a response.
///
/// `None` for a packet that is not an answer for this name, which includes every unrelated
/// announcement on the group. Never panics and never indexes past the end: a malformed
/// packet from the LAN is the one input here that is not under this program's control.
fn answer(packet: &[u8], name: &str) -> Option<Ipv4Addr> {
    let header = packet.get(..12)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    // Bit 15 is QR: this has to be a response. An mDNS *query* on the group carrying a
    // known-answer section would otherwise be read as an answer.
    if flags & 0x8000 == 0 {
        return None;
    }
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let answers = u16::from_be_bytes([header[6], header[7]]);

    let mut at = 12usize;
    for _ in 0..questions {
        at = skip_name(packet, at)?;
        // QTYPE and QCLASS.
        at = at.checked_add(4).filter(|n| *n <= packet.len())?;
    }
    for _ in 0..answers {
        let (record, next) = read_name(packet, at)?;
        at = next;
        let fixed = packet.get(at..at.checked_add(10)?)?;
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let class = u16::from_be_bytes([fixed[2], fixed[3]]) & CLASS_MASK;
        let rdlength = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
        at = at.checked_add(10)?;
        let rdata = packet.get(at..at.checked_add(rdlength)?)?;
        at = at.checked_add(rdlength)?;
        if rtype == TYPE_A && class == CLASS_IN && rdlength == 4 && same_name(&record, name) {
            return Some(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
        }
    }
    None
}

/// DNS names are case-insensitive (RFC 4343), and a responder is free to answer
/// `WLL.local` to a question about `wll.local`.
fn same_name(got: &str, want: &str) -> bool {
    got.trim_end_matches('.')
        .eq_ignore_ascii_case(want.trim_end_matches('.'))
}

/// Walk past a name without decoding it.
fn skip_name(packet: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *packet.get(at)?;
        match len & 0xc0 {
            // A compression pointer is two bytes and always terminal.
            0xc0 => return at.checked_add(2).filter(|n| *n <= packet.len()),
            0 if len == 0 => return at.checked_add(1),
            0 => at = at.checked_add(1 + usize::from(len))?,
            // 0x40 and 0x80 are reserved label types; a packet using one is not one this
            // resolver can walk, so it is refused rather than guessed at.
            _ => return None,
        }
    }
}

/// Decode a name, following compression pointers, and return where it ended.
fn read_name(packet: &[u8], start: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut at = start;
    let mut end = None;
    // A pointer must point strictly backwards, so the number of jumps is bounded by the
    // packet length -- which is what makes a pointer loop impossible rather than merely
    // unlikely.
    let mut jumps = 0usize;
    loop {
        let len = *packet.get(at)?;
        match len & 0xc0 {
            0xc0 => {
                let second = *packet.get(at.checked_add(1)?)?;
                let target = usize::from(u16::from_be_bytes([len & 0x3f, second]));
                if target >= at {
                    return None;
                }
                jumps += 1;
                if jumps > packet.len() {
                    return None;
                }
                end = end.or_else(|| at.checked_add(2));
                at = target;
            }
            0 if len == 0 => {
                let finished = end.or_else(|| at.checked_add(1))?;
                return Some((name, finished));
            }
            0 => {
                let from = at.checked_add(1)?;
                let label = packet.get(from..from.checked_add(usize::from(len))?)?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(&String::from_utf8_lossy(label));
                at = from.checked_add(usize::from(len))?;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response packet: one question, one A answer, built the way a responder would.
    fn response(name: &str, addr: Ipv4Addr, compressed: bool) -> Vec<u8> {
        let labels = labels(name).expect("a literal name");
        let mut p = Vec::new();
        p.extend_from_slice(&0u16.to_be_bytes()); // ID -- a responder may answer with 0
        p.extend_from_slice(&0x8400u16.to_be_bytes()); // QR | AA
        p.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        p.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        let question_at = p.len();
        p.extend_from_slice(&labels);
        p.extend_from_slice(&TYPE_A.to_be_bytes());
        p.extend_from_slice(&CLASS_IN.to_be_bytes());
        if compressed {
            // A pointer back to the question's name, which is what every real responder
            // emits and what a hand-rolled parser forgets to handle.
            let pointer = 0xc000u16 | u16::try_from(question_at).expect("a small offset");
            p.extend_from_slice(&pointer.to_be_bytes());
        } else {
            p.extend_from_slice(&labels);
        }
        p.extend_from_slice(&TYPE_A.to_be_bytes());
        // The cache-flush bit, which every mDNS answer sets and which a comparison against
        // a bare `1` would reject.
        p.extend_from_slice(&(CLASS_IN | 0x8000).to_be_bytes());
        p.extend_from_slice(&120u32.to_be_bytes()); // TTL
        p.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        p.extend_from_slice(&addr.octets());
        p
    }

    #[test]
    fn a_query_is_the_bytes_rfc_6762_describes() {
        let q = query("wll.local", 0x1234).expect("a literal name");
        assert_eq!(
            q,
            vec![
                0x12, 0x34, // ID
                0x00, 0x00, // a standard query
                0x00, 0x01, // one question
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // no answers, no authority, no extra
                3, b'w', b'l', b'l', 5, b'l', b'o', b'c', b'a', b'l', 0, 0x00, 0x01, // A
                0x80, 0x01, // IN, with the unicast-response bit of §5.4
            ]
        );
    }

    #[test]
    fn an_answer_is_found_whether_or_not_its_name_is_compressed() {
        let addr = Ipv4Addr::new(192, 168, 1, 50);
        for compressed in [false, true] {
            let packet = response("wll.local", addr, compressed);
            assert_eq!(
                answer(&packet, "wll.local"),
                Some(addr),
                "compressed: {compressed}"
            );
        }
    }

    #[test]
    fn an_answer_for_a_different_name_is_not_this_answer() {
        let packet = response("printer.local", Ipv4Addr::new(10, 0, 0, 9), true);
        assert_eq!(answer(&packet, "wll.local"), None);
        // ... and the comparison is case-insensitive, because a responder may answer in
        // any case (RFC 4343).
        let packet = response("WLL.local", Ipv4Addr::new(10, 0, 0, 9), true);
        assert_eq!(
            answer(&packet, "wll.local"),
            Some(Ipv4Addr::new(10, 0, 0, 9))
        );
    }

    /// A query on the group carrying a known-answer section is not a response.
    #[test]
    fn a_query_is_not_mistaken_for_an_answer() {
        let mut packet = response("wll.local", Ipv4Addr::LOCALHOST, true);
        packet[2] = 0x00; // clear QR
        packet[3] = 0x00;
        assert_eq!(answer(&packet, "wll.local"), None);
    }

    /// The one input to this module that is not under this program's control.
    #[test]
    fn a_malformed_packet_is_refused_rather_than_panicking() {
        let full = response("wll.local", Ipv4Addr::LOCALHOST, true);
        for cut in 0..full.len() {
            // Every truncation of a valid packet, which is what a short read produces.
            assert!(
                answer(&full[..cut], "wll.local").is_none(),
                "a {cut}-byte packet answered"
            );
        }
        assert_eq!(answer(&[], "wll.local"), None);
        // A compression pointer that points at itself, which is the loop a parser that
        // followed pointers forwards would hang on. `read_name` requires a pointer to go
        // strictly backwards, so this is refused at the first jump.
        let self_pointer = [0x00, 0x00, 0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0, 0xc0, 0x0c];
        assert_eq!(answer(&self_pointer, "wll.local"), None);
    }

    #[test]
    fn a_name_that_cannot_be_asked_about_is_refused_with_the_rule_it_broke() {
        for name in ["", ".", "a..b", "wll space.local"] {
            let e = query(name, 0).expect_err("rejected");
            assert!(matches!(e, MdnsError::BadName { .. }), "{name}: {e}");
        }
        let long = "x".repeat(64);
        assert!(matches!(
            query(&format!("{long}.local"), 0),
            Err(MdnsError::BadName { .. })
        ));
        // A trailing dot is how a fully-qualified name is written and is not an error.
        assert!(query("wll.local.", 0).is_ok());
    }

    #[test]
    fn nothing_answering_is_an_actionable_message() {
        let e = MdnsError::NoAnswer {
            name: "wll.local".into(),
            timeout: Duration::from_millis(1500),
        };
        let text = e.to_string();
        assert!(text.contains("1500ms"), "{text}");
        assert!(text.contains("multicast has to reach that LAN"), "{text}");
    }
}
