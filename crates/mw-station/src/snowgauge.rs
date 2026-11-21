//! The snow gauge driver: one `GET /data` per occurrence, against a `snowgauge-rs` gauge.
//!
//! The device is `chrissnell/snowgauge-rs` and its `docs/integration.md` is the authority
//! for this file. Two unauthenticated JSON endpoints over plain HTTP on port 80, **poll
//! only** — no webhooks, no push, no stream, and no history on the device, which "is
//! responsible for persistence downstream". So the gauge is `HttpOut`, `Fixed` identity and
//! `Every`, and the whole driver is a request, a decode, a liveness comparison and an
//! admission. `mw_codec::SnowGaugeData` is the decode.
//!
//! # What this replaces, and why none of it is kept
//!
//! The gauge used to be the one `GrpcOut { streaming: true }` station: we dialled
//! `snowgauge.SnowGaugeService/StreamReading` and it streamed protobuf. That required the
//! driver to live in `mw-grpc`, because `ci/layers.toml` forbids this crate from naming
//! `tonic` — so the gauge was split across three crates, had a second station registrar in
//! the binary, and was the one `Fixed` receiver `crate::plan` validated and did not plan.
//! **All of that is gone**: the new firmware speaks HTTP, `crate::http` is this crate's own
//! outbound request, and the gauge is now an ordinary row in `crate::plan`'s table beside
//! the AirGradient monitor.
//!
//! The old firmware is decommissioned, so there is no compatibility path. The one place it
//! is still visible is [`OldFirmware`]: a `404` on `/data` is a gauge that predates the
//! endpoint, and its own documentation says what that means — `/events`, the SSE endpoint
//! it has instead, was removed in v0.1.x and this build has never spoken it.
//!
//! # The cadence
//!
//! [`DEFAULT_POLL`] is five seconds, which is the slow end of the "500 ms – 5 s" the device
//! documentation calls typical, and it is deliberately the slow end: the gauge serves **at
//! most seven concurrent sockets** and its own web UI spends one request every 500 ms per
//! open browser tab. A snowpack settles over hours and `trim_mean` is a mean over the last
//! thirty samples, so there is nothing at the fast end to buy. The period is **not
//! configurable** for `crate::airgradient`'s reason: the schema's only interval column is
//! `wll_poll_interval_seconds`, which is the WeatherLink Live's, and adding one is a
//! migration and an owner decision rather than this item's.
//!
//! # Liveness is the part a reading alone cannot carry
//!
//! A gauge whose UART link to the rangefinder has died **keeps answering `/data`**, keeps
//! growing `uptime_ms`, and keeps returning the last `trim_mean` it computed. So a driver
//! that admitted every successful response would store a frozen depth indefinitely and
//! nothing would say so — which is worse than a gap, because a flat line in a snow chart
//! reads as settled weather.
//!
//! The device documentation's own test is `readings` static while `uptime_ms` grows, and
//! applying it needs state **across** occurrences. So this driver carries one
//! [`mw_codec::SnowLiveness`] and one streak counter:
//!
//! - `uptime_ms` **decreased** → the gauge rebooted. Logged at INFO and treated as a fresh
//!   start, because `readings` restarts at zero there too and a smaller count would
//!   otherwise read as a sensor that had stopped.
//! - `readings` **advanced** → a new sample. The reading is admitted and the streak resets.
//! - `readings` **did not advance** → the gauge is serving the sample that was already
//!   stored. Counted as [`FrameOutcome::Stale`] and **not** admitted: a second copy of one
//!   observation is a data point the aggregates will average twice.
//! - the same for a response that carried no measurement at all —
//!   `status: "no_data"`, a `distance` still zero before the first reading, or a distance
//!   past the station's base distance (**DEV-9**).
//!
//! After [`SILENT_OCCURRENCES`] consecutive occurrences with nothing to store, the
//! occurrence **fails** ([`Silent`]) — so `ComponentStatus::last_error` carries the reason,
//! `mw_component_ticks_total{outcome="failed"}` moves, and the supervisor's own backoff is
//! the retry rate (**D-27**). That is one minute at the default period, which is longer
//! than the ~1 minute the documentation says `trim_mean` takes to land and longer than any
//! reboot, so an ordinary start never reaches it.
//!
//! # Discovery
//!
//! [`discover`] enumerates gauges over mDNS — `_http._tcp.local.`, filtered by the
//! `snowgauge-` hostname prefix, with the firmware version and OTA slot read out of the TXT
//! records the firmware publishes. It is an operator-facing convenience (`modernweather
//! discover`) and **not** part of any component: the firmware's own documentation says mDNS
//! is usually unavailable on a server or in Kubernetes and names a DHCP reservation by the
//! gauge's stable MAC as the fallback, so a configured station is configured by hostname and
//! [`crate::mdns::resolve`] is what turns that into an address at poll time.
//!
//! # `/health` is read on the fault paths and nowhere else
//!
//! Its fields are telemetry and not measurements — no registry metric describes a free-heap
//! figure — so there is no second poll of it on the happy path, which would also double this
//! driver's share of those seven sockets. It is read in exactly the two places the device
//! documentation points at it: to turn "no measurement" into a sentence carrying
//! `bytes_rx` (which separates a sensor that is unpowered from one whose framing the gauge
//! cannot parse) and `fw_version`, and to name the version in a `404`.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mw_codec::{
    snow_health, CodecError, Frame, SnowGaugeData, SnowHealth, SnowLiveness, SNOW_DATA, SNOW_HEALTH,
};
use mw_component::{BoxFuture, ComponentError, ComponentId, ComponentIdError, Cx, Ingress, Tick};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use tracing::{debug, error, info, warn};

