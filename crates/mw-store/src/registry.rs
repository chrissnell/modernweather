//! `StationRegistry` — identity resolution, over a published snapshot.
//!
//! **The one gate** (W2a §4a.6). `Ingress::admit` calls [`ConfigRegistry::resolve`] before
//! an `Envelope` exists, and this is the only producer of a
//! [`StationKey`](mw_types::StationKey) on that path — so an unauthenticated reading cannot
//! be stored, structurally rather than by convention. The Go gRPC receiver does the
//! opposite (`station.go:174` stores, `:194` consults the registry) and that ordering is
//! not expressible here.
//!
//! **Synchronous, over an `ArcSwap`.** W3 **D-87** requires `admit` to do no I/O but the
//! spool append, and `mw_component::StationRegistry::resolve` is a `fn` returning a
//! `Result` rather than a future — which is that requirement in a signature. So this holds
//! a *snapshot* and a resolve is a pointer load, two map lookups and a comparison. The Go
//! collector calls `LoadConfig()` **on every single reading**, which is the behaviour the
//! signature makes unreachable.
//!
//! **Two lookups, because there are two kinds of claim.**
//!
//! - `IdentitySource::Fixed` — configuration says which station, so the frame presents
//!   nothing and resolution is **by receiving component**
//!   (`mw_component::FrameMeta::presented_key` is documented as being for every source but
//!   this one). The receiver-to-station map is minted by the registrar that minted the
//!   component ids (`mw_station::fixed_receivers`), so nothing here parses an id.
//! - everything else — the frame presented a key, and matching it against
//!   `config.stations.presented_key` *is* the authentication for a transport with no
//!   credential.
//!
//! **Who publishes this is still open.** W2a §4.1b gives `StationRegistry` to a
//! `config:sqlite` component, which also owns `ConfigStore`, `ConfigAdmin` and the change
//! watch — and no work item in `docs/implementation-plan.md` creates it. So the binary owns
//! the snapshot as a root resource for now, built once at boot beside the SQLite pool
//! (**D-41**), and [`ConfigRegistry::publish`] is the seam a change watch will drive when
//! that component arrives. What that costs today is stated rather than hidden: a station
//! added to `config.stations` while the daemon runs is not resolvable until it restarts,
//! which is also true of every other configuration change until `ConfigAdmin` exists
//! (M7-1).

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use mw_component::{ComponentId, FrameMeta, IdentitySource, Reject, StationRegistry};
use mw_types::StationKey;

use crate::config_db::StationRow;

/// What the gate knows about one station.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationEntry {
    /// The station this resolves to.
    pub station: StationKey,
    /// Whether the operator has switched collection on.
    pub enabled: bool,
    /// Whether the protocol presents a secret that has to be verified.
    pub has_secret: bool,
}

/// One immutable view of `config.stations`, as the gate reads it.
#[derive(Debug, Default)]
pub struct RegistrySnapshot {
    /// Lower-cased `presented_key` to station.
    ///
    /// Lower-cased on both sides, which makes the comparison ASCII-case-insensitive. The
    /// reason is the same one **DEV-2** gives for `Host` matching: a WeatherLink Live
    /// reports `001D0A700002` and an operator types `001d0a700002`, and a case-sensitive
    /// comparison turns that into a station whose datagrams silently never resolve — no
    /// error, no log line, just a flat line on a graph.
    by_claim: BTreeMap<String, StationEntry>,
    /// Receiving component to station, for a `Fixed` identity.
    by_receiver: BTreeMap<ComponentId, StationEntry>,
}

impl RegistrySnapshot {
    /// Build a snapshot from the station rows and the `Fixed`-identity receivers.
    ///
    /// `receivers` comes from the registrar that minted the component ids, so the two
    /// cannot disagree about what a component is called.
    pub fn new<'a>(
        rows: &[StationRow],
        receivers: impl IntoIterator<Item = (&'a ComponentId, &'a StationKey)>,
    ) -> Self {
        let mut entries: BTreeMap<&str, StationEntry> = BTreeMap::new();
        let mut by_claim = BTreeMap::new();
        for row in rows {
            let entry = StationEntry {
                station: StationKey::new(row.station_key.as_str()),
                enabled: row.enabled,
                has_secret: row
                    .ingest_secret_hash
                    .as_deref()
                    .is_some_and(|h| !h.is_empty()),
            };
            entries.insert(row.station_key.as_str(), entry.clone());
            if let Some(claim) = row.presented_key.as_deref().filter(|c| !c.is_empty()) {
                // A duplicate claim is rejected at plan time (`StationPlanError::
                // DuplicateClaim`) and by the schema's own unique index, so the last writer
                // here would be a row set that cannot be planned. Inserting rather than
                // refusing keeps this constructor total: the gate's job is to resolve what
                // it was given, and the plan is where a contradiction is reported.
                by_claim.insert(claim.to_ascii_lowercase(), entry);
            }
        }
        let by_receiver = receivers
            .into_iter()
            .filter_map(|(component, station)| {
                entries
                    .get(station.as_str())
                    .map(|entry| (component.clone(), entry.clone()))
            })
            .collect();
        Self {
            by_claim,
            by_receiver,
        }
    }

    /// How many stations can be resolved by a claim.
    pub fn claims(&self) -> usize {
        self.by_claim.len()
    }

    /// How many components resolve by being the receiver.
    pub fn receivers(&self) -> usize {
        self.by_receiver.len()
    }
}

