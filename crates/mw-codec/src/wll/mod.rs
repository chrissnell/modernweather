//! `WllConditions` — the WeatherLink Live codec, for both of its transports.
//!
//! One codec, two transports, which is W2a **D-44**: the UDP broadcast datagram and the
//! body of `GET /v1/current_conditions` are the same document with different fields
//! populated, so decoding them twice would be two implementations of one sensor mapping.
//! The third piece of the driver — the outbound call that arms the broadcast — produces no
//! readings and is not a codec at all; [`real_time`] is the one function here that serves
//! it, and it returns a port and a duration rather than a [`PartialReading`].
//!
//! # What is pure about this, and what that buys
//!
//! No clock: the observation instant is the payload's own `ts`, and a frame without one
//! leaves [`PartialReading::observed_at`] `None` for admission to fill from the frame's
//! arrival (`mw_types::PartialReading`). No socket, no filesystem, no runtime. So every
//! test below and in `tests/wll_golden.rs` is a function call over a byte string, which is
//! what "golden tests against captured frames" means in this item's done-criteria.

mod map;
mod mapping;
mod wire;

use mw_types::{PartialReading, Reading, StationKey};
use serde::Deserialize;

use crate::frame::{Codec, CodecError, Frame};
use wire::{ApiError, Condition, Document};

pub use mapping::{MappingError, SensorEntry, SensorMapping, SensorRole};

/// The broadcast arming response: where the datagrams will come from, and for how long.
///
/// Not a reading, so not a [`Codec`] output. It is here rather than in `mw-station`
/// because it is the same pure "bytes to a value" step over the same device's JSON, and
/// putting it in the driver would be the one piece of WeatherLink Live parsing that no
/// golden test could reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Broadcast {
    /// The UDP port the unit will send conditions datagrams to.
    pub port: u16,
    /// How many seconds the unit will keep broadcasting before it stops.
    pub duration_seconds: i64,
}

/// The longest device serial this codec will carry as a claim.
///
/// A WeatherLink Live reports twelve hex characters (`001D0A700002`). The bound is on the
/// claim and not on the frame because a claim outlives the frame: it reaches a log field
/// through `FrameMeta`, and a resolved one reaches a metric label.
pub const MAX_DSN_BYTES: usize = 64;

/// Decode `GET /v1/real_time`'s body.
///
/// The unit answers `200` with a populated `error` object rather than a status code, so a
/// driver that looked only at the transport would read a refusal as a successful arming
/// and then listen on a port nothing sends to.
pub fn real_time(frame: &Frame) -> Result<Broadcast, CodecError> {
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        error: Option<ApiError>,
        #[serde(default)]
        data: Option<wire::RealTime>,
        #[serde(default)]
        broadcast_port: Option<u16>,
        #[serde(default)]
        duration: Option<i64>,
    }

    let payload: Payload = json(frame)?;
    if let Some(e) = payload.error {
        return Err(CodecError::Device {
            code: e.code,
            message: e.message,
        });
    }
    let (port, duration) = match payload.data {
        Some(d) => (d.broadcast_port, d.duration),
        None => (payload.broadcast_port, payload.duration),
    };
    // A response with no port is not a successful arming, whatever its status line said.
    let Some(port) = port.filter(|p| *p != 0) else {
        return Err(CodecError::Incomplete {
            what: "broadcast port",
        });
    };
    Ok(Broadcast {
        port,
        // A unit that reports no duration is treated as having armed for nothing, so the
        // re-arm component's own period governs and the next occurrence re-arms. The
        // alternative -- inventing 3600 because that is what was asked for -- would make a
        // lapsed broadcast look like a working one.
        duration_seconds: duration.unwrap_or(0),
    })
}

/// The WeatherLink Live conditions codec.
///
/// Holds the resolved sensor mapping, which is the only configuration it has: *which*
/// transmitter is the outdoor one is a fact about the deployment, and nothing in the
/// payload says it (see [`SensorMapping`]).
#[derive(Debug, Clone)]
pub struct WllConditions {
    mapping: SensorMapping,
}

impl WllConditions {
    /// The codec for one station's sensor mapping.
    pub fn new(mapping: SensorMapping) -> Self {
        Self { mapping }
    }

    /// The mapping this codec decodes against.
    pub fn mapping(&self) -> &SensorMapping {
        &self.mapping
    }