use crate::endpoint::Endpoint;
use crate::http::{self, HttpError};
use crate::identity::StationIdentity;
use crate::mdns;
use crate::metrics::{self as m, FrameOutcome};
use crate::transport::HttpMethod;

/// The `type` column value this driver serves.
pub const STATION_TYPE: &str = "snowgauge";

/// The poll period. See this module's documentation for why it is the slow end of the
/// device's own range.
pub const DEFAULT_POLL: Duration = Duration::from_secs(5);

/// The floor W2a **Decision 56** declares here and `crate::plan` enforces once.
///
/// One second. The device answers in 40–100 ms on a healthy link, so a sub-second period
/// would be a request every time the previous one finished, against an appliance with seven
/// sockets.
pub const POLL_FLOOR: Duration = Duration::from_secs(1);

/// How long one request may take.
///
/// Three seconds, which is the device documentation's own example client, and comfortably
/// inside the occurrence's deadline so that a gauge answering slowly produces the request
/// timeout's message — which names the gauge — rather than `DeadlineExceeded`, which names
/// nothing.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the `/health` request on a fault path may take.
///
/// **Shorter than [`REQUEST_TIMEOUT`], and that is the point.** The telemetry request only
/// ever runs *after* a `/data` request in the same occurrence, so two full
/// [`REQUEST_TIMEOUT`]s would be six seconds against a five-second deadline — and an
/// occurrence that outran its deadline would report `DeadlineExceeded`, which names nothing,
/// instead of the sentence this whole path exists to produce. One second keeps the pair
/// inside the deadline with a margin, and the request is best-effort anyway: the gauge
/// answers in 40–100 ms on a healthy link, and a `/health` that is too slow to read is
/// reported as such rather than retried.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);

/// How many consecutive occurrences may carry no measurement before the station fails.
///
/// Twelve, which is one minute at [`DEFAULT_POLL`]. Longer than the ~1 minute the device
/// documentation gives for `trim_mean` to land is not needed, because the raw `distance` is
/// the fallback in that window and a booting gauge is only silent until its sensor's first
/// frame; longer than a reboot is, and twelve covers one comfortably.
///
/// **The assumption, stated because the device documentation does not state the number it
/// rests on**: the sensor produces frames faster than one a minute. Its `trim_mean` is
/// documented as filling a thirty-sample window in "about a minute", which puts the frame
/// rate near 2 Hz, so a minute of *no* new frame is a fault rather than a slow sensor. A
/// gauge that genuinely reported less often than this would fail its station while healthy,
/// and the fix then is this constant rather than the design -- neither it nor the period is
/// configurable, for the reason this module's documentation gives.
pub const SILENT_OCCURRENCES: u32 = 12;

/// The component id of one station's poll.
pub fn station_id(station_key: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("station:snowgauge:{station_key}"))
}

/// What every gauge's hostname begins with.
///
/// The firmware builds it as `snowgauge-<4hex>`, where the four hex characters are the last
/// of the station interface's MAC address — so the prefix is the only part that is the same
/// on every device, and it is what separates a gauge from the printer and the three other
/// things on a homelab advertising `_http._tcp`.
pub const HOSTNAME_PREFIX: &str = "snowgauge-";

/// How long [`discover`] listens.
///
/// Two seconds: a device on the same LAN answers in milliseconds, and RFC 6762 §5.2
/// retransmits after one — so this is one retransmit's worth of patience for a device whose
/// first answer was lost. [`discover`] always waits the whole window, because an
/// enumeration cannot know whether a second gauge is about to speak.
pub const DISCOVER_TIMEOUT: Duration = Duration::from_secs(2);

/// One gauge found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// The host its `SRV` record names, `snowgauge-f878.local`.
    pub host: String,
    /// Its address, when the responder sent one.
    pub addr: Option<IpAddr>,
    /// The port its `SRV` record names, which is 80 on every shipped firmware.
    pub port: u16,
    /// The `fw=` TXT record: the firmware version.
    pub firmware: Option<String>,
    /// The `slot=` TXT record: which OTA partition is running.
    pub slot: Option<String>,
}

impl Discovered {
    /// The address to configure this gauge as, which is what `transport_url` holds.
    ///
    /// Through [`Endpoint`] rather than as a formatted string, so that what an operator is
    /// shown is a value that has already been through the parser the column is validated
    /// by — a discovery that printed an address `plan()` would then refuse would be worse
    /// than printing nothing.
    pub fn endpoint(&self) -> Result<Endpoint, crate::endpoint::EndpointError> {
        Endpoint::parse(&format!("http://{}:{}", self.host, self.port))
    }
}

