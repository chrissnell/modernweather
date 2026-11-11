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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordVerifier};
use mw_component::{
    ComponentId, FrameMeta, IdentitySource, Reject, StationMeta, StationProjection,
    StationRegistry, Zone, ZoneError,
};
use mw_types::StationKey;
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;

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
    /// The stored `ingest_secret_hash`, for the one identity strategy that presents a
    /// secret.
    ///
    /// An Argon2id PHC string, as `admin_credential.password_phc` is. `None` is a station
    /// whose protocol has no secret — every transport but `from_query` — and a `from_query`
    /// station with `None` here is a row [`crate::registry::RegistrySnapshot::secret_problems`]
    /// reports and `mw_station::plan` refuses, because its uploads cannot be authenticated.
    pub secret_phc: Option<String>,
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
                secret_phc: row
                    .ingest_secret_hash
                    .clone()
                    .filter(|hash| !hash.trim().is_empty()),
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

    /// Stored ingest secrets that will never verify anything, for the startup warning.
    ///
    /// Both kinds are **reachable, not unreachable**, and neither is visible from the
    /// counters alone: `mw_readings_rejected_total{bad_secret}` only moves when traffic
    /// arrives, and a station that never reports looks exactly like a station that is
    /// switched off. So they are reported once, at startup, by
    /// `crates/modernweather/src/boot.rs`.
    pub fn secret_problems(&self) -> Vec<SecretProblem> {
        let mut out = Vec::new();
        for entry in self.by_station.values() {
            let Some(phc) = entry.secret_phc.as_deref() else {
                continue;
            };
            if entry.identity != IdentitySource::FromQuery {
                out.push(SecretProblem::NotConsulted {
                    station: entry.station.clone(),
                    identity: entry.identity,
                });
            } else if PasswordHash::new(phc).is_err() {
                out.push(SecretProblem::Unparseable {
                    station: entry.station.clone(),
                });
            }
        }
        out
    }
}

/// A stored ingest secret that cannot authenticate anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretProblem {
    /// A secret on a station whose identity strategy presents none.
    ///
    /// Nothing ties `ingest_secret_hash` to `identity_kind` in the schema, so a hash pasted
    /// onto a WeatherLink Live row is a row the schema accepts, `plan()` does not look at,
    /// and no frame ever consults — a configured credential that is silently ignored.
    #[error(
        "station `{station}` has an ingest secret and its identity is `{identity}`, which \
         presents none: only an `from_query` station's `?PASSWORD=` is verified against it, \
         so this secret authenticates nothing. Clear `ingest_secret_hash`, or correct \
         `identity_kind`."
    )]
    NotConsulted {
        /// Which station.
        station: StationKey,
        /// Its configured strategy.
        identity: IdentitySource,
    },

    /// A secret that is not a hash this build can verify against.
    ///
    /// Every upload from that station is then `bad_secret` for the life of the process, with
    /// nothing naming the station — which is a station that reports nothing and an operator
    /// who has no way to find out why.
    #[error(
        "station `{station}`'s `ingest_secret_hash` is not an Argon2 PHC string, so every \
         upload from it will be refused. Set the secret again."
    )]
    Unparseable {
        /// Which station.
        station: StationKey,
    },
}

/// How often one station's secret may be verified against its stored hash while no
/// verification has succeeded.
///
/// The rate limit [`ConfigRegistry::verify`] exists for: see its documentation. One second
/// is two orders of magnitude above an Ambient console's own upload cadence and two orders
/// below what an Argon2 verification per request would cost.
const VERIFY_INTERVAL: Duration = Duration::from_secs(1);

/// What is known about one station's secret, after the first upload that presented one.
#[derive(Debug, Default)]
struct SecretState {
    /// The secret that last verified against the stored hash.
    ///
    /// **Plaintext, in memory, for the life of the snapshot**, and that is the trade this
    /// whole mechanism is: it is the same material the console puts in a query string on
    /// every upload, it is a `SecretString` so it redacts itself in every `Debug` and log
    /// field, and it never reaches a disk. What it buys is one Argon2 verification per
    /// station per snapshot instead of one per reading, which is what makes a memory-hard
    /// hash compatible with **D-87** at all.
    verified: Option<SecretString>,
    /// When a verification was last attempted while [`SecretState::verified`] was `None`.
    last_attempt: Option<Instant>,
}

