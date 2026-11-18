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
//! The depth is `base_distance - distance` and remains the enrich stage's
//! (`mw_pipeline::enrich::snow_depth`, **R1**).
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
//! # The discard, which is this item's inherited constraint
//!
//! **DEV-9**: `snowgauge/station.go:172-176` discarded a distance **greater than the
//! station's configured base distance** at the device, and M3-3 moved `snowdepth` to
//! ingest — so without the discard such a reading stores a *negative* depth into a column
//! with `min`/`mean`/`max` aggregates over it, where in Go it never reached storage at all.
//! The enrich stage is the wrong layer (it is handed an already-admitted reading, and
//! dropping a metric there would make it a filter rather than a transform), so the discard
//! is here, where the base distance is configuration the codec holds and the reading is a
//! fault the decoder can name.
//!
//! It is a [`CodecError::Implausible`] rather than an empty batch, because an empty batch is
//! indistinguishable from a frame that decoded to nothing: a gauge whose base distance is
//! set two metres too short would otherwise report nothing, count nothing and log nothing.
//! `mw_station::metrics::FrameOutcome::Implausible` is the counter.
//!
//! **A station with no configured base distance discards nothing.** That was the one
//! departure from Go and it still is: `config.DeviceData.BaseSnowDistance` is a
//! non-nullable `float64` there, so an unconfigured gauge has a base of zero, every real
//! reading is `distance > 0`, and the Go driver discarded **all** of them. The column here
//! is nullable with a `CHECK (> 0)`, so "not configured" is expressible: the distance is
//! stored, and `enrich` derives no depth from it because it has no base either.
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

/// The `status` value a gauge with a working sensor reports.
pub const SNOW_STATUS_OK: &str = "ok";

/// The `status` value a gauge whose sensor has sent nothing since boot reports.
pub const SNOW_STATUS_NO_DATA: &str = "no_data";

/// `GET /data`, as this build reads it.
///
/// `distance` is a `u16` on the wire and a `u32` here, so that a firmware fault reporting a
/// wider number is a value the discard below can *name* rather than a parse failure that
/// says only "not the JSON this codec decodes". `trim_mean` defaults, because `null` and
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

/// The snow gauge codec for one station's configured base distance.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnowGaugeData {
    base_distance_mm: Option<f32>,
}

impl SnowGaugeData {
    /// The codec for a gauge mounted `base_distance_mm` above bare ground.
    ///
    /// `None` is a gauge whose `snow_base_distance_mm` column is NULL, which discards
    /// nothing — see this module's documentation.
    #[must_use]
    pub fn new(base_distance_mm: Option<f32>) -> Self {
        // A non-positive or non-finite base is not a base: the column's own `CHECK` is
        // `> 0`, and a zero one here would discard every reading, which is the Go
        // behaviour this type exists to avoid reproducing.
        Self {
            base_distance_mm: base_distance_mm.filter(|mm| mm.is_finite() && *mm > 0.0),
        }
    }

