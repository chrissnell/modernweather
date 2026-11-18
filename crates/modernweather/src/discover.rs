//! `modernweather discover` — find snow gauges on the LAN, over mDNS.
//!
//! The one subcommand that opens nothing. It takes no `--config`, reads no database, plans
//! nothing and changes nothing: it sends one multicast question, prints what answered, and
//! exits. So it is safe to run on a host that is already serving, and running it is the
//! answer to "what is the gauge's address" without reading four hex characters off a
//! device.
//!
//! `chrissnell/snowgauge-rs` ships a `snowgauge-cli discover` that prints the same four
//! columns from the same records; this is the daemon's own answer to the same question, and
//! the reason it is here rather than left to that tool is **D-43**: every Linux artefact is
//! statically linked against musl, so the shipped binary cannot resolve a `.local` name
//! through `getaddrinfo` at all and already carries its own mDNS client
//! (`mw_station::mdns`). Browsing is twenty lines on top of a resolver this build has to
//! have.
//!
//! # No subscriber, so the output is the console's
//!
//! This runs instead of the daemon, like `crate::recover`'s three, so everything it says
//! goes through [`mw_obs::console_banner`] — `ci/layers.toml`'s `no-stdout-logging` rule
//! allows the printing macros only on the binary's argument-parsing path, and this is not
//! it. (Its scan is a substring match over whole files and does not know what a comment is,
//! which is why those macros are not named here.)
//!
//! # Finding nothing is an outcome and not a failure
//!
//! The firmware's own integration document says mDNS is "common[ly]" unavailable on servers
//! and in Kubernetes, because the multicast group does not reach the process — which is
//! most of where this daemon runs. So an empty answer is printed as the two things it can
//! mean, with the fallback that needs no multicast at all, and the exit code is still zero:
//! a provisioning script that treated "no gauge today" as a failed command would be wrong
//! about a LAN that simply has no gauge on it.

use std::time::Duration;

use mw_station::snowgauge::{self, Discovered};

/// Browse for gauges and print what answered.
pub async fn run(timeout: Duration) -> anyhow::Result<()> {
    let found = snowgauge::discover(timeout).await?;
    mw_obs::console_banner(&render(&found, timeout));
    Ok(())
}

/// The lines one browse produces. Pure, so the table is testable with no LAN.
fn render(found: &[Discovered], timeout: Duration) -> Vec<String> {
    if found.is_empty() {
        return vec![
            format!(
                "No snow gauge answered an mDNS query for `{}` within {}ms.",
                mw_station::mdns::HTTP_SERVICE,
                timeout.as_millis()
            ),
            String::new(),
            "That means one of two things, and they look identical from here: there is no \
             gauge on this LAN, or this process cannot reach the LAN's multicast group — \
             which is the usual case on a server and in Kubernetes."
                .to_string(),
            String::new(),
            "A gauge needs no multicast to be collected from. It always requests the same \
             hostname, so reserve its DHCP lease by MAC and configure the station with \
             `transport_url = http://snowgauge-<4hex>` (or its address). The four hex \
             characters are the last of the gauge's WiFi MAC, and its own web UI shows them."
                .to_string(),
        ];
    }
    // The firmware's own `snowgauge-cli discover` columns, in its own order, so that two
    // tools describing one device describe it the same way.
    let mut lines = vec![format!(
        "{:<24} {:<15} {:<8} {}",
        "HOSTNAME", "IP", "FW", "SLOT"
    )];
    for gauge in found {
        lines.push(format!(
            "{:<24} {:<15} {:<8} {}",
            printable(&gauge.host),
            gauge
                .addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string()),
            printable(gauge.firmware.as_deref().unwrap_or("-")),
            printable(gauge.slot.as_deref().unwrap_or("-")),
        ));
    }
    lines.push(String::new());
    // The address, through `Endpoint` rather than formatted again, so what an operator
    // pastes into the column is a value the column's own parser has already accepted.
    lines.push(
        "Configure a station of type `snowgauge` with `transport_kind = http_out` and:".to_string(),
    );
    for gauge in found {
        match gauge.endpoint() {
            Ok(endpoint) => lines.push(format!("    transport_url = {endpoint}")),
            Err(e) => lines.push(format!(
                "    {}: this gauge's advertised address is not one a station can hold: {}",
                printable(&gauge.host),
                mw_types::DisplayChain(&e)
            )),
        }
    }
    // **The caveat the empty answer already carried**, because the two halves of this
    // command were recommending different things: a `.local` address is re-resolved by
    // multicast on every poll, and the device's own documentation says multicast usually
    // does not reach a daemon like this one. Discovery working once says nothing about it
    // working every five seconds.
    lines.push(String::new());
    lines.push(
        "A `.local` address is re-resolved by mDNS on every poll. If this daemon is in a \
         container or on a server, reserve the gauge's DHCP lease by MAC and configure its \
         address instead -- it always requests the same hostname."
            .to_string(),
    );
    lines
}