/// The `StationRegistry` the daemon runs on.
#[derive(Debug)]
pub struct ConfigRegistry {
    snapshot: ArcSwap<RegistrySnapshot>,
    /// One [`SecretState`] per station that has presented a secret, cleared whenever a
    /// snapshot is published -- because a published snapshot may carry a rotated hash, and
    /// a cache that outlived it would keep accepting the old secret. That is the one
    /// property the hand-written `Hash` on a station row exists for one layer up
    /// (`mw_station::StationRow`), stated here for the other half of the same rotation.
    secrets: Mutex<BTreeMap<StationKey, SecretState>>,
}

impl ConfigRegistry {
    /// A registry over one snapshot.
    pub fn new(snapshot: RegistrySnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(snapshot),
            secrets: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replace the snapshot. One store, so no reader can observe a half-updated registry.
    ///
    /// The seam a configuration change watch will drive. Nothing calls it yet; see this
    /// module's documentation for what that costs and who owns closing it.
    pub fn publish(&self, snapshot: RegistrySnapshot) {
        self.snapshot.store(Arc::new(snapshot));
        // Before or after the store, and it has to be after: a reader that verified
        // against the old snapshot between the clear and the store would repopulate the
        // cache with the old secret. Clearing second means the worst case is one extra
        // Argon2 verification, which is the direction to be wrong in.
        self.secrets
            .lock()
            .expect("the secret cache is not poisoned")
            .clear();
    }

    /// The snapshot, for a test and for the startup log line.
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.snapshot.load_full()
    }

