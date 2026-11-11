//! `SnowGaugeProto` — one `snowgauge.Reading` off the gauge's server stream.
//!
//! The gauge is an ultrasonic rangefinder that reports **distance to the snow surface in
//! millimetres**, and W2a §4a.5 gives it `GrpcOut { streaming: true }`, `Fixed` identity and
//! `Service`: we dial it, it streams, and the wire is the credential. The depth is
//! `base_distance - distance` and is the enrich stage's (`mw_pipeline::enrich::snow_depth`,
//! **R1**).
//!
//! # Why the message is declared here rather than compiled from the schema
//!
//! `ci/layers.toml` puts `mw-proto` and this crate in the same layer, so this crate may not
//! name it — and `mw-proto` is where a `build.rs` and `protox` live. What is needed is two
//! scalar fields out of `protocols/snowgauge/snowgauge.proto`:
//!
//! ```proto
//! message Reading {
//!     string stationName = 1;
//!     int32  distance     = 2;                        // millimetres
//!     google.protobuf.Duration systemUptime      = 3; // not a measurement
//!     google.protobuf.Duration applicationUptime = 4; // not a measurement
//! }
//! ```
//!
//! so the declaration below is those two tags and `prost` discards the rest, which is what
//! makes a four-field message a two-field type with no loss. The two uptimes are the gauge's
//! own health rather than a reading, are in no registry entry, and would need
//! `prost-types`.
//!
//! **This is the one schema.** `mw-grpc`'s client calls the method with a byte-passthrough
//! codec and hands the response frame straight here, so there is no second generated
//! `Reading` anywhere to disagree with this one and no re-encode between them.
//!
//! # The discard, which is this item's inherited constraint
//!
//! **DEV-9**, as a constraint on M6-1: `snowgauge/station.go:172-176` discards a distance
//! **greater than the station's configured base distance** at the device, and M3-3 moved
//! `snowdepth` to ingest — so without the discard such a reading stores a *negative* depth
//! into a column with `min`/`mean`/`max` aggregates over it, where in Go it never reached
//! storage at all. The enrich stage is the wrong layer (it is handed an already-admitted
//! reading, and dropping a metric there would make it a filter rather than a transform), so
//! the discard is here, where the base distance is configuration the codec holds and the
//! reading is a fault the decoder can name.
//!
//! It is a [`CodecError::Implausible`] rather than an empty batch, because an empty batch is
//! indistinguishable from a frame that decoded to nothing: a gauge whose base distance is
//! set two metres too short would otherwise report nothing, count nothing and log nothing.
//! `mw_station::metrics::FrameOutcome::Implausible` is the counter.
//!
//! **A station with no configured base distance discards nothing**, which is the one place
//! this departs from Go. `config.DeviceData.BaseSnowDistance` is a non-nullable `float64`
//! there, so an unconfigured gauge has a base of zero, `distance > 0` is true of every real
//! reading, and the Go driver discards **all** of them — a gauge that reports nothing until
//! somebody notices. The column here is nullable with a `CHECK (> 0)`, so "not configured"
//! is expressible: the distance is stored, and `enrich` derives no depth from it because it
//! has no base either.

use mw_types::{PartialReading, Reading};
use prost::Message;

use crate::frame::{Codec, CodecError, Frame};

/// The two fields of `snowgauge.Reading` that carry a measurement.
#[derive(Clone, PartialEq, Message)]
struct Wire {
    #[prost(string, tag = "1")]
    station_name: String,
    /// Millimetres from the sensor to whatever is below it.
    #[prost(int32, tag = "2")]
    distance: i32,
}

/// The fully-qualified gRPC method the driver calls, for `mw-grpc`.
///
/// Here rather than there because it names the same schema this module decodes, and a path
/// and a message that can be edited independently are a client that compiles and returns
/// `Unimplemented`.
pub const STREAM_READING: &str = "/snowgauge.SnowGaugeService/StreamReading";

/// The snow gauge codec for one station's configured base distance.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnowGaugeProto {
    base_distance_mm: Option<f32>,
}

impl SnowGaugeProto {
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
}

