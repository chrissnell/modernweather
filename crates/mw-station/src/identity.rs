//! [`StationIdentity`] — W2a §4a.4's five strategies, and the one place a frame becomes a
//! claim.
//!
//! **Where the authentication is.** Not here. `mw_component::StationRegistry::resolve` is
//! the single gate (W2a §4a.6) and it is reached from inside `Ingress::admit`, so there is
//! no `Envelope` on this path before a `StationKey` exists and no way for a driver to
//! produce one. What this type does is build the [`FrameMeta`] that gate reads: it decides
//! *what the frame claimed*, and the registry decides *whether that claim is a station*.
//!
//! **Why the variants carry no data, where W2a's sketch has `Fixed(StationId)` and
//! `FromQuery { id, secret }`.** Both of those are known at *declaration* time and belong
//! to the thing that declares the component, not to the frame. `Fixed`'s station is
//! configuration, so the driver closes over it and `FrameMeta::fixed` carries only the
//! receiving component — which is what makes `resolve` on a `Fixed` frame a lookup by
//! component rather than by a key the device sent (`mw_component::FrameMeta`, which states
//! that `presented_key` is for every source *but* `Fixed`). And `?ID=`/`?PASSWORD=` are the
//! Ambient *protocol's* parameter names, so they are that driver's constants (M6-1) rather
//! than a per-station value. What is left on the enum is exactly the part that varies per
//! frame, which is the part a `FrameMeta` needs.

use chrono::{DateTime, Utc};
use mw_component::{ComponentId, FrameMeta, IdentitySource};
use mw_types::StationKey;

/// How a station component's endpoint says which station a frame is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StationIdentity {
    /// Configuration says which station. Exactly one station per endpoint.
    Fixed,
    /// A path segment — `/ingest/ambient/{station_key}`, or an MQTT topic segment.
    FromPath,
    /// A query parameter, with the protocol's own secret beside it.
    FromQuery,
    /// Inside the payload — a WeatherLink Live's `did`, an APRS callsign.
    FromPayload,
    /// A peer registers and presents the key it was issued.
    FromRegistration,
}

/// Why an `identity_kind` column is not a [`StationIdentity`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{kind}` is not an identity strategy")]
pub struct IdentityError {
    /// What the column held.
    pub kind: String,
}

impl StationIdentity {
    /// Parse the `identity_kind` column.
    ///
    /// The spellings are the schema's `CHECK` list, so a value this rejects is a value that
    /// `CHECK` would have rejected on the way in.
    pub fn parse(kind: &str) -> Result<Self, IdentityError> {
        match kind {
            "fixed" => Ok(StationIdentity::Fixed),
            "from_path" => Ok(StationIdentity::FromPath),
            "from_query" => Ok(StationIdentity::FromQuery),
            "from_payload" => Ok(StationIdentity::FromPayload),
            "from_registration" => Ok(StationIdentity::FromRegistration),
            other => Err(IdentityError {
                kind: other.to_string(),
            }),
        }
    }

    /// The `identity_kind` value this is.
    pub const fn as_str(self) -> &'static str {
        match self {
            StationIdentity::Fixed => "fixed",
            StationIdentity::FromPath => "from_path",
            StationIdentity::FromQuery => "from_query",
            StationIdentity::FromPayload => "from_payload",
            StationIdentity::FromRegistration => "from_registration",
        }
    }

    /// The declarative half, for the shape `plan()` checks
    /// (`mw_component::IdentityShape`).
    pub const fn source(self) -> IdentitySource {
        match self {
            StationIdentity::Fixed => IdentitySource::Fixed,
            StationIdentity::FromPath => IdentitySource::FromPath,
            StationIdentity::FromQuery => IdentitySource::FromQuery,
            StationIdentity::FromPayload => IdentitySource::FromPayload,
            StationIdentity::FromRegistration => IdentitySource::FromRegistration,
        }
    }

    /// Whether a frame has to carry an identity for this strategy to resolve.
    pub const fn needs_a_claim(self) -> bool {
        !matches!(self, StationIdentity::Fixed)
    }

    /// Build the frame metadata the single gate reads.
    ///
    /// `claim` is what the frame presented and is ignored for [`StationIdentity::Fixed`],
    /// which carries none by construction. A strategy that needs a claim and was handed
    /// none produces a `FrameMeta` with `presented_key: None`, and `resolve` refuses it as
    /// `UnknownStation` — which is the right answer and is *counted*, where a driver that
    /// decided for itself would be a second gate.
    pub fn frame_meta(
        self,
        receiver: &ComponentId,
        claim: Option<&StationKey>,
        received_at: DateTime<Utc>,
    ) -> FrameMeta {
        match (self, claim) {
            (StationIdentity::Fixed, _) => FrameMeta::fixed(receiver.clone(), received_at),
            (_, Some(key)) => {
                FrameMeta::presenting(receiver.clone(), self.source(), key.as_str(), received_at)
            }
            (_, None) => FrameMeta {
                receiver: receiver.clone(),
                source: self.source(),
                presented_key: None,
                presented_secret: None,
                peer: None,
                received_at,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> ComponentId {
        ComponentId::new("station:wll:udp:22222").expect("a literal id")
    }

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_762_200_000, 0).expect("a literal instant")
    }

    #[test]
    fn every_schema_spelling_round_trips() {
        for kind in [
            "fixed",
            "from_path",
            "from_query",
            "from_payload",
            "from_registration",
        ] {
            let parsed = StationIdentity::parse(kind).expect("a schema spelling");
            assert_eq!(parsed.as_str(), kind);
        }
        assert_eq!(
            StationIdentity::parse("whoever")
                .expect_err("rejected")
                .kind,
            "whoever"
        );
    }

    /// `presented_key` is for every source but `Fixed`, so a fixed frame carries none even
    /// when the driver knows the station it is for.
    #[test]
    fn a_fixed_frame_presents_nothing_and_is_resolved_by_its_receiver() {
        let key = StationKey::new("suncrest:roof");
        let meta = StationIdentity::Fixed.frame_meta(&id(), Some(&key), at());
        assert_eq!(meta.source, IdentitySource::Fixed);
        assert_eq!(meta.presented_key, None);
        assert_eq!(meta.receiver, id());
        assert_eq!(meta.received_at, at());
        assert!(!StationIdentity::Fixed.needs_a_claim());
    }

    #[test]
    fn a_payload_identity_carries_the_claim_verbatim() {
        let key = StationKey::new("001D0A700002");
        let meta = StationIdentity::FromPayload.frame_meta(&id(), Some(&key), at());
        assert_eq!(meta.source, IdentitySource::FromPayload);
        assert_eq!(meta.presented_key.as_deref(), Some("001D0A700002"));
        assert!(meta.presented_secret.is_none(), "UDP carries no credential");
        assert!(StationIdentity::FromPayload.needs_a_claim());
    }

    /// A datagram with no `did` in it. The driver does not decide; it hands the gate a
    /// frame with nothing to resolve and the gate counts the refusal.
    #[test]
    fn a_claimless_frame_is_left_for_the_gate_to_refuse() {
        let meta = StationIdentity::FromPayload.frame_meta(&id(), None, at());
        assert_eq!(meta.source, IdentitySource::FromPayload);
        assert_eq!(meta.presented_key, None);
    }
}
