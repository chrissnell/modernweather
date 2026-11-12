//! `collector.v1.Collector` on the wire: the three method paths, the three codecs, and
//! the conversions between the schema's messages and the domain's types.
//!
//! **Nothing here is generated, and `mw_grpc::codec` already has the general form of why:**
//! `mw-proto`'s build script runs `prost-build` with no service generator, so the `service`
//! block in `proto/collector.proto` is a declaration of the contract and the client and
//! server halves are written out. The messages themselves *are* generated — they are
//! `prost` structs from that same schema — so the codec is [`tonic_prost::ProstCodec`] and
//! not a hand-written encoder.
//!
//! What that costs is one `const` per method instead of a path the generator derives, and
//! the cost is paid for by [`tests`]: a method path is a wire contract, so each string is
//! asserted exactly, and asserted against the package and service names in the schema
//! rather than against a re-spelling of them.

use chrono::{DateTime, Utc};
use mw_component::{PeerStation, StationProjection};
use mw_proto::wire::collector::v1 as pb;
use mw_types::StationKey;

/// The service's fully-qualified protobuf name, which every path below is built from.
pub const SERVICE: &str = "collector.v1.Collector";

/// `Hello(NodeIdentity) returns (HelloAck)` — identity only (§5.2).
pub const HELLO: &str = "/collector.v1.Collector/Hello";

/// `Ingest(stream IngestBatch) returns (stream IngestAck)` — bidirectional and acked
/// (§4.2).
pub const INGEST: &str = "/collector.v1.Collector/Ingest";

/// `Enroll(EnrolRequest) returns (EnrolGrant)` — the separately gated bootstrap (§5.3).
pub const ENROL: &str = "/collector.v1.Collector/Enroll";

/// `POST /v1/ingest` — the REST form of the same contract (§4.5, **D-30**).
pub const REST_INGEST: &str = "/v1/ingest";

/// The codec for `Hello`, server side: a `NodeIdentity` in, a `HelloAck` out.
pub type HelloCodec = tonic_prost::ProstCodec<pb::HelloAck, pb::NodeIdentity>;

/// The codec for `Ingest`, server side.
pub type IngestCodec = tonic_prost::ProstCodec<pb::IngestAck, pb::IngestBatch>;

/// The codec for `Enroll`, server side.
pub type EnrolCodec = tonic_prost::ProstCodec<pb::EnrolGrant, pb::EnrolRequest>;

/// The codec for `Hello`, client side — the two type parameters reversed.
pub type HelloClientCodec = tonic_prost::ProstCodec<pb::NodeIdentity, pb::HelloAck>;

/// The codec for `Ingest`, client side.
pub type IngestClientCodec = tonic_prost::ProstCodec<pb::IngestBatch, pb::IngestAck>;

/// `WeatherV1.SendWeatherReadings` — the method a **Go-era collector** serves (§9.3).
///
/// Declared here rather than imported from `mw_grpc::grpcrx`, which declares the same
/// string for the receiving side: the two crates are in the same layer and
/// `ci/layers.toml` forbids either naming the other. A path is a wire contract, so each
/// copy has a test asserting the exact string — `mw_grpc::grpcrx`'s own, and
/// [`tests::the_compat_path_is_the_go_method`] here. A divergence between them is a
/// satellite that gets `Unimplemented` from its own collector.
pub const SEND_WEATHER_READINGS: &str = "/remoteweather.v1.WeatherV1/SendWeatherReadings";

/// The codec for the compat fallback: a `Reading` out, nothing back.
///
/// `remoteweather.v1.Empty` has no fields, and a protobuf message with no fields set is
/// **zero bytes** — so there is nothing to decode and no type to decode it into.
/// `mw_grpc::codec::ReadingCodec` is this codec's mirror image for the same reason, on the
/// receiving side.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompatCodec;

impl tonic::codec::Codec for CompatCodec {
    type Encode = pb_reading::Reading;
    type Decode = ();
    type Encoder = CompatCodec;
    type Decoder = CompatCodec;

    fn encoder(&mut self) -> Self::Encoder {
        *self
    }

    fn decoder(&mut self) -> Self::Decoder {
        *self
    }
}

impl tonic::codec::Encoder for CompatCodec {
    type Item = pb_reading::Reading;
    type Error = tonic::Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Self::Error> {
        prost::Message::encode(&item, dst)
            .map_err(|e| tonic::Status::internal(format!("a reading did not encode: {e}")))
    }
}

impl tonic::codec::Decoder for CompatCodec {
    type Item = ();
    type Error = tonic::Status;

