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
            gauge.host,
            gauge
                .addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string()),
            gauge.firmware.as_deref().unwrap_or("-"),
            gauge.slot.as_deref().unwrap_or("-"),
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
                gauge.host,
                mw_types::DisplayChain(&e)
            )),
        }
    }
    lines
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