/// The `StationRegistry` the daemon runs on.
#[derive(Debug)]
pub struct ConfigRegistry {
    snapshot: ArcSwap<RegistrySnapshot>,
}

impl ConfigRegistry {
    /// A registry over one snapshot.
    pub fn new(snapshot: RegistrySnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(snapshot),
        }
    }

    /// Replace the snapshot. One store, so no reader can observe a half-updated registry.
    ///
    /// The seam a configuration change watch will drive. Nothing calls it yet; see this
    /// module's documentation for what that costs and who owns closing it.
    pub fn publish(&self, snapshot: RegistrySnapshot) {
        self.snapshot.store(Arc::new(snapshot));
    }

    /// The snapshot, for a test and for the startup log line.
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.snapshot.load_full()
    }

    fn admit(entry: &StationEntry) -> Result<StationKey, Reject> {
        if !entry.enabled {
            return Err(Reject::Disabled);
        }
        if entry.has_secret {
            // Fail closed. A station with `ingest_secret_hash` set is an Ambient station,
            // whose `?PASSWORD=` has to be verified in constant time against the stored
            // hash (W2a §4a.6, **D-33**) -- and that verification is Argon2, which costs
            // ~100 ms by design and therefore has to be reconciled with **D-87**'s "no I/O
            // and nothing slow on the admit path" before it goes here. That is M6-1's, with
            // the driver that presents a secret. Until then no plannable component can
            // present one (`mw_station::plan` refuses every type but WeatherLink Live), so
            // this branch is unreachable in a running daemon -- and refusing is still the
            // right answer, because the alternative is a configured secret that is ignored.
            return Err(Reject::BadSecret);
        }
        Ok(entry.station.clone())
    }
}

impl StationRegistry for ConfigRegistry {
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject> {
        let snapshot = self.snapshot.load();
        match meta.source {
            IdentitySource::Fixed => snapshot
                .by_receiver
                .get(&meta.receiver)
                .ok_or(Reject::UnknownStation)
                .and_then(Self::admit),
            _ => {
                let claim = meta
                    .presented_key
                    .as_deref()
                    .ok_or(Reject::UnknownStation)?
                    .to_ascii_lowercase();
                snapshot
                    .by_claim
                    .get(&claim)
                    .ok_or(Reject::UnknownStation)
                    .and_then(Self::admit)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn row(key: &str, claim: Option<&str>) -> StationRow {
        StationRow {
            station_key: key.into(),
            name: key.into(),
            kind: "weatherlink-live".into(),
            enabled: true,
            identity_kind: if claim.is_some() {
                "from_payload"
            } else {
                "fixed"
            }
            .into(),
            presented_key: claim.map(str::to_string),
            ingest_secret_hash: None,
            transport_kind: Some("udp_in".into()),
            transport_dev: None,
            transport_baud: None,
            transport_host: None,
            transport_port: Some(22222),
            transport_url: None,
            transport_method: None,
            transport_auth: None,
            transport_streaming: None,
            transport_mount: None,
            transport_group: None,
            transport_topic: None,
            transport_tls_ca_path: None,
            transport_tls_cert_path: None,
            transport_tls_key_path: None,
            wll_sensor_mapping: Some("th:1".into()),
            wll_poll_interval_seconds: None,
        }
    }

    fn id(text: &str) -> ComponentId {
        ComponentId::new(text).expect("a literal component id")
    }

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_762_200_000, 0).expect("a literal instant")
    }

    fn registry() -> ConfigRegistry {
        let rows = vec![
            row("suncrest:roof", Some("001D0A700002")),
            row("suncrest:field", None),
        ];
        let component = id("station:wll:suncrest:field");
        let station = StationKey::new("suncrest:field");
        let snapshot = RegistrySnapshot::new(&rows, [(&component, &station)]);
        assert_eq!((snapshot.claims(), snapshot.receivers()), (1, 1));
        ConfigRegistry::new(snapshot)
    }

    #[test]
    fn a_presented_claim_resolves_whatever_case_it_arrives_in() {
        let registry = registry();
        for claim in ["001D0A700002", "001d0a700002", "001D0a700002"] {
            let meta = FrameMeta::presenting(
                id("station:wll:udp:22222"),
                IdentitySource::FromPayload,
                claim,
                at(),
            );
            assert_eq!(
                registry.resolve(&meta).expect("resolves").as_str(),
                "suncrest:roof",
                "claim {claim}"
            );
        }
    }

    #[test]
    fn an_unknown_claim_is_a_counted_rejection_and_not_a_registration() {
        let registry = registry();
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A7FFFFF",
            at(),
        );
        assert_eq!(registry.resolve(&meta), Err(Reject::UnknownStation));
        // ... and the registry did not learn it, which is §4a.6's "never registered".
        assert_eq!(registry.snapshot().claims(), 1);
    }

