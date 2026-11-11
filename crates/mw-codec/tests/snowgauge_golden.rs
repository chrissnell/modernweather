//! Level 1 of W3 **D-82**'s three, for the snow gauge: the pure codec against a **captured**
//! protobuf message.
//!
//! The fixture was produced by `protocols/snowgauge`'s own generated marshaller at the
//! pinned commit — vendored verbatim in `tests/emulators/snowgauge` and digest-verified by
//! the `driver-emulators` gate. That is what makes this file worth having at all.
//!
//! # What it checks that nothing else did
//!
//! `mw_codec::SnowGaugeProto` declares **two fields of a four-field message by hand**, with
//! `#[prost(string, tag = "1")]` and `#[prost(int32, tag = "2")]` written out in Rust —
//! because `ci/layers.toml` puts `mw-proto` in `mw-codec`'s own layer, so the codec cannot
//! use a generated type. A wrong tag or a wrong wire type there is a distance of zero on
//! every reading, for ever, with nothing failing.
//!
//! This asserts that declaration against bytes the reference's own `protoc` output wrote.
//! It needs no socket, no gRPC and no Go toolchain, which is what makes it level 1 rather
//! than a second copy of `crates/mw-grpc/tests/emulator_snowgauge.rs`.

use mw_codec::{Codec, CodecError, Frame, SnowGaugeProto};

/// One `snowgauge.Reading`: `stationName = "roof-gauge"`, `distance = 1352`, and both
/// `google.protobuf.Duration` uptimes at 42 s.
const READING: &[u8] = include_bytes!("frames/snowgauge-reading.bin");

/// The distance the captured message carries, in millimetres.
const DISTANCE: f32 = 1352.0;

fn decode(base: Option<f32>) -> Result<Vec<mw_types::PartialReading>, CodecError> {
    SnowGaugeProto::new(base).decode(&Frame::new(READING.to_vec()))
}

/// **The hand-written field tags are the generated schema's.** A wrong tag is a zero
/// distance; a wrong wire type does not decode at all.
#[test]
fn the_captured_message_decodes_to_the_distance_the_marshaller_wrote() {
    let batch = decode(Some(1500.0)).expect("a captured message decodes");
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].values.snow_distance,
        Some(DISTANCE),
        "the bytes carry 1352 mm; a wrong `tag = \"2\"` would be zero"
    );
    assert_eq!(
        batch[0].observed_at, None,
        "the gauge streams a current distance and no instant"
    );
    // The depth is the enrich stage's subtraction (DEV-9), not the codec's.
    assert_eq!(batch[0].values.snow_depth, None);
    // And nothing else: a gauge measures one thing.
    assert_eq!(batch[0].values.out_temp, None);
    assert_eq!(batch[0].values.barometer, None);
}

/// **The two fields the codec declares no tag for are discarded**, which is the property the
/// hand-written declaration rests on: `systemUptime` and `applicationUptime` are in the
/// captured bytes and in no registry entry.
#[test]
fn the_undeclared_duration_fields_are_discarded_rather_than_refused() {
    // The fixture really does carry them -- field 3 and field 4, each a two-byte
    // length-delimited submessage -- so the assertion below is not vacuous.
    assert!(
        READING.windows(4).any(|w| w == [0x1a, 0x02, 0x08, 0x2a]),
        "field 3 is in the capture: {READING:02x?}"
    );
    assert!(
        READING.windows(4).any(|w| w == [0x22, 0x02, 0x08, 0x2a]),
        "and field 4: {READING:02x?}"
    );
    // And it still decodes, which is what "prost discards what is not declared" means.
    decode(Some(1500.0)).expect("the two uptimes cost nothing");
}

/// **DEV-9's discard**, against bytes a real gauge's marshaller wrote: the same message
/// against a base distance below it is a counted fault rather than a negative depth.
#[test]
fn the_same_message_is_a_fault_against_a_base_distance_below_it() {
    let e = decode(Some(1300.0)).expect_err("1352 mm is past a 1300 mm base");
    let CodecError::Implausible { value, limit, .. } = e else {
        panic!("expected an implausible distance, got {e}");
    };
    assert_eq!(value, f64::from(DISTANCE));
    assert_eq!(limit, 1300.0);
    // Bare ground is the boundary and is not a fault.
    assert!(decode(Some(DISTANCE)).is_ok());
}

/// A gauge whose base distance is not configured discards nothing — the one departure from
/// Go, where an unconfigured gauge has a base of zero and reports nothing at all.
#[test]
fn an_unconfigured_gauge_admits_the_captured_distance() {
    for base in [None, Some(0.0), Some(-1.0), Some(f32::NAN)] {
        let batch = decode(base).unwrap_or_else(|e| panic!("{base:?}: {e}"));
        assert_eq!(batch[0].values.snow_distance, Some(DISTANCE), "{base:?}");
    }
}

#[test]
fn decoding_is_a_function_of_the_message_alone() {
    let first = decode(Some(1500.0)).expect("decodes");
    let again = decode(Some(1500.0)).expect("decodes");
    assert_eq!(first, again);
    assert_eq!(SnowGaugeProto::new(Some(1500.0)).name(), "snowgauge-proto");
    assert_eq!(
        mw_codec::STREAM_READING,
        "/snowgauge.SnowGaugeService/StreamReading"
    );
}
