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
use mw_component::{
    ComponentId, FrameMeta, IdentitySource, Reject, StationMeta, StationProjection,
    StationRegistry, Zone, ZoneError,
};
use mw_types::StationKey;

use crate::config_db::StationRow;

/// What the gate knows about one station.
///
/// `PartialEq` but not `Eq`: the coordinates and the snow base distance are `f64`, which
/// has no total equality, and the comparison this type is compared for is a test asserting
/// a snapshot's contents.
#[derive(Debug, Clone, PartialEq)]
pub struct StationEntry {
    /// The station this resolves to.
    pub station: StationKey,
    /// Whether the operator has switched collection on.
    pub enabled: bool,
    /// How this station is configured to be identified.
    ///
    /// Checked against the strategy the frame arrived under. W2a §4a.6 writes the gate as
    /// `resolve(meta, id: &StationIdentity, reg)` — the configured strategy is a *separate
    /// argument* — and describes a **per-transport** allow-list: "the station set is an
    /// allow-list from the registry" for `UdpIn`, "topic → station from the registry
    /// allow-list" for `MqttSub`. A single map keyed on the claim alone collapses those
    /// into one namespace, so an `?ID=` could resolve a WeatherLink Live's DSN and a topic
    /// segment could resolve an Ambient id. Nothing exploits that today — WeatherLink Live
    /// is the only driver — but this is the authentication gate, and one field now is
    /// cheaper than auditing three drivers later.
    pub identity: IdentitySource,
    /// Whether the protocol presents a secret that has to be verified.
    pub has_secret: bool,
    /// What the enrich stage reads (**R1**, W3 §2.5).
    pub meta: StationMeta,
    /// What the weather schema's `stations` projection should say (**D-56**, W4 §2.1).
    ///
    /// Carried on the entry rather than looked up separately, because both are a copy of
    /// the same row and a second map keyed the same way is a second thing to keep in step.
    pub projection: StationProjection,
    /// `config.stations.timezone`, **resolved** (W2a Decision 11's parse-is-the-validation).
    ///
    /// `None` when the column is absent, blank, or not an IANA name — and which of those it
    /// was is in [`RegistrySnapshot::zone_problems`], because the three need different
    /// things said about them. Resolved here rather than at each reader so that the string
    /// is parsed **once**: `rain:rollover:<station>`'s `TzSource` and `window:rollup`'s
    /// calendar boundaries both need it, and a second parse is a second answer to "which
    /// midnight does this station's day begin at".
    pub zone: Option<Zone>,
}