    /// Verify a presented secret against one station's stored hash.
    ///
    /// # The D-87 reconciliation, which is this item's to make
    ///
    /// **D-33** requires a credential comparison to be constant-time and **W2a §4a.6**
    /// requires Ambient's `?PASSWORD=` to be compared against the registry's copy; the
    /// stored copy is an Argon2id PHC string, as `admin_credential.password_phc` is, so
    /// that a stolen configuration database does not hand over a reusable secret. **D-87**
    /// requires `admit` to do no I/O and nothing slow, and `resolve` is synchronous
    /// *because of that requirement* — so an Argon2 verification per reading is exactly
    /// what cannot go here. This function is how the two are both satisfied:
    ///
    /// - **The first upload that presents a secret pays for an Argon2 verification.** Once
    ///   per station, per published snapshot.
    /// - **Every later upload is a `subtle` constant-time comparison** against the secret
    ///   that verified. An Ambient console sends the same string every minute for years, so
    ///   the cache hits essentially always.
    /// - **A failure is never cached**, and while a station has no verified secret a
    ///   verification is attempted at most once per [`VERIFY_INTERVAL`]. Without that, a
    ///   flood of wrong passwords would be a flood of memory-hard hashes — one upload's
    ///   worth of CPU per packet, which is a denial of service against a node's own
    ///   admission path. Refusals inside the interval are `BadSecret` and are counted like
    ///   any other (`mw_readings_rejected_total{reason}`), so the condition is visible.
    ///
    /// What this costs is written on [`SecretState::verified`]: a verified secret is held in
    /// memory in the clear for the life of the snapshot. It is the same material the console
    /// transmits in a URL on every upload.
    fn verify(&self, entry: &StationEntry, presented: Option<&SecretString>) -> Result<(), Reject> {
        let Some(phc) = entry.secret_phc.as_deref() else {
            // No stored secret: there is nothing to compare against, so nothing can be
            // authenticated. `mw_station::plan` refuses a `from_query` station in this
            // state and `RegistrySnapshot::secret_problems` reports one that reached here
            // anyway; failing closed is what makes those two a belt and a brace rather
            // than the only thing standing between an unauthenticated upload and the spool.
            return Err(Reject::BadSecret);
        };
        let Some(presented) = presented else {
            // The protocol requires the parameter and the driver refuses an upload without
            // one, so this is a frame from somewhere else entirely.
            return Err(Reject::BadSecret);
        };

        let mut cache = self
            .secrets
            .lock()
            .expect("the secret cache is not poisoned");
        let state = cache.entry(entry.station.clone()).or_default();
        if let Some(verified) = &state.verified {
            // The hot path: constant-time, and `subtle` rather than `==` because a
            // short-circuiting comparison on a credential is a timing oracle (**D-33**).
            if verified
                .expose_secret()
                .as_bytes()
                .ct_eq(presented.expose_secret().as_bytes())
                .into()
            {
                return Ok(());
            }
            // A second secret for the same station cannot be right while this snapshot is
            // live -- `publish` clears the cache -- so this is a wrong password and is
            // refused without a verification. Rate-limiting it is unnecessary for the same
            // reason: the expensive path is not reached.
            return Err(Reject::BadSecret);
        }

        let now = Instant::now();
        if let Some(last) = state.last_attempt {
            if now.duration_since(last) < VERIFY_INTERVAL {
                return Err(Reject::BadSecret);
            }
        }
        state.last_attempt = Some(now);
        let Ok(parsed) = PasswordHash::new(phc) else {
            // Not a hash this build can verify against. Reported at startup by
            // `RegistrySnapshot::secret_problems`, because the counter alone does not name
            // the station.
            return Err(Reject::BadSecret);
        };
        if Argon2::default()
            .verify_password(presented.expose_secret().as_bytes(), &parsed)
            .is_err()
        {
            return Err(Reject::BadSecret);
        }
        state.verified = Some(presented.clone());
        Ok(())
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
                let entry = snapshot
                    .by_claim
                    .get(&claim)
                    .ok_or(Reject::UnknownStation)?;
                let station = Self::admit(entry, source)?;
                // The secret check is **after** the strategy and the enabled flag and
                // **before** the key is handed back, which is the only ordering that both
                // fails closed and does not spend an Argon2 verification on a frame that
                // was going to be refused anyway.
                if source == IdentitySource::FromQuery {
                    self.verify(entry, meta.presented_secret.as_ref())?;
                }
                Ok(station)
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
            wind_dir_correction_degrees: 0,
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
    fn a_station_whose_stored_secret_is_not_a_hash_refuses_every_upload() {
        let registry = ConfigRegistry::new(RegistrySnapshot::new(
            &[ambient_row("$argon2id$v=19$...")],
            [],
        ));
        assert_eq!(
            registry.resolve(&presenting_secret("hunter2")),
            Err(Reject::BadSecret)
        );
        // ... and the startup check names it, because the counter alone does not.
        let problems = registry.snapshot().secret_problems();
        assert!(
            matches!(&problems[..], [SecretProblem::Unparseable { station }]
                     if station.as_str() == "ambient:backyard"),
            "{problems:?}"
        );
    }

    /// An Ambient station row: `from_query`, because the strategy check runs first and a
    /// `from_payload` row presented on a query transport is `UnknownStation` before the
    /// secret is ever reached.
    fn ambient_row(phc: &str) -> StationRow {
        let mut row = row("ambient:backyard", Some("KAMBIENT1"));
        row.identity_kind = "from_query".into();
        row.kind = "ambient-customized".into();
        row.ingest_secret_hash = Some(phc.to_string());
        row
    }

    fn presenting_secret(secret: &str) -> FrameMeta {
        FrameMeta::presenting(
            id("station:ambient"),
            IdentitySource::FromQuery,
            "KAMBIENT1",
            at(),
        )
        .with_secret(SecretString::from(secret.to_string()))
    }

    /// An Argon2id PHC string for `secret`, at the cheapest parameters the crate permits --
    /// which is what keeps this test file from spending a second per hash. The PHC carries
    /// its own parameters, so what `verify` does with it is unaffected.
    fn phc(secret: &str) -> String {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let params = argon2::Params::new(
            argon2::Params::MIN_M_COST,
            argon2::Params::MIN_T_COST,
            argon2::Params::MIN_P_COST,
            None,
        )
        .expect("the crate's own minimum parameters");
        let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        argon
            .hash_password(
                secret.as_bytes(),
                &SaltString::encode_b64(b"0123456789abcdef").expect("a literal salt"),
            )
            .expect("hashing a literal")
            .to_string()
    }

    /// The whole of the D-87 reconciliation, as behaviour: the right secret verifies, the
    /// wrong one does not, and a station with no stored secret is refused rather than
    /// admitted.
    #[test]
    fn the_right_secret_verifies_and_the_wrong_one_does_not() {
        let registry =
            ConfigRegistry::new(RegistrySnapshot::new(&[ambient_row(&phc("hunter2"))], []));
        assert_eq!(
            registry.resolve(&presenting_secret("hunter2")),
            Ok(StationKey::new("ambient:backyard"))
        );
        // Again, which is the cached path: constant-time, no hash.
        assert_eq!(
            registry.resolve(&presenting_secret("hunter2")),
            Ok(StationKey::new("ambient:backyard"))
        );
        assert_eq!(
            registry.resolve(&presenting_secret("hunter3")),
            Err(Reject::BadSecret)
        );
        // No secret presented at all: the driver refuses such an upload, so this is a frame
        // from somewhere else.
        let mut meta = presenting_secret("hunter2");
        meta.presented_secret = None;
        assert_eq!(registry.resolve(&meta), Err(Reject::BadSecret));
    }

    /// **The rate limit**, which is what stops a password flood becoming a flood of
    /// memory-hard hashes. Nothing has verified yet, so the first attempt is the only one
    /// inside the interval that reaches Argon2 -- and a later correct secret is refused
    /// until the interval passes, which is the cost and is bounded.
    #[test]
    fn a_failed_verification_is_not_cached_and_is_rate_limited() {
        let registry =
            ConfigRegistry::new(RegistrySnapshot::new(&[ambient_row(&phc("hunter2"))], []));
        assert_eq!(
            registry.resolve(&presenting_secret("wrong")),
            Err(Reject::BadSecret)
        );
        // Inside the interval, so this is refused WITHOUT a verification even though it is
        // the right secret. Recorded as the behaviour rather than discovered: an operator
        // who has just fixed a console's password waits up to one second.
        assert_eq!(
            registry.resolve(&presenting_secret("hunter2")),
            Err(Reject::BadSecret)
        );
        // Reaching past the interval is what makes the next attempt verify. The interval is
        // one second and the test does not sleep for it: the state is reached directly,
        // which asserts the mechanism rather than the wall clock.
        registry
            .secrets
            .lock()
            .expect("not poisoned")
            .get_mut(&StationKey::new("ambient:backyard"))
            .expect("one attempt has been made")
            .last_attempt = Some(Instant::now() - VERIFY_INTERVAL * 2);
        assert_eq!(
            registry.resolve(&presenting_secret("hunter2")),
            Ok(StationKey::new("ambient:backyard"))
        );
    }

    /// A rotated secret must not keep verifying against the cached one.
    #[test]
    fn publishing_a_snapshot_clears_the_verified_secret() {
        let registry = ConfigRegistry::new(RegistrySnapshot::new(&[ambient_row(&phc("old"))], []));
        assert!(registry.resolve(&presenting_secret("old")).is_ok());
        registry.publish(RegistrySnapshot::new(&[ambient_row(&phc("new"))], []));
        assert_eq!(
            registry.resolve(&presenting_secret("old")),
            Err(Reject::BadSecret),
            "the rotated hash is what the next upload is verified against"
        );
        // And the new one verifies -- after the interval, because the attempt above spent
        // this station's budget.
        registry
            .secrets
            .lock()
            .expect("not poisoned")
            .get_mut(&StationKey::new("ambient:backyard"))
            .expect("one attempt has been made")
            .last_attempt = Some(Instant::now() - VERIFY_INTERVAL * 2);
        assert!(registry.resolve(&presenting_secret("new")).is_ok());
    }

    /// A secret on a station whose strategy presents none authenticates nothing, and the
    /// startup check is the only thing that would ever say so.
    #[test]
    fn a_secret_on_a_station_that_presents_none_is_reported_at_startup() {
        let mut row = row("suncrest:roof", Some("001D0A700002"));
        row.ingest_secret_hash = Some(phc("hunter2"));
        let snapshot = RegistrySnapshot::new(&[row], []);
        let problems = snapshot.secret_problems();
        assert!(
            matches!(&problems[..], [SecretProblem::NotConsulted { station, identity }]
                     if station.as_str() == "suncrest:roof"
                        && *identity == IdentitySource::FromPayload),
            "{problems:?}"
        );
        assert!(problems[0].to_string().contains("authenticates nothing"));
        // ... and it does not affect that station's own resolution, which has no secret.
        let registry = ConfigRegistry::new(snapshot);
        let meta = FrameMeta::presenting(
            id("station:wll:udp:22222"),
            IdentitySource::FromPayload,
            "001D0A700002",
            at(),
        );
        assert_eq!(
            registry.resolve(&meta),
            Ok(StationKey::new("suncrest:roof")),
            "a secret nothing consults does not refuse the frames that do arrive"
        );
    }

    #[test]
    fn a_healthy_configuration_has_no_secret_problems() {
        let snapshot = RegistrySnapshot::new(&[ambient_row(&phc("hunter2"))], []);
        assert!(snapshot.secret_problems().is_empty());
        assert!(registry().snapshot().secret_problems().is_empty());
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