/// Find every snow gauge on the LAN.
///
/// mDNS, because that is what the firmware advertises: `_http._tcp.local.` on port 80, with
/// `fw=` and `slot=` in its TXT records, under a hostname of `snowgauge-<4hex>.local`. The
/// repository's own `snowgauge-cli discover` prints the same four columns from the same
/// records, and this is the daemon's answer to the same question.
///
/// **mDNS is often unavailable where this daemon runs**, which the firmware's own
/// documentation says before anything else: a server or a Kubernetes pod usually cannot
/// reach the LAN's multicast group, and the fallback it names is a DHCP reservation against
/// the gauge's stable MAC — the device always asks for the same hostname. So an empty list
/// is the expected answer in production and is **not an error**: the station is configured
/// by its hostname either way, and `crate::mdns::resolve` is what turns that hostname into
/// an address at poll time, through the same socket and with no browsing at all.
pub async fn discover(timeout: Duration) -> Result<Vec<Discovered>, mdns::MdnsError> {
    let instances = mdns::browse(mdns::HTTP_SERVICE, timeout).await?;
    let mut out = Vec::new();
    for instance in instances {
        let host = instance.host.trim_end_matches('.');
        if !is_gauge(instance.label(), host) {
            debug!(
                instance = %instance.instance,
                "an mDNS service that is not a snow gauge"
            );
            continue;
        }
        // **A host this build could not ask about is not a device it can reach.** A name
        // decoded off the group is whatever a responder sent, and `crate::mdns::resolve`
        // would refuse it at poll time -- so a row for it would offer an address that
        // cannot work, and `Endpoint::parse` is not the filter: it accepts a host with
        // control bytes in it, which is how this came to be checked here.
        if !mdns::is_resolvable(host) {
            debug!(
                instance = %instance.instance,
                "a snow gauge advertising a hostname this build cannot resolve"
            );
            continue;
        }
        out.push(Discovered {
            host: host.to_string(),
            addr: instance.addr,
            port: instance.port,
            firmware: instance.value("fw").map(str::to_string),
            slot: instance.value("slot").map(str::to_string),
        });
    }
    Ok(out)
}

/// Whether one `_http._tcp` instance is a snow gauge.
///
/// **Either name**, because the two can differ: the firmware builds its hostname as
/// `snowgauge-<4hex>` and an mDNS instance name is free text an operator could have renamed
/// — and a homelab's `_http._tcp` group is a printer, a NAS, three dashboards and the
/// gauge, so a browse that returned all of them would be a list to read rather than an
/// answer.
///
/// Case-insensitively, because DNS names are (RFC 4343) and a responder may answer in any
/// case. A prefix and not a pattern over the four hex characters: a firmware that widened
/// the suffix would otherwise stop being discoverable by a daemon that had not been
/// rebuilt.
fn is_gauge(label: &str, host: &str) -> bool {
    [label, host]
        .iter()
        .any(|name| name.to_ascii_lowercase().starts_with(HOSTNAME_PREFIX))
}

/// A gauge that answers and has no measurement to give.
///
/// Retryable, and the message is the whole point of the type: an operator reading
/// `ComponentStatus::last_error` has to be sent to the sensor's wiring, to the station's
/// configured base distance, or to the gauge's own reboot history — never to this daemon.
#[derive(Debug, thiserror::Error)]
#[error(
    "the snow gauge at {authority} has carried no measurement for {occurrences} \
     consecutive polls. The last reason was: {reason}. {telemetry}"
)]
pub struct Silent {
    /// Which gauge.
    pub authority: String,
    /// How many occurrences in a row.
    pub occurrences: u32,
    /// What the last of them said, as the decoder's own sentence.
    pub reason: String,
    /// What `/health` said, or why it could not be read.
    pub telemetry: String,
}

impl Classify for Silent {
    /// Retryable. A sensor somebody is about to reconnect is a sensor that will report
    /// again, and `Backoff` caps the retry rate either way (**D-27**).
    fn class(&self) -> Class {
        Class::RETRYABLE
    }
}

/// A gauge whose firmware predates `/data`.
///
/// The one trace the decommissioned protocol leaves in this build. `/events`, the SSE
/// endpoint such a gauge has instead, was removed in the firmware's own v0.1.x and this
/// daemon has never spoken it — so the action is an upgrade, and the message says which
/// version is running rather than leaving a `404` to be interpreted.
#[derive(Debug, thiserror::Error)]
#[error(
    "the snow gauge at {authority} answered 404 on `{path}`, which is firmware that \
     predates that endpoint ({telemetry}). Upgrade the gauge: the `/events` SSE endpoint it \
     has instead was removed in firmware v0.1.x and this build does not speak it."
)]
pub struct OldFirmware {
    /// Which gauge.
    pub authority: String,
    /// The path it refused.
    pub path: &'static str,
    /// What `/health` said about the version, or why it could not be read.
    pub telemetry: String,
}

impl Classify for OldFirmware {
    /// Retryable, for `crate::http`'s reason: the thing that clears it is an OTA upload,
    /// which this daemon cannot do and does not need to restart for.
    fn class(&self) -> Class {
        Class::RETRYABLE
    }
}

/// One `GET /data` per occurrence, with the liveness state that spans them.
pub struct Poll {
    endpoint: Endpoint,
    station: StationKey,
    codec: SnowGaugeData,
    ingress: Arc<dyn Ingress>,
    /// What the previous occurrence saw, or `None` on the first one.
    seen: Option<SnowLiveness>,
    /// How many occurrences in a row have carried no measurement.
    silent: u32,
    /// The last `/health` sentence, so the failing occurrences between two reads of it
    /// carry the same diagnosis rather than a shorter one. Empty until the first read.
    telemetry: String,
    /// Whether the gauge reported a calibration last time, or `None` before the first
    /// poll. Held only to log the **transition**: a gauge that has never been calibrated
    /// would otherwise say so once per occurrence for ever.
    calibrated: Option<bool>,
}

