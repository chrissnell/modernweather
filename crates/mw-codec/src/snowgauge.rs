//! `SnowGaugeData` — the body of `GET /data` from a `snowgauge-rs` gauge, and
//! [`snow_health`], the body of `GET /health`.
//!
//! The gauge is an ultrasonic rangefinder that reports **distance to the snow surface in
//! millimetres**. The firmware is `chrissnell/snowgauge-rs`, whose `docs/integration.md`
//! is the authority for everything in this module: two unauthenticated JSON endpoints over
//! plain HTTP on port 80, **poll only** — no webhooks, no push, no history on the device.
//! So the gauge is `HttpOut`, `Fixed` identity and `Every`, exactly like the AirGradient
//! monitor, and one response is one observation.
//!
//! The depth is the **device's**: it holds its calibration in NVS and reports
//! `snow_depth` derived from it, which this codec carries. `mw_pipeline::enrich` derives
//! one only for a reading that carries none, which is a gauge on firmware older than
//! v0.1.0.
//!
//! # The predecessor, and why none of it is kept
//!
//! The gauge used to speak gRPC: `snowgauge.SnowGaugeService/StreamReading`, a server
//! stream of a four-field protobuf message, which is what the Go daemon dialled and what
//! this module decoded by hand (`mw-proto` is in this crate's own layer, so the message was
//! two `prost` tags declared here). That firmware is **decommissioned**, so the schema, the
//! method path, the hand-declared message and `CodecError::Protobuf` are all gone rather
//! than kept behind a flag: a compatibility path nothing speaks is a second decoder to keep
//! correct, and `/data` and `StreamReading` have no configuration in common — the operator
//! has to retype the transport either way.
//!
//! # `GET /data`
//!
//! ```json
//! { "distance": 551, "status": "ok", "readings": 36, "trim_mean": 551.0, "uptime_ms": 70825 }
//! ```
//!
//! | Field | What this build does with it |
//! |---|---|
//! | `trim_mean` | **the measurement**, when it is there: the trimmed mean of the middle 24 of the last 30 samples, which is the firmware's own outlier rejection and what its documentation says to prefer for logging and graphing |
//! | `distance` | the measurement until `trim_mean` lands, about a minute after boot |
//! | `status` | `"no_data"` means the sensor has sent no frames since boot, and there is **no reading** |
//! | `readings`, `uptime_ms` | not measurements: the liveness pair [`SnowLiveness`] carries to the driver |
//! | `base_distance` | the gauge's own calibration, in mm. `null` until somebody calibrates it |
//! | `snow_depth` | the gauge's own `base_distance − reading`, **signed**, and what reaches the `snow_depth` metric |
//! | `rssi` | signal strength in dBm, not stored. `0` is the firmware's "lookup failed" |
//!
//! # Zero is not a distance, and that is the one behavioural change
//!
//! `distance` is **`0` until the first frame arrives**, and `status` is `"no_data"` for the
//! same window. The Go driver's predecessor stored whatever number came off the wire and
//! left `mw_pipeline::enrich::snow_depth`'s `> 0` guard to drop the depth — which was right
//! for a protocol where zero meant "the sensor said zero", and is wrong for this one, where
//! zero is documented to mean "ask again". Stored, it is a `snowdistance` of 0 mm in a
//! column with `min`/`mean`/`max` aggregates over it, next to a NULL depth; refused, it is
//! a counted drop with a sentence. So a frame that carries no measurement is
//! [`CodecError::Incomplete`] and not a reading, and `mw_station::snowgauge` is what
//! decides how long a gauge may stay in that state before the station is failed.
//!
//! **The guard is on the value this codec selected, not on the raw field**, and the first
//! version of it was not: a `trim_mean` of `0.0` is finite, so it was chosen as the
//! measurement and admitted while `distance` was never consulted. The mean of thirty zeroes
//! is a sensor fault by exactly the reading of the firmware's `0` above, and a negative
//! mean is not a distance at all, so one branch refuses all three.
//!
//! # The depth is the device's, and so is the calibration
//!
//! The gauge stores its calibrated sensor-to-ground distance in NVS — set through the
//! device's own web interface, surviving a reboot and an OTA — and reports both that
//! `base_distance` and the `snow_depth` it derives from it. So this codec does no
//! arithmetic: it carries the gauge's depth onto the `snow_depth` metric, which
//! `metrics/registry.toml` declares in millimetres exactly as the firmware reports it.
//!
//! **That retires DEV-9.** The inherited constraint was that
//! `snowgauge/station.go:172-176` discarded a distance greater than the station's
//! *configured* base distance, because M3-3 moved `snowdepth` to ingest and a wrong
//! configuration would otherwise store a negative depth into a column with
//! `min`/`mean`/`max` aggregates over it. Two things dissolve it: the base distance is no
//! longer configuration that can disagree with the device, and a negative depth is no
//! longer a symptom of that disagreement. The firmware reports the sign deliberately — a
//! stable reading that overshot the base is frost heave or a calibration coming due — and
//! the owner's instruction is to record it rather than clamp it, because a zero would hide
//! the thing worth seeing.
//!
//! **An uncalibrated gauge still reports.** `base_distance` and `snow_depth` are both
//! `null` until somebody calibrates, and the reading that carries a distance and no depth
//! is a complete reading of a working device, not a refusal. `is_calibrated` is how a
//! driver surfaces the state; the depth simply starts arriving once the calibration is
//! done, with no configuration change and no restart.
//!
//! # `GET /health`
//!
//! Operational telemetry and **not a reading**: no registry metric describes a free-heap
//! figure, and inventing nine `extrafloat` slots for one is what
//! `crate::airgradient`'s own documentation calls a holding pen rather than a model. So
//! [`snow_health`] returns a struct, the driver reads it on the two paths its
//! documentation names — a gauge whose sensor has gone silent, and a `404` that means old
//! firmware — and what reaches an operator is a sentence rather than a series.