    /// The record a mapping entry reads, by the two lookups the payload supports.
    ///
    /// **A transmitter id alone is not enough**, which is a correction to the Go lookup.
    /// `findCondition` returns the first record whose `txid` matches, so `th:2` on a
    /// deployment with a leaf/soil station on transmitter 2 returns the leaf/soil record
    /// and reads `temp` out of it — a field that record does not have — and the outdoor
    /// temperature silently stops. Preferring the record type the role implies and only
    /// then falling back to any record with that transmitter keeps every mapping the Go
    /// driver resolves resolving the same way, while making the ambiguous case right.
    ///
    /// The unit's own two records carry no transmitter id at all, so they are found by
    /// record type — and *that* is the other correction: Go's `th_indoor` arm looks for
    /// `DataStructureBarometer`, which is record type 3, while `temp_in` and `hum_in` are
    /// in record type 4. The Go driver therefore never stores an indoor temperature from a
    /// WeatherLink Live. Both corrections are **DEV-8**.
    fn find<'a>(&self, conditions: &'a [Condition], entry: &SensorEntry) -> Option<&'a Condition> {
        let wanted = entry.role.structure();
        match entry.txid {
            Some(txid) => {
                let on_transmitter = || {
                    conditions
                        .iter()
                        .filter(|c| c.txid == Some(i64::from(txid)))
                };
                on_transmitter()
                    .find(|c| c.structure() == wanted)
                    .or_else(|| on_transmitter().next())
            }
            None => conditions.iter().find(|c| c.structure() == wanted),
        }
    }

    fn document(&self, frame: &Frame) -> Result<Document, CodecError> {
        /// Both shapes in one type: the HTTP response wraps the document in `data` and the
        /// broadcast datagram **is** the document. An untagged enum would be the obvious
        /// spelling and is wrong — every field of the wrapper is optional, so a bare
        /// datagram deserialises into it as "no data and no error" and decodes to nothing.
        #[derive(Deserialize)]
        struct Payload {
            #[serde(default)]
            error: Option<ApiError>,
            #[serde(default)]
            data: Option<Document>,
            #[serde(default)]
            did: Option<String>,
            #[serde(default)]
            ts: Option<i64>,
            #[serde(default)]
            conditions: Vec<Condition>,
        }

        let payload: Payload = json(frame)?;
        if let Some(e) = payload.error {
            return Err(CodecError::Device {
                code: e.code,
                message: e.message,
            });
        }
        Ok(payload.data.unwrap_or(Document {
            did: payload.did,
            ts: payload.ts,
            conditions: payload.conditions,
        }))
    }
}

impl Codec for WllConditions {
    fn name(&self) -> &'static str {
        "wll-conditions"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let doc = self.document(frame)?;
        if doc.conditions.is_empty() {
            return Err(CodecError::NoRecords);
        }

        // `station_name` and `station_type` are deliberately **not** set here, and the
        // reason is not that a codec could not know them. `Ingress::admit` refuses a
        // reading that carries no populated metric (`Reject::EmptyReading`, W7 §4.6) by
        // comparing it against a default `Reading`, so a codec that stamped the driver
        // name onto every reading would make every reading non-empty -- and a frame whose
        // mapping matched nothing would be stored as a row of NULLs with a driver name on
        // it. The configured type is configuration, the enrich stage (M3-3) is where
        // configuration meets a reading, and that is where it goes.
        let mut values = Reading::default();
        let mut th_index = 0usize;
        for entry in self.mapping.entries() {
            let Some(condition) = self.find(&doc.conditions, entry) else {
                continue;
            };
            // The ordering rule: a `th` entry consumes an index when it MATCHED A RECORD,
            // whether or not that record populated anything. See `map::temp_humidity`.
            if entry.role == SensorRole::Th {
                map::apply(&mut values, entry, condition, th_index);
                th_index += 1;
            } else {
                map::apply(&mut values, entry, condition, th_index);
            }
        }

        // One frame is one observation across several sensors, so one reading. A reading
        // that populated nothing is still returned: "no metric was populated" is
        // admission's `Reject::EmptyReading`, counted there with every other refusal,
        // rather than a second place where a reading can vanish.
        Ok(vec![match doc.ts.and_then(instant) {
            Some(at) => PartialReading::at(at, values),
            None => PartialReading::new(values),
        }])
    }

    fn claimed_station(&self, frame: &Frame) -> Option<StationKey> {
        // Verbatim, because it is a claim: `StationRegistry::resolve` is what turns it into
        // a station, and the comparison it makes -- against `config.stations.presented_key`
        // -- is where the unit's upper-case hex and an operator's typing are reconciled.
        //
        // Bounded, because the input is a datagram from a LAN. A claim that does not
        // resolve reaches a log field, and a claim that does becomes a metric label, so
        // "verbatim" has to stop somewhere: a WeatherLink Live serial is twelve hex
        // characters, and `MAX_DSN_BYTES` is generous against that rather than against the
        // frame. A claim this rejects is one no `presented_key` could have matched anyway,
        // so nothing resolvable is lost.
        self.document(frame)
            .ok()?
            .did
            .filter(|did| {
                !did.is_empty()
                    && did.len() <= MAX_DSN_BYTES
                    && did.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .map(StationKey::new)
    }
}

/// Parse a frame's JSON, attributing a failure to the parse rather than to the device.
fn json<T: for<'de> Deserialize<'de>>(frame: &Frame) -> Result<T, CodecError> {
    serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })
}

