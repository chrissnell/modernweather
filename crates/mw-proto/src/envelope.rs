//! The two conversions between the domain [`Envelope`] and the wire one.
//!
//! This is the boundary W7 **D-11** puts the spool record on: the record's payload is
//! byte-for-byte the `collector.v1.Envelope` that goes on the wire, so one encoding
//! serves the segment file, the gRPC stream and `POST /v1/ingest`, and a spool written by
//! version *n* is read by version *n+1* under protobuf's own compatibility rules.
//!
//! **The decode is where provenance is re-sealed, and it is the only place.**
//! [`mw_types::Provenance::sealed`] refuses a path that no honest encoder produces —
//! empty, too long, repeating a node, or not starting at its origin — so an envelope whose
//! provenance is structurally impossible cannot be constructed from bytes, let alone
//! spooled. The three *positional* clauses of W7 **D-7**'s receive check (addressed to me,
//! I am not an interior hop, `path[len - 2]` is the sending peer) need to know who is
//! reading, so they belong to `Ingress::accept` and not here.
//!
//! **A relay re-encodes rather than copying the payload bytes**, because `forward` appends
//! a hop to `path` and that field lives in the payload. At the measured rate — roughly 2
//! envelopes/s over ~4 stations, ~120 bytes encoded at 30 % column population — that is a
//! few microseconds against a 10-second deadline (docs/collector.md §3.2). Hoisting `path`
//! into the record header to make the payload immutable is the available optimisation and
//! is not taken, because it would put one field in two places.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mw_types::{Envelope, OriginId, PathProblem, Provenance, Reading, Seq, StationKey};

use crate::reading::WireError;
use crate::wire;

/// Why a wire envelope could not become a domain envelope.
///
/// Every variant is a per-envelope rejection: W7 **D-8** requires the envelope to be
/// dropped and counted with the batch still acknowledged, because a batch that is retried
/// forever over one malformed member is a satellite that never drains.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvelopeError {
    /// `origin` is not 16 bytes, so it is not a `NodeId`.
    #[error("origin is {len} bytes, not the 16 a NodeId is")]
    BadOrigin {
        /// How many bytes arrived.
        len: usize,
    },

    /// An element of `path` is not 16 bytes.
    #[error("path element {index} is {len} bytes, not the 16 a NodeId is")]
    BadPathElement {
        /// Which element.
        index: usize,
        /// How many bytes it had.
        len: usize,
    },

    /// The path is not one an honest encoder produces (W7 **D-6**, **D-8**).
    #[error("the provenance is not well formed: {0}")]
    Provenance(PathProblem),

    /// `station_key` was empty. There is no envelope without a station (W2a §4a.6), and an
    /// empty key would create history under a name nothing else references.
    #[error("the envelope names no station")]
    NoStation,

    /// `reading` was absent. An envelope with no reading is an acknowledgement-shaped
    /// nothing; it would store as an all-`NULL` row the aggregates still have to process.
    #[error("the envelope carries no reading")]
    NoReading,

    /// `observed_at` or `received_at` is not a point in time.
    #[error("{field} is not a representable instant ({millis} ms since the epoch)")]
    BadInstant {
        /// Which field.
        field: &'static str,
        /// What it claimed.
        millis: i64,
    },

    /// The reading itself did not decode.
    #[error("the reading did not decode")]
    Reading(#[source] WireError),
}

impl mw_types::Classify for EnvelopeError {
    /// `Ignorable`, uniformly: a malformed envelope is a counted rejection and not a
    /// failure of the component that received it (**D-46**), and re-reading the same bytes
    /// cannot change the answer.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Ignorable
    }
}

fn origin_id(bytes: &[u8]) -> Option<OriginId> {
    <[u8; 16]>::try_from(bytes).ok().map(OriginId::new)
}

fn millis(field: &'static str, at: i64) -> Result<DateTime<Utc>, EnvelopeError> {
    DateTime::from_timestamp_millis(at).ok_or(EnvelopeError::BadInstant { field, millis: at })
}

impl From<&Envelope> for wire::Envelope {
    fn from(e: &Envelope) -> Self {
        wire::Envelope {
            origin: prost::bytes::Bytes::copy_from_slice(e.provenance.origin().as_bytes()),
            seq: e.provenance.seq().get(),
            path: e
                .provenance
                .path()
                .hops()
                .iter()
                .map(|h| prost::bytes::Bytes::copy_from_slice(h.as_bytes()))
                .collect(),
            station_key: e.station.as_str().to_string(),
            observed_at: e.observed_at.timestamp_millis(),
            received_at: e.received_at.timestamp_millis(),
            reading: Some(wire::Reading::from(e.reading.as_ref())),
        }
    }
}

impl TryFrom<&wire::Envelope> for Envelope {
    type Error = EnvelopeError;