use mw_types::{PartialReading, Reading};
use serde::Deserialize;

use crate::frame::{Codec, CodecError, Frame};

/// `GET /data` — the measurement endpoint.
pub const SNOW_DATA: &str = "/data";

/// `GET /health` — the telemetry endpoint.
pub const SNOW_HEALTH: &str = "/health";

/// `GET /calibration` — the calibration-state endpoint. Unauthenticated, like the two
/// above: only `POST /calibrate` and `POST /ota` need the admin credential.
pub const SNOW_CALIBRATION: &str = "/calibration";

/// The `status` value a gauge with a working sensor reports.
pub const SNOW_STATUS_OK: &str = "ok";

/// The `status` value a gauge whose sensor has sent nothing since boot reports.
pub const SNOW_STATUS_NO_DATA: &str = "no_data";

/// `GET /data`, as this build reads it.
///
/// `distance` is a `u16` on the wire and a `u32` here, so that a firmware fault reporting a
/// wider number decodes rather than failing the parse with a sentence that says only "not
/// the JSON this codec decodes" — the guards below then refuse it by what it is.
/// `trim_mean` defaults, because `null` and
/// absent mean the same thing — "not yet" — and a firmware that stopped sending the key
/// should degrade to the raw distance rather than stop the station.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SnowData {
    /// Most recent raw distance in millimetres. `0` before the first reading.
    pub distance: u32,
    /// `"ok"` or `"no_data"`.
    pub status: String,
    /// Monotonic count of valid frames parsed off the sensor's UART since boot.
    pub readings: u64,
    /// The trimmed mean of the middle 24 of the last 30 samples, in millimetres.
    #[serde(default)]
    pub trim_mean: Option<f32>,
    /// The gauge's calibrated sensor-to-ground distance, in millimetres.
    ///
    /// `None` until somebody calibrates it. Persisted in the device's NVS — the firmware
    /// reads it back with `get_u16`, so it survives a reboot and an OTA — which is what
    /// makes it the device's property rather than this daemon's.
    #[serde(default)]
    pub base_distance: Option<u32>,

    /// The gauge's own `base_distance − current_reading`, in millimetres, **signed**.
    ///
    /// The firmware computes it as `base as i32 - trim_mean.round() as i32` when a trimmed
    /// mean exists and against the raw distance otherwise — the same preference this codec
    /// applies to the distance, which is why the two numbers in one reading agree about
    /// which sample they describe.
    ///
    /// **A negative is a measurement, not a fault**, and it is stored as one: the firmware
    /// documents it as a stable reading that overshot the calibrated base — frost heave, or
    /// a calibration that is drifting — and the owner's instruction is to record it. The
    /// predecessor of this field could not represent that at all.
    ///
    /// `None` when the gauge is uncalibrated or has no reading yet.
    #[serde(default)]
    pub snow_depth: Option<i32>,

    /// Signal strength in dBm, which `/data` carries as of firmware v0.1.0.
    ///
    /// Not a stored metric — no registry entry describes one — and carried so that an
    /// operator-facing read has it without a second request. **`0` is the firmware's
    /// sentinel for "the access-point lookup failed"**, not a strong signal, so a consumer
    /// has to filter it rather than render it.
    #[serde(default)]
    pub rssi: Option<i32>,

    /// Milliseconds since boot. A decrease is a reboot.
    pub uptime_ms: u64,
}

