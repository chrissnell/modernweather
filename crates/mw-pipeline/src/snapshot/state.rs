//! [`SnapshotState`] — the published table, and the [`LatestSnapshot`] over it.
//!
//! **The hot path is three pointer reads.** `/latest` resolves a website from an
//! `ArcSwap<HostTable>` (M4-3's), resolves a station from the configuration, loads this
//! table from an [`ArcSwap`], looks one key up and clones an `Arc<[u8]>`. Nothing on that
//! path can wait, and that asymmetry is the whole point of **D-28**: the readers are
//! request handlers and the writer is a component tick, so a `RwLock` would make a request
//! wait on a fold.
//!
//! # Why the key set is fixed at construction
//!
//! W3 §2a.4 sizes the worst case at the **cross product** of websites and devices — 30 keys
//! in W1's capture rig, ~5 KB each, 150 KB — and explicitly declines an eviction policy, a
//! TTL and an LRU, because "the configuration cannot express 10,000 websites or devices".
//! Entries are "created and destroyed by the config watch's plan diff", which in this
//! implementation means: the key set comes from configuration when `main` builds the state,
//! and a configuration change restarts the component (its [`ConfigDigest`] covers the key
//! set).
//!
//! [`ConfigDigest`]: mw_component::ConfigDigest
//!
//! Fixing it buys the thing that is otherwise awkward: **one `watch` per key, allocated
//! once**, so publishing is lock-free and so is subscribing. A map of senders built on
//! demand would need a lock on the publish path — which is the fold, once per reading per
//! website — and would have to decide what to do about a sender for a key nobody has
//! reported for. There is nothing to decide here: every configured key has a sender from
//! the start, and a key that is not configured is [`None`] from both
//! [`LatestSnapshot::get`] and [`LatestSnapshot::subscribe`].
//!
//! # The two writers, and why neither can undo the other
//!
//! | Writer | Writes | When |
//! |---|---|---|
//! | `snapshot:latest` | every configured website's key for the station that reported | `OnReading { All, Latest, 100 ms }` |
//! | `snapshot:warm` | a key whose entry is absent or **older** than the seed | `OnStartup`, once |
//!
//! Both go through [`SnapshotState::publish`] and [`SnapshotState::seed`], and the seed is
//! the one that compares: it writes only where it would not move `observed_at` backwards.
//! Without that, a warm tick that lands after the first reading has arrived would replace a
//! two-second-old snapshot with a ten-minute-old one — which is not a race the ordering
//! rules out, because `OnStartup` has no hard edge to `snapshot:latest` and should not: a
//! seed that waited for the first reading would have nothing left to do.
//!
//! **The comparison happens inside the `rcu`, not before it**, so it is an invariant rather
//! than a very likely outcome. `SnapshotState::store`'s own comment has the interleaving
//! that a compare-then-write would lose to.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mw_component::{LatestSnapshot, Snapshot, SnapshotKey, SnapshotTable, WebsiteView};
use mw_types::StationKey;
use tokio::sync::watch;

/// One key's slot: the `watch` SSE subscribes to.
///
/// The snapshot itself lives in the published [`SnapshotTable`] rather than here, so that
/// a reader takes one `ArcSwap` load for the whole table instead of one per key.
#[derive(Debug)]
struct Slot {
    tx: watch::Sender<Option<Arc<Snapshot>>>,
}

/// Every configured key's current snapshot.
///
/// A root resource `main` owns and hands to both writers through the registrar, the way
/// `Ingress` and [`crate::RollupState`] are: two components write it, and the capability bus
/// holds one handle *type* per provider, so the shared state cannot travel on it
/// (`crate::rollup`'s module documentation has the whole of that argument).
#[derive(Debug)]
pub struct SnapshotState {
    /// Every configured website, in id order — the left half of **D-31**'s cross product.
    websites: Vec<WebsiteView>,
    /// Every station this node has a snapshot key for, in key order.
    stations: Vec<StationKey>,
    /// One slot per key, fixed at construction.
    slots: BTreeMap<SnapshotKey, Slot>,
    /// The published table. The `/latest` path's only read.
    table: ArcSwap<SnapshotTable>,
    /// How many snapshots have been rendered into the table, for a log line and a test.
    published: AtomicU64,
}

impl SnapshotState {
    /// The state for a fixed website set and station set.
    ///
    /// The cross product is materialised as slots, not as entries: a key has a `watch` from
    /// the start and a *snapshot* only once something has reported for it, which is what
    /// makes `/latest`'s 404 the cold-start answer rather than an empty payload.
    pub fn new(
        websites: impl IntoIterator<Item = WebsiteView>,
        stations: impl IntoIterator<Item = StationKey>,
    ) -> Self {
        let mut websites: Vec<WebsiteView> = websites.into_iter().collect();
        websites.sort_by_key(|w| w.id);
        let mut stations: Vec<StationKey> = stations.into_iter().collect();
        stations.sort();
        stations.dedup();
        let mut slots = BTreeMap::new();
        for website in &websites {
            for station in &stations {
                slots.insert(
                    SnapshotKey::new(website.id, station.clone()),
                    Slot {
                        tx: watch::Sender::new(None),
                    },
                );
            }
        }
        Self {
            websites,
            stations,
            slots,
            table: ArcSwap::from_pointee(SnapshotTable::new()),
            published: AtomicU64::new(0),
        }
    }

    /// Every configured website, in id order.
    pub fn websites(&self) -> &[WebsiteView] {
        &self.websites
    }