impl fmt::Debug for Poll {
    /// The endpoint, the station and the state. Not the handle, which would print a spool.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Poll")
            .field("endpoint", &self.endpoint)
            .field("station", &self.station)
            .field("calibrated", &self.calibrated)
            .field("seen", &self.seen)
            .field("silent", &self.silent)
            .finish_non_exhaustive()
    }
}

impl Poll {
    /// The poll for one station.
    ///
    /// **No base distance.** The gauge holds its own calibration in NVS — set through the
    /// device's own web interface — and reports the depth derived from it, so there is no
    /// per-station number to hand in and `crate::plan` no longer reads
    /// `snow_base_distance_mm` to find one.
    pub fn new(endpoint: Endpoint, station: StationKey, ingress: Arc<dyn Ingress>) -> Self {
        Self {
            endpoint,
            station,
            codec: SnowGaugeData,
            ingress,
            seen: None,
            silent: 0,
            telemetry: String::new(),
            calibrated: None,
        }
    }

    /// Decode one `/data` body, apply the liveness test, and admit what it carried.
    ///
    /// The seam the transcript-level test drives: a recorded sequence of bodies, asserting
    /// that a gauge whose sensor is alive produces one reading per body and that one whose
    /// sensor has stopped produces none (W3 **D-82**). `&mut self` rather than `&self`,
    /// unlike every other polling driver here, because the liveness test is a comparison
    /// with the previous occurrence and that state has to live somewhere.
    pub async fn on_body(&mut self, id: &ComponentId, frame: &Frame) -> Result<(), ComponentError> {
        let (data, batch) = match self.codec.read(frame) {
            Ok(decoded) => decoded,
            Err(e) => {
                let outcome = match &e {
                    // **No producer since the gauge began reporting its own depth.** This
                    // was DEV-9's discard, against a *configured* base distance that could
                    // disagree with the device; the arm stays because `CodecError` is
                    // shared with every other codec, and
                    // `crate::metrics::FrameOutcome::Implausible` stays for the reason
                    // stated there.
                    CodecError::Implausible { .. } => FrameOutcome::Implausible,
                    // A well-formed body with no measurement in it: the first minute after
                    // boot, a sensor that has never spoken, or a `status` this build does
                    // not read.
                    CodecError::Incomplete { .. } => FrameOutcome::Stale,
                    _ => FrameOutcome::Undecodable,
                };
                m::frame(id, outcome);
                // **A body that is not this body fails the occurrence at once**, where the
                // two outcomes above are counted into a streak. The streak exists for
                // states a *working* gauge passes through -- a fresh boot, a snowpack
                // settling slower than the poll -- and bytes that are not a `/data`
                // document are not one of them: a gauge answering HTML, or answering
                // `/health` because something is pointed at the wrong path, will answer
                // the same way for ever, so sixty seconds of silence before
                // `last_error` says anything is sixty seconds wasted. This is
                // `crate::airgradient`'s behaviour for the same input.
                if outcome == FrameOutcome::Undecodable {
                    return Err(ComponentError::tick("decoding a snow gauge response", e));
                }
                // Flattened before the await: `DisplayChain` borrows the error, is not
                // `Sync`, and a `Tick` body has to be `Send`.
                let reason = DisplayChain(&e).to_string();
                return self.nothing_to_store(outcome, &reason).await;
            }
        };

        // **The calibration, logged on the transition only.** A gauge nobody has
        // calibrated is working and is reporting a distance; what it cannot report is a
        // depth, and the fix is at the device's own web interface rather than here. Said
        // once per change instead of once per occurrence, which at a five-second period
        // would be seventeen thousand lines a day.
        if self.calibrated != Some(data.is_calibrated()) {
            if data.is_calibrated() {
                info!(
                    station = %self.station,
                    base_distance_mm = ?data.base_distance,
                    "a snow gauge reported its calibration"
                );
            } else {
                warn!(
                    station = %self.station,
                    "a snow gauge has no calibration, so it reports a distance and no \
                     depth. Calibrate it from the gauge's own web interface; the depth \
                     starts arriving on the next poll, with nothing to change here."
                );
            }
            self.calibrated = Some(data.is_calibrated());
        }

        // **Before the sensor test**, because a reboot restarts `readings` at zero and a
        // smaller count would otherwise read as a sensor that had stopped.
        let now = data.liveness();
        let fresh = match self.seen {
            None => true,
            Some(prev) if prev.rebooted_into(now) => {
                info!(
                    station = %self.station,
                    uptime_ms = now.uptime_ms,
                    "a snow gauge rebooted since the last poll"
                );
                true
            }
            Some(prev) => prev.sensor_advanced(now),
        };
        self.seen = Some(now);
        if !fresh {
            m::frame(id, FrameOutcome::Stale);
            return self
                .nothing_to_store(
                    FrameOutcome::Stale,
                    "the gauge's `readings` counter did not advance, so its sensor has \
                     produced no new sample since the last poll and the value it is \
                     serving is the one already stored",
                )
                .await;
        }

        self.silent = 0;
        // `Fixed`: configuration says which station, so the frame presents nothing.
        let meta = StationIdentity::Fixed.frame_meta(id, None, Utc::now());
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    station = %self.station,
                    accepted = admitted.accepted,
                    rejected = admitted.rejected,
                    readings = now.readings,
                    "admitted a snow gauge reading"
                );
                Ok(())
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                match e.class() {
                    // A station pointed at the wrong endpoint is a counted signal, not a
                    // failure of the component that received it (§4.4).
                    Class::Ignorable => {
                        debug!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "a snow gauge reading was refused"
                        );
                        Ok(())
                    }
                    Class::Fatal => {
                        error!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "the spool could not be written, so this node cannot durably \
                             accept readings"
                        );
                        Err(ComponentError::tick("admitting a snow gauge reading", e))
                    }
                    _ => {
                        warn!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "a snow gauge reading could not be admitted"
                        );
                        Err(ComponentError::tick("admitting a snow gauge reading", e))
                    }
                }
            }
        }
    }

    /// One occurrence that found nothing to store: count the streak, and fail once it is
    /// long enough that an operator should be told.
    ///
    /// `Ok` below the threshold rather than a failure per occurrence, because the states
    /// this covers are all states a *working* gauge passes through — a fresh boot, a
    /// snowpack settling slower than the poll — and a component that failed on each of them
    /// would be in permanent `Backoff` for a station that is fine.
    async fn nothing_to_store(
        &mut self,
        outcome: FrameOutcome,
        reason: &str,
    ) -> Result<(), ComponentError> {
        self.silent = self.silent.saturating_add(1);
        if self.silent < SILENT_OCCURRENCES {
            debug!(
                station = %self.station,
                outcome = %outcome,
                occurrences = self.silent,
                reason,
                "a snow gauge poll carried no measurement"
            );
            return Ok(());
        }
        // **`/health` is read on the occurrence that crosses the threshold, and then once
        // per further window**, not on every failing occurrence. A gauge nobody has fixed
        // keeps failing, and two requests every five seconds for ever is twice this
        // driver's share of an appliance that serves seven sockets. The sentence is kept so
        // that the ones in between carry the same diagnosis rather than a shorter one.
        if self.silent.is_multiple_of(SILENT_OCCURRENCES) || self.telemetry.is_empty() {
            self.telemetry = self.read_health().await;
        }
        let telemetry = self.telemetry.clone();
        warn!(
            station = %self.station,
            occurrences = self.silent,
            reason,
            telemetry = %telemetry,
            "a snow gauge has carried no measurement for long enough to fail the station"
        );
        Err(ComponentError::tick(
            "polling a snow gauge",
            Silent {
                authority: self.endpoint.authority(),
                occurrences: self.silent,
                reason: reason.to_string(),
                telemetry,
            },
        ))
    }

    /// `GET /health`, rendered as the sentence a fault path carries.
    ///
    /// Named apart from the field it fills, because one is a request and the other is what
    /// the last request said.
    ///
    /// Never an error: this runs **because** something is already wrong, and a failed
    /// telemetry request must not replace the sentence about the sensor with a sentence
    /// about telemetry.
    async fn read_health(&self) -> String {
        match http::request(
            &self.endpoint,
            HttpMethod::Get,
            SNOW_HEALTH,
            None,
            HEALTH_TIMEOUT,
        )
        .await
        {
            Ok(frame) => match snow_health(&frame) {
                Ok(health) => render(&health),
                Err(e) => format!("Its `/health` did not decode: {}", DisplayChain(&e)),
            },
            Err(e) => format!("Its `/health` could not be read: {}", DisplayChain(&e)),
        }
    }
}