    #[test]
    fn a_claimless_non_fixed_frame_is_refused() {
        let registry = registry();
        let meta = FrameMeta {
            receiver: id("station:wll:udp:22222"),
            source: IdentitySource::FromPayload,
            presented_key: None,
            presented_secret: None,
            peer: None,
            received_at: at(),
        };
        assert_eq!(registry.resolve(&meta), Err(Reject::UnknownStation));
    }

    #[test]
    fn a_fixed_frame_resolves_by_its_receiving_component() {
        let registry = registry();
        let meta = FrameMeta::fixed(id("station:wll:suncrest:field"), at());
        assert_eq!(
            registry.resolve(&meta).expect("resolves").as_str(),
            "suncrest:field"
        );
        // A component nobody minted a receiver entry for resolves to nothing, which is what
        // a `Fixed` frame from an unplanned component is.
        let other = FrameMeta::fixed(id("station:wll:elsewhere"), at());
        assert_eq!(registry.resolve(&other), Err(Reject::UnknownStation));
    }

    /// The claim and the receiver are separate lookups: a `Fixed` frame must not resolve by
    /// accidentally matching a serial, and a presented claim must not resolve by matching a
    /// component id.
    #[test]
    fn the_two_lookups_do_not_leak_into_each_other() {
        let registry = registry();
        // The broadcasting station's claim, presented on a `Fixed` frame.
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::Fixed,
            "001D0A700002",
            at(),
        );
        assert_eq!(
            registry.resolve(&meta),
            Err(Reject::UnknownStation),
            "a Fixed frame is resolved by its receiver and nothing else"
        );
    }

    #[test]
    fn a_disabled_station_is_refused_as_disabled_and_not_as_unknown() {
        let mut rows = vec![row("suncrest:roof", Some("001D0A700002"))];
        rows[0].enabled = false;
        let registry = ConfigRegistry::new(RegistrySnapshot::new(&rows, []));
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A700002",
            at(),
        );
        assert_eq!(registry.resolve(&meta), Err(Reject::Disabled));
    }

    /// Fail closed: a configured secret that nothing verifies would be worse than a refusal.
    #[test]
    fn a_station_with_a_configured_secret_is_refused_until_something_verifies_it() {
        let mut rows = vec![row("ambient:backyard", Some("KAMBIENT1"))];
        rows[0].ingest_secret_hash = Some("$argon2id$v=19$...".into());
        let registry = ConfigRegistry::new(RegistrySnapshot::new(&rows, []));
        let meta = FrameMeta::presenting(
            id("station:ambient"),
            IdentitySource::FromQuery,
            "KAMBIENT1",
            at(),
        );
        assert_eq!(registry.resolve(&meta), Err(Reject::BadSecret));
    }

    #[test]
    fn publishing_a_new_snapshot_replaces_the_whole_view_at_once() {
        let registry = registry();
        registry.publish(RegistrySnapshot::new(
            &[row("suncrest:barn", Some("001D0A700099"))],
            [],
        ));
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A700099",
            at(),
        );
        assert_eq!(
            registry.resolve(&meta).expect("resolves").as_str(),
            "suncrest:barn"
        );
        // The old claim is gone, rather than both being live.
        let old = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A700002",
            at(),
        );
        assert_eq!(registry.resolve(&old), Err(Reject::UnknownStation));
    }
}