/// One LAN-supplied string, with anything unprintable replaced.
///
/// A hostname and a TXT value are **whatever a responder sent**: `mw_station::mdns` decodes
/// them with `from_utf8_lossy`, so a control byte survives the decode intact and would
/// otherwise be written straight into a terminal — where an ANSI escape can move the cursor
/// and rewrite the table around it, and a carriage return can hide the row it belongs to.
/// The `transport_url` lines need no filtering of their own, because they have been through
/// `Endpoint::parse`; these three columns have been through nothing.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn gauge(host: &str, addr: Option<IpAddr>, fw: Option<&str>) -> Discovered {
        Discovered {
            host: host.to_string(),
            addr,
            port: 80,
            firmware: fw.map(str::to_string),
            slot: Some("ota_0".to_string()),
        }
    }

    #[test]
    fn two_gauges_are_two_rows_and_two_addresses_to_configure() {
        let lines = render(
            &[
                gauge(
                    "snowgauge-f878.local",
                    Some(IpAddr::V4(Ipv4Addr::new(10, 50, 0, 175))),
                    Some("0.1.0"),
                ),
                gauge("snowgauge-1a2b.local", None, None),
            ],
            snowgauge::DISCOVER_TIMEOUT,
        );
        let text = lines.join("\n");
        assert!(text.starts_with("HOSTNAME"), "{text}");
        assert!(text.contains("snowgauge-f878.local"), "{text}");
        assert!(text.contains("10.50.0.175"), "{text}");
        assert!(text.contains("0.1.0"), "{text}");
        assert!(text.contains("ota_0"), "{text}");
        // A gauge whose responder sent no address is still a row: the hostname is what a
        // station is configured with, and `mw_station::mdns::resolve` is what resolves it
        // at poll time.
        assert!(text.contains("snowgauge-1a2b.local"), "{text}");
        assert!(
            text.contains('-'),
            "the absent fields print as a dash: {text}"
        );
        // Port 80 is the default, so the address an operator pastes carries no port.
        assert!(
            text.contains("transport_url = http://snowgauge-f878.local"),
            "{text}"
        );
        assert!(!text.contains(":80"), "{text}");
    }

    /// A responder's own strings reach a terminal, so an escape sequence in one must not.
    ///
    /// **Two layers, and the first one is why the second exists.** A *hostname* carrying a
    /// control byte never reaches this function at all — `mw_station::snowgauge::discover`
    /// drops the instance, because `mw_station::mdns::is_resolvable` refuses the name and a
    /// host this build cannot ask about is not a device it can reach. That filter was
    /// added *because* of this test: the first version of it filtered only the table's
    /// three columns, and the `transport_url` line below them went through
    /// `Endpoint::parse`, which accepts a host with an escape in it.
    ///
    /// What is left for `printable` is the TXT values, which are free-form by design and
    /// are printed rather than resolved.
    #[test]
    fn a_txt_value_carrying_control_bytes_cannot_rewrite_the_table() {
        let lines = render(
            &[Discovered {
                host: "snowgauge-f878.local".into(),
                addr: None,
                port: 80,
                firmware: Some("0.1.0\u{1b}[2K\u{1b}[A".into()),
                slot: Some("ota_0\r".into()),
            }],
            snowgauge::DISCOVER_TIMEOUT,
        );
        let text = lines.join("\n");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\r'),
            "an escape reached the terminal: {text:?}"
        );
        // The printable part survives, because the row is still the useful answer.
        assert!(text.contains("snowgauge-f878.local"), "{text}");
        assert!(text.contains("0.1.0"), "{text}");
        assert!(text.contains("ota_0"), "{text}");
        // And exactly one header line, so a forged one cannot start a second table.
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("HOSTNAME")).count(),
            1,
            "{text}"
        );
    }

    /// Both halves of the output say the same thing about a `.local` address, which they
    /// did not: the success path recommended the form that needs multicast on every poll
    /// and the empty path recommended the form that needs none.
    #[test]
    fn the_success_path_carries_the_same_caveat_the_empty_one_does() {
        let lines = render(
            &[gauge("snowgauge-f878.local", None, Some("0.1.0"))],
            snowgauge::DISCOVER_TIMEOUT,
        );
        let text = lines.join("\n");
        assert!(text.contains("re-resolved by mDNS on every poll"), "{text}");
        assert!(text.contains("DHCP lease by MAC"), "{text}");
    }

    /// The expected answer in production, and the one that has to carry the fallback.
    #[test]
    fn nothing_answering_says_what_that_means_and_what_to_do_instead() {
        let lines = render(&[], Duration::from_millis(2000));
        let text = lines.join("\n");
        assert!(text.contains("2000ms"), "{text}");
        assert!(text.contains("_http._tcp.local"), "{text}");
        assert!(text.contains("multicast group"), "{text}");
        assert!(text.contains("DHCP lease by MAC"), "{text}");
        assert!(text.contains("transport_url"), "{text}");
    }
}