/// The two counters that say whether a gauge's **sensor** is alive, as distinct from
/// whether the gauge answered.
///
/// A gauge whose UART link to the rangefinder has died keeps serving `/data`, keeps
/// growing `uptime_ms`, and keeps returning the last `trim_mean` it computed — so "the
/// request succeeded" says nothing. `readings` static while `uptime_ms` grows is the
/// firmware documentation's own liveness test, and this pair is what a driver has to carry
/// across occurrences to apply it. `mw_station::snowgauge` is where that state lives;
/// what is here is the pure comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnowLiveness {
    /// `readings`.
    pub readings: u64,
    /// `uptime_ms`.
    pub uptime_ms: u64,
}

impl SnowLiveness {
    /// Whether the gauge rebooted between `self` and `next`.
    ///
    /// `uptime_ms` is milliseconds since boot, so it decreases across exactly one event.
    /// Both counters restart there, which is why this is asked **before**
    /// [`SnowLiveness::sensor_advanced`]: a reboot resets `readings` to zero and a smaller
    /// number would otherwise read as a sensor that had stopped.
    #[must_use]
    pub fn rebooted_into(self, next: SnowLiveness) -> bool {
        next.uptime_ms < self.uptime_ms
    }

    /// Whether the sensor produced at least one new frame between `self` and `next`.
    #[must_use]
    pub fn sensor_advanced(self, next: SnowLiveness) -> bool {
        next.readings > self.readings
    }
}

impl SnowData {
    /// The liveness pair this body carries.
    #[must_use]
    pub fn liveness(&self) -> SnowLiveness {
        SnowLiveness {
            readings: self.readings,
            uptime_ms: self.uptime_ms,
        }
    }

    /// Whether the gauge says its sensor is producing frames.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.status == SNOW_STATUS_OK
    }

    /// Whether the gauge has a calibration, which is what makes a depth possible.
    ///
    /// An uncalibrated gauge is **working**: it reports a distance and no depth, and the
    /// thing it cannot report is one a person fixes at the device's own web interface. So
    /// this is a state to surface, not a reading to refuse.
    #[must_use]
    pub fn is_calibrated(&self) -> bool {
        // `> 0` for the reason `mw_pipeline::enrich::snow_depth` and the backfill migration
        // both guard it: a zero base distance is not a calibration, and if the firmware's
        // NVS read ever yields one for "unset" rather than omitting the key, treating it as
        // calibrated would mean the uncalibrated path never fires.
        self.base_distance.is_some_and(|mm| mm > 0)
    }
}

/// `GET /health`, as this build reads it.
///
/// Every field is optional and none of them is a measurement: this is read on a fault path
/// to turn "the gauge is not reporting" into a sentence naming the firmware version and the
/// sensor's byte count, and a telemetry endpoint that dropped a key must not be the reason
/// an operator gets no sentence at all.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SnowHealth {
    /// Always `"connected"` — a gauge whose WiFi is down answers nothing.
    #[serde(default)]
    pub wifi: Option<String>,
    /// The station interface's address.
    #[serde(default)]
    pub ip: Option<String>,
    /// Signal strength in dBm. `-30` is strong, `-80` loses packets.
    #[serde(default)]
    pub rssi: Option<i32>,
    /// `snowgauge-<4hex>`, with no `.local`.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Cumulative bytes received from the sensor. **The cross-check** for a `status` of
    /// `no_data`: still zero is a sensor that is not wired or not powered, and a growing
    /// count with no `readings` is a sensor whose framing the gauge cannot parse.
    #[serde(default)]
    pub bytes_rx: Option<u64>,
    /// Free heap in bytes. ~160 KB is the healthy baseline; a downward trend is a leak.
    #[serde(default)]
    pub heap_free: Option<u64>,
    /// The running firmware version.
    #[serde(default)]
    pub fw_version: Option<String>,
    /// The active OTA partition, `ota_0` or `ota_1`.
    #[serde(default)]
    pub fw_slot: Option<String>,
}

/// Decode one `GET /health` body.
///
/// A free function and not a [`Codec`], because it produces no readings: a codec that
/// returned an empty batch for a well-formed telemetry document would be a decoder whose
/// success and whose failure look the same to the one counter that watches it.
pub fn snow_health(frame: &Frame) -> Result<SnowHealth, CodecError> {
    serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })
}