    /// The base distance this codec discards against.
    #[must_use]
    pub fn base_distance_mm(&self) -> Option<f32> {
        self.base_distance_mm
    }

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
        let distance = match data.trim_mean.filter(|mm| mm.is_finite()) {
            Some(mm) => mm,
            // Documented to mean "no reading has arrived yet", which is not a measurement
            // of zero millimetres. This module's documentation has why it is refused
            // rather than stored.
            None if data.distance == 0 => {
                return Err(CodecError::Incomplete {
                    what: "snow-gauge distance: no reading has reached the gauge since \
                           boot, so `distance` is still zero and `trim_mean` is null",
                })
            }
            None => data.distance as f32,
        };
        if let Some(base) = self.base_distance_mm {
            if distance > base {
                return Err(CodecError::Implausible {
                    what: "snow sensor distance, in millimetres",
                    value: f64::from(distance),
                    limit: f64::from(base),
                });
            }
        }
        Ok(vec![PartialReading::new(Reading {
            snow_distance: Some(distance),
            // `snowdepth` is NOT set here. It is `base - distance`, the enrich stage
            // computes it from the station's metadata for every station rather than for the
            // driver that remembered to (DEV-9), and a codec that computed it would be the
            // second implementation of one subtraction.
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

    fn body(distance: u32, status: &str, readings: u64, trim: &str, uptime: u64) -> Frame {
        Frame::new(
            format!(
                r#"{{"distance":{distance},"status":"{status}","readings":{readings},
                     "trim_mean":{trim},"uptime_ms":{uptime}}}"#
            )
            .into_bytes(),
        )
    }

    fn decode(codec: &SnowGaugeData, frame: &Frame) -> Vec<PartialReading> {
        codec.decode(frame).expect("decodes")
    }

    #[test]
    fn the_documented_body_decodes_to_the_trimmed_mean() {
        let codec = SnowGaugeData::new(Some(1500.0));
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
        let codec = SnowGaugeData::new(Some(1500.0));
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
        let codec = SnowGaugeData::new(Some(1500.0));
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
        let codec = SnowGaugeData::new(Some(1500.0));
        let e = codec
            .decode(&body(0, "ok", 0, "null", 900))
            .expect_err("no measurement");
        let CodecError::Incomplete { what } = e else {
            panic!("expected an incomplete body, got {e}");
        };
        assert!(what.contains("still zero"), "{what}");
        assert_eq!(
            e.to_string(),
            "the device's answer carries no snow-gauge distance: no reading has reached \
             the gauge since boot, so `distance` is still zero and `trim_mean` is null"
        );
        // A trimmed mean that HAS landed is a measurement even if the raw field is zero:
        // the mean is over the last thirty samples and the raw field is the last one.
        let batch = decode(&codec, &body(0, "ok", 40, "551.0", 70_000));
        assert_eq!(batch[0].values.snow_distance, Some(551.0));
    }

    /// The inherited constraint. The Go driver discarded at the device; this discards at
    /// the decoder, and the number it would otherwise store is the one in the body.
    #[test]
    fn a_distance_past_the_base_is_a_named_discard_rather_than_a_negative_depth() {
        let codec = SnowGaugeData::new(Some(1500.0));
        let e = codec
            .decode(&body(1600, "ok", 40, "1600.0", 70_000))
            .expect_err("a fault");
        let CodecError::Implausible { value, limit, .. } = e else {
            panic!("expected an implausible distance, got {e}");
        };
        assert_eq!(value, 1600.0);
        assert_eq!(limit, 1500.0);
        assert_eq!(
            e.to_string(),
            "snow sensor distance, in millimetres is 1600 and the station's own limit is 1500"
        );
        // Bare ground is the boundary and is not a fault.
        assert_eq!(
            decode(&codec, &body(1500, "ok", 40, "1500.0", 70_000))[0]
                .values
                .snow_distance,
            Some(1500.0)
        );
        // The discard is against the value that would be STORED, so a trimmed mean inside
        // the base is admitted even when the latest raw sample is past it -- which is what
        // the trimmed mean exists to do with an outlier echo.
        assert_eq!(
            decode(&codec, &body(1600, "ok", 40, "1480.0", 70_000))[0]
                .values
                .snow_distance,
            Some(1480.0)
        );
    }

    /// The one departure from Go, as the behaviour it replaces.
    #[test]
    fn a_gauge_with_no_configured_base_distance_discards_nothing() {
        for base in [None, Some(0.0), Some(-1.0), Some(f32::NAN)] {
            let codec = SnowGaugeData::new(base);
            assert_eq!(codec.base_distance_mm(), None, "{base:?}");
            assert_eq!(
                decode(&codec, &body(1600, "ok", 40, "1600.0", 70_000))[0]
                    .values
                    .snow_distance,
                Some(1600.0),
                "{base:?}: Go discards every reading for a base of zero"
            );
        }
    }

    /// A `u16` field that arrives wider is a value the discard can *name*, rather than a
    /// parse failure whose sentence is "not the JSON this codec decodes".
    #[test]
    fn a_distance_wider_than_the_wire_type_is_still_a_number_the_discard_reports() {
        let e = SnowGaugeData::new(Some(1500.0))
            .decode(&body(70_000, "ok", 40, "null", 70_000))
            .expect_err("a fault");
        let CodecError::Implausible { value, .. } = e else {
            panic!("expected an implausible distance, got {e}");
        };
        assert_eq!(value, 70_000.0);
    }

    #[test]
    fn bytes_that_are_not_this_body_are_a_counted_drop() {
        let codec = SnowGaugeData::new(Some(1500.0));
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

    #[test]
    fn the_codec_names_itself_claims_no_station_and_names_the_two_paths() {
        let codec = SnowGaugeData::new(Some(1500.0));
        assert_eq!(codec.name(), "snowgauge-data");
        assert_eq!(
            codec.claimed_station(&Frame::new(DATA_BODY.as_bytes().to_vec())),
            None
        );
        assert_eq!(SNOW_DATA, "/data");
        assert_eq!(SNOW_HEALTH, "/health");
    }
}