    fn try_from(w: &wire::Envelope) -> Result<Self, Self::Error> {
        let origin = origin_id(&w.origin).ok_or(EnvelopeError::BadOrigin {
            len: w.origin.len(),
        })?;
        let mut hops = Vec::with_capacity(w.path.len());
        for (index, raw) in w.path.iter().enumerate() {
            hops.push(origin_id(raw).ok_or(EnvelopeError::BadPathElement {
                index,
                len: raw.len(),
            })?);
        }
        let provenance = Provenance::sealed(origin, Seq::new(w.seq), &hops)
            .map_err(EnvelopeError::Provenance)?;
        if w.station_key.is_empty() {
            return Err(EnvelopeError::NoStation);
        }
        let reading = w.reading.as_ref().ok_or(EnvelopeError::NoReading)?;
        let reading = Reading::try_from(reading).map_err(EnvelopeError::Reading)?;
        Ok(Envelope {
            station: StationKey::new(w.station_key.as_str()),
            observed_at: millis("observed_at", w.observed_at)?,
            received_at: millis("received_at", w.received_at)?,
            provenance,
            reading: Arc::new(reading),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn node(n: u8) -> OriginId {
        OriginId::new([n; 16])
    }

    fn envelope() -> Envelope {
        let reading = Reading {
            time: DateTime::from_timestamp_millis(1_762_000_000_000).expect("a real instant"),
            station_id: "backyard".into(),
            barometer: Some(29.92),
            ..Reading::default()
        };
        Envelope {
            station: StationKey::new("backyard"),
            observed_at: DateTime::from_timestamp_millis(1_762_000_000_000).expect("instant"),
            received_at: DateTime::from_timestamp_millis(1_762_000_000_250).expect("instant"),
            provenance: Provenance::admitted(node(1), Seq::new(4711)),
            reading: Arc::new(reading),
        }
    }

    #[test]
    fn an_envelope_round_trips_through_the_bytes() {
        let before = envelope();
        let bytes = wire::Envelope::from(&before).encode_to_vec();
        let decoded = wire::Envelope::decode(bytes.as_slice()).expect("our own bytes");
        let after = Envelope::try_from(&decoded).expect("our own encoding decodes");
        assert_eq!(before, after);
    }

    #[test]
    fn a_relayed_envelope_round_trips_with_its_whole_path() {
        let before = envelope()
            .forward(node(2))
            .expect("A -> B")
            .forward(node(3))
            .expect("B -> C");
        let bytes = wire::Envelope::from(&before).encode_to_vec();
        let decoded = wire::Envelope::decode(bytes.as_slice()).expect("our own bytes");
        let after = Envelope::try_from(&decoded).expect("decodes");
        assert_eq!(after.provenance.path().hops(), &[node(1), node(2), node(3)]);
        assert_eq!(after.provenance.origin(), node(1), "never rewritten");
        assert_eq!(after.provenance.seq(), Seq::new(4711), "never renumbered");
    }

    /// The decode is the second of the two loop guards, and it has to refuse what
    /// `forward` could not have produced — otherwise a peer could present a forged path
    /// and get an envelope the type system says cannot exist.
    #[test]
    fn a_forged_path_cannot_be_decoded_into_an_envelope() {
        let mut w = wire::Envelope::from(&envelope());
        let dup = prost::bytes::Bytes::copy_from_slice(node(1).as_bytes());
        w.path = vec![
            dup.clone(),
            prost::bytes::Bytes::copy_from_slice(node(2).as_bytes()),
            dup,
        ];
        assert_eq!(
            Envelope::try_from(&w),
            Err(EnvelopeError::Provenance(PathProblem::Duplicate {
                node: node(1)
            }))
        );

        let mut w = wire::Envelope::from(&envelope());
        w.path = (1..=9)
            .map(|n| prost::bytes::Bytes::copy_from_slice(node(n).as_bytes()))
            .collect();
        assert_eq!(
            Envelope::try_from(&w),
            Err(EnvelopeError::Provenance(PathProblem::TooLong { len: 9 }))
        );

        let mut w = wire::Envelope::from(&envelope());
        w.path = vec![prost::bytes::Bytes::copy_from_slice(node(2).as_bytes())];
        assert_eq!(
            Envelope::try_from(&w),
            Err(EnvelopeError::Provenance(PathProblem::OriginNotFirst {
                origin: node(1)
            }))
        );
    }

    #[test]
    fn a_short_origin_is_not_a_node_id() {
        let mut w = wire::Envelope::from(&envelope());
        w.origin = prost::bytes::Bytes::from_static(&[1, 2, 3]);
        assert_eq!(
            Envelope::try_from(&w),
            Err(EnvelopeError::BadOrigin { len: 3 })
        );
    }

    #[test]
    fn an_envelope_without_a_station_is_refused() {
        let mut w = wire::Envelope::from(&envelope());
        w.station_key = String::new();
        assert_eq!(Envelope::try_from(&w), Err(EnvelopeError::NoStation));
    }

    #[test]
    fn an_envelope_without_a_reading_is_refused() {
        let mut w = wire::Envelope::from(&envelope());
        w.reading = None;
        assert_eq!(Envelope::try_from(&w), Err(EnvelopeError::NoReading));
    }

    /// The payload is the wire message, so a field this version does not know about
    /// survives a decode-and-re-encode. That is what W7 D-11 buys and it is worth an
    /// assertion rather than an appeal to protobuf's documentation.
    #[test]
    fn an_unknown_field_is_what_makes_an_in_place_upgrade_safe() {
        let mut bytes = wire::Envelope::from(&envelope()).encode_to_vec();
        // Field 99, wire type 0 (varint), value 1 -- a field a later version might add.
        // The key is the varint 99 << 3 == 792, which is two bytes.
        bytes.extend_from_slice(&[0x98, 0x06, 0x01]);
        let decoded = wire::Envelope::decode(bytes.as_slice()).expect("unknown fields are skipped");
        assert_eq!(
            Envelope::try_from(&decoded).expect("decodes"),
            envelope(),
            "an unknown field does not stop this version reading its own data"
        );
    }
}