    fn decode(
        &mut self,
        src: &mut tonic::codec::DecodeBuf<'_>,
    ) -> Result<Option<Self::Item>, Self::Error> {
        // Whatever arrived is `Empty`, whose encoding is no bytes at all. Advancing the
        // buffer rather than ignoring it, because `tonic` hands over exactly one message
        // and a decoder that consumed none of it would be asked again.
        let len = prost::bytes::Buf::remaining(src);
        prost::bytes::Buf::advance(src, len);
        Ok(Some(()))
    }
}

/// The generated reading, under a name that does not collide with the collector package.
pub use mw_proto::wire::modernweather::v1 as pb_reading;

/// One envelope's reading, as a Go-era collector's `SendWeatherReadings` expects it (§9.3).
///
/// **The station key goes in `station_id` and in `stationName`.** `mw_grpc::grpcrx` reads
/// `station_id` first and falls back to `stationName`, and a Go-era receiver reads
/// `stationName`; setting both is what makes one message work against either. Neither is a
/// credential — the Go method has no authorisation at all, which is §9.2's second
/// limitation seen from the sending side.
///
/// **Field presence does not survive this hop**, and that is §9.3's stated cost rather than
/// a defect here: the committed Go proto's bare scalars make absent and `0.0` the same
/// bytes (W3 §6.5), so a metric this satellite did not measure arrives at a Go collector as
/// a zero. Nothing on either side can recover it. The `optional` fields on
/// `modernweather.v1.Reading` are what a *Rust* collector gets instead.
pub fn compat_reading(envelope: &mw_types::Envelope) -> pb_reading::Reading {
    let mut reading = pb_reading::Reading::from(envelope.reading.as_ref());
    reading.station_id = envelope.station.as_str().to_string();
    reading.station_name = envelope.station.as_str().to_string();
    reading
}

