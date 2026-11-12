//! Doubles and fixtures for this crate's own tests and for its integration tests.
//!
//! Unconditionally compiled, as `mw_pipeline::testing` and `mw_station::testing` are and
//! for their reason: an integration test in `tests/` links the crate as a dependency, so a
//! `#[cfg(test)]` module is invisible to it and the alternative is a copy of every double
//! per test file.
//!
//! **The peer double is not a `BTreeMap` wrapper.** It holds exactly one peer, because
//! every question in `crate::admit` is "is *this* connection *that* peer" and a double with
//! a collection in it makes a test that passes because the lookup found *some* peer
//! indistinguishable from one that passes because it found the right one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, Enrolled, Enrolment, FrameMeta, PeerAuth, PeerEntry, PeerName, PeerRegistry,
    PeerStation, PeerWriteError, Reject, StationMeta, StationProjection, StationRegistry,
};
use mw_types::{Envelope, OriginId, Provenance, Reading, Seq, StationKey};
use rustls::pki_types::CertificateDer;

/// The path of a file in `tests/data`.
pub fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

/// One checked-in test certificate, as DER. `tests/data/README.md` has how they were made.
pub fn cert(name: &str) -> CertificateDer<'static> {
    let path = data(name);
    let pem =
        std::fs::read(&path).unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
    // Collected and owned, because `rustls_pemfile`'s iterator borrows the buffer and the
    // buffer is local to this function.
    let mut reader = pem.as_slice();
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .map(|cert| cert.expect("a parseable certificate").into_owned())
        .collect();
    certs
        .into_iter()
        .next()
        .expect("the fixture holds one PEM certificate")
}

/// The pin an operator would configure for one of those certificates, from the checked-in
/// `openssl` digest beside it.
pub fn pin(name: &str) -> [u8; 32] {
    let text = std::fs::read_to_string(data(name)).expect("the checked-in digest is readable");
    let text = text.trim();
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &text[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).expect("a hex digest");
    }
    out
}

/// A registry holding exactly one peer.
#[derive(Debug)]
pub struct FakePeers {
    peer: Mutex<Option<PeerEntry>>,
    /// What `enrolment_open` answers.
    open: bool,
    /// What `record_hello` recorded, for a test to assert against.
    hellos: Mutex<Vec<(OriginId, String, Vec<PeerStation>)>>,
}

impl FakePeers {
    fn one(peer: PeerEntry) -> Arc<Self> {
        Arc::new(Self {
            peer: Mutex::new(Some(peer)),
            open: false,
            hellos: Mutex::new(Vec::new()),
        })
    }

    /// A registry that trusts nobody — a collector awaiting enrolment.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            peer: Mutex::new(None),
            open: false,
            hellos: Mutex::new(Vec::new()),
        })
    }

    /// One `mtls-ca` peer.
    pub fn ca(name: &str, node: OriginId) -> Arc<Self> {
        Self::one(entry(name, node, PeerAuth::MtlsCa))
    }

    /// One `mtls-pinned` peer.
    pub fn pinned(name: &str, node: OriginId, spki_sha256: [u8; 32]) -> Arc<Self> {
        Self::one(entry(name, node, PeerAuth::MtlsPinned { spki_sha256 }))
    }

    /// One `psk` peer, whose secret is hashed here so the comparison under test is the real
    /// one.
    pub fn psk(name: &str, node: OriginId, secret: &str) -> Arc<Self> {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let salt = SaltString::encode_b64(b"mw-collector-test").expect("a legal salt");
        let phc = argon2::Argon2::default()
            .hash_password(secret.as_bytes(), &salt)
            .expect("hashable")
            .to_string();
        Self::one(entry(name, node, PeerAuth::Psk { phc: phc.into() }))
    }

    /// The same registry with its peer revoked.
    pub fn revoked(self: Arc<Self>) -> Arc<Self> {
        if let Some(peer) = self.peer.lock().expect("not poisoned").as_mut() {
            peer.revoked_at = DateTime::from_timestamp_millis(1);
        }
        self
    }

    /// The same registry with its peer granted relay over `origins`.
    pub fn relaying(self: Arc<Self>, origins: &[OriginId]) -> Arc<Self> {
        if let Some(peer) = self.peer.lock().expect("not poisoned").as_mut() {
            peer.relay = true;
            peer.origins = origins.iter().copied().collect();
        }
        self
    }

    /// The same registry with enrolment open.
    pub fn with_enrolment_open(self: Arc<Self>) -> Arc<Self> {
        // `open` is not behind the mutex because nothing mutates it after construction;
        // this rebuilds instead, which keeps the field immutable.
        Arc::new(Self {
            peer: Mutex::new(self.peer.lock().expect("not poisoned").clone()),
            open: true,
            hellos: Mutex::new(Vec::new()),
        })
    }

    /// What `record_hello` was called with.
    pub fn hellos(&self) -> Vec<(OriginId, String, Vec<PeerStation>)> {
        self.hellos.lock().expect("not poisoned").clone()
    }
}