    /// Every station a key exists for, in key order.
    pub fn stations(&self) -> &[StationKey] {
        &self.stations
    }

    /// Every key, in order — what `snapshot:warm` iterates.
    pub fn keys(&self) -> impl Iterator<Item = &SnapshotKey> {
        self.slots.keys()
    }

    /// One website's view, by id.
    pub fn website(&self, id: mw_types::WebsiteId) -> Option<&WebsiteView> {
        self.websites.iter().find(|w| w.id == id)
    }

    /// How many keys the configuration produces — **D-31**'s cross product.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether there is nothing to snapshot: no websites, or no stations.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How many entries are populated right now.
    pub fn populated(&self) -> usize {
        self.table.load().len()
    }

    /// How many snapshots have been published since the process started.
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    /// Replace one key's snapshot. `snapshot:latest`'s fold.
    ///
    /// `false` for a key the configuration does not contain, which is a caller that built a
    /// key out of a station or website this state does not know. Reported rather than
    /// inserted, because inserting it would grow a map that **D-31**'s sizing argument
    /// assumes is bounded by configuration.
    pub fn publish(&self, snapshot: Arc<Snapshot>) -> bool {
        self.store(snapshot, Replace::Always)
    }

    /// Insert one key's snapshot **only if it would not move the key backwards in time**.
    /// `snapshot:warm`'s seed; see the module documentation.
    ///
    /// `false` for an unknown key and for a seed that lost to a reading that had already
    /// arrived — two different non-events, both of which mean "nothing was written", and
    /// neither of which is a failure. The caller counts them separately by asking
    /// [`SnapshotState::get`] first if it cares, which `snapshot:warm` does for its log
    /// line.
    pub fn seed(&self, snapshot: Arc<Snapshot>) -> bool {
        self.store(snapshot, Replace::IfNewer)
    }

    fn store(&self, snapshot: Arc<Snapshot>, replace: Replace) -> bool {
        let Some(slot) = self.slots.get(snapshot.key()) else {
            return false;
        };
        // **The decision is inside the `rcu`, not before it.** `rcu` loads, calls the
        // closure, and compare-exchanges; on a losing exchange it calls the closure again
        // with the newer value. Deciding outside would be a read-then-write with a gap in
        // the middle, and the gap is where the invariant this type documents gets lost:
        // `snapshot:warm` loads an empty key, decides to seed its ten-minute-old row,
        // `snapshot:latest` folds a two-second-old reading into that key, and the seed's
        // write lands on top. `observed_at` would move **backwards**, which is the one
        // thing the module documentation promises cannot happen.
        //
        // The window is microseconds and the next reading repairs it, so this was never a
        // correctness emergency — but a documented invariant that holds "almost always" is
        // worse than one that holds, because the next reader trusts it.
        //
        // `rcu` rather than `store` for the original reason too: the map is
        // read-copy-updated, so a concurrent fold for a *different* key cannot lose its
        // entry to this one's clone.
        let mut wrote = false;
        self.table.rcu(|current| {
            // Assigned unconditionally on every pass, not OR-ed: the closure may run more
            // than once, and a pass that loses the exchange must not leave `true` behind
            // from an earlier attempt that would now decide differently.
            wrote = replace == Replace::Always
                || current
                    .get(snapshot.key())
                    .is_none_or(|c| c.observed_at() < snapshot.observed_at());
            let mut next = SnapshotTable::clone(current);
            if wrote {
                next.insert(Arc::clone(&snapshot));
            }
            next
        });
        if !wrote {
            return false;
        }
        self.published.fetch_add(1, Ordering::Relaxed);
        // After the table, so a subscriber woken by this never reads a table that does not
        // have it. `send_replace` rather than `send`: a key with no subscriber is the
        // normal case and a closed-channel error would be noise.
        slot.tx.send_replace(Some(snapshot));
        true
    }
}

/// Whether a write may move a key backwards in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replace {
    /// `snapshot:latest`: arrival order wins.
    ///
    /// **Not "the newest reading wins", which is what this said and is not what it does.**
    /// A fold writes whatever arrived most recently, and arrival order is not timestamp
    /// order: an NTP correction that steps a station's clock backwards, or a peer's
    /// forwarded replay interleaving with local admits, produces a reading whose `time` is
    /// earlier than the one already published. `Always` accepts it, deliberately — the
    /// station's own notion of now has moved, and `/latest`'s job is to report what the
    /// station last said rather than to adjudicate its clock. `Snapshot::age_seconds`
    /// reports a negative age for the same reason.
    ///
    /// The consequence worth knowing: `observed_at` and the `ETag` can move backwards on a
    /// clock step, and if the step is larger than ten minutes W1's recency rule will 404 a
    /// station that is live. That is a station whose clock is wrong, and
    /// `mw_snapshot_age_seconds` going negative is the signal for it.
    Always,
    /// `snapshot:warm`: a seed never overwrites something fresher. Enforced inside the
    /// `rcu`, so it is a real invariant rather than a race.
    IfNewer,
}

impl LatestSnapshot for SnapshotState {
    fn get(&self, key: &SnapshotKey) -> Option<Arc<Snapshot>> {
        self.table.load().get(key).map(Arc::clone)
    }

    fn subscribe(&self, key: &SnapshotKey) -> Option<watch::Receiver<Option<Arc<Snapshot>>>> {
        self.slots.get(key).map(|slot| slot.tx.subscribe())
    }

    fn table(&self) -> Arc<SnapshotTable> {
        self.table.load_full()
    }
}