/// `/health` as one sentence, naming the fields a fault is diagnosed from.
///
/// `bytes_rx` first, because it is the one that separates the two causes of a silent
/// sensor: still zero is a sensor that is unpowered or unwired, and a growing count with no
/// `readings` is a sensor whose frames the gauge cannot parse.
fn render(health: &SnowHealth) -> String {
    let mut parts = Vec::new();
    match health.bytes_rx {
        Some(0) => parts.push(
            "It has received no bytes at all from the sensor, so check the sensor's power \
             and wiring"
                .to_string(),
        ),
        Some(n) => parts.push(format!("It has received {n} bytes from the sensor")),
        None => {}
    }
    if let Some(v) = &health.fw_version {
        parts.push(format!("firmware {v}"));
    }
    if let Some(slot) = &health.fw_slot {
        parts.push(format!("slot {slot}"));
    }
    if let Some(rssi) = health.rssi {
        parts.push(format!("{rssi} dBm"));
    }
    if let Some(heap) = health.heap_free {
        parts.push(format!("{heap} bytes of free heap"));
    }
    if parts.is_empty() {
        return "Its `/health` carried none of the fields this build reads.".to_string();
    }
    format!("{}.", parts.join(", "))
}

impl Tick for Poll {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            m::arm(cx.id());
            let frame = match http::request(
                &self.endpoint,
                HttpMethod::Get,
                SNOW_DATA,
                None,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(frame) => frame,
                // **The one place the decommissioned protocol is still named.** A bare
                // `404` would read as a wrong address; this one has a documented meaning.
                Err(HttpError::Status { status: 404, .. }) => {
                    let telemetry = self.read_health().await;
                    return Err(ComponentError::tick(
                        "polling a snow gauge",
                        OldFirmware {
                            authority: self.endpoint.authority(),
                            path: SNOW_DATA,
                            telemetry,
                        },
                    ));
                }
                Err(e) => return Err(ComponentError::tick("polling a snow gauge", e)),
            };
            self.on_body(cx.id(), &frame).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingIngress;
    use mw_component::Reject;