fn entry(name: &str, node: OriginId, auth: PeerAuth) -> PeerEntry {
    PeerEntry {
        node,
        name: PeerName::new(name),
        auth,
        relay: false,
        origins: [node].into_iter().collect(),
        revoked_at: None,
        software: None,
    }
}

impl PeerRegistry for FakePeers {
    fn peer(&self, node: OriginId) -> Option<PeerEntry> {
        self.peer
            .lock()
            .expect("not poisoned")
            .clone()
            .filter(|p| p.node == node)
    }

    fn peer_by_spki(&self, spki_sha256: &[u8; 32]) -> Option<PeerEntry> {
        self.peer.lock().expect("not poisoned").clone().filter(
            |p| matches!(&p.auth, PeerAuth::MtlsPinned { spki_sha256: s } if s == spki_sha256),
        )
    }

    fn peers(&self) -> Vec<PeerEntry> {
        self.peer
            .lock()
            .expect("not poisoned")
            .clone()
            .into_iter()
            .collect()
    }

    fn record_hello<'a>(
        &'a self,
        node: OriginId,
        software: &'a str,
        stations: &'a [PeerStation],
    ) -> BoxFuture<'a, Result<(), PeerWriteError>> {
        Box::pin(async move {
            self.hellos.lock().expect("not poisoned").push((
                node,
                software.to_string(),
                stations.to_vec(),
            ));
            Ok(())
        })
    }

    fn enrolment_open(&self) -> BoxFuture<'_, Result<bool, PeerWriteError>> {
        Box::pin(async move { Ok(self.open) })
    }

    fn enrol<'a>(
        &'a self,
        request: &'a Enrolment,
    ) -> BoxFuture<'a, Result<Enrolled, PeerWriteError>> {
        Box::pin(async move {
            if !self.open {
                return Err(PeerWriteError::EnrolmentClosed);
            }
            let peer = entry(
                &request.name,
                request.node,
                PeerAuth::MtlsPinned {
                    spki_sha256: request.spki_sha256,
                },
            );
            *self.peer.lock().expect("not poisoned") = Some(peer.clone());
            Ok(Enrolled {
                peer,
                pinned_spki_sha256: Some(request.spki_sha256),
            })
        })
    }
}

/// A station registry over `(station_key, owning node byte)` pairs.
///
/// `resolve` answers a presented key that is one of those stations and refuses everything
/// else. Nothing in `mw-collector` calls it — `admit` is the station drivers' path and this
/// crate is `accept`'s side — but the **satellite** half of a two-node test does, because
/// a satellite seals its own readings before it forwards them.
pub fn station_registry(stations: &[(&str, u8)]) -> Arc<dyn StationRegistry> {
    owned_station_registry(
        &stations
            .iter()
            .map(|(key, owner)| (*key, OriginId::new([*owner; 16])))
            .collect::<Vec<_>>(),
    )
}

/// [`station_registry`] over real `NodeId`s rather than the uniform-byte ones.
///
/// Separate because the owner is **load-bearing**: W7 §4.4's step 5 refuses an envelope
/// whose station is owned by a node that is not on its inbound path, so a test over a real
/// certificate's node id cannot use a `[n; 16]` shorthand — it would be refused for the
/// right reason and the test would read as a bug in the gate.
pub fn owned_station_registry(stations: &[(&str, OriginId)]) -> Arc<dyn StationRegistry> {
    Arc::new(Stations {
        owners: stations
            .iter()
            .map(|(key, owner)| (StationKey::new(*key), *owner))
            .collect(),
    })
}

#[derive(Debug)]
struct Stations {
    owners: Vec<(StationKey, OriginId)>,
}

impl StationRegistry for Stations {
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject> {
        let claim = meta
            .presented_key
            .as_deref()
            .ok_or(Reject::UnknownStation)?;
        self.owners
            .iter()
            .find(|(key, _)| key.as_str() == claim)
            .map(|(key, _)| key.clone())
            .ok_or(Reject::UnknownStation)
    }