/// `GET /calibration` — the gauge's calibration state, as one of five shapes.
///
/// Four are the firmware's (`net/server.rs`'s `calibration_json`); the fifth is
/// [`SnowCalibration::Other`], because this is read in order to be **shown** and a firmware
/// newer than this daemon must not blank the panel that shows it.
///
/// The operator runs a calibration from the gauge's own web interface — this build only
/// reads the result (the owner's instruction, 2026-11-21), which is why there is no
/// encoder here and nothing that needs the admin credential `POST /calibrate` requires.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnowCalibration {
    /// Never calibrated, or cleared since boot.
    Idle,
    /// The sample window is open.
    Running {
        /// Milliseconds since the window opened.
        elapsed_ms: u64,
        /// Milliseconds left in it.
        remaining_ms: u64,
        /// Samples collected so far.
        samples: u32,
    },
    /// Finished. The new base distance is live on `/data` immediately.
    Complete {
        /// The calibrated sensor-to-ground distance, in millimetres.
        base_distance: u32,
        /// How many samples the trimmed mean used.
        samples_used: u32,
        /// Device uptime when it finished — the same clock as `/data.uptime_ms`.
        finished_at_ms: u64,
    },
    /// The window expired with nothing in it: the sensor is dark or disconnected.
    Failed {
        /// The firmware's own sentence.
        reason: String,
        /// Device uptime when it gave up.
        finished_at_ms: u64,
    },
    /// A state this build does not know, from a firmware newer than it.
    #[serde(other)]
    Other,
}

/// Decode one `GET /data` body **without** asking it for a reading.
///
/// [`SnowGaugeData::read`] is the collection path and refuses a body that carries no
/// storable measurement — a gauge whose sensor has not spoken, or one still showing a zero
/// distance. An operator-facing read wants the body anyway: a gauge with a dead sensor
/// still reports what it is calibrated to, and that is exactly the moment somebody goes
/// looking for the number. So the parse is available on its own, and the refusal stays
/// where the storing happens.
pub fn snow_data(frame: &Frame) -> Result<SnowData, CodecError> {
    serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })
}

/// Decode one `GET /calibration` body.
pub fn snow_calibration(frame: &Frame) -> Result<SnowCalibration, CodecError> {
    serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })
}

/// The snow gauge codec.
///
/// **Stateless, where it used to hold the station's configured base distance.** The gauge
/// persists its own calibration and reports the depth derived from it, so there is nothing
/// per-station to hold here and nothing to derive — which also retires **DEV-9**'s
/// discard. That discard existed because a *configured* base distance could be wrong and a
/// distance past it would store a negative depth; the device's calibration cannot disagree
/// with itself, and a negative depth is now a measurement the owner has asked to keep.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnowGaugeData;

impl SnowGaugeData {
    /// Decode one `/data` body into the reading it carried **and** the body itself.
    ///
    /// The driver needs both halves of one parse: the batch to admit, and `readings` and
    /// `uptime_ms` to decide whether the gauge's sensor is still alive. Parsing the body
    /// twice — once through [`Codec::decode`] and once for the counters — would be two
    /// readers of the same bytes that can disagree about them, so this is the seam and
    /// [`Codec::decode`] is written over it.
    pub fn read(&self, frame: &Frame) -> Result<(SnowData, Vec<PartialReading>), CodecError> {
        let data: SnowData =
            serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })?;
        let batch = self.reading(&data)?;
        Ok((data, batch))
    }

    /// The reading one parsed body carries, or why it carries none.
    fn reading(&self, data: &SnowData) -> Result<Vec<PartialReading>, CodecError> {
        if !data.is_ok() {
            // Reported by the gauge rather than inferred, and the two spellings are
            // separated because they are different sentences: one is a sensor that has
            // never spoken, the other is a firmware this build does not understand.
            return Err(CodecError::Incomplete {
                what: if data.status == SNOW_STATUS_NO_DATA {
                    "snow-gauge distance: the gauge reports `no_data`, so its sensor has \
                     produced no frames since boot. Check the sensor's power and wiring, \
                     and cross-reference `bytes_rx` on the gauge's own `/health`"
                } else {
                    "snow-gauge distance: the gauge's `status` is neither `ok` nor \
                     `no_data`, so this build cannot tell whether its sensor is reporting"
                },
            });
        }
        // `trim_mean` is the firmware's own outlier rejection over the last thirty samples
        // and is what its documentation says to prefer; `distance` is the fallback for the
        // first minute after boot, before thirty have landed. A non-finite trimmed mean is
        // treated as absent rather than carried: it would reach a `float4` column as a NaN
        // whose aggregates are NaN too.
        let distance = data
            .trim_mean
            .filter(|mm| mm.is_finite())
            .unwrap_or(data.distance as f32);
        // **The test is on the value that would be STORED, not on the raw field**, and
        // that distinction is load-bearing: a `trim_mean` of `0.0` is finite, so a guard
        // that only looked at `distance` would admit it and write a 0 mm `snowdistance`
        // into a column with `min`/`mean`/`max` aggregates over it — which is the exact
        // contamination this module's documentation says is refused. The mean of thirty
        // zeroes is a sensor fault by the same reading of the firmware's `0`, and so is a
        // negative one, which no physical distance is.
        if distance <= 0.0 {
            return Err(CodecError::Incomplete {
                what: "snow-gauge distance: no measurement has reached the gauge since \
                       boot, or its sensor is reporting a distance of zero, which is the \
                       firmware's own `no reading yet`",
            });
        }
        Ok(vec![PartialReading::new(Reading {
            snow_distance: Some(distance),
            // **The device's depth, verbatim, sign and all.** It was the enrich stage's
            // subtraction of a configured base distance (DEV-9); the gauge now holds its
            // own calibration in NVS and does that arithmetic itself, so carrying its
            // answer is the whole of the mapping.
            //
            // `mw_pipeline::enrich` is what makes this final rather than one of two
            // numbers: `derive` fills a slot **only** where the reading left it `None`, so
            // a station-reported depth is never recomputed and there is no second
            // subtraction to disagree with this one.
            //
            // Negatives are kept deliberately (the owner's instruction, 2026-11-21): the
            // firmware reports the sign to say that a stable reading overshot the base, and
            // clamping it to zero would hide frost heave and a drifting calibration behind
            // a plausible number.
            snow_depth: data.snow_depth.map(|mm| mm as f32),
            ..Reading::default()
        })])
    }
}

