//! Test support: a scratch spool, a hand-fed registry, and envelope builders.
//!
//! Public rather than `#[cfg(test)]`, because this crate's integration tests are separate
//! compilation units — the same reason `mw_supervisor::testing` is public. Every type here
//! is a test double with no production use: the real `StationRegistry` is
//! `config:sqlite`'s over an `ArcSwap` of the configuration snapshot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use mw_component::{ComponentId, FrameMeta, IdentitySource, Reject, StationRegistry};
use mw_types::{Envelope, OriginId, PartialReading, Provenance, Reading, Seq, StationKey};

use crate::clock::{Clock, TestClock};
use crate::spool::{Spool, SpoolConfig};

/// A scratch directory per test, removed on drop.
///
/// The same shape `mw-store`'s and `modernweather`'s tests use, and for the same reason:
/// `tempfile` would be a dev-dependency for twenty lines, and `ci/layers.toml` counts
/// dev-dependencies.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    /// A fresh directory, named after `tag`.
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-spool-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        Scratch(dir)
    }

    /// Where the spool goes.
    pub fn spool(&self) -> PathBuf {
        self.0.join("spool")
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A spool config with the group-commit window off and small segments.
///
/// Both are the test seams [`SpoolConfig`] documents. The window is zeroed because 50 ms
/// per append makes a thousand-append test take a minute; the segment is small because
/// rotation and reclamation are otherwise 8 MiB of writing away. Everything else is the
/// shipped default, including the ceiling and `max_lag`.
pub fn config(path: impl Into<PathBuf>) -> SpoolConfig {
    SpoolConfig {
        segment_bytes: 4 * 1024,
        group_commit_window: std::time::Duration::ZERO,
        ..SpoolConfig::new(path)
    }
}

/// A node id made of one repeated byte, for a test that needs several distinguishable
/// nodes.
pub fn node(n: u8) -> OriginId {
    OriginId::new([n; 16])
}

/// A component id from a literal.
pub fn id(text: &str) -> ComponentId {
    ComponentId::new(text).expect("a literal component id")
}

/// Open a spool at `path` for node `node_id`, on `clock`.
pub fn spool(path: impl Into<PathBuf>, node_id: OriginId, clock: &TestClock) -> Arc<Spool> {
    Spool::open(
        config(path),
        node_id,
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool opens")
}

/// A reading with one populated metric, so it is not refused as empty.
pub fn reading(barometer: f32) -> Reading {
    Reading {
        barometer: Some(barometer),
        ..Reading::default()
    }
}

/// One decoded observation, timestamped.
pub fn partial(at: DateTime<Utc>, barometer: f32) -> PartialReading {
    PartialReading::at(at, reading(barometer))
}

/// A sealed envelope, as a peer would send it: admitted at `origin`, then forwarded along
/// `hops`.
pub fn forwarded(
    origin: OriginId,
    seq: u64,
    station: &str,
    at: DateTime<Utc>,
    hops: &[OriginId],
) -> Envelope {
    let mut envelope = Envelope {
        station: StationKey::new(station),
        observed_at: at,
        received_at: at,
        provenance: Provenance::admitted(origin, Seq::new(seq)),
        reading: Arc::new(Reading {
            time: at,
            station_id: station.to_string(),
            ..reading(29.92)
        }),
    };
    for hop in hops {
        envelope = envelope
            .forward(*hop)
            .expect("the test's own path has no repeat");
    }
    envelope
}

/// The frame metadata of a `Fixed`-identity transport.
pub fn frame(receiver: &str, at: DateTime<Utc>) -> FrameMeta {
    FrameMeta::fixed(id(receiver), at)
}

/// A [`StationRegistry`] fed by hand.
///
/// Resolution is by the frame's `presented_key`, or by a single configured station for a
/// `Fixed` transport — the two cases `admit` actually takes. The real one compares a
/// secret in constant time against `config.stations`; what a spool test needs is only that
/// *something* decides, and that a refusal is a [`Reject`] rather than an absence.
#[derive(Debug, Default)]
pub struct MemoryRegistry {
    fixed: Mutex<Option<StationKey>>,
    presented: Mutex<BTreeMap<String, StationKey>>,
    refuse: Mutex<Option<Reject>>,
}

impl MemoryRegistry {
    /// A registry that resolves every `Fixed` frame to `station`.
    pub fn fixed(station: &str) -> Arc<Self> {
        Arc::new(Self {
            fixed: Mutex::new(Some(StationKey::new(station))),
            ..Self::default()
        })
    }

    /// A registry that refuses everything with `reason`.
    pub fn refusing(reason: Reject) -> Arc<Self> {
        Arc::new(Self {
            refuse: Mutex::new(Some(reason)),
            ..Self::default()
        })
    }

    /// Map a presented key to a station.
    pub fn allow(&self, presented: &str, station: &str) {
        self.presented
            .lock()
            .expect("the test registry is not poisoned")
            .insert(presented.to_string(), StationKey::new(station));
    }
}

impl StationRegistry for MemoryRegistry {
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject> {
        if let Some(reason) = *self.refuse.lock().expect("not poisoned") {
            return Err(reason);
        }
        match (&meta.source, &meta.presented_key) {
            (IdentitySource::Fixed, _) => self
                .fixed
                .lock()
                .expect("not poisoned")
                .clone()
                .ok_or(Reject::UnknownStation),
            (_, Some(key)) => self
                .presented
                .lock()
                .expect("not poisoned")
                .get(key)
                .cloned()
                .ok_or(Reject::UnknownStation),
            (_, None) => Err(Reject::UnknownStation),
        }
    }
}