    fn metadata(&self, _station: &StationKey) -> StationMeta {
        StationMeta::EMPTY
    }

    fn projection(&self, station: &StationKey) -> Option<StationProjection> {
        self.owners
            .iter()
            .find(|(key, _)| key == station)
            .map(|(key, _)| StationProjection {
                name: key.as_str().to_string(),
                kind: "davis".to_string(),
                timezone: Some("America/Denver".to_string()),
                latitude: Some(39.64),
                longitude: Some(-105.2),
                altitude_m: Some(1800.0),
                snow_base_distance_mm: None,
            })
    }

    fn station_owner(&self, station: &StationKey) -> Option<OriginId> {
        self.owners
            .iter()
            .find(|(key, _)| key == station)
            .map(|(_, owner)| *owner)
    }
}

/// One sealed envelope, as it would arrive from a peer.
///
/// `path` is given as node bytes so a test reads as the table in §2.1 does:
/// `&[1, 9]` is "admitted at node 1, addressed to node 9".
pub fn envelope(
    station: &str,
    origin: OriginId,
    seq: u64,
    path: &[u8],
    observed_at: DateTime<Utc>,
) -> Envelope {
    let hops: Vec<OriginId> = path
        .iter()
        .copied()
        .map(|b| OriginId::new([b; 16]))
        .collect();
    Envelope {
        station: StationKey::new(station),
        observed_at,
        received_at: observed_at,
        provenance: Provenance::sealed(origin, Seq::new(seq), &hops)
            .expect("the test's path is one an honest encoder produces"),
        reading: Arc::new(Reading {
            time: observed_at,
            station_id: station.to_string(),
            // One populated metric, because an all-`None` reading is refused by §4.6 and
            // every test here is about a different check.
            barometer: Some(29.92),
            ..Reading::default()
        }),
    }
}

/// An `Ingress` that accepts nothing, for a test that only reaches admission.
///
/// Every method answers as though the spool refused, so a test that accidentally got past
/// the gate fails loudly instead of appearing to store something.
#[derive(Debug)]
pub struct NullIngress;

impl mw_component::Ingress for NullIngress {
    fn admit<'a>(
        &'a self,
        _batch: &'a [mw_types::PartialReading],
        _meta: &'a FrameMeta,
    ) -> BoxFuture<'a, Result<mw_component::Admitted, mw_component::AdmitError>> {
        Box::pin(async move {
            Err(mw_component::AdmitError::spool(std::io::Error::other(
                "this test's ingress accepts nothing",
            )))
        })
    }

    fn accept<'a>(
        &'a self,
        _batch: &'a [Envelope],
        _from: OriginId,
    ) -> BoxFuture<'a, Result<mw_component::Accepted, mw_component::AcceptError>> {
        Box::pin(async move {
            Err(mw_component::AcceptError::spool(std::io::Error::other(
                "this test's ingress accepts nothing",
            )))
        })
    }
}

/// A spool in a scratch directory, for a test that needs the handle and not the log.
///
/// Named for what it is: the two numbers the collector reads off a spool —
/// `high_water` for `known_through` and `tail_seq` for `sender_depth` — both answer from an
/// empty log, so a test about admission or about planning does not need records in it.
pub fn null_spool() -> Arc<mw_pipeline::Spool> {
    spool_at(&scratch("null"))
}

/// A fresh scratch directory, unique per call.
///
/// `mw_pipeline::testing::Scratch`'s arrangement, and that crate may not be named from a
/// `tests/` target here — so the shape is repeated rather than imported. **Unique per
/// call** and not merely per process: `cargo test` runs the tests in one binary in
/// parallel, so a directory keyed on the pid alone is one two tests race to delete, which
/// is exactly the failure this counter exists to stop.
pub fn scratch(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mw-collector-{}-{name}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("a writable scratch directory");
    dir.join("spool")
}

/// Open a spool at `path`, owned by node 1.
pub fn spool_at(path: &std::path::Path) -> Arc<mw_pipeline::Spool> {
    spool_for(path, OriginId::new([1; 16]))
}

/// Open a spool at `path`, owned by `node`.
pub fn spool_for(path: &std::path::Path, node: OriginId) -> Arc<mw_pipeline::Spool> {
    mw_pipeline::Spool::open(
        mw_pipeline::SpoolConfig::new(path),
        node,
        Arc::new(mw_pipeline::SystemClock) as Arc<dyn mw_pipeline::Clock>,
    )
    .expect("a spool opens in a writable directory")
}