/// A declared time zone that is not one.
///
/// Reported rather than ignored, because nothing else would notice. **The W4 migration's
/// comment on `stations.timezone` says the column is "an IANA name, validated by the writer
/// rather than by a CHECK" — and the writer does not validate it.** It copies the column
/// and substitutes `'UTC'` for a NULL or blank one (`mw_store::writer`), so an operator's
/// typo becomes a `stations.timezone` of `Amercia/Denver` and a rollover that happens in
/// UTC. A SQL `CHECK` genuinely cannot do it: `now() AT TIME ZONE x` is `STABLE`, so it is
/// not an expression a constraint may contain.
///
/// So this is the check, and `crates/modernweather/src/boot.rs` makes it a **startup
/// refusal naming the station**. A station with no zone at all is not reported here: that is
/// a `PlanError::MissingTimeZone` from `plan()`, which names the component as well, and
/// reporting it twice would make the louder message the less precise one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "station `{station}` declares the time zone `{declared}`, which is not usable: \
     {source}. Its rainfall day, month and year roll over at its own local midnight (W3 \
     D-24), so the value has to be an `Area/Location` IANA name -- `America/Denver`, not \
     `MST` and not `-0700`. Fix `config.stations.timezone` for it."
)]
pub struct ZoneProblem {
    /// Which station.
    pub station: StationKey,
    /// What the column held.
    pub declared: String,
    /// Why it is not a zone.
    #[source]
    pub source: ZoneError,
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
    /// Declared time zones that are not zones, in station-key order.
    zone_problems: Vec<ZoneProblem>,
    /// Station key to entry, for the two lookups that are **not** authentication.
    ///
    /// Keyed by the station's own key rather than by a claim or a receiver, because
    /// `metadata` and `projection` answer "what is configured for this station" and the
    /// station is already resolved by the time either is asked. Deliberately **not** a
    /// resolution path: a key that is not here answers "nothing configured", which is what
    /// keeps a peer's unregistered station storable (see `StationRegistry::metadata`).
    by_station: BTreeMap<String, StationEntry>,
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
        let mut zone_problems = Vec::new();
        for row in rows {
            // A column value its own `CHECK` would reject means this build and its schema
            // disagree. `Fixed` is the safe reading: it is the strategy that resolves by
            // receiver, so the row simply never matches a presented claim.
            let identity = IdentitySource::from_schema_name(&row.identity_kind)
                .unwrap_or(IdentitySource::Fixed);
            // The parse is the validation (W2a Decision 11). A blank column is absent
            // rather than a problem -- `StationProjection`'s own normalisation below does
            // the same thing, for the same reason -- and a non-blank value that does not
            // resolve is a problem with the station named.
            let declared = row
                .timezone
                .as_deref()
                .map(str::trim)
                .filter(|z| !z.is_empty());
            let zone = match declared.map(Zone::new) {
                None => None,
                Some(Ok(zone)) => Some(zone),
                Some(Err(source)) => {
                    zone_problems.push(ZoneProblem {
                        station: StationKey::new(row.station_key.as_str()),
                        declared: declared.unwrap_or("").to_string(),
                        source,
                    });
                    None
                }
            };
            let entry = StationEntry {
                station: StationKey::new(row.station_key.as_str()),
                zone,
                enabled: row.enabled,
                identity,
                has_secret: row
                    .ingest_secret_hash
                    .as_deref()
                    .is_some_and(|h| !h.is_empty()),
                meta: StationMeta {
                    latitude: row.latitude,
                    longitude: row.longitude,
                    altitude_m: row.altitude_m,
                    snow_base_distance_mm: row.snow_base_distance_mm,
                },
                projection: StationProjection {
                    name: row.name.clone(),
                    kind: row.kind.clone(),
                    // Empty is not a zone. The column is nullable and an operator can
                    // also leave it blank, and `chrono-tz` would reject `""` at the
                    // calendar trigger -- so it is normalised to absent here, where the
                    // row is read, rather than at every reader.
                    timezone: row.timezone.clone().filter(|z| !z.trim().is_empty()),
                    latitude: row.latitude,
                    longitude: row.longitude,
                    altitude_m: row.altitude_m,
                    snow_base_distance_mm: row.snow_base_distance_mm,
                },
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
        let by_station = entries
            .into_iter()
            .map(|(key, entry)| (key.to_string(), entry))
            .collect();
        Self {
            by_claim,
            by_receiver,
            by_station,
            zone_problems,
        }
    }

    /// Every station and its resolved zone, in key order.
    ///
    /// What the rollup's registrar plans from: one `rain:rollover:<station>` per station,
    /// carrying the zone its `TzSource::Station` resolves. A station whose zone is `None`
    /// still gets a component, and `plan()` then refuses it by name — `mw_pipeline::rollup`
    /// has the whole of why skipping it would be worse.
    pub fn zones(&self) -> impl Iterator<Item = (&StationKey, Option<Zone>)> {
        self.by_station.values().map(|e| (&e.station, e.zone))
    }

    /// Declared time zones that are not zones. Empty is the expected case.
    pub fn zone_problems(&self) -> &[ZoneProblem] {
        &self.zone_problems
    }

    /// How many stations can be resolved by a claim.
    pub fn claims(&self) -> usize {
        self.by_claim.len()
    }

    /// How many components resolve by being the receiver.
    pub fn receivers(&self) -> usize {
        self.by_receiver.len()
    }

    /// The stations carrying a secret nothing can verify yet, for the startup warning.
    ///
    /// See [`ConfigRegistry::admit`]: such a station resolves to `BadSecret` on every
    /// frame, which is the right answer and an invisible one until traffic arrives.
    pub fn unverifiable_secrets(&self) -> Vec<&StationKey> {
        self.by_claim
            .values()
            .filter(|e| e.has_secret)
            .map(|e| &e.station)
            .collect()
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

    fn admit(entry: &StationEntry, source: IdentitySource) -> Result<StationKey, Reject> {
        // The configured strategy has to be the one the frame arrived under. Without this,
        // `by_claim` is one global namespace across every non-`Fixed` strategy and a frame
        // from one transport can resolve another transport's station (see
        // `StationEntry::identity`). `UnknownStation` rather than a new reason: from the
        // gate's point of view there is no station with that claim on that transport.
        if entry.identity != source {
            return Err(Reject::UnknownStation);
        }
        if !entry.enabled {
            return Err(Reject::Disabled);
        }
        if entry.has_secret {
            // Fail closed. A station with `ingest_secret_hash` set is an Ambient station,
            // whose `?PASSWORD=` has to be verified in constant time against the stored
            // hash (W2a §4a.6, **D-33**) -- and that verification is Argon2, which costs
            // ~100 ms by design and therefore has to be reconciled with **D-87**'s "no I/O
            // and nothing slow on the admit path" before it goes here. That is M6-1's, with
            // the driver that presents a secret.
            //
            // **Reachable, not unreachable.** Nothing ties `ingest_secret_hash` to
            // `identity_kind`, so a hash pasted onto a WeatherLink Live row is a row the
            // schema accepts and `plan()` does not look at: the listener binds, the daemon
            // reports ready, and every datagram is counted `bad_secret` with nothing naming
            // the station. The counter alone only moves when traffic arrives, so
            // `crates/modernweather/src/boot.rs` warns once per such row at startup
            // instead. Refusing is still the right answer -- the alternative is a
            // configured secret that is silently ignored.
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
                .and_then(|entry| Self::admit(entry, IdentitySource::Fixed)),
            source => {
                let claim = meta
                    .presented_key
                    .as_deref()
                    .ok_or(Reject::UnknownStation)?
                    .to_ascii_lowercase();
                snapshot
                    .by_claim
                    .get(&claim)
                    .ok_or(Reject::UnknownStation)
                    .and_then(|entry| Self::admit(entry, source))
            }
        }
    }

    /// The four static values **R1** admits into a derived column.
    ///
    /// Total: an unknown station is [`StationMeta::EMPTY`], not a refusal. A `disabled`
    /// station answers its configured metadata rather than nothing, because `enabled` is
    /// about *collection* and this is asked after a reading already exists -- a reading
    /// forwarded by a peer, or one admitted before the operator switched the station off.
    fn metadata(&self, station: &StationKey) -> StationMeta {
        self.snapshot
            .load()
            .by_station
            .get(station.as_str())
            .map_or(StationMeta::EMPTY, |e| e.meta)
    }

    fn projection(&self, station: &StationKey) -> Option<StationProjection> {
        self.snapshot
            .load()
            .by_station
            .get(station.as_str())
            .map(|e| e.projection.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn row(key: &str, claim: Option<&str>) -> StationRow {
        StationRow {
            id: 1,
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
            latitude: Some(39.7392),
            longitude: Some(-104.9903),
            altitude_m: Some(1609.0),
            timezone: Some("America/Denver".into()),
            snow_base_distance_mm: Some(1500.0),
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

    /// A frame must not resolve a station configured for a different identity strategy.
    ///
    /// `by_claim` is one map, so without the strategy check an Ambient `?ID=` could resolve
    /// a WeatherLink Live's DSN — one namespace across every transport, which is not the
    /// per-transport allow-list §4a.6 specifies.
    #[test]
    fn a_claim_does_not_resolve_a_station_configured_for_another_strategy() {
        let registry = registry();
        for source in [
            IdentitySource::FromQuery,
            IdentitySource::FromPath,
            IdentitySource::FromRegistration,
        ] {
            let meta = FrameMeta::presenting(
                id("station:ambient"),
                source,
                // The broadcasting station's serial, presented on the wrong transport.
                "001D0A700002",
                at(),
            );
            assert_eq!(
                registry.resolve(&meta),
                Err(Reject::UnknownStation),
                "{source} resolved a from_payload station"
            );
        }
        // ... and the strategy it IS configured for still resolves.
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A700002",
            at(),
        );
        assert_eq!(
            registry.resolve(&meta).expect("resolves").as_str(),
            "suncrest:roof"
        );
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
        // An Ambient station: `from_query`, because the strategy check runs first and a
        // `from_payload` row presented on a query transport is `UnknownStation` before the
        // secret is ever reached.
        rows[0].identity_kind = "from_query".into();
        rows[0].kind = "ambient-customized".into();
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

    /// **The validation the W4 migration's comment claimed the writer did.** A typo in
    /// `config.stations.timezone` is reported with the station and the value named, and the
    /// entry's resolved zone is `None` rather than a silent `UTC` — which is what the
    /// writer substitutes today and what makes the column decorative.
    #[test]
    fn a_declared_zone_that_is_not_a_zone_is_reported_with_the_station_named() {
        let mut rows = vec![
            row("suncrest:backyard", Some("001D0A700002")),
            row("suncrest:roof", Some("001D0A700003")),
        ];
        rows[0].timezone = Some("Amercia/Denver".into());
        rows[1].timezone = Some("MST".into());
        let snapshot = RegistrySnapshot::new(&rows, []);

        let problems = snapshot.zone_problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        let backyard = problems
            .iter()
            .find(|p| p.station.as_str() == "suncrest:backyard")
            .expect("reported");
        assert_eq!(backyard.declared, "Amercia/Denver");
        let text = mw_types::DisplayChain(backyard).to_string();
        assert!(text.contains("suncrest:backyard"), "{text}");
        assert!(text.contains("Amercia/Denver"), "{text}");
        assert!(text.contains("config.stations.timezone"), "{text}");
        // `MST` is a zone abbreviation: one fixed offset, so a station in it cannot have a
        // DST transition at all (W2a Decision 11). Rejected for a different reason from a
        // typo and with the same loudness.
        assert!(problems.iter().any(|p| p.declared == "MST"));

        // And no station resolved a zone, so nothing will roll over in the wrong one.
        let zones: Vec<(&str, Option<&'static str>)> = snapshot
            .zones()
            .map(|(k, z)| (k.as_str(), z.map(|z| z.name())))
            .collect();
        assert_eq!(
            zones,
            vec![("suncrest:backyard", None), ("suncrest:roof", None)]
        );
    }

    /// A blank or absent column is **absent**, not a problem — the same normalisation the
    /// projection already made, and for the same reason: `chrono-tz` would reject `""` at
    /// the calendar trigger. It is still not a zone, so `plan()` refuses the station's
    /// rollover by name; what it is not is a *typo*, and conflating the two would make the
    /// louder message the less precise one.
    #[test]
    fn a_blank_or_absent_zone_is_absent_rather_than_a_typo() {
        let mut rows = vec![
            row("suncrest:backyard", Some("001D0A700002")),
            row("suncrest:roof", Some("001D0A700003")),
        ];
        rows[0].timezone = None;
        rows[1].timezone = Some("   ".into());
        let snapshot = RegistrySnapshot::new(&rows, []);
        assert!(snapshot.zone_problems().is_empty());
        assert!(snapshot.zones().all(|(_, z)| z.is_none()));
        // The projection normalises the same way, so the two cannot disagree about whether
        // this station has a zone.
        for (_, entry) in snapshot.by_station.iter() {
            assert_eq!(entry.projection.timezone, None);
        }
    }

    /// The ordinary case, and the one the rollup plans from: a valid name resolves once,
    /// here, and both readers of it get the same `Zone`.
    #[test]
    fn a_valid_zone_is_parsed_once_and_carried_on_the_entry() {
        let snapshot = RegistrySnapshot::new(
            &[row("suncrest:backyard", Some("001D0A700002")), {
                let mut r = row("suncrest:roof", Some("001D0A700003"));
                r.timezone = Some("Asia/Kathmandu".into());
                r
            }],
            [],
        );
        assert!(snapshot.zone_problems().is_empty());
        let zones: Vec<(&str, Option<&'static str>)> = snapshot
            .zones()
            .map(|(k, z)| (k.as_str(), z.map(|z| z.name())))
            .collect();
        assert_eq!(
            zones,
            vec![
                ("suncrest:backyard", Some("America/Denver")),
                ("suncrest:roof", Some("Asia/Kathmandu"))
            ],
            "in station-key order, which is what makes a plan deterministic"
        );
        // The projection still carries the text, because `stations.timezone` is a text
        // column and the writer copies it.
        let entry = snapshot
            .by_station
            .get("suncrest:roof")
            .expect("configured");
        assert_eq!(entry.projection.timezone.as_deref(), Some("Asia/Kathmandu"));
        assert_eq!(entry.zone.map(|z| z.name()), Some("Asia/Kathmandu"));
    }
}
