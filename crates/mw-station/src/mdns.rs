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
//! **There is now a second question**, and it is the one thing above that is no longer
//! quite true: [`browse`] enumerates a service type rather than resolving one name, which
//! is what "no service browsing" ruled out. It is here because the snow gauge's firmware
//! advertises `_http._tcp.local.` with its firmware version and OTA slot in TXT records and
//! ships a `discover` subcommand of its own, so a `modernweather discover` that finds a
//! gauge on the LAN is the difference between an operator reading four hex characters off a
//! serial number and typing an address. It is still one question, asked once, answered into
//! a `Vec` and discarded — no cache, no responder, no `_services._dns-sd._udp` enumeration,
//! and no long-lived socket (`crate::snowgauge::discover` is the one caller, and nothing in
//! the daemon's steady state calls either).
//!
//! **Unicast-response, and why it matters here.** RFC 6762 §5.4 gives the top bit of
//! `QCLASS` to "please answer me directly" (QU). Setting it means the responder replies to
//! this socket's ephemeral port, so the client needs no multicast group membership, no
//! `SO_REUSEADDR` on port 5353 and no interference with a `systemd-resolved` or `avahi`
//! that already holds it — which is the difference between a resolver that works on the
//! owner's homelab and one that fights the host for a socket.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tracing::debug;

/// The IPv4 mDNS group and port (RFC 6762 §3).
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// `QTYPE`/`TYPE` for an IPv4 address record.
const TYPE_A: u16 = 1;
/// `QTYPE`/`TYPE` for a service-instance pointer (RFC 6763 §4.1).
const TYPE_PTR: u16 = 12;
/// `TYPE` for a service instance's key/value records (RFC 6763 §6).
const TYPE_TXT: u16 = 16;
/// `TYPE` for a service instance's host and port (RFC 2782).
const TYPE_SRV: u16 = 33;
/// `QCLASS`/`CLASS` for the Internet class, and the two bits layered onto it: the QU bit of
/// RFC 6762 §5.4 on a question, and the cache-flush bit of §10.2 on an answer. Both occupy
/// the same position, which is why the answer's class is masked before it is compared.
const CLASS_IN: u16 = 1;
const UNICAST_RESPONSE: u16 = 0x8000;
const CLASS_MASK: u16 = 0x7fff;

/// The suffix this resolver is for.
pub const LOCAL_SUFFIX: &str = ".local";

/// The service type a `snowgauge-rs` gauge advertises, and the only one anything here
/// browses for (`crate::snowgauge::discover`).
pub const HTTP_SERVICE: &str = "_http._tcp.local";

/// How many instances [`browse`] will chase missing records for.
///
/// A responder that answers a `PTR` question with the matching `SRV`, `TXT` and `A` records
/// in the same packet needs none of this, and that is what the gauge's firmware does. One
/// that answers the pointer alone needs to be asked again, and the bound is here so that a
/// hostile or broken responder advertising ten thousand instances cannot turn one question
/// into ten thousand.
const MAX_FOLLOW_UPS: usize = 16;

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

/// One service instance an mDNS responder advertised.
///
/// Everything a caller needs to reach the device and nothing it does not: the instance's
/// own name, where it is, and the TXT pairs it published. Deliberately not a
/// [`crate::Endpoint`] — this crate's endpoint type is what a *configured* transport
/// parses to, and a discovered device is a suggestion an operator has not accepted yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The full instance name, `snowgauge-f878._http._tcp.local`.
    pub instance: String,
    /// The host the `SRV` record names, `snowgauge-f878.local`.
    pub host: String,
    /// The port the `SRV` record names.
    pub port: u16,
    /// The address, when the responder sent an `A` record for `host` as well.
    ///
    /// `None` is a responder that answered the pointer and the service and left the
    /// address to a second question. The host is still usable — [`resolve`] is what asks
    /// — so this is a convenience rather than the answer.
    pub addr: Option<IpAddr>,
    /// The `TXT` record's key/value pairs, in the order they were published.
    pub txt: Vec<(String, String)>,
}