impl Codec for SnowGaugeProto {
    fn name(&self) -> &'static str {
        "snowgauge-proto"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let wire = Wire::decode(frame.bytes()).map_err(|source| CodecError::Protobuf { source })?;
        // `int32` to `f32`, which is the column's type. Lossy above 2^24 mm -- sixteen
        // kilometres -- and the value is carried verbatim rather than clamped, because a
        // faulty gauge's number is what the discard below has to be able to report.
        let distance = wire.distance as f32;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One `snowgauge.Reading` on the wire, including the two fields this type does not
    /// declare -- so the test asserts that `prost` discards them rather than assuming it.
    fn wire(name: &str, distance: i32) -> Frame {
        let mut bytes = Vec::new();
        // Field 1, wire type 2 (length-delimited).
        bytes.push(0x0a);
        bytes.push(u8::try_from(name.len()).expect("a short name"));
        bytes.extend_from_slice(name.as_bytes());
        // Field 2, wire type 0 (varint), zig-zag-free because `int32` is not `sint32`.
        bytes.push(0x10);
        // `int32` encodes a negative value as the ten-byte varint of its 64-bit
        // sign-extension, which is the protobuf encoding and the one a hand-built fixture
        // gets wrong.
        let mut v = i64::from(distance) as u64;
        loop {
            let byte = u8::try_from(v & 0x7f).expect("seven bits");
            v >>= 7;
            if v == 0 {
                bytes.push(byte);
                break;
            }
            bytes.push(byte | 0x80);
        }
        // Field 3: a `Duration` submessage, which this type does not declare.
        bytes.extend_from_slice(&[0x1a, 0x02, 0x08, 0x2a]);
        Frame::new(bytes)
    }

    fn decode(codec: &SnowGaugeProto, name: &str, distance: i32) -> Vec<PartialReading> {
        codec.decode(&wire(name, distance)).expect("decodes")
    }

    #[test]
    fn a_reading_decodes_to_the_distance_and_the_undeclared_fields_are_discarded() {
        let batch = decode(&SnowGaugeProto::new(Some(1500.0)), "roof-gauge", 1200);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].values.snow_distance, Some(1200.0));
        assert_eq!(
            batch[0].values.snow_depth, None,
            "the depth is the enrich stage's subtraction (DEV-9)"
        );
        assert_eq!(
            batch[0].observed_at, None,
            "the gauge streams a current distance and no instant"
        );
    }

    /// The inherited constraint. The Go driver discards at the device; this discards at the
    /// decoder, and the number it would otherwise store is the one in the message.
    #[test]
    fn a_distance_past_the_base_is_a_named_discard_rather_than_a_negative_depth() {
        let codec = SnowGaugeProto::new(Some(1500.0));
        let e = codec
            .decode(&wire("roof-gauge", 1600))
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
            decode(&codec, "roof-gauge", 1500)[0].values.snow_distance,
            Some(1500.0)
        );
    }

    /// The one departure from Go, as the behaviour it replaces.
    #[test]
    fn a_gauge_with_no_configured_base_distance_discards_nothing() {
        for base in [None, Some(0.0), Some(-1.0), Some(f32::NAN)] {
            let codec = SnowGaugeProto::new(base);
            assert_eq!(codec.base_distance_mm(), None, "{base:?}");
            assert_eq!(
                decode(&codec, "roof-gauge", 1200)[0].values.snow_distance,
                Some(1200.0),
                "{base:?}: Go discards every reading for a base of zero"
            );
        }
    }

    /// A gauge reporting zero or a negative distance is a sensor fault, and it is **stored**
    /// rather than discarded -- which is Go's behaviour and the enrich stage's own
    /// `snow_distance > 0` guard is what stops a depth being derived from it.
    #[test]
    fn a_non_positive_distance_is_stored_and_derives_no_depth() {
        let codec = SnowGaugeProto::new(Some(1500.0));
        for distance in [0, -5] {
            let batch = decode(&codec, "roof-gauge", distance);
            assert_eq!(batch[0].values.snow_distance, Some(distance as f32));
            assert_eq!(batch[0].values.snow_depth, None);
        }
    }

    #[test]
    fn bytes_that_are_not_this_message_are_a_counted_drop() {
        let codec = SnowGaugeProto::new(Some(1500.0));
        // A length-delimited field whose length runs off the end.
        let e = codec
            .decode(&Frame::new(vec![0x0a, 0x40, 0x01]))
            .expect_err("truncated");
        assert!(matches!(e, CodecError::Protobuf { .. }), "{e}");
        // An empty frame is a valid, empty message -- every field is at its default, which
        // for `distance` is zero. Faithful to protobuf and asserted so nobody has to guess.
        let batch = codec
            .decode(&Frame::new(Vec::new()))
            .expect("empty decodes");
        assert_eq!(batch[0].values.snow_distance, Some(0.0));
    }

    #[test]
    fn the_codec_names_itself_and_claims_no_station() {
        let codec = SnowGaugeProto::new(Some(1500.0));
        assert_eq!(codec.name(), "snowgauge-proto");
        assert_eq!(codec.claimed_station(&wire("roof-gauge", 1200)), None);
        assert_eq!(STREAM_READING, "/snowgauge.SnowGaugeService/StreamReading");
    }
}