impl Codec for SnowGaugeData {
    fn name(&self) -> &'static str {
        "snowgauge-data"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        self.read(frame).map(|(_, batch)| batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document the firmware's own `docs/integration.md` prints.
    const DATA_BODY: &str = r#"{"distance":551,"status":"ok","readings":36,
                                "trim_mean":551.0,"uptime_ms":70825}"#;

    /// The `/health` document from the same page.
    const HEALTH_BODY: &str = r#"{"wifi":"connected","ip":"10.50.0.175","rssi":-56,
                                  "hostname":"snowgauge-f878","bytes_rx":240,
                                  "heap_free":164228,"fw_version":"0.1.0","fw_slot":"ota_0"}"#;

    /// A pre-v0.1.0 body: no `base_distance`, no `snow_depth`, no `rssi`. Kept as the
    /// default helper because every guard around the *distance* is independent of them.
    fn body(distance: u32, status: &str, readings: u64, trim: &str, uptime: u64) -> Frame {
        Frame::new(
            format!(
                r#"{{"distance":{distance},"status":"{status}","readings":{readings},
                     "trim_mean":{trim},"uptime_ms":{uptime}}}"#
            )
            .into_bytes(),
        )
    }

    /// A v0.1.0 body. `base` and `depth` are JSON literals so a test can write `null`.
    fn v1_body(distance: u32, trim: &str, base: &str, depth: &str) -> Frame {
        Frame::new(
            format!(
                r#"{{"distance":{distance},"status":"ok","readings":1288,
                     "trim_mean":{trim},"base_distance":{base},"snow_depth":{depth},
                     "rssi":-63,"uptime_ms":2369755}}"#
            )
            .into_bytes(),
        )
    }

    fn decode(codec: &SnowGaugeData, frame: &Frame) -> Vec<PartialReading> {
        codec.decode(frame).expect("decodes")
    }

    #[test]
    fn the_documented_body_decodes_to_the_trimmed_mean() {
        let codec = SnowGaugeData;
        let (data, batch) = codec
            .read(&Frame::new(DATA_BODY.as_bytes().to_vec()))
            .expect("decodes");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].values.snow_distance, Some(551.0));
        assert_eq!(
            batch[0].values.snow_depth, None,
            "the depth is the enrich stage's subtraction (DEV-9)"
        );
        assert_eq!(
            batch[0].observed_at, None,
            "`/data` is the gauge's current state and carries no instant"
        );
        assert_eq!(
            data.liveness(),
            SnowLiveness {
                readings: 36,
                uptime_ms: 70_825
            }
        );
        assert!(data.is_ok());
    }

    /// The firmware's own preference, asserted as a *choice*: the two fields disagree in
    /// the fixture, so a codec reading the raw one is visible.
    #[test]
    fn the_trimmed_mean_wins_over_the_raw_distance_and_the_raw_one_is_the_fallback() {
        let codec = SnowGaugeData;
        let batch = decode(&codec, &body(900, "ok", 40, "551.0", 70_825));
        assert_eq!(batch[0].values.snow_distance, Some(551.0));
        // The first minute after boot: fewer than thirty samples, so no trimmed mean yet.
        let batch = decode(&codec, &body(900, "ok", 12, "null", 20_000));
        assert_eq!(batch[0].values.snow_distance, Some(900.0));
        // A NaN trimmed mean would reach a `float4` column whose aggregates are NaN too.
        let batch = decode(&codec, &body(900, "ok", 40, "null", 20_000));
        assert_eq!(batch[0].values.snow_distance, Some(900.0));
    }

    /// `status: "no_data"` is the gauge saying its sensor has never spoken, and the
    /// sentence has to send an operator to the wiring rather than to this daemon.
    #[test]
    fn a_gauge_whose_sensor_has_never_spoken_carries_no_reading() {
        let codec = SnowGaugeData;
        let e = codec
            .decode(&body(0, "no_data", 0, "null", 4_000))
            .expect_err("no measurement");
        let CodecError::Incomplete { what } = e else {
            panic!("expected an incomplete body, got {e}");
        };
        assert!(what.contains("no_data"), "{what}");
        assert!(what.contains("power and wiring"), "{what}");
        assert!(what.contains("bytes_rx"), "{what}");

        // A status this build does not know is a different sentence, because it is a
        // different problem: the gauge is answering and this daemon cannot read it.
        let e = codec
            .decode(&body(551, "degraded", 40, "551.0", 70_000))
            .expect_err("not a status this build reads");
        let CodecError::Incomplete { what } = e else {
            panic!("expected an incomplete body, got {e}");
        };
        assert!(what.contains("neither `ok` nor `no_data`"), "{what}");
    }

    /// **Zero is not a distance**, which is this module's one behavioural change from the
    /// gRPC predecessor — and the reason is arithmetic: stored under a 1500 mm base, a zero
    /// distance is a 1500 mm snowpack the moment the depth stops being NULL.
    #[test]
    fn a_gauge_that_has_not_measured_yet_carries_no_reading_rather_than_a_zero() {
        let codec = SnowGaugeData;
        let e = codec
            .decode(&body(0, "ok", 0, "null", 900))
            .expect_err("no measurement");
        let CodecError::Incomplete { what } = e else {
            panic!("expected an incomplete body, got {e}");
        };
        assert!(what.contains("distance of zero"), "{what}");
        assert_eq!(
            e.to_string(),
            "the device's answer carries no snow-gauge distance: no measurement has \
             reached the gauge since boot, or its sensor is reporting a distance of zero, \
             which is the firmware's own `no reading yet`"
        );
        // A trimmed mean that HAS landed is a measurement even if the raw field is zero:
        // the mean is over the last thirty samples and the raw field is the last one.
        let batch = decode(&codec, &body(0, "ok", 40, "551.0", 70_000));
        assert_eq!(batch[0].values.snow_distance, Some(551.0));
    }

    /// **The hole the first version of this guard had.** The test was on the raw
    /// `distance` field, and a `trim_mean` of `0.0` is *finite* — so a gauge whose sensor
    /// reports zeroes for thirty samples admitted a 0 mm `snowdistance` into a column with
    /// `min`/`mean`/`max` aggregates over it, which is the contamination the test above
    /// says is refused. The guard is on the selected value now, so both spellings of zero
    /// and a negative mean are refused by the same branch.
    #[test]
    fn a_zero_or_negative_trimmed_mean_is_refused_like_a_zero_raw_distance() {
        let codec = SnowGaugeData;
        for trim in ["0.0", "-0.0", "-120.5"] {
            // The raw field is a plausible distance here, so a guard that read it instead
            // of the value it had selected would admit the reading.
            let e = codec
                .decode(&body(1_200, "ok", 40, trim, 70_000))
                .unwrap_err();
            assert!(
                matches!(e, CodecError::Incomplete { .. }),
                "trim_mean {trim}: {e}"
            );
        }
        // And an unconfigured gauge is no different: this is about the gauge having
        // nothing to give, not about the base distance.
        assert!(matches!(
            SnowGaugeData.decode(&body(1_200, "ok", 40, "0.0", 70_000)),
            Err(CodecError::Incomplete { .. })
        ));
        // A raw distance of zero with no trimmed mean is the same refusal, which is the
        // branch that already worked.
        assert!(matches!(
            codec.decode(&body(0, "ok", 40, "null", 70_000)),
            Err(CodecError::Incomplete { .. })
        ));
    }

    /// The v0.1.0 shape: the gauge did the subtraction, and the codec carries both numbers
    /// off one parse.
    #[test]
    fn a_calibrated_gauge_reports_its_own_depth_and_the_codec_carries_it() {
        let (data, batch) = SnowGaugeData
            .read(&v1_body(1_771, "1700.0", "1771", "71"))
            .expect("decodes");
        assert_eq!(batch.len(), 1);
        // The distance is still the trimmed mean, which is what `snowdistance` means and
        // what the pre-cutover history is made of.
        assert_eq!(batch[0].values.snow_distance, Some(1700.0));
        // And the depth is the DEVICE's number, not `base - trim_mean` computed here.
        assert_eq!(batch[0].values.snow_depth, Some(71.0));
        assert_eq!(data.base_distance, Some(1_771));
        assert_eq!(data.rssi, Some(-63));
        assert!(data.is_calibrated());
    }

    /// **The owner's instruction, 2026-11-21: record a negative depth.**
    ///
    /// The firmware reports the sign to say a stable reading overshot the calibrated base
    /// — frost heave, or a calibration coming due. Clamping it to zero would hide exactly
    /// that behind a plausible number, so it is stored as it arrives. Zero is a
    /// measurement too: bare ground.
    #[test]
    fn a_negative_depth_is_recorded_rather_than_clamped_or_dropped() {
        for (depth, expected) in [("-4", -4.0), ("-250", -250.0), ("0", 0.0)] {
            let batch = SnowGaugeData
                .decode(&v1_body(1_775, "1775.0", "1771", depth))
                .unwrap_or_else(|e| panic!("depth {depth} is a reading, not a fault: {e}"));
            assert_eq!(batch[0].values.snow_depth, Some(expected), "depth {depth}");
            // And the distance that produced it is stored alongside, unaffected: the
            // zero-distance guard is about the DISTANCE and says nothing about the depth.
            assert_eq!(batch[0].values.snow_distance, Some(1775.0));
        }
    }

    /// An uncalibrated gauge is a working gauge with nothing to subtract from. The reading
    /// carries a distance and no depth, and that is a complete reading rather than a
    /// refusal: the depth starts arriving when somebody calibrates, with no configuration
    /// change and no restart.
    #[test]
    fn an_uncalibrated_gauge_reports_a_distance_and_no_depth() {
        let (data, batch) = SnowGaugeData
            .read(&v1_body(1_771, "1771.0", "null", "null"))
            .expect("decodes");
        assert_eq!(batch[0].values.snow_distance, Some(1771.0));
        assert_eq!(
            batch[0].values.snow_depth, None,
            "nothing to derive it from"
        );
        assert_eq!(data.base_distance, None);
        assert!(!data.is_calibrated());
    }

    /// A firmware that predates the three new keys still decodes, which is what makes the
    /// upgrade orderless: this daemon can poll a gauge that has not been reflashed yet and
    /// store its distance exactly as before.
    #[test]
    fn a_body_without_the_new_fields_still_decodes_to_a_distance() {
        let (data, batch) = SnowGaugeData
            .read(&body(1_771, "ok", 1_288, "1771.0", 2_369_755))
            .expect("decodes");
        assert_eq!(batch[0].values.snow_distance, Some(1771.0));
        assert_eq!(batch[0].values.snow_depth, None);
        assert_eq!(data.base_distance, None);
        assert_eq!(data.rssi, None);
    }

    #[test]
    fn bytes_that_are_not_this_body_are_a_counted_drop() {
        let codec = SnowGaugeData;
        for bytes in [
            &b"<html>404</html>"[..],
            // `/health`'s body on `/data`'s path: valid JSON, and none of the four
            // required keys.
            HEALTH_BODY.as_bytes(),
            b"",
        ] {
            let e = codec
                .decode(&Frame::new(bytes.to_vec()))
                .expect_err("not a `/data` body");
            assert!(matches!(e, CodecError::Json { .. }), "{e}");
        }
        let e = codec
            .decode(&Frame::new(vec![0x7b, 0xff, 0x7d]))
            .expect_err("not text");
        assert!(matches!(e, CodecError::NotText), "{e}");
    }

    #[test]
    fn the_liveness_pair_names_a_reboot_before_it_names_a_dead_sensor() {
        let before = SnowLiveness {
            readings: 900,
            uptime_ms: 600_000,
        };
        let after_reboot = SnowLiveness {
            readings: 3,
            uptime_ms: 4_000,
        };
        assert!(before.rebooted_into(after_reboot));
        // **The ordering that matters**: a reboot resets `readings` too, so a driver that
        // asked this question first would report a dead sensor on every reboot.
        assert!(!before.sensor_advanced(after_reboot));

        let still_alive = SnowLiveness {
            readings: 901,
            uptime_ms: 605_000,
        };
        assert!(!before.rebooted_into(still_alive));
        assert!(before.sensor_advanced(still_alive));

        // The firmware's own liveness test: `uptime_ms` grows and `readings` does not.
        let sensor_dead = SnowLiveness {
            readings: 900,
            uptime_ms: 605_000,
        };
        assert!(!before.rebooted_into(sensor_dead));
        assert!(!before.sensor_advanced(sensor_dead));
    }

    #[test]
    fn the_health_body_decodes_to_the_telemetry_a_fault_path_reports() {
        let h = snow_health(&Frame::new(HEALTH_BODY.as_bytes().to_vec())).expect("decodes");
        assert_eq!(h.wifi.as_deref(), Some("connected"));
        assert_eq!(h.ip.as_deref(), Some("10.50.0.175"));
        assert_eq!(h.rssi, Some(-56));
        assert_eq!(h.hostname.as_deref(), Some("snowgauge-f878"));
        assert_eq!(h.bytes_rx, Some(240));
        assert_eq!(h.heap_free, Some(164_228));
        assert_eq!(h.fw_version.as_deref(), Some("0.1.0"));
        assert_eq!(h.fw_slot.as_deref(), Some("ota_0"));

        // Every field optional: this is read on a fault path, and a firmware that dropped
        // a key must not be the reason an operator gets no sentence at all.
        let partial =
            snow_health(&Frame::new(br#"{"fw_version":"0.0.9"}"#.to_vec())).expect("decodes");
        assert_eq!(partial.fw_version.as_deref(), Some("0.0.9"));
        assert_eq!(partial.bytes_rx, None);
        assert!(snow_health(&Frame::new(b"not json".to_vec())).is_err());
    }

    /// The five shapes `/calibration` answers with. Four are the firmware's and the fifth
    /// is this build's tolerance for a firmware newer than itself.
    #[test]
    fn every_calibration_shape_decodes_to_its_own_variant() {
        let idle = snow_calibration(&Frame::new(br#"{"state":"idle"}"#.to_vec())).expect("decodes");
        assert_eq!(idle, SnowCalibration::Idle);

        let running = snow_calibration(&Frame::new(
            br#"{"state":"running","elapsed_ms":12000,"remaining_ms":18000,"samples":7}"#.to_vec(),
        ))
        .expect("decodes");
        assert_eq!(
            running,
            SnowCalibration::Running {
                elapsed_ms: 12_000,
                remaining_ms: 18_000,
                samples: 7
            }
        );

        let complete = snow_calibration(&Frame::new(
            br#"{"state":"complete","base_distance":1771,"samples_used":10,
                 "finished_at_ms":2342978}"#
                .to_vec(),
        ))
        .expect("decodes");
        assert_eq!(
            complete,
            SnowCalibration::Complete {
                base_distance: 1771,
                samples_used: 10,
                finished_at_ms: 2_342_978
            }
        );

        let failed = snow_calibration(&Frame::new(
            br#"{"state":"failed","reason":"no samples collected","finished_at_ms":82000}"#
                .to_vec(),
        ))
        .expect("decodes");
        assert_eq!(
            failed,
            SnowCalibration::Failed {
                reason: "no samples collected".to_string(),
                finished_at_ms: 82_000
            }
        );

        // **A state this build does not know is not a decode failure.** This is read to be
        // shown, and a panel that went blank on a firmware upgrade would be worse than one
        // that says it does not recognise the state.
        assert_eq!(
            snow_calibration(&Frame::new(br#"{"state":"warming_up"}"#.to_vec())).expect("decodes"),
            SnowCalibration::Other
        );
        // Bytes that are not the document still are.
        assert!(snow_calibration(&Frame::new(b"not json".to_vec())).is_err());
        assert_eq!(SNOW_CALIBRATION, "/calibration");
    }

    #[test]
    fn the_codec_names_itself_claims_no_station_and_names_the_two_paths() {
        let codec = SnowGaugeData;
        assert_eq!(codec.name(), "snowgauge-data");
        assert_eq!(
            codec.claimed_station(&Frame::new(DATA_BODY.as_bytes().to_vec())),
            None
        );
        assert_eq!(SNOW_DATA, "/data");
        assert_eq!(SNOW_HEALTH, "/health");
    }
}