impl Instance {
    /// The instance's own label: `snowgauge-f878` out of
    /// `snowgauge-f878._http._tcp.local`.
    #[must_use]
    pub fn label(&self) -> &str {
        self.instance.split('.').next().unwrap_or(&self.instance)
    }

    /// One `TXT` value by key, case-insensitively (RFC 6763 §6.4).
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        self.txt
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Browse one service type and return every instance that answered inside `timeout`.
///
/// **This always waits the whole window**, where [`resolve`] returns on the first useful
/// answer: a question with one right answer can stop, and an enumeration cannot know
/// whether a second device is about to speak. So `timeout` is the cost of the call rather
/// than a bound on it, which is why the one caller is an operator-facing command and not
/// anything in the daemon's steady state.
///
/// Nothing answering is an **empty list and not an error**. A LAN with no gauge on it and a
/// LAN whose multicast does not reach this process look identical from here, and the
/// difference is the operator's to know; [`MdnsError::NoAnswer`]'s message belongs to a
/// question that named a device.
pub async fn browse(service: &str, timeout: Duration) -> Result<Vec<Instance>, MdnsError> {
    let service = service.trim_end_matches('.').to_string();
    let query = question(&service, 0, TYPE_PTR)?;
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

    let mut found = Found::default();
    let mut asked_again = 0usize;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(received) = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await else {
            break;
        };
        let (n, from) = received.map_err(|source| MdnsError::Io { source })?;
        // A packet from the group is the one input here that is not under this program's
        // control, so `harvest` reads it and `None` is a packet it could not walk.
        let before = found.instances.len();
        harvest(&buf[..n], &service, &mut found);
        if found.instances.len() > before {
            debug!(%from, instances = found.instances.len(), "an mDNS responder answered");
        }
        // A responder that sent the pointer alone has to be asked again. The gauge's
        // firmware does not, so this is for the rest of the LAN.
        for instance in found.pending() {
            if asked_again >= MAX_FOLLOW_UPS {
                break;
            }
            asked_again += 1;
            for qtype in [TYPE_SRV, TYPE_TXT] {
                // `None` for an instance name `labels` will not encode -- RFC 6763 §4.1
                // allows a UTF-8 instance label with spaces in it, and this resolver's
                // encoder is deliberately the narrow one `resolve` needs. Such an instance
                // is left without its records rather than dropping the whole browse, and a
                // gauge is never one: the firmware's instance name is its hostname.
                let Ok(q) = question(&instance, 0, qtype) else {
                    continue;
                };
                let _ = socket
                    .send_to(&q, SocketAddr::from((MDNS_GROUP, MDNS_PORT)))
                    .await;
            }
            // **The lower-cased key, which is what `pending` looks for.** Inserting the
            // display form instead left every pointer-only instance permanently pending, so
            // each packet that arrived re-asked for it until `MAX_FOLLOW_UPS` ran out.
            found.asked.insert(instance.to_ascii_lowercase());
        }
    }
    Ok(found.into_instances())
}

/// What the responders on the group have said so far, keyed case-insensitively.
///
/// DNS names are case-insensitive (RFC 4343) and the records that describe one instance
/// arrive in several packets from several sections, so a lower-cased key is what joins
/// them. The display forms are the ones that were received.
#[derive(Debug, Default)]
struct Found {
    /// Instance names from `PTR` records, in the order they arrived.
    instances: Vec<String>,
    /// Instance key -> (port, host).
    srv: BTreeMap<String, (u16, String)>,
    /// Instance key -> the `TXT` pairs.
    txt: BTreeMap<String, Vec<(String, String)>>,
    /// Host key -> address.
    addrs: BTreeMap<String, Ipv4Addr>,
    /// Instances a second question has already been sent for.
    asked: BTreeSet<String>,
}

impl Found {
    /// Instances with a pointer and no service record, which is what a second question is
    /// for.
    fn pending(&self) -> Vec<String> {
        self.instances
            .iter()
            .filter(|name| {
                let key = name.to_ascii_lowercase();
                !self.srv.contains_key(&key) && !self.asked.contains(&key)
            })
            .cloned()
            .collect()
    }