    /// A v0.1.0 `/data` body: calibrated to 1500 mm, with the depth the device would have
    /// computed from the trimmed mean.
    fn v1_body(trim: f32, readings: u64, uptime: u64) -> Frame {
        Frame::new(
            format!(
                r#"{{"distance":{},"status":"ok","readings":{readings},"trim_mean":{trim},
                     "base_distance":1500,"snow_depth":{},"rssi":-63,"uptime_ms":{uptime}}}"#,
                trim as u32,
                1500 - trim.round() as i32
            )
            .into_bytes(),
        )
    }

    /// A `/data` body, in the shape the device documentation prints.
    fn body(distance: u32, status: &str, readings: u64, trim: &str, uptime: u64) -> Frame {
        Frame::new(
            format!(
                r#"{{"distance":{distance},"status":"{status}","readings":{readings},
                     "trim_mean":{trim},"uptime_ms":{uptime}}}"#
            )
            .into_bytes(),
        )
    }

    fn poll(ingress: Arc<dyn Ingress>) -> Poll {
        Poll::new(
            Endpoint::parse("http://snowgauge-f878.local").expect("a literal address"),
            StationKey::new("suncrest:gauge"),
            ingress,
        )
    }

    fn id() -> ComponentId {
        station_id("suncrest:gauge").expect("a legal id")
    }

    /// A gauge whose sensor is producing frames: one reading per occurrence, which is the
    /// D-82 property for a polling driver.
    #[tokio::test]
    async fn every_occurrence_with_a_new_sample_produces_a_reading() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        for (n, mm) in [1_200.0, 1_190.0, 1_185.0, 1_180.0f32]
            .into_iter()
            .enumerate()
        {
            let readings = 40 + n as u64;
            poll.on_body(
                &id(),
                &body(1_300, "ok", readings, &mm.to_string(), 70_000 + n as u64),
            )
            .await
            .expect("the body is admitted");
        }
        let admissions = ingress.admissions();
        assert_eq!(admissions.len(), 4);
        assert_eq!(
            admissions
                .iter()
                .map(|a| a.batch[0].values.snow_distance)
                .collect::<Vec<_>>(),
            vec![Some(1_200.0), Some(1_190.0), Some(1_185.0), Some(1_180.0)],
            "the trimmed mean, which is what the firmware says to prefer"
        );
        for admission in &admissions {
            assert_eq!(admission.receiver, "station:snowgauge:suncrest:gauge");
            assert_eq!(admission.presented_key, None, "a Fixed identity");
            assert_eq!(
                admission.batch[0].values.snow_depth, None,
                "the depth is the enrich stage's subtraction (DEV-9)"
            );
        }
    }

    /// **The frozen-depth defect.** The gauge answers, `uptime_ms` grows, `trim_mean` is
    /// unchanged — and `readings` is static, which is the device documentation's own test
    /// for a sensor that has lost its UART link. Nothing is stored.
    #[tokio::test]
    async fn a_gauge_whose_sensor_has_stopped_stores_nothing_further() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        poll.on_body(&id(), &body(1_200, "ok", 900, "1200.0", 600_000))
            .await
            .expect("the first body is a new sample");
        for n in 1..SILENT_OCCURRENCES {
            poll.on_body(
                &id(),
                &body(1_200, "ok", 900, "1200.0", 600_000 + u64::from(n) * 5_000),
            )
            .await
            .unwrap_or_else(|e| panic!("occurrence {n} is counted and not failed: {e}"));
        }
        assert_eq!(
            ingress.admissions().len(),
            1,
            "only the occurrence that carried a new sample"
        );
        // The one that crosses the threshold fails, so an operator is told.
        let e = poll
            .on_body(
                &id(),
                &body(1_200, "ok", 900, "1200.0", 600_000 + 12 * 5_000),
            )
            .await
            .expect_err("a minute of nothing is a failed occurrence");
        let text = DisplayChain(&e).to_string();
        assert!(
            text.contains("no measurement for 12 consecutive polls"),
            "{text}"
        );
        assert!(
            text.contains("`readings` counter did not advance"),
            "{text}"
        );
        assert_eq!(ingress.admissions().len(), 1);
    }

    /// A reboot resets both counters, and asking about the sensor first would report it as
    /// dead. The reading after a reboot is admitted.
    #[tokio::test]
    async fn a_reboot_is_a_fresh_start_and_not_a_dead_sensor() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        poll.on_body(&id(), &body(1_200, "ok", 900, "1200.0", 600_000))
            .await
            .expect("admitted");
        // `uptime_ms` decreased and `readings` is far smaller: the gauge rebooted.
        poll.on_body(&id(), &body(1_205, "ok", 31, "1205.0", 61_000))
            .await
            .expect("admitted");
        assert_eq!(ingress.admissions().len(), 2);
        assert_eq!(
            ingress.admissions()[1].batch[0].values.snow_distance,
            Some(1_205.0)
        );
    }

    /// The first minute after a boot, which an ordinary start passes through: `status` is
    /// `no_data` until the sensor's first frame, then the raw distance stands in until
    /// thirty samples have landed. Nothing fails.
    #[tokio::test]
    async fn a_booting_gauge_is_counted_until_its_sensor_speaks_and_then_admitted() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        for n in 0..3u64 {
            poll.on_body(&id(), &body(0, "no_data", 0, "null", 1_000 + n * 5_000))
                .await
                .expect("a booting gauge is not a failure");
        }
        assert!(ingress.admissions().is_empty());
        // The sensor speaks. Fewer than thirty samples, so the raw distance is the value.
        poll.on_body(&id(), &body(1_210, "ok", 4, "null", 20_000))
            .await
            .expect("admitted");
        let admissions = ingress.admissions();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].batch[0].values.snow_distance, Some(1_210.0));
    }

    /// A body that is not a `/data` document **fails the occurrence at once**, where a
    /// body that decoded and carried nothing is counted into the streak.
    ///
    /// The difference is whether a working gauge passes through the state. A fresh boot
    /// does; answering HTML does not — so an immediate failure is sixty seconds sooner than
    /// the streak, and it is also what `crate::airgradient` does for the same input.
    #[tokio::test]
    async fn a_body_that_is_not_this_body_fails_the_occurrence_at_once() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        for bytes in [
            &b"<html>404</html>"[..],
            // The `/health` body on `/data`'s path: valid JSON, none of the keys.
            br#"{"wifi":"connected","fw_version":"0.1.0"}"#,
        ] {
            let e = poll
                .on_body(&id(), &Frame::new(bytes.to_vec()))
                .await
                .expect_err("not a measurement, and never will be");
            let text = DisplayChain(&e).to_string();
            assert!(text.contains("decoding a snow gauge response"), "{text}");
        }
        assert!(ingress.admissions().is_empty());

        // A body that DOES decode and carries nothing is still counted rather than failed,
        // which is the half of this distinction the streak exists for.
        poll.on_body(&id(), &body(0, "no_data", 0, "null", 4_000))
            .await
            .expect("a booting gauge is not a failure");
    }

    /// Both numbers reach the spool off one poll, and the depth is the device's.
    #[tokio::test]
    async fn a_reading_carries_the_distance_and_the_devices_own_depth() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        poll.on_body(&id(), &v1_body(1_429.0, 40, 70_000))
            .await
            .expect("admitted");
        let r = &ingress.admissions()[0].batch[0].values;
        assert_eq!(r.snow_distance, Some(1_429.0));
        assert_eq!(
            r.snow_depth,
            Some(71.0),
            "1500 - 1429, the device's arithmetic"
        );
    }

    /// An uncalibrated gauge reports for ever without failing its station: it is working,
    /// and the missing depth is fixed at the device rather than here. The distance is kept
    /// — it is the series `/snow`'s history is made of.
    #[tokio::test]
    async fn an_uncalibrated_gauge_keeps_reporting_rather_than_failing() {
        let ingress = RecordingIngress::accepting();
        let mut poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        for n in 0..SILENT_OCCURRENCES + 2 {
            let body = Frame::new(
                format!(
                    r#"{{"distance":1771,"status":"ok","readings":{},"trim_mean":1771.0,
                         "base_distance":null,"snow_depth":null,"uptime_ms":{}}}"#,
                    40 + n,
                    70_000 + u64::from(n) * 5_000
                )
                .into_bytes(),
            );
            poll.on_body(&id(), &body).await.unwrap_or_else(|e| {
                panic!("occurrence {n}: an uncalibrated gauge is still reporting: {e}")
            });
        }
        let admissions = ingress.admissions();
        assert_eq!(admissions.len() as u32, SILENT_OCCURRENCES + 2);
        assert!(admissions
            .iter()
            .all(|a| a.batch[0].values.snow_distance == Some(1771.0)
                && a.batch[0].values.snow_depth.is_none()));
    }

    /// An unknown station is a counted refusal and **not** a failed occurrence: the
    /// component is working and the configuration is not (§4.4).
    #[tokio::test]
    async fn an_unknown_station_is_a_counted_refusal_rather_than_a_failed_occurrence() {
        let ingress = RecordingIngress::refusing(Reject::UnknownStation);
        poll(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_body(&id(), &body(1_200, "ok", 40, "1200.0", 70_000))
            .await
            .expect("an Ignorable refusal is not an occurrence failure");
    }

    /// The sentence an operator reads off a silent gauge, with `/health`'s two diagnostic
    /// numbers in it.
    #[test]
    fn the_telemetry_sentence_separates_an_unwired_sensor_from_an_unparseable_one() {
        let unwired = render(&SnowHealth {
            bytes_rx: Some(0),
            fw_version: Some("0.1.0".into()),
            fw_slot: Some("ota_0".into()),
            rssi: Some(-56),
            heap_free: Some(164_228),
            ..SnowHealth::default()
        });
        assert!(
            unwired.contains("no bytes at all from the sensor"),
            "{unwired}"
        );
        assert!(unwired.contains("power and wiring"), "{unwired}");
        assert!(unwired.contains("firmware 0.1.0"), "{unwired}");

        let talking = render(&SnowHealth {
            bytes_rx: Some(240),
            ..SnowHealth::default()
        });
        assert_eq!(talking, "It has received 240 bytes from the sensor.");
        // A firmware that answered with none of these must not produce an empty sentence.
        assert!(render(&SnowHealth::default()).contains("none of the fields"));
    }

    #[test]
    fn the_old_firmware_message_names_the_upgrade_rather_than_the_status() {
        let text = OldFirmware {
            authority: "snowgauge-f878.local".into(),
            path: SNOW_DATA,
            telemetry: "firmware 0.0.9.".into(),
        }
        .to_string();
        assert!(text.contains("404 on `/data`"), "{text}");
        assert!(text.contains("firmware 0.0.9"), "{text}");
        assert!(text.contains("Upgrade the gauge"), "{text}");
        assert!(text.contains("removed in firmware v0.1.x"), "{text}");
        assert_eq!(
            OldFirmware {
                authority: String::new(),
                path: SNOW_DATA,
                telemetry: String::new()
            }
            .class(),
            Class::RETRYABLE
        );
        assert_eq!(
            Silent {
                authority: String::new(),
                occurrences: 1,
                reason: String::new(),
                telemetry: String::new()
            }
            .class(),
            Class::RETRYABLE
        );
    }

    /// A homelab's `_http._tcp` group is a printer, a NAS, three dashboards and the gauge,
    /// so the prefix is what makes a browse an answer rather than a list to read.
    #[test]
    fn only_a_gauge_is_taken_out_of_the_services_that_answer() {
        assert!(is_gauge("snowgauge-f878", "snowgauge-f878.local"));
        // Either name on its own, because an instance name is free text an operator could
        // have renamed and the hostname is the firmware's.
        assert!(is_gauge("Driveway gauge", "snowgauge-f878.local"));
        assert!(is_gauge("snowgauge-f878", "roof.local"));
        // Case-insensitively: DNS names are (RFC 4343) and a responder may answer in any
        // case.
        assert!(is_gauge("SnowGauge-F878", "SNOWGAUGE-F878.LOCAL"));

        assert!(!is_gauge("printer", "printer.local"));
        assert!(!is_gauge("nas", "nas.local"));
        // A name that merely contains the prefix is not one: the firmware puts it first.
        assert!(!is_gauge("my-snowgauge-f878", "my-snowgauge-f878.local"));
        assert_eq!(HOSTNAME_PREFIX, "snowgauge-");
        assert_eq!(DISCOVER_TIMEOUT, Duration::from_secs(2));
    }

    /// The address an operator is shown is one the `transport_url` column's own parser has
    /// accepted, and port 80 is the default so it carries no port.
    #[test]
    fn a_discovered_gauge_renders_as_an_address_a_station_can_hold() {
        let gauge = Discovered {
            host: "snowgauge-f878.local".into(),
            addr: None,
            port: 80,
            firmware: Some("0.1.0".into()),
            slot: Some("ota_0".into()),
        };
        assert_eq!(
            gauge.endpoint().expect("a legal address").to_string(),
            "http://snowgauge-f878.local"
        );
        let moved = Discovered {
            port: 8080,
            ..gauge
        };
        assert_eq!(
            moved.endpoint().expect("a legal address").to_string(),
            "http://snowgauge-f878.local:8080"
        );
    }

    #[test]
    fn the_period_the_floor_and_the_request_timeout_are_consistent() {
        assert_eq!(DEFAULT_POLL, Duration::from_secs(5));
        assert!(POLL_FLOOR <= DEFAULT_POLL);
        // `deadline <= period` is enforced at plan time, and the request has to finish
        // inside the deadline for its own message to be the one an operator reads.
        assert!(REQUEST_TIMEOUT < DEFAULT_POLL);
        // **Both of them**, because a fault path makes two requests in one occurrence: the
        // `/data` poll that found nothing, and the `/health` read that explains it. Two
        // full request timeouts would outrun the deadline and report `DeadlineExceeded`,
        // which names nothing, instead of the sentence this pair exists to produce.
        assert!(HEALTH_TIMEOUT < REQUEST_TIMEOUT);
        assert!(REQUEST_TIMEOUT + HEALTH_TIMEOUT < DEFAULT_POLL);
        // The silent window has to outlast the ~1 minute `trim_mean` takes to land, or a
        // gauge that is merely warming up would fail its station.
        assert!(DEFAULT_POLL * SILENT_OCCURRENCES >= Duration::from_secs(60));
        assert_eq!(
            station_id("suncrest:gauge").expect("a legal id").as_str(),
            "station:snowgauge:suncrest:gauge"
        );
        assert!(station_id("Snow Gauge").is_err());
        assert_eq!(STATION_TYPE, "snowgauge");
    }

    #[test]
    fn the_debug_representation_carries_no_handle() {
        let rendered = format!("{:?}", poll(RecordingIngress::accepting()));
        assert!(rendered.contains("snowgauge-f878.local"), "{rendered}");
        assert!(rendered.contains("suncrest:gauge"), "{rendered}");
        assert!(rendered.contains("calibrated"), "{rendered}");
        assert!(!rendered.contains("Ingress"), "{rendered}");
    }
}