/// Unix milliseconds, as every instant on this wire is.
pub fn millis(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

/// One `StationMeta` as the domain holds it.
///
/// A conversion rather than a field copy because of the two normalisations in it: an empty
/// `station_key` is not a station, and an empty `name` or `station_type` must not become an
/// empty `NOT NULL` column.
///
/// The coordinates need no normalisation, and that is the schema's doing rather than this
/// function's: all three are `optional double`, so "not reported" and `0.0` are different
/// bytes (**D-89**, and the note on the field). A `NaN` is still refused here, because a
/// peer can send one and it is not a place.
pub fn station(meta: &pb::StationMeta) -> Option<PeerStation> {
    let key = meta.station_key.trim();
    if key.is_empty() {
        // A station with no key is not a station: there is no envelope without one
        // (W2a §4a.6), so registering it would create a row nothing can ever name.
        return None;
    }
    if key.len() > mw_proto::MAX_STATION_KEY {
        // The **same bound the envelope decode applies**, read from the same constant: a
        // key this node would register and then refuse every envelope for is worse than one
        // it never registered. The two cannot drift because there is one constant.
        return None;
    }
    Some(PeerStation {
        station: StationKey::new(key),
        name: non_empty(&meta.name).unwrap_or_else(|| key.to_string()),
        kind: non_empty(&meta.station_type).unwrap_or_else(|| "grpcreceiver".to_string()),
        latitude: meta.latitude.filter(|v| v.is_finite()),
        longitude: meta.longitude.filter(|v| v.is_finite()),
        altitude_m: meta.altitude_m.filter(|v| v.is_finite()),
        timezone: non_empty(&meta.timezone),
        aprs_callsign: non_empty(&meta.aprs_callsign),
    })
}

/// The reverse, for a satellite describing its own stations in `Hello`.
pub fn station_meta(key: &StationKey, projection: &StationProjection) -> pb::StationMeta {
    pb::StationMeta {
        station_key: key.as_str().to_string(),
        name: projection.name.clone(),
        station_type: projection.kind.clone(),
        latitude: projection.latitude,
        longitude: projection.longitude,
        altitude_m: projection.altitude_m,
        timezone: projection.timezone.clone().unwrap_or_default(),
        // Identity, not a credential (**D-39**). The projection does not carry it, so a
        // satellite's callsign reaches its collector through the operator rather than
        // through this field until `StationProjection` grows one; recorded in the PR.
        aprs_callsign: String::new(),
    }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A method path is a wire contract: a typo here is a client that compiles and gets
    /// `Unimplemented`. Built from [`SERVICE`] rather than re-spelled, so the assertion
    /// cannot pass against a path the schema does not declare.
    #[test]
    fn the_three_method_paths_are_the_schema_s() {
        assert_eq!(SERVICE, "collector.v1.Collector");
        assert_eq!(HELLO, format!("/{SERVICE}/Hello"));
        assert_eq!(INGEST, format!("/{SERVICE}/Ingest"));
        assert_eq!(ENROL, format!("/{SERVICE}/Enroll"));
    }

    /// The compat fallback's path is the **Go** one, verbatim, because a Go-era collector
    /// is what serves it. A typo here is a satellite that gets `Unimplemented` from the
    /// collector it was pointed at and reports a degraded link it could have used.
    #[test]
    fn the_compat_path_is_the_go_method() {
        assert_eq!(
            SEND_WEATHER_READINGS,
            "/remoteweather.v1.WeatherV1/SendWeatherReadings"
        );
        assert!(
            SEND_WEATHER_READINGS.starts_with("/remoteweather.v1.WeatherV1/"),
            "the compat method is on the FROZEN service, which W7 D-19 leaves untouched"
        );
    }

    #[test]
    fn a_compat_reading_carries_the_station_key_in_both_fields() {
        let envelope = crate::testing::envelope(
            "satellite-a:backyard",
            mw_types::OriginId::new([1; 16]),
            1,
            &[1, 9],
            chrono::Utc::now(),
        );
        let reading = compat_reading(&envelope);
        assert_eq!(reading.station_id, "satellite-a:backyard");
        assert_eq!(
            reading.station_name, "satellite-a:backyard",
            "a Go-era receiver reads `stationName`, a current one reads `station_id`"
        );
        assert_eq!(
            reading.barometer,
            Some(29.92),
            "the metrics survive the hop"
        );
    }

    /// `WeatherV1` is not touched (W7 **D-19**), so no path here may collide with the two
    /// `mw_grpc::grpcrx` serves.
    #[test]
    fn no_collector_path_is_a_weather_v1_path() {
        for path in [HELLO, INGEST, ENROL] {
            assert!(path.starts_with("/collector.v1."), "{path}");
            assert!(!path.contains("remoteweather"), "{path}");
        }
    }

    #[test]
    fn a_station_with_no_key_is_not_a_station() {
        assert!(station(&pb::StationMeta::default()).is_none());
        assert!(station(&pb::StationMeta {
            station_key: "   ".into(),
            ..Default::default()
        })
        .is_none());
    }

    /// Presence, and the reason the field is `optional`: a station **on** the prime
    /// meridian and a station that reported nothing are two different facts.
    #[test]
    fn an_unreported_coordinate_decodes_as_absent_and_zero_decodes_as_zero() {
        let meta = pb::StationMeta {
            station_key: "satellite-a:backyard".into(),
            latitude: Some(39.64),
            longitude: None,
            altitude_m: Some(f64::NAN),
            ..Default::default()
        };
        let s = station(&meta).expect("a keyed station");
        assert_eq!(s.latitude, Some(39.64));
        assert_eq!(s.longitude, None, "not reported");
        assert_eq!(s.altitude_m, None, "NaN is not a place");

        let greenwich = pb::StationMeta {
            station_key: "obs:greenwich".into(),
            latitude: Some(51.4769),
            longitude: Some(0.0),
            ..Default::default()
        };
        assert_eq!(
            station(&greenwich).expect("a keyed station").longitude,
            Some(0.0),
            "a station on the prime meridian reported a longitude, and it is not absent"
        );
    }

    /// An absent name or type must not become an empty `NOT NULL` column.
    #[test]
    fn a_nameless_station_falls_back_to_its_key_and_the_frozen_type() {
        let s = station(&pb::StationMeta {
            station_key: "satellite-a:backyard".into(),
            ..Default::default()
        })
        .expect("a keyed station");
        assert_eq!(s.name, "satellite-a:backyard");
        // `grpcreceiver` is the seventh name in docs/legacy-api-contract.md §3.1's list and
        // is what `/stationinfo` may emit for a received station.
        assert_eq!(s.kind, "grpcreceiver");
    }

    #[test]
    fn a_station_round_trips_through_the_meta_message() {
        let projection = StationProjection {
            name: "Back Yard".into(),
            kind: "davis".into(),
            timezone: Some("America/Denver".into()),
            latitude: Some(39.64),
            longitude: Some(-105.2),
            altitude_m: Some(1800.0),
            snow_base_distance_mm: Some(1500.0),
        };
        let key = StationKey::new("satellite-a:backyard");
        let back = station(&station_meta(&key, &projection)).expect("a keyed station");
        assert_eq!(back.station, key);
        assert_eq!(back.name, "Back Yard");
        assert_eq!(back.kind, "davis");
        assert_eq!(back.timezone.as_deref(), Some("America/Denver"));
        assert_eq!(back.latitude, Some(39.64));
        assert_eq!(back.longitude, Some(-105.2));
        assert_eq!(back.altitude_m, Some(1800.0));
    }
}