    /// Every instance that has both a pointer and a service record.
    ///
    /// An instance with no `SRV` is **dropped**: it has no port, so there is nothing to
    /// print and nothing to configure, and a row with a blank address would be worse than
    /// no row.
    fn into_instances(self) -> Vec<Instance> {
        let Found {
            instances,
            srv,
            mut txt,
            addrs,
            ..
        } = self;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for instance in instances {
            let key = instance.to_ascii_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            let Some((port, host)) = srv.get(&key) else {
                continue;
            };
            out.push(Instance {
                instance,
                host: host.clone(),
                port: *port,
                addr: addrs
                    .get(&host.to_ascii_lowercase())
                    .copied()
                    .map(IpAddr::V4),
                txt: txt.remove(&key).unwrap_or_default(),
            });
        }
        out
    }
}

/// Read every record in one response into `found`.
///
/// All four sections, not just the answers: a responder answering a `PTR` question puts the
/// matching `SRV`, `TXT` and `A` records in the **additional** section, which is what makes
/// one question enough. Never panics and never indexes past the end — `answer`'s reason,
/// one more time.
fn harvest(packet: &[u8], service: &str, found: &mut Found) {
    let Some(records) = walk(packet) else {
        return;
    };
    for rr in records {
        let key = rr.name.to_ascii_lowercase();
        let rdata = match packet.get(rr.rdata..rr.rdata.saturating_add(rr.rdlength)) {
            Some(bytes) => bytes,
            None => continue,
        };
        match rr.rtype {
            TYPE_PTR if same_name(&rr.name, service) => {
                if let Some((target, _)) = read_name(packet, rr.rdata) {
                    let target = target.trim_end_matches('.').to_string();
                    if !target.is_empty()
                        && !found
                            .instances
                            .iter()
                            .any(|known| same_name(known, &target))
                    {
                        found.instances.push(target);
                    }
                }
            }
            // Priority, weight, port, target -- and the target is a NAME, so it can be
            // compressed and has to be read through `read_name` rather than sliced.
            TYPE_SRV => {
                if rdata.len() < 6 {
                    continue;
                }
                let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                if let Some((host, _)) = read_name(packet, rr.rdata.saturating_add(6)) {
                    let host = host.trim_end_matches('.').to_string();
                    if !host.is_empty() {
                        found.srv.insert(key, (port, host));
                    }
                }
            }
            TYPE_TXT => {
                let pairs = txt_pairs(rdata);
                if !pairs.is_empty() {
                    found.txt.insert(key, pairs);
                }
            }
            TYPE_A if rdata.len() == 4 => {
                found
                    .addrs
                    .insert(key, Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
            }
            _ => {}
        }
    }
}

/// One `TXT` record's `key=value` strings (RFC 6763 §6.3).
///
/// A string with no `=` is a key with no value, which the specification permits and the
/// gauge does not send; it is kept with an empty value rather than dropped, because a
/// caller asking for it should be told it was there.
fn txt_pairs(rdata: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(&len) = rdata.get(at) {
        let from = at + 1;
        let Some(text) = rdata.get(from..from + usize::from(len)) else {
            break;
        };
        at = from + usize::from(len);
        // Lossy, deliberately: a TXT value is "opaque binary data" that is conventionally
        // UTF-8, and a responder with one bad byte in a comment field should not cost the
        // firmware version next to it.
        let text = String::from_utf8_lossy(text);
        if text.is_empty() {
            continue;
        }
        match text.split_once('=') {
            Some((k, v)) => out.push((k.to_string(), v.to_string())),
            None => out.push((text.to_string(), String::new())),
        }
    }
    out
}

/// One resource record's name, type and where its `RDATA` is.
struct Rr {
    name: String,
    rtype: u16,
    rdata: usize,
    rdlength: usize,
}

/// Every record in every section of a response.
///
/// `None` for a packet that is not a response or that cannot be walked. The sections are
/// read in order because that is the only way to find the one after it: a DNS record is
/// variable-length and there is no index.
fn walk(packet: &[u8]) -> Option<Vec<Rr>> {
    let header = packet.get(..12)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    // Bit 15 is QR, `answer`'s reason: a query on the group carrying a known-answer
    // section is not a response.
    if flags & 0x8000 == 0 {
        return None;
    }
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let counts = [
        u16::from_be_bytes([header[6], header[7]]),   // ANCOUNT
        u16::from_be_bytes([header[8], header[9]]),   // NSCOUNT
        u16::from_be_bytes([header[10], header[11]]), // ARCOUNT
    ];

    let mut at = 12usize;
    for _ in 0..questions {
        at = skip_name(packet, at)?;
        at = at.checked_add(4).filter(|n| *n <= packet.len())?;
    }
    let mut out = Vec::new();
    for count in counts {
        for _ in 0..count {
            let (name, next) = read_name(packet, at)?;
            at = next;
            let fixed = packet.get(at..at.checked_add(10)?)?;
            let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
            let class = u16::from_be_bytes([fixed[2], fixed[3]]) & CLASS_MASK;
            let rdlength = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
            at = at.checked_add(10)?;
            let rdata = at;
            at = at.checked_add(rdlength).filter(|n| *n <= packet.len())?;
            if class == CLASS_IN {
                out.push(Rr {
                    name: name.trim_end_matches('.').to_string(),
                    rtype,
                    rdata,
                    rdlength,
                });
            }
        }
    }
    Some(out)
}

/// Encode a one-question mDNS query for `name`'s A record.
///
/// Pure, so the wire format is golden-testable (the tests below).
fn query(name: &str, id: u16) -> Result<Vec<u8>, MdnsError> {
    question(name, id, TYPE_A)
}

/// Encode a one-question mDNS query of any type.
fn question(name: &str, id: u16, qtype: u16) -> Result<Vec<u8>, MdnsError> {
    let labels = labels(name)?;
    let mut out = Vec::with_capacity(12 + labels.len() + 4);
    out.extend_from_slice(&id.to_be_bytes()); // ID
    out.extend_from_slice(&0u16.to_be_bytes()); // flags: a standard query
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&labels);
    out.extend_from_slice(&qtype.to_be_bytes());
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

    /// A responder's answer to a `PTR` question, built the way a device's mDNS stack does
    /// it: the pointer in the answer section and the `SRV`, `TXT` and `A` records that
    /// complete it in the additional section, with every name compressed.
    fn advertisement(
        instance: &str,
        host: &str,
        port: u16,
        addr: Ipv4Addr,
        txt: &[&str],
    ) -> Vec<u8> {
        let service = labels(HTTP_SERVICE).expect("a literal name");
        let mut p = Vec::new();
        p.extend_from_slice(&0u16.to_be_bytes()); // ID
        p.extend_from_slice(&0x8400u16.to_be_bytes()); // QR | AA
        p.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        p.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT: the pointer
        p.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        p.extend_from_slice(&3u16.to_be_bytes()); // ARCOUNT: SRV, TXT, A
        let service_at = u16::try_from(p.len()).expect("a small offset");
        p.extend_from_slice(&service);
        p.extend_from_slice(&TYPE_PTR.to_be_bytes());
        p.extend_from_slice(&CLASS_IN.to_be_bytes());

        // The pointer, whose name is the service and whose RDATA is the instance.
        let record = |name: &[u8], rtype: u16, rdata: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(name);
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&(CLASS_IN | 0x8000).to_be_bytes()); // the cache-flush bit
            out.extend_from_slice(&120u32.to_be_bytes());
            out.extend_from_slice(
                &u16::try_from(rdata.len())
                    .expect("a small record")
                    .to_be_bytes(),
            );
            out.extend_from_slice(rdata);
        };
        let pointer = 0xc000u16 | service_at;
        // The instance name, written as its own label plus a pointer to the service --
        // which is exactly what a real responder emits and what a parser that sliced
        // RDATA instead of reading a name would get wrong.
        let mut instance_rdata = vec![u8::try_from(instance.len()).expect("a short label")];
        instance_rdata.extend_from_slice(instance.as_bytes());
        instance_rdata.extend_from_slice(&pointer.to_be_bytes());
        record(&pointer.to_be_bytes(), TYPE_PTR, &instance_rdata, &mut p);

        // The SRV and TXT records name the instance; written out rather than pointed at,
        // because the instance name was inside another record's RDATA.
        let instance_name = {
            let mut v = vec![u8::try_from(instance.len()).expect("a short label")];
            v.extend_from_slice(instance.as_bytes());
            v.extend_from_slice(&labels(HTTP_SERVICE).expect("a literal name"));
            v
        };
        let host_labels = labels(host).expect("a literal name");
        let mut srv = Vec::new();
        srv.extend_from_slice(&0u16.to_be_bytes()); // priority
        srv.extend_from_slice(&0u16.to_be_bytes()); // weight
        srv.extend_from_slice(&port.to_be_bytes());
        srv.extend_from_slice(&host_labels);
        record(&instance_name, TYPE_SRV, &srv, &mut p);

        let mut txt_rdata = Vec::new();
        for entry in txt {
            txt_rdata.push(u8::try_from(entry.len()).expect("a short string"));
            txt_rdata.extend_from_slice(entry.as_bytes());
        }
        record(&instance_name, TYPE_TXT, &txt_rdata, &mut p);
        record(&host_labels, TYPE_A, &addr.octets(), &mut p);
        p
    }

    fn harvested(packet: &[u8]) -> Vec<Instance> {
        let mut found = Found::default();
        harvest(packet, HTTP_SERVICE, &mut found);
        found.into_instances()
    }

    /// One packet is enough, which is the property that makes `browse` one question: the
    /// `SRV`, `TXT` and `A` records are in the **additional** section, so a parser that
    /// read only the answers would find a pointer and nothing to do with it.
    #[test]
    fn one_advertisement_carries_the_host_the_port_and_the_txt_records() {
        let packet = advertisement(
            "snowgauge-f878",
            "snowgauge-f878.local",
            80,
            Ipv4Addr::new(10, 50, 0, 175),
            &["fw=0.1.0", "slot=ota_0"],
        );
        let found = harvested(&packet);
        assert_eq!(found.len(), 1);
        let gauge = &found[0];
        assert_eq!(gauge.instance, "snowgauge-f878._http._tcp.local");
        assert_eq!(gauge.label(), "snowgauge-f878");
        assert_eq!(gauge.host, "snowgauge-f878.local");
        assert_eq!(gauge.port, 80);
        assert_eq!(gauge.addr, Some(IpAddr::V4(Ipv4Addr::new(10, 50, 0, 175))));
        assert_eq!(gauge.value("fw"), Some("0.1.0"));
        assert_eq!(gauge.value("slot"), Some("ota_0"));
        // RFC 6763 §6.4: a TXT key is compared case-insensitively.
        assert_eq!(gauge.value("FW"), Some("0.1.0"));
        assert_eq!(gauge.value("nothing"), None);
    }

    /// Two devices on one group, which is the ordinary homelab: both are returned, and the
    /// service question is what excludes everything that is not advertising it.
    #[test]
    fn two_responders_are_two_instances_and_another_service_is_neither() {
        let mut found = Found::default();
        harvest(
            &advertisement(
                "snowgauge-f878",
                "snowgauge-f878.local",
                80,
                Ipv4Addr::new(10, 50, 0, 175),
                &["fw=0.1.0"],
            ),
            HTTP_SERVICE,
            &mut found,
        );
        harvest(
            &advertisement(
                "snowgauge-1a2b",
                "snowgauge-1a2b.local",
                80,
                Ipv4Addr::new(10, 50, 0, 176),
                &["fw=0.1.1", "slot=ota_1"],
            ),
            HTTP_SERVICE,
            &mut found,
        );
        // The same device announcing again, which responders do: not a third instance.
        harvest(
            &advertisement(
                "snowgauge-f878",
                "snowgauge-f878.local",
                80,
                Ipv4Addr::new(10, 50, 0, 175),
                &["fw=0.1.0"],
            ),
            HTTP_SERVICE,
            &mut found,
        );
        let instances = found.into_instances();
        assert_eq!(instances.len(), 2);
        assert_eq!(
            instances.iter().map(|i| i.label()).collect::<Vec<_>>(),
            vec!["snowgauge-f878", "snowgauge-1a2b"]
        );

        // A printer advertising `_ipp._tcp` answers the group and not this question.
        let printer = advertisement(
            "printer",
            "printer.local",
            631,
            Ipv4Addr::new(10, 50, 0, 9),
            &["rp=ipp/print"],
        );
        let mut other = Found::default();
        harvest(&printer, "_ipp._tcp.local", &mut other);
        assert!(
            other.into_instances().is_empty(),
            "the pointer's own name is `_http._tcp.local`, so it answers that question only"
        );
    }

    /// A responder that sends the pointer alone has no port, so there is nothing to print
    /// and nothing to configure — and `browse` asks it again rather than returning a row
    /// with a blank address.
    #[test]
    fn an_instance_with_no_service_record_is_pending_rather_than_returned() {
        let full = advertisement(
            "snowgauge-f878",
            "snowgauge-f878.local",
            80,
            Ipv4Addr::new(10, 50, 0, 175),
            &["fw=0.1.0"],
        );
        // The same packet with the additional section declared empty, which is a pointer
        // and nothing else.
        let mut pointer_only = full.clone();
        pointer_only[10] = 0;
        pointer_only[11] = 0;
        let mut found = Found::default();
        harvest(&pointer_only, HTTP_SERVICE, &mut found);
        assert_eq!(found.pending(), vec!["snowgauge-f878._http._tcp.local"]);

        // **Asking is recorded under the lower-cased key**, which is what `pending` reads.
        // Recording the display form instead left the instance permanently pending, so
        // every packet that arrived re-asked for it until `MAX_FOLLOW_UPS` ran out --
        // `browse` sends two questions per entry here, which is up to 32 packets for one
        // responder that answered a pointer.
        found
            .asked
            .insert("snowgauge-f878._http._tcp.local".to_ascii_lowercase());
        assert!(found.pending().is_empty());

        assert!(found.into_instances().is_empty());
    }

    /// The one input to this module that is not under this program's control, for the
    /// browse path as well.
    #[test]
    fn a_malformed_advertisement_is_refused_rather_than_panicking() {
        let full = advertisement(
            "snowgauge-f878",
            "snowgauge-f878.local",
            80,
            Ipv4Addr::new(10, 50, 0, 175),
            &["fw=0.1.0"],
        );
        for cut in 0..full.len() {
            // Every truncation of a valid advertisement, which is what a short read
            // produces. Some prefixes legitimately carry the pointer; none may panic, and
            // none may produce an instance with no service record.
            let found = harvested(&full[..cut]);
            assert!(found.len() <= 1, "a {cut}-byte packet produced {found:?}");
        }
        assert!(harvested(&[]).is_empty());
        // A TXT record whose last string runs off the end: the strings before it survive.
        assert_eq!(
            txt_pairs(b"\x08fw=0.1.0\x40slot=ota_0"),
            vec![("fw".to_string(), "0.1.0".to_string())]
        );
        // A string with no `=` is a key with no value, which RFC 6763 §6.3 permits.
        assert_eq!(
            txt_pairs(b"\x05plain"),
            vec![("plain".to_string(), String::new())]
        );
    }

    #[test]
    fn a_browse_question_is_a_pointer_question_for_the_service() {
        let q = question(HTTP_SERVICE, 0, TYPE_PTR).expect("a literal name");
        assert_eq!(&q[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &q[q.len() - 4..],
            &[0x00, 0x0c, 0x80, 0x01],
            "PTR, IN, and the unicast-response bit of §5.4"
        );
        assert_eq!(HTTP_SERVICE, "_http._tcp.local");
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