/// Unix seconds as an instant, or `None` for one that is not a time.
///
/// `chrono` is a dependency of this crate for the type and for nothing else: the layout
/// gate rejects a call that reads the clock, which is what keeps the type legitimate here
/// (`ci/layers.toml`, `mw-codec`).
fn instant(ts: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(ts, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPPING: &str = "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, \
                           thsw:1:appTemp, th_indoor, baro, battery:1";

    fn codec() -> WllConditions {
        WllConditions::new(SensorMapping::parse(MAPPING).expect("a literal mapping"))
    }

    #[test]
    fn the_broadcast_datagram_and_the_http_body_decode_identically() {
        let bare = Frame::new(
            br#"{"did":"001D0A700002","ts":1531754005,"conditions":[
                 {"lsid":48308,"data_structure_type":1,"txid":1,"temp":60.0,"hum":50.0}]}"#
                .to_vec(),
        );
        let wrapped = Frame::new(
            br#"{"data":{"did":"001D0A700002","ts":1531754005,"conditions":[
                 {"lsid":48308,"data_structure_type":1,"txid":1,"temp":60.0,"hum":50.0}]},
                 "error":null}"#
                .to_vec(),
        );
        let c = codec();
        let a = c.decode(&bare).expect("the datagram decodes");
        let b = c.decode(&wrapped).expect("the body decodes");
        assert_eq!(a, b, "one codec, two transports (D-44)");
        assert_eq!(a[0].values.out_temp, Some(60.0));
        assert_eq!(
            c.claimed_station(&bare).map(|k| k.as_str().to_string()),
            Some("001D0A700002".to_string())
        );
    }

    #[test]
    fn the_payloads_own_timestamp_is_the_observation_instant() {
        let frame = Frame::new(
            br#"{"ts":1531754005,"conditions":[{"data_structure_type":1,"txid":1,"temp":1.0}]}"#
                .to_vec(),
        );
        let decoded = codec().decode(&frame).expect("decodes");
        let at = decoded[0].observed_at.expect("the frame carried one");
        assert_eq!(at.timestamp(), 1_531_754_005);
        // ... and a frame without one leaves it for admission.
        let no_ts = Frame::new(
            br#"{"conditions":[{"data_structure_type":1,"txid":1,"temp":1.0}]}"#.to_vec(),
        );
        assert_eq!(
            codec().decode(&no_ts).expect("decodes")[0].observed_at,
            None
        );
    }

    #[test]
    fn a_device_error_is_not_an_empty_reading() {
        let frame = Frame::new(
            br#"{"data":null,"error":{"code":409,"message":"Request Rejected"}}"#.to_vec(),
        );
        let e = codec().decode(&frame).expect_err("a device error");
        assert!(matches!(e, CodecError::Device { code: 409, .. }), "{e}");
        assert_eq!(
            e.to_string(),
            "the device reported error 409: Request Rejected"
        );
    }

    #[test]
    fn the_transmitter_lookup_prefers_the_record_type_the_role_implies() {
        // The Go lookup returns the leaf/soil record for `th:2` and reads `temp` out of a
        // record that has none.
        let frame = Frame::new(
            br#"{"conditions":[
                   {"data_structure_type":2,"txid":2,"temp_1":55.0},
                   {"data_structure_type":1,"txid":2,"temp":70.0,"hum":40.0}]}"#
                .to_vec(),
        );
        let c = WllConditions::new(SensorMapping::parse("th:2, soil_temp:2:1").expect("parses"));
        let decoded = c.decode(&frame).expect("decodes");
        assert_eq!(decoded[0].values.out_temp, Some(70.0), "the ISS record");
        assert_eq!(decoded[0].values.soil_temp1, Some(55.0), "the soil record");
    }

    #[test]
    fn the_units_own_records_are_found_by_type_and_not_by_transmitter() {
        let frame = Frame::new(
            br#"{"conditions":[
                   {"data_structure_type":3,"bar_sea_level":29.9,"bar_absolute":24.2},
                   {"data_structure_type":4,"temp_in":68.3,"hum_in":41.0}]}"#
                .to_vec(),
        );
        let c = WllConditions::new(SensorMapping::parse("baro, th_indoor").expect("parses"));
        let r = &c.decode(&frame).expect("decodes")[0].values;
        assert_eq!(r.barometer, Some(29.9));
        // The Go driver reads this out of record type 3 and gets nothing.
        assert_eq!(r.in_temp, Some(68.3), "record type 4 is the indoor one");
        assert_eq!(r.in_humidity, Some(41.0));
    }

    #[test]
    fn a_silent_transmitter_does_not_renumber_the_extra_temperatures() {
        // `th:1` is configured and transmitter 1 is not reporting. The second `th` entry
        // must stay `extratemp1` rather than becoming the outdoor temperature.
        let frame = Frame::new(
            br#"{"conditions":[{"data_structure_type":1,"txid":2,"temp":44.0,"hum":80.0}]}"#
                .to_vec(),
        );
        let c = WllConditions::new(SensorMapping::parse("th:1, th:2").expect("parses"));
        let r = &c.decode(&frame).expect("decodes")[0].values;
        assert_eq!(r.out_temp, Some(44.0), "the first entry that matched");
        assert_eq!(r.extra_temp1, None);
    }

    #[test]
    fn an_empty_conditions_array_is_a_decode_failure_and_not_a_reading() {
        let frame = Frame::new(br#"{"did":"x","ts":1,"conditions":[]}"#.to_vec());
        assert!(matches!(codec().decode(&frame), Err(CodecError::NoRecords)));
    }

    #[test]
    fn a_mapping_that_matches_nothing_decodes_to_a_reading_admission_will_refuse() {
        let frame = Frame::new(
            br#"{"conditions":[{"data_structure_type":1,"txid":8,"temp":1.0}]}"#.to_vec(),
        );
        let decoded = codec().decode(&frame).expect("the frame is well formed");
        assert_eq!(decoded.len(), 1);
        // Nothing at all, which is what makes `Ingress::admit` count it as
        // `empty_reading` rather than store a row of NULLs (W7 §4.6).
        assert_eq!(decoded[0].values, Reading::default());
    }

    #[test]
    fn arming_decodes_both_shapes_and_refuses_a_refusal() {
        let wrapped = Frame::new(
            br#"{"data":{"broadcast_port":22222,"duration":3600},"error":null}"#.to_vec(),
        );
        assert_eq!(
            real_time(&wrapped).expect("decodes"),
            Broadcast {
                port: 22222,
                duration_seconds: 3600
            }
        );
        let bare = Frame::new(br#"{"broadcast_port":22223,"duration":300}"#.to_vec());
        assert_eq!(real_time(&bare).expect("decodes").port, 22223);

        let refused =
            Frame::new(br#"{"data":null,"error":{"code":403,"message":"Forbidden"}}"#.to_vec());
        assert!(matches!(
            real_time(&refused),
            Err(CodecError::Device { code: 403, .. })
        ));
        let portless = Frame::new(br#"{"data":{"duration":3600}}"#.to_vec());
        let e = real_time(&portless).expect_err("no port is not an arming");
        assert_eq!(
            e.to_string(),
            "the device's answer carries no broadcast port",
            "an operator debugging a broadcast that never starts reads this sentence"
        );
    }

    /// A datagram from a LAN is untrusted input, and a claim outlives the frame it came in.
    #[test]
    fn a_claim_that_no_configured_station_could_match_is_not_carried() {
        let c = codec();
        let with_did = |did: &str| {
            Frame::new(
                format!(
                    r#"{{"did":"{did}","ts":1,"conditions":[{{"data_structure_type":1,
                       "txid":1,"temp":1.0}}]}}"#
                )
                .into_bytes(),
            )
        };
        // A kilobyte of serial, and one with a newline in it: both would otherwise reach a
        // log field verbatim.
        assert!(c.claimed_station(&with_did(&"a".repeat(1024))).is_none());
        assert!(c.claimed_station(&with_did("001D0A70\\n0002")).is_none());
        assert!(c.claimed_station(&with_did("001D0A70 0002")).is_none());
        // The real thing still resolves, in either case, and a hyphen is permitted because
        // a vendor may yet use one.
        assert_eq!(
            c.claimed_station(&with_did("001D0A700002"))
                .map(|k| k.as_str().to_string()),
            Some("001D0A700002".to_string())
        );
        assert!(c.claimed_station(&with_did("001d-0a70-0002")).is_some());
    }

    #[test]
    fn a_frame_that_is_not_json_is_a_parse_failure_with_a_position() {
        let frame = Frame::new(b"<html>404</html>".to_vec());
        let e = codec().decode(&frame).expect_err("not JSON");
        assert!(matches!(e, CodecError::Json { .. }), "{e}");
    }
}
