//! The spool: W7 §3's format with W3 §2's mechanics, which **D-30a** makes one thing.
//!
//! The spool is the fan-out point. That is the structural commitment W3 §2.1 makes and it
//! falls straight out of three facts that cannot all hold otherwise: a source may never be
//! blocked by a sink (W2a Decision 61), a bounded in-memory queue means loss when it fills,
//! and an unbounded one means an OOM when the database stalls. So durability lives on disk
//! at the point where fan-out happens, and `Latest` and `Lossy` subscribers are notified
//! *from the append path* rather than from a second channel the source also has to write.
//!
//! ```text
//!  admit / accept                                         cursors, watch, broadcast
//!        │                                                            │
//!        ▼                                                            │
//!   ┌──────────────────────────────────────────┐                      │
//!   │ one synchronous write_all of the whole   │   8 MiB segments      │
//!   │ batch, then a group-committed fsync      │──▶ 00…004608.seg ─────┤
//!   └──────────────────────────────────────────┘    00…011392.seg      │
//!                                                   00…018004.seg ◀ open
//!   MANIFEST   node id, origin table, per-origin high water
//!   cursors/   one two-slot O_SYNC file per registered cursor
//! ```
//!
//! # Four things that are load-bearing rather than incidental
//!
//! **The append call is the atom** (W7 **D-14**). One `admit` of a decoded batch, one
//! `accept` of one `IngestBatch`: the whole batch's records are encoded into one buffer and
//! written with one `write_all`, which is not an await point, so there is no state in which
//! half a batch is in the file. That is what lets W7 §4.2's ack be per-batch with no
//! within-batch partial to express, and it is what `Ingress::admit`'s cancellation contract
//! already promises.
//!
//! **The group-commit cap of 64 counts append *calls*, not records** (W7 **D-13**). One
//! 256-envelope batch is one append and one `fsync`. The cap bounds how many *concurrent
//! callers* coalesce, which at ~4 stations is never reached in steady state and is exactly
//! what makes a reconnecting satellite's replay burst cost one disk sync instead of 256.
//!
//! **A bad record truncates the segment at that point** (W7 **D-12**), counted and logged.
//! A bad *manifest* refuses the start (§3.6 step 1), because a lost origin table reads
//! every `hw_seq` as zero and makes every envelope a peer ever sent look new.
//!
//! **At the ceiling the policy is to refuse at the edge** (W3 §2.7, W7 **D-17**), never
//! drop-oldest: silently converting "this node is misconfigured" into "this node has holes
//! in its history" would make **D-4**'s indefinite raw retention mean nothing.
//!
//! # Blocking I/O, and why it is not behind `spawn_blocking`
//!
//! Every file operation here is `std::fs`, called from inside an `async fn`. Three reasons,
//! in order of weight. The measured load is ~2 appends/s across ~4 stations, so a worker
//! thread is occupied by one `fsync` twice a second on a runtime with one worker per core.
//! `tokio::fs` is itself `spawn_blocking`, so it would add a task hop per reading and a
//! thread pool whose lifetime nothing owns — against §3.2's invariant that every task is in
//! the supervisor's table. And `ci/layers.toml`'s one-cursor-one-task rule plus a
//! `std::sync::Mutex` around the segment state is what makes "one `write_all`, no await
//! point" checkable by reading the code rather than by reasoning about executor
//! scheduling. The *waiting* — the 50 ms group-commit window, and a follower waiting for
//! the leader's `fsync` — is properly asynchronous.

mod cursor;
mod dedup;
pub mod format;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use mw_component::ComponentId;
use mw_types::{Envelope, OriginId, Seq, StationKey};
use prost::Message as _;
use tokio::sync::{broadcast, watch};
use tracing::{error, warn};

use crate::clock::Clock;
use crate::metrics as m;

pub use cursor::{Cursor, CursorPosition, Read};
pub use dedup::{Verdict, WINDOW as DEDUP_WINDOW};
pub use format::{CursorState, MAX_ORIGINS};

use dedup::Dedup;
use format::{
    encode_record, live_slot, scan_records, CursorSlot, Manifest, ManifestProblem, SegmentHeader,
    CURSOR_FILE_BYTES, CURSOR_SLOT_BYTES, SEGMENT_HEADER_BYTES,
};

/// 8 MiB per segment (W3 §2.4).
///
/// The segment is the granularity of reclamation — a segment is deleted when every active
/// cursor has acked past it — so its size is the trade between reclaiming promptly after a
/// brief outage and rotating rarely. At a worst-case ~1 KiB encoded envelope and 2/s,
/// 8 MiB is about 68 minutes of history.
pub const SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// 2 GiB on disk (W3 §2.4).
///
/// At 2 envelopes/s that is ~12 days at a worst-case ~1 KiB encoding and ~100 days at the
/// measured 30 % column population. Chosen so the ceiling is never the thing that fires
/// first: `mw_spool_depth_bytes` is a gauge an operator watches rise for days beforehand.
pub const CEILING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Seven days (W3 §2.7).
///
/// Longer than any plausible link outage and shorter than the ceiling's worst case, which
/// is the inequality W7 **D-16** requires: a spool filled by a *stuck cursor* is then
/// always resolved by abandoning that cursor, so the operator gets a named failure attached
/// to the cursor that caused it rather than a ceiling event attributed to whichever station
/// happened to admit last.
pub const MAX_LAG: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The group-commit window (W3 §2.4).
///
/// At 2 readings/s a 50 ms window almost never coalesces, so the cost is 50 ms of added
/// latency on a path whose fastest consumer is a 2-second snapshot. It exists for the
/// replay burst.
pub const GROUP_COMMIT_WINDOW: Duration = Duration::from_millis(50);

/// How many concurrent append **calls** one `fsync` may cover (W7 **D-13**).
pub const GROUP_COMMIT_APPENDS: usize = 64;

/// How many envelopes a durable cursor reads at once (W3 §2.4).
///
/// PostgreSQL's wire protocol caps a statement at 65,535 bind parameters; at the registry's
/// 216 allocated columns the hard ceiling is 303 rows per multi-row `INSERT`. 256 is the
/// power of two below it, leaves headroom for the dimension columns, and makes the batch one
/// round trip.
pub const READ_AHEAD: usize = 256;

/// `tokio::sync::broadcast` capacity for `Lossy` subscribers (W3 §2.4).
///
/// ~8.5 minutes of slack at the measured rate. A subscriber that falls further behind is
/// broken, and `lagged_total` says which one.
pub const LOSSY_CAPACITY: usize = 1024;

/// How the spool is sized and where it lives.
///
/// Every field has a named default from a design document; the two that are *also* test
/// seams say so, because a test that cannot shrink a segment cannot reach rotation or
/// reclamation without writing 8 MiB, and a test that cannot shorten the group-commit
/// window pays 50 ms per append.
#[derive(Debug, Clone)]
pub struct SpoolConfig {
    /// `[spool].path`, a sibling of `config.db` and not inside it (W7 **D-10**).
    pub path: PathBuf,
    /// `[spool].ceiling` — [`CEILING_BYTES`] by default.
    pub ceiling_bytes: u64,
    /// `[spool].max_lag` — [`MAX_LAG`] by default.
    pub max_lag: Duration,
    /// [`SEGMENT_BYTES`] in production. A test seam.
    pub segment_bytes: u64,
    /// [`GROUP_COMMIT_WINDOW`] in production. A test seam.
    pub group_commit_window: Duration,
    /// [`GROUP_COMMIT_APPENDS`] in production.
    pub group_commit_appends: usize,
}

impl SpoolConfig {
    /// The shipped defaults, at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ceiling_bytes: CEILING_BYTES,
            max_lag: MAX_LAG,
            segment_bytes: SEGMENT_BYTES,
            group_commit_window: GROUP_COMMIT_WINDOW,
            group_commit_appends: GROUP_COMMIT_APPENDS,
        }
    }
}

/// Why the spool could not be read, written or opened.
///
/// **Everything but [`SpoolError::Full`] is `Fatal`** (W2a Decision 62): a daemon that
/// cannot durably accept readings is not degraded, it is wrong, so the chain is logged at
/// `ERROR`, the process exits non-zero, systemd restarts it and the restart loop is
/// visible. `Full` is `Retryable`, because the condition clears when a cursor catches up or
/// `max_lag` abandons one.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// A filesystem operation failed.
    #[error("{what} {} failed", path.display())]
    Io {
        /// What was being done.
        what: &'static str,
        /// To what.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// The manifest is unreadable, so the origin table is lost (§3.6 step 1).
    #[error("the spool manifest {} is unusable", path.display())]
    Manifest {
        /// Which file.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: ManifestProblem,
    },

    /// The manifest belongs to a different node.
    #[error("the spool at {} belongs to node {found}, and this node is {expected}", path.display())]
    WrongNode {
        /// Which spool.
        path: PathBuf,
        /// Whose it is.
        found: OriginId,
        /// Whose it should be.
        expected: OriginId,
    },

    /// A segment's header is unreadable, which is not a torn tail: it means the file was
    /// never validly created, and truncating it to zero records would discard every record
    /// behind it.
    #[error("the segment {} is unusable", path.display())]
    Segment {
        /// Which file.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: format::HeaderProblem,
    },

    /// A record this spool wrote does not decode, which is corruption its own CRC32C did
    /// not catch.
    #[error("the record at seq {seq} does not decode as a collector.v1.Envelope")]
    Record {
        /// Which record.
        seq: Seq,
        /// The decode failure.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The spool is at its configured ceiling.
    #[error("the spool holds {depth_bytes} bytes and its ceiling is {ceiling_bytes}")]
    Full {
        /// What is on disk.
        depth_bytes: u64,
        /// What is allowed.
        ceiling_bytes: u64,
    },

    /// Two tasks asked for the same cursor. `Spool::open_cursor` returns a cursor owned by
    /// **exactly one** task (W3 Decision 20), and a second holder would mean two tasks
    /// advancing one offset.
    #[error("the cursor {cursor} is already open")]
    CursorBusy {
        /// Which cursor.
        cursor: ComponentId,
    },

    /// A previous append's `fsync` failed, and that failure is sticky.
    ///
    /// Sticky because it is fatal: the process is on its way out, and an append that
    /// reported success after the one before it failed to reach the disk would be a lie
    /// told during shutdown.
    #[error("the spool has already failed to commit: {reason}")]
    Poisoned {
        /// What went wrong the first time.
        reason: Arc<str>,
    },
}

impl mw_types::Classify for SpoolError {
    fn class(&self) -> mw_types::Class {
        match self {
            SpoolError::Full { .. } => mw_types::Class::RETRYABLE,
            _ => mw_types::Class::Fatal,
        }
    }
}

fn io<'a>(what: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> SpoolError + 'a {
    move |source| SpoolError::Io {
        what,
        path: path.to_path_buf(),
        source,
    }
}

/// Everything but the provenance: what `admit` has before the spool mints a position.
///
/// **Why the spool mints the `seq` rather than the caller.** W7 **D-4** makes `seq` the
/// origin's spool append position, so a locally admitted envelope's `seq` and the position
/// of the record holding it are the same number — and the only place that number exists is
/// inside the append's critical section. A caller that reserved a range first would have
/// two operations to keep atomic instead of one, and a reservation abandoned by a
/// cancelled `admit` would leave a gap that looks exactly like the ceiling having dropped
/// a batch.
#[derive(Debug, Clone)]
pub struct Draft {
    /// The resolved station. There is no envelope without one (W2a §4a.6), which is why
    /// this is a [`StationKey`] and not a claim.
    pub station: StationKey,
    /// When the station says it measured.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// When this node received the frame.
    pub received_at: chrono::DateTime<chrono::Utc>,
    /// The reading, enriched.
    pub reading: Arc<mw_types::Reading>,
}

/// How many append calls and how many `fsync`s the spool has done.
///
/// The ratio **is** W7 **D-13**: one 256-envelope batch is one append and one `fsync`, and
/// 64 concurrent callers are 64 appends and one `fsync`. "The cap counts append calls, not
/// records" is otherwise a sentence nothing can check, and the difference between 64 disk
/// syncs and one is the whole reason a reconnecting satellite's replay burst is cheap.
///
/// Exposed as a struct rather than only as Prometheus counters because a test must be able
/// to read them without installing a process-global recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolStats {
    /// Append calls committed.
    pub appends: u64,
    /// `fsync`s performed.
    pub fsyncs: u64,
}

/// What one committed append did.
#[derive(Debug, Clone)]
pub struct Committed {
    /// The highest local position the append consumed. For an empty append, the unchanged
    /// tail.
    pub seq_high: Seq,
    /// The envelopes as they were written, provenance included.
    pub envelopes: Vec<Envelope>,
}

/// The durable fan-out point.
pub struct Spool {
    cfg: SpoolConfig,
    clock: Arc<dyn Clock>,
    node_id: OriginId,
    /// Everything on disk, behind one lock. Held across `write_all` and `fsync` and across
    /// no await point whatsoever, which is what makes the append an atom.
    state: StdMutex<State>,
    /// The group-commit leader slot. Whoever holds it does the `fsync` for everybody.
    syncing: tokio::sync::Mutex<()>,
    /// `(highest append committed, the sticky failure)`.
    synced: watch::Sender<(u64, Option<Arc<str>>)>,
    /// How many appends are written but not yet committed. The leader waits on this to
    /// leave the window early when the cap is reached.
    pending: watch::Sender<u64>,
    /// The tail, so a cursor can await new records rather than poll for them.
    tail: watch::Sender<Seq>,
    /// `Latest` subscribers: coalescing by construction, never blocks, never lags.
    latest: watch::Sender<Option<Arc<Envelope>>>,
    /// `Lossy` subscribers.
    lossy: broadcast::Sender<Arc<Envelope>>,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool")
            .field("path", &self.cfg.path)
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

struct State {
    manifest: Manifest,
    manifest_dirty: bool,
    /// Closed segments, oldest first.
    closed: Vec<SegmentFile>,
    open: OpenSegment,
    depth_bytes: u64,
    tail_seq: Seq,
    /// Appends assigned, ever.
    write_no: u64,
    /// Appends committed, ever — the same number, counted after the `fsync`.
    appends: u64,
    /// `fsync`s performed, ever.
    fsyncs: u64,
    /// Appends committed.
    synced_write_no: u64,
    /// When the oldest uncommitted append was written.
    group_started: Option<tokio::time::Instant>,
    dedup: Dedup,
    cursors: BTreeMap<ComponentId, CursorFile>,
    /// Which cursors currently have a live [`Cursor`] handle.
    attached: BTreeMap<ComponentId, ()>,
    rate: ByteRate,
}

#[derive(Debug)]
struct SegmentFile {
    first_seq: Seq,
    path: PathBuf,
    bytes: u64,
}

struct OpenSegment {
    first_seq: Seq,
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

/// One cursor's persisted position, and the file it lives in.
struct CursorFile {
    path: PathBuf,
    file: std::fs::File,
    /// Which of the two slots the live position is in. The next write goes to the other.
    slot: usize,
    generation: u64,
    acked_seq: Seq,
    acked_at: i64,
    state: CursorState,
}

impl Spool {
    /// Open or create the spool at `cfg.path`, recovering per §3.6.
    ///
    /// ```text
    /// 1. read MANIFEST; CRC fail -> refuse to start
    /// 2. list segments, sort by first_seq
    /// 3. scan the LAST segment forward, validating each record's CRC and len
    ///      -> truncate at the first bad or short record; count mw_spool_truncated_total
    /// 4. tail_seq = the last valid record's seq, or MANIFEST's node seq if empty
    /// 5. rebuild the dedup seen-window by scanning back over the newest segment only
    /// 6. for each cursor file: the valid slot with the higher generation, then clamp
    /// ```
    ///
    /// Only the newest segment is scanned, so start-up is bounded by 8 MiB regardless of
    /// spool depth (W7 **D-18**). Nothing has started yet when this returns: the spool is a
    /// root resource (W3 §3.1).
    pub fn open(
        cfg: SpoolConfig,
        node_id: OriginId,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, SpoolError> {
        let now = clock.now().timestamp_millis();
        std::fs::create_dir_all(&cfg.path)
            .map_err(io("creating the spool directory", &cfg.path))?;
        let cursors_dir = cfg.path.join("cursors");
        std::fs::create_dir_all(&cursors_dir)
            .map_err(io("creating the cursor directory", &cursors_dir))?;

        // 1. the manifest.
        let manifest_path = cfg.path.join("MANIFEST");
        let mut manifest = match std::fs::read(&manifest_path) {
            Ok(bytes) => {
                let m = Manifest::decode(&bytes).map_err(|source| SpoolError::Manifest {
                    path: manifest_path.clone(),
                    source,
                })?;
                if m.node_id != node_id {
                    return Err(SpoolError::WrongNode {
                        path: cfg.path.clone(),
                        found: m.node_id,
                        expected: node_id,
                    });
                }
                m
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let m = Manifest::new(node_id, now);
                write_manifest(&cfg.path, &m)?;
                m
            }
            Err(e) => return Err(io("reading", &manifest_path)(e)),
        };

        // 2. the segments.
        let mut found = Vec::new();
        for entry in
            std::fs::read_dir(&cfg.path).map_err(io("listing the spool directory", &cfg.path))?
        {
            let entry = entry.map_err(io("listing the spool directory", &cfg.path))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("seg") {
                continue;
            }
            let bytes = entry
                .metadata()
                .map_err(io("reading the metadata of", &path))?
                .len();
            let header = std::fs::read(&path).map_err(io("reading", &path))?;
            let header = SegmentHeader::decode(&header).map_err(|source| SpoolError::Segment {
                path: path.clone(),
                source,
            })?;
            found.push(SegmentFile {
                first_seq: header.first_seq,
                path,
                bytes,
            });
        }
        found.sort_by_key(|s| s.first_seq.get());

        // 3-5. the newest segment is the open one; scan it, truncate a torn tail, and
        // rebuild the dedup window from the records it still holds.
        let mut dedup = Dedup::new();
        for o in &manifest.origins {
            dedup.seed(o.node_id, o.hw_seq);
        }
        let mut tail_seq = manifest.tail_seq();
        let open = match found.pop() {
            Some(newest) => {
                let bytes = std::fs::read(&newest.path).map_err(io("reading", &newest.path))?;
                let scan = scan_records(&bytes).map_err(|source| SpoolError::Segment {
                    path: newest.path.clone(),
                    source,
                })?;
                if let Some(torn) = scan.torn {
                    error!(
                        segment = %newest.path.display(),
                        valid_bytes = scan.valid_bytes,
                        was_bytes = bytes.len(),
                        reason = %torn,
                        "the newest spool segment had a torn tail; truncating it there \
                         (W7 D-12). Everything before it is intact and nothing \
                         acknowledged is lost."
                    );
                    let f = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&newest.path)
                        .map_err(io("opening", &newest.path))?;
                    f.set_len(scan.valid_bytes as u64)
                        .map_err(io("truncating", &newest.path))?;
                    f.sync_all().map_err(io("syncing", &newest.path))?;
                    m::spool_truncated();
                }
                if let Some(last) = scan.records.last() {
                    tail_seq = tail_seq.max(last.seq);
                }
                // 5. the seen-window, from the newest segment only.
                for record in &scan.records {
                    let env = decode_record(record.seq, record.payload)?;
                    dedup.accept(env.provenance.origin(), env.provenance.seq().get());
                }
                OpenSegment {
                    first_seq: newest.first_seq,
                    file: append_handle(&newest.path)?,
                    path: newest.path,
                    bytes: scan.valid_bytes as u64,
                }
            }
            None => create_segment(&cfg.path, tail_seq.next(), now)?,
        };
        if manifest.tail_seq() < tail_seq {
            manifest.set_tail_seq(tail_seq);
            write_manifest(&cfg.path, &manifest)?;
        }

        // 6. the cursors.
        let oldest_seq = found.first().map_or(open.first_seq, |s| s.first_seq);
        let mut cursors = BTreeMap::new();
        for entry in std::fs::read_dir(&cursors_dir)
            .map_err(io("listing the cursor directory", &cursors_dir))?
        {
            let entry = entry.map_err(io("listing the cursor directory", &cursors_dir))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("cur") {
                continue;
            }
            let Some(id) = cursor::id_from_file(&path) else {
                warn!(
                    file = %path.display(),
                    "ignoring a file in the spool's cursors/ directory whose name is not \
                     a component id"
                );
                continue;
            };
            let mut record = read_cursor(&path, &id, now)?;
            clamp(&mut record, tail_seq, oldest_seq, &cfg.path)?;
            cursors.insert(id, record);
        }

        let depth_bytes = found.iter().map(|s| s.bytes).sum::<u64>() + open.bytes;
        let (synced, _) = watch::channel((0u64, None));
        let (pending, _) = watch::channel(0u64);
        let (tail, _) = watch::channel(tail_seq);
        let (latest, _) = watch::channel(None);
        let (lossy, _) = broadcast::channel(LOSSY_CAPACITY);
        let spool = Arc::new(Spool {
            cfg,
            clock,
            node_id,
            state: StdMutex::new(State {
                manifest,
                manifest_dirty: false,
                closed: found,
                open,
                depth_bytes,
                tail_seq,
                write_no: 0,
                appends: 0,
                fsyncs: 0,
                synced_write_no: 0,
                group_started: None,
                dedup,
                cursors,
                attached: BTreeMap::new(),
                rate: ByteRate::new(),
            }),
            syncing: tokio::sync::Mutex::new(()),
            synced,
            pending,
            tail,
            latest,
            lossy,
        });
        spool.maintain();
        Ok(spool)
    }

    /// This node's own identity, as the manifest records it.
    pub fn node_id(&self) -> OriginId {
        self.node_id
    }

    /// How the spool is configured.
    pub fn config(&self) -> &SpoolConfig {
        &self.cfg
    }

    /// The tail: the highest local position appended.
    pub fn tail_seq(&self) -> Seq {
        self.locked().tail_seq
    }

    /// How many bytes are on disk.
    pub fn depth_bytes(&self) -> u64 {
        self.locked().depth_bytes
    }

    /// The group-commit counters.
    pub fn stats(&self) -> SpoolStats {
        let st = self.locked();
        SpoolStats {
            appends: st.appends,
            fsyncs: st.fsyncs,
        }
    }

    /// Whether `(origin, seq)` has already been accepted — the dedup gate's read half
    /// (W7 §4.4 step 7).
    pub fn is_duplicate(&self, origin: OriginId, seq: Seq) -> bool {
        self.locked().dedup.is_duplicate(origin, seq.get())
    }

    /// One origin's persisted high water.
    pub fn high_water(&self, origin: OriginId) -> Seq {
        Seq::new(self.locked().dedup.high_water(origin))
    }

    /// Subscribe to the newest envelope — `tokio::sync::watch`, capacity 1 by
    /// construction (W3 §2.4).
    ///
    /// Coalescing is the *semantics* here and not an optimisation: the snapshot and the SSE
    /// stream want current conditions, and an intermediate value nobody read is not a loss.
    /// A `watch` never blocks the sender and never lags, which is what keeps it off
    /// `admit`'s critical path.
    pub fn subscribe_latest(&self) -> watch::Receiver<Option<Arc<Envelope>>> {
        self.latest.subscribe()
    }

    /// Subscribe to every envelope, losing the oldest when 1024 behind (W3 §2.4).
    pub fn subscribe_lossy(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.lossy.subscribe()
    }

    /// Watch the tail, so a cursor awaits new records instead of polling.
    pub(crate) fn watch_tail(&self) -> watch::Receiver<Seq> {
        self.tail.subscribe()
    }

    /// Admit readings **this node measured**: mint `origin = self` and the next position.
    ///
    /// This is `Ingress::admit`'s step 5. One call, one `write_all`, one group-committed
    /// `fsync`, one atom.
    pub async fn admit_local(&self, drafts: &[Draft]) -> Result<Committed, SpoolError> {
        self.append_batch(Batch::Local(drafts)).await
    }

    /// Append envelopes a peer already sealed, preserving their provenance (**D-60**).
    ///
    /// This is `Ingress::accept`'s step 9.
    pub async fn append_sealed(&self, envelopes: &[Envelope]) -> Result<Committed, SpoolError> {
        self.append_batch(Batch::Sealed(envelopes)).await
    }

    /// The append, for either kind of batch.
    ///
    /// The `Latest` and `Lossy` notifications happen **after** the commit and never before
    /// (W3 §2.6 step 6), so a subscriber cannot observe an envelope that is not durable.
    /// That matters most for `live:stream`: a browser must not be shown a reading the
    /// database will not have.
    async fn append_batch(&self, batch: Batch<'_>) -> Result<Committed, SpoolError> {
        if batch.is_empty() {
            return Ok(Committed {
                seq_high: self.tail_seq(),
                envelopes: Vec::new(),
            });
        }
        let (write_no, committed, pending) = {
            let mut st = self.locked();
            let (write_no, committed) = self.write_batch(&mut st, batch)?;
            (write_no, committed, st.write_no - st.synced_write_no)
        };
        self.pending.send_replace(pending);
        self.commit(write_no).await?;

        // After the commit, never before.
        self.tail.send_replace(committed.seq_high);
        for e in &committed.envelopes {
            let shared = Arc::new(e.clone());
            // `send_replace` on a watch and `send` on a broadcast both return without
            // waiting for anybody, which is the property no reading policy may break
            // (W2a Decision 61).
            self.latest.send_replace(Some(Arc::clone(&shared)));
            // An error means no `Lossy` subscriber exists, which is the normal case.
            let _ = self.lossy.send(shared);
        }
        m::spool_appended(committed.envelopes.len());
        self.maintain();
        Ok(committed)
    }

    /// The whole of the critical section: the ceiling check, rotation, the `seq`
    /// assignment, one `write_all`, and the manifest's new tail.
    ///
    /// Synchronous and `&mut State`, so the compiler enforces what the atom needs: there is
    /// no `.await` in here to be cancelled at.
    fn write_batch(
        &self,
        st: &mut State,
        batch: Batch<'_>,
    ) -> Result<(u64, Committed), SpoolError> {
        if let (_, Some(reason)) = &*self.synced.borrow() {
            return Err(SpoolError::Poisoned {
                reason: Arc::clone(reason),
            });
        }

        // Seal and encode first, because the ceiling is a byte count and the bytes are not
        // known until the envelopes are encoded. Nothing is written if the ceiling refuses.
        let mut seq = st.tail_seq;
        let mut envelopes = Vec::with_capacity(batch.len());
        let mut buf = Vec::new();
        for i in 0..batch.len() {
            seq = seq.next();
            let envelope = batch.seal(i, self.node_id, seq);
            let payload = mw_proto::wire::Envelope::from(&envelope).encode_to_vec();
            encode_record(seq, &payload, &mut buf);
            envelopes.push(envelope);
        }
        let bytes = buf.len() as u64;
        if st.depth_bytes + bytes > self.cfg.ceiling_bytes {
            // Drop at the edge, loudly (W3 §2.7). Nothing is written, so the batch is lost
            // and the spool is unchanged -- which is what keeps shutdown working, and is
            // the half the "stall ingest" alternative gets wrong.
            return Err(SpoolError::Full {
                depth_bytes: st.depth_bytes,
                ceiling_bytes: self.cfg.ceiling_bytes,
            });
        }

        let now = self.clock.now().timestamp_millis();
        if st.open.bytes > SEGMENT_HEADER_BYTES as u64
            && st.open.bytes + bytes > self.cfg.segment_bytes
        {
            // Rotate. The open segment is `fsync`ed before being closed, because its
            // records stop being covered by this file handle's commits.
            st.open
                .file
                .sync_data()
                .map_err(io("syncing", &st.open.path))?;
            let next = create_segment(&self.cfg.path, st.tail_seq.next(), now)?;
            let closed = std::mem::replace(&mut st.open, next);
            st.depth_bytes += SEGMENT_HEADER_BYTES as u64;
            st.closed.push(SegmentFile {
                first_seq: closed.first_seq,
                path: closed.path,
                bytes: closed.bytes,
            });
        }

        // One `write_all` of the whole batch: the atom (W7 D-14).
        st.open
            .file
            .write_all(&buf)
            .map_err(io("appending to", &st.open.path))?;
        st.open.bytes += bytes;
        st.depth_bytes += bytes;
        st.tail_seq = seq;
        st.manifest.set_tail_seq(seq);
        st.manifest_dirty = true;
        st.rate.record(now, bytes);
        st.write_no += 1;
        if st.group_started.is_none() {
            st.group_started = Some(tokio::time::Instant::now());
        }
        Ok((
            st.write_no,
            Committed {
                seq_high: seq,
                envelopes,
            },
        ))
    }

    /// Make append `write_no` durable, coalescing with whatever else is waiting.
    ///
    /// One caller becomes the leader by taking [`Spool::syncing`]; it waits out the window,
    /// then `fsync`s everything written so far and publishes how far it got. Everybody else
    /// waits for that publication. No task is spawned, because nothing outside
    /// `mw_supervisor::task` may spawn (§3.2) — so the `fsync` happens on a caller's own
    /// future, which is also the only place it could be cancelled, and cancelling it after
    /// the `write_all` leaves the records to be committed by the next caller.
    async fn commit(&self, write_no: u64) -> Result<(), SpoolError> {
        let mut rx = self.synced.subscribe();
        loop {
            {
                let (through, failed) = &*rx.borrow_and_update();
                if let Some(reason) = failed {
                    return Err(SpoolError::Poisoned {
                        reason: Arc::clone(reason),
                    });
                }
                if *through >= write_no {
                    return Ok(());
                }
            }
            if let Ok(_leading) = self.syncing.try_lock() {
                self.wait_out_window().await;
                match self.flush() {
                    Ok(through) => {
                        self.synced.send_replace((through, None));
                        self.pending.send_replace(0);
                    }
                    Err(e) => {
                        let reason: Arc<str> =
                            Arc::from(mw_types::DisplayChain(&e).to_string().as_str());
                        // Published before returning, so a follower is told rather than
                        // left waiting on a commit that will never happen.
                        self.synced.send_modify(|s| s.1 = Some(Arc::clone(&reason)));
                        return Err(e);
                    }
                }
            } else {
                // Somebody else is in the window. Their `fsync` covers this append,
                // because it syncs the file and this append's bytes are already in it.
                let _ = rx.changed().await;
            }
        }
    }

    /// Wait out the group-commit window, or leave early when the append cap is reached.
    async fn wait_out_window(&self) {
        if self.cfg.group_commit_window.is_zero() {
            return;
        }
        let Some(started) = self.locked().group_started else {
            return;
        };
        let remaining = self
            .cfg
            .group_commit_window
            .saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return;
        }
        let cap = self.cfg.group_commit_appends as u64;
        let mut rx = self.pending.subscribe();
        // Either the window closes or 64 appends have joined, whichever is first.
        let _ = tokio::time::timeout(remaining, rx.wait_for(|&n| n >= cap)).await;
    }

    /// `fsync` the open segment, then the manifest, in that order.
    ///
    /// **The order matters and §3.3 says why**: a manifest claiming a high water the
    /// segments do not contain would drop a real envelope as a duplicate, whereas the
    /// reverse merely costs one deduplicated replay. So the records go first.
    fn flush(&self) -> Result<u64, SpoolError> {
        let mut st = self.locked();
        let through = st.write_no;
        if through == st.synced_write_no {
            return Ok(through);
        }
        st.open
            .file
            .sync_data()
            .map_err(io("syncing", &st.open.path))?;
        if st.manifest_dirty {
            write_manifest(&self.cfg.path, &st.manifest)?;
            st.manifest_dirty = false;
        }
        st.appends += through - st.synced_write_no;
        st.fsyncs += 1;
        st.synced_write_no = through;
        st.group_started = None;
        m::spool_fsynced();
        Ok(through)
    }

    /// Intern the origin, take the dedup verdict, and record the high water — in one
    /// critical section (W7 §4.4 steps 6 and 7).
    ///
    /// Called from `Ingress::accept` *before* the append, so the `hw_seq` the manifest
    /// writes in the commit is the one the segment's records justify. The three steps are
    /// one call rather than three because they share an invariant: a high water recorded
    /// against an origin row that does not exist is silently lost at the next restart, and
    /// every envelope that peer ever sent then looks new.
    pub fn record_accepted(&self, origin: OriginId, seq: Seq) -> Verdict {
        let now = self.clock.now().timestamp_millis();
        let mut st = self.locked();
        let before = st.manifest.origins.len();
        if st.manifest.intern(origin, now).is_none() {
            return Verdict::OriginTableFull;
        }
        if st.manifest.origins.len() != before {
            st.manifest_dirty = true;
            st.dedup.seed(origin, 0);
            m::spool_origins(st.manifest.origins.len());
        }
        let verdict = st.dedup.accept(origin, seq.get());
        if verdict != Verdict::Duplicate {
            let hw = st.dedup.high_water(origin);
            let row = st
                .manifest
                .find_mut(origin)
                .expect("the origin was interned two statements ago");
            row.hw_seq = hw;
            st.manifest_dirty = true;
        }
        verdict
    }

    /// Hand out the cursor named `id`, owned by exactly one task (W3 Decision 20).
    ///
    /// Cursor identity is the **component id**, so a cursor survives a restart and a
    /// renamed peer is a new cursor starting at the current tail — which is correct,
    /// because a renamed peer is operationally a new peer.
    pub fn open_cursor(self: &Arc<Self>, id: ComponentId) -> Result<Cursor, SpoolError> {
        let now = self.clock.now().timestamp_millis();
        let (position, state) = {
            let mut st = self.locked();
            if st.attached.contains_key(&id) {
                return Err(SpoolError::CursorBusy { cursor: id });
            }
            let oldest = st.oldest_seq();
            let tail = st.tail_seq;
            if !st.cursors.contains_key(&id) {
                let path = cursor::file_for(&self.cfg.path, &id);
                let mut record = create_cursor(&path, tail, now)?;
                clamp(&mut record, tail, oldest, &self.cfg.path)?;
                st.cursors.insert(id.clone(), record);
            }
            let (acked_seq, state) = {
                let record = st.cursors.get(&id).expect("just inserted or already there");
                (record.acked_seq, record.state)
            };
            st.attached.insert(id.clone(), ());
            (acked_seq, state)
        };
        Ok(Cursor::new(Arc::clone(self), id, position, state))
    }

    /// Advance a cursor to `through`, durably. **The only place an offset advances.**
    ///
    /// One fixed-size, in-place, synced write to an already-allocated block: no
    /// allocation, no rename, no directory `fsync`. A cursor is written on every ack, which
    /// makes it the most frequent durable write in the system.
    pub(crate) fn ack(&self, id: &ComponentId, through: Seq) -> Result<(), SpoolError> {
        let now = self.clock.now().timestamp_millis();
        {
            let mut st = self.locked();
            let Some(record) = st.cursors.get_mut(id) else {
                // A cursor that is not registered cannot advance, and a silent success
                // here would mean an ack that never reached the disk.
                return Err(SpoolError::CursorBusy { cursor: id.clone() });
            };
            if through <= record.acked_seq {
                return Ok(());
            }
            write_cursor(record, through, now, record.state)?;
        }
        self.maintain();
        Ok(())
    }

    /// Release a cursor's exclusive hold. Its **position stays on disk**, so a component in
    /// `Backoff` resumes where it was rather than at the tail.
    pub(crate) fn detach(&self, id: &ComponentId) {
        self.locked().attached.remove(id);
    }

    /// One cursor's persisted position and state, for a metric or a test.
    pub fn cursor_position(&self, id: &ComponentId) -> Option<CursorPosition> {
        self.locked().cursors.get(id).map(|c| CursorPosition {
            acked_seq: c.acked_seq,
            acked_at: c.acked_at,
            state: c.state,
        })
    }

    /// Read up to `limit` records after `after`, oldest first.
    ///
    /// Reads whole segment files and scans them. There is no index, and at ~2 envelopes/s
    /// against a 256-envelope read-ahead that is one 8 MiB read per two minutes of
    /// history — so an index would be apparatus with no measurement behind it. If one is
    /// ever wanted, the segment's name already is one.
    pub fn read_after(&self, after: Seq, limit: usize) -> Result<Read, SpoolError> {
        let st = self.locked();
        let mut out = Vec::new();
        let mut through = after;
        let mut paths: Vec<&PathBuf> = st
            .closed
            .iter()
            .filter(|s| {
                s.first_seq.get() > after.get() || next_first(&st, s.first_seq).get() > after.get()
            })
            .map(|s| &s.path)
            .collect();
        paths.push(&st.open.path);
        for path in paths {
            if out.len() >= limit {
                break;
            }
            let bytes = std::fs::read(path).map_err(io("reading", path))?;
            let scan = scan_records(&bytes).map_err(|source| SpoolError::Segment {
                path: path.clone(),
                source,
            })?;
            for record in scan.records {
                if record.seq <= after {
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                out.push((record.seq, decode_record(record.seq, record.payload)?));
                through = record.seq;
            }
        }
        Ok(Read {
            envelopes: out,
            through,
        })
    }

    /// Reclamation, `max_lag`, and the gauges. Called after every append and every ack.
    fn maintain(&self) {
        let now_ms = self.clock.now().timestamp_millis();
        let mut st = self.locked();

        // max_lag: a cursor that has been behind longer than the budget is abandoned, its
        // component moves to `Failed { CursorAbandoned }` and its segments are reclaimed
        // (W3 §2.7). Seven days is longer than any plausible link outage, so what this
        // catches is a peer that is gone rather than slow.
        let lag_ms = i64::try_from(self.cfg.max_lag.as_millis()).unwrap_or(i64::MAX);
        let tail = st.tail_seq;
        let abandoned: Vec<ComponentId> = st
            .cursors
            .iter()
            .filter(|(_, c)| c.state == CursorState::Active && c.acked_seq < tail)
            .filter(|(_, c)| now_ms.saturating_sub(c.acked_at) > lag_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in abandoned {
            let Some(record) = st.cursors.get_mut(&id) else {
                continue;
            };
            let at = record.acked_at;
            let seq = record.acked_seq;
            if let Err(e) = write_cursor(record, seq, now_ms, CursorState::Abandoned) {
                // Not fatal: the in-memory state is what reclamation reads, and the worst
                // consequence of a lost `Abandoned` flag is that the cursor is abandoned
                // again after the next restart.
                warn!(cursor = %id, err = %mw_types::DisplayChain(&e),
                      "could not record that a cursor was abandoned");
            }
            error!(
                cursor = %id,
                acked_seq = seq.get(),
                behind_s = (now_ms.saturating_sub(at)) / 1_000,
                max_lag_s = self.cfg.max_lag.as_secs(),
                "the cursor has been behind longer than [spool].max_lag and is abandoned; \
                 its segments will be reclaimed. Raise [spool].max_lag, or fix what is \
                 not consuming it."
            );
            m::spool_cursor_abandoned(&id);
        }

        // Reclamation: a segment goes when every ACTIVE cursor has acked past the first
        // `seq` of the segment after it. Abandoned cursors are ignored, which is the whole
        // reason `max_lag` exists.
        let hold = st
            .cursors
            .values()
            .filter(|c| c.state == CursorState::Active)
            .map(|c| c.acked_seq.get())
            .min();
        let keep_from = hold.unwrap_or(u64::MAX);
        let mut reclaimed = 0usize;
        while st.closed.len() > 1 || (st.closed.len() == 1 && st.open.bytes > 0) {
            let next_first = st
                .closed
                .get(1)
                .map(|s| s.first_seq.get())
                .unwrap_or(st.open.first_seq.get());
            if keep_from < next_first {
                break;
            }
            let Some(gone) = st.closed.first() else { break };
            let path = gone.path.clone();
            let bytes = gone.bytes;
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(segment = %path.display(), err = %e,
                      "could not reclaim a fully-acknowledged spool segment");
                break;
            }
            st.closed.remove(0);
            st.depth_bytes = st.depth_bytes.saturating_sub(bytes);
            reclaimed += 1;
        }
        if reclaimed > 0 {
            m::spool_reclaimed(reclaimed);
        }

        // The gauges W7 §7 says every node carries.
        m::spool_depth(st.depth_bytes, st.tail_seq.get());
        for (id, record) in &st.cursors {
            m::spool_cursor(
                id,
                st.tail_seq.get().saturating_sub(record.acked_seq.get()),
                (now_ms.saturating_sub(record.acked_at)) / 1_000,
                record.state,
            );
        }
        let rate = st.rate.per_second(now_ms);
        let headroom = self.cfg.ceiling_bytes.saturating_sub(st.depth_bytes);
        m::spool_time_remaining(if rate > 0.0 {
            headroom as f64 / rate
        } else {
            f64::INFINITY
        });

        // W7 §3.5's invariant, checked rather than assumed: an operator who sets a 256 MiB
        // spool on an SD card and leaves `max_lag` at seven days has inverted it, and the
        // ceiling then fires before the thing that was supposed to fire first. It cannot be
        // a `plan()` check, because `plan()` is pure and the admit rate is a runtime fact.
        if rate > 0.0 {
            let needed = rate * self.cfg.max_lag.as_secs_f64();
            if (self.cfg.ceiling_bytes as f64) < needed {
                warn!(
                    ceiling_bytes = self.cfg.ceiling_bytes,
                    max_lag_s = self.cfg.max_lag.as_secs(),
                    trailing_bytes_per_s = rate,
                    needed_bytes = needed as u64,
                    "the spool will fill before a stuck peer is abandoned; raise \
                     [spool].ceiling or lower [spool].max_lag"
                );
            }
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a panic while holding the segment state, which is not a
        // condition this code can continue from: the file position and `tail_seq` would
        // disagree. `expect` is the honest spelling.
        self.state
            .lock()
            .expect("the spool state is not poisoned; a panic under it is a bug")
    }
}

/// What is being appended. The two arms are **D-60**'s two entry points.
#[derive(Debug, Clone, Copy)]
enum Batch<'a> {
    /// Readings this node measured. The spool mints `origin = self` and the position.
    Local(&'a [Draft]),
    /// Envelopes a peer sealed. Provenance is carried through untouched.
    Sealed(&'a [Envelope]),
}

impl Batch<'_> {
    fn len(self) -> usize {
        match self {
            Batch::Local(d) => d.len(),
            Batch::Sealed(e) => e.len(),
        }
    }

    fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The envelope for item `i`, at local position `seq`.
    ///
    /// For a local draft this is the **only** place `Provenance::admitted` is called on
    /// the write path, and `seq` is the position the record will occupy. For a sealed
    /// envelope `seq` is ignored, because reassigning it is exactly what **D-60** forbids:
    /// it would renumber the envelope in this node's coordinate space and destroy the
    /// `(origin, seq)` identity the cumulative ack and the dedup gate depend on.
    fn seal(self, i: usize, node_id: OriginId, seq: Seq) -> Envelope {
        match self {
            Batch::Local(drafts) => {
                let d = &drafts[i];
                Envelope {
                    station: d.station.clone(),
                    observed_at: d.observed_at,
                    received_at: d.received_at,
                    provenance: mw_types::Provenance::admitted(node_id, seq),
                    reading: Arc::clone(&d.reading),
                }
            }
            Batch::Sealed(envelopes) => envelopes[i].clone(),
        }
    }
}

impl State {
    fn oldest_seq(&self) -> Seq {
        self.closed
            .first()
            .map_or(self.open.first_seq, |s| s.first_seq)
    }
}

/// The `first_seq` of the segment after the one starting at `first`.
fn next_first(st: &State, first: Seq) -> Seq {
    let mut after = st.closed.iter().map(|s| s.first_seq).filter(|s| *s > first);
    after.next().unwrap_or(st.open.first_seq)
}

fn decode_record(seq: Seq, payload: &[u8]) -> Result<Envelope, SpoolError> {
    let wire = mw_proto::wire::Envelope::decode(payload).map_err(|source| SpoolError::Record {
        seq,
        source: Box::new(source),
    })?;
    Envelope::try_from(&wire).map_err(|source| SpoolError::Record {
        seq,
        source: Box::new(source),
    })
}

fn segment_name(first_seq: Seq) -> String {
    // Seventeen digits, so the lexical order of the names is the order of the sequence
    // numbers and `ls` reads as history. `u64::MAX` is twenty digits, which no spool
    // reaches: at 2 appends/s, 1e17 is three billion years.
    format!("{:017}.seg", first_seq.get())
}

fn create_segment(dir: &Path, first_seq: Seq, now: i64) -> Result<OpenSegment, SpoolError> {
    let path = dir.join(segment_name(first_seq));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)
        .map_err(io("creating", &path))?;
    file.write_all(
        &SegmentHeader {
            first_seq,
            created_at: now,
        }
        .encode(),
    )
    .map_err(io("writing the header of", &path))?;
    // The header is `fsync`ed before the first record (§3.2), so a segment that exists at
    // all is a segment whose `first_seq` can be trusted.
    file.sync_all().map_err(io("syncing", &path))?;
    sync_dir(dir)?;
    Ok(OpenSegment {
        first_seq,
        path,
        file,
        bytes: SEGMENT_HEADER_BYTES as u64,
    })
}

fn append_handle(path: &Path) -> Result<std::fs::File, SpoolError> {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(io("opening", path))
}

/// Rewrite the manifest atomically: write-new, `fsync`, rename, `fsync` the directory.
///
/// Part of the same commit as the records it describes, and **after** their `fsync` (§3.3).
fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<(), SpoolError> {
    let new = dir.join("MANIFEST.new");
    let final_path = dir.join("MANIFEST");
    {
        let mut f = std::fs::File::create(&new).map_err(io("creating", &new))?;
        f.write_all(&manifest.encode())
            .map_err(io("writing", &new))?;
        f.sync_all().map_err(io("syncing", &new))?;
    }
    std::fs::rename(&new, &final_path).map_err(io("renaming over", &final_path))?;
    sync_dir(dir)
}

/// `fsync` a directory, so a rename or a create is itself durable.
fn sync_dir(dir: &Path) -> Result<(), SpoolError> {
    std::fs::File::open(dir)
        .map_err(io("opening", dir))?
        .sync_all()
        .map_err(io("syncing", dir))
}

fn create_cursor(path: &Path, tail: Seq, now: i64) -> Result<CursorFile, SpoolError> {
    // A brand-new cursor starts at the **current tail**, not at the oldest record. A
    // cursor that appeared today has no claim on history it was never promised, and
    // starting it at the oldest segment would replay the whole spool into a peer that has
    // just been configured.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(io("creating", path))?;
    // Allocate both slots up front, so every later write is in-place into a block that
    // already exists -- no allocation, no metadata update, no directory `fsync`.
    file.write_all(&[0u8; CURSOR_FILE_BYTES])
        .map_err(io("allocating", path))?;
    file.sync_all().map_err(io("syncing", path))?;
    if let Some(dir) = path.parent() {
        sync_dir(dir)?;
    }
    let mut record = CursorFile {
        path: path.to_path_buf(),
        file,
        slot: 1,
        generation: 0,
        acked_seq: Seq::new(0),
        acked_at: now,
        state: CursorState::Active,
    };
    write_cursor(&mut record, tail, now, CursorState::Active)?;
    Ok(record)
}

fn read_cursor(path: &Path, id: &ComponentId, now: i64) -> Result<CursorFile, SpoolError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io("opening", path))?;
    let bytes = std::fs::read(path).map_err(io("reading", path))?;
    match live_slot(&bytes) {
        Some((slot, live)) => Ok(CursorFile {
            path: path.to_path_buf(),
            file,
            slot,
            generation: live.generation,
            acked_seq: Seq::new(live.acked_seq),
            acked_at: live.acked_at,
            state: live.state,
        }),
        None => {
            // Both slots torn, or the file was never written. The position is lost, so the
            // cursor starts from nothing and replays -- which costs duplicates the writer's
            // `ON CONFLICT DO UPDATE` and the dedup gate both absorb, and loses nothing.
            warn!(
                cursor = %id,
                file = %path.display(),
                "neither cursor slot is readable; the cursor restarts from the oldest \
                 retained record and replays. Nothing is lost; duplicates are absorbed \
                 by the dedup gate and the writer's upsert."
            );
            Ok(CursorFile {
                path: path.to_path_buf(),
                file,
                slot: 1,
                generation: 0,
                acked_seq: Seq::new(0),
                acked_at: now,
                state: CursorState::Active,
            })
        }
    }
}

/// Write the other slot and make it the live one.
///
/// `write_all_at` plus `sync_data` rather than `O_SYNC`: the effect is the same for a
/// fixed-size in-place write to an already-allocated block, and the alternative is a `libc`
/// dependency that is not in **D-32**'s table for the sake of one open flag.
fn write_cursor(
    record: &mut CursorFile,
    acked_seq: Seq,
    now: i64,
    state: CursorState,
) -> Result<(), SpoolError> {
    use std::os::unix::fs::FileExt as _;
    let slot = 1 - record.slot;
    let generation = record.generation + 1;
    let encoded = CursorSlot {
        generation,
        acked_seq: acked_seq.get(),
        acked_at: now,
        state,
    }
    .encode();
    record
        .file
        .write_all_at(&encoded, (slot * CURSOR_SLOT_BYTES) as u64)
        .map_err(io("writing a slot of", &record.path))?;
    record
        .file
        .sync_data()
        .map_err(io("syncing", &record.path))?;
    record.slot = slot;
    record.generation = generation;
    record.acked_seq = acked_seq;
    record.acked_at = now;
    record.state = state;
    Ok(())
}

/// §3.6 step 6: a recovered cursor position that the segments cannot honour.
fn clamp(record: &mut CursorFile, tail: Seq, oldest: Seq, dir: &Path) -> Result<(), SpoolError> {
    let now = record.acked_at;
    if record.acked_seq > tail {
        // A truncation ate records this cursor had already acked. Clamp back: the records
        // are gone, so claiming to have handled them would hide the gap.
        warn!(
            cursor = %record.path.display(),
            acked_seq = record.acked_seq.get(),
            tail = tail.get(),
            "the cursor is ahead of the spool's tail, which means a torn tail was \
             truncated under it; clamping it back to the tail"
        );
        write_cursor(record, tail, now, record.state)?;
        return Ok(());
    }
    // `oldest - 1` is the position that means "everything before the oldest retained
    // record is handled". A cursor behind that has had segments reclaimed under it, which
    // happens when it was `Abandoned` and has since been re-activated.
    let floor = Seq::new(oldest.get().saturating_sub(1));
    if record.acked_seq < floor {
        let skipped = floor.get() - record.acked_seq.get();
        warn!(
            cursor = %record.path.display(),
            acked_seq = record.acked_seq.get(),
            oldest_seq = oldest.get(),
            skipped,
            spool = %dir.display(),
            "the cursor is behind the oldest retained segment, so {skipped} envelopes \
             were reclaimed before it read them; clamping it forward"
        );
        m::spool_cursor_skipped(skipped);
        write_cursor(record, floor, now, record.state)?;
    }
    Ok(())
}

/// A trailing-hour byte rate, in one-minute buckets.
///
/// The input to `mw_spool_time_remaining_seconds` — the number W5 renders as "about 94
/// days" — and to W7 §3.5's configuration advisory. Sixty buckets of two numbers is small
/// enough to keep inline and coarse enough that a burst does not read as a trend.
#[derive(Debug)]
struct ByteRate {
    buckets: [(i64, u64); 60],
}

impl ByteRate {
    fn new() -> Self {
        Self {
            buckets: [(0, 0); 60],
        }
    }

    fn record(&mut self, now_ms: i64, bytes: u64) {
        let minute = now_ms.div_euclid(60_000);
        let slot = minute.rem_euclid(60) as usize;
        if self.buckets[slot].0 == minute {
            self.buckets[slot].1 += bytes;
        } else {
            self.buckets[slot] = (minute, bytes);
        }
    }

    /// Bytes per second over the trailing hour, counting only the minutes in it.
    fn per_second(&self, now_ms: i64) -> f64 {
        let minute = now_ms.div_euclid(60_000);
        let total: u64 = self
            .buckets
            .iter()
            .filter(|(m, _)| *m > minute - 60 && *m <= minute)
            .map(|(_, b)| *b)
            .sum();
        if total == 0 {
            return 0.0;
        }
        total as f64 / 3_600.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_named_by_the_seq_of_its_first_record() {
        assert_eq!(segment_name(Seq::new(4_608)), "00000000000004608.seg");
        assert_eq!(segment_name(Seq::FIRST), "00000000000000001.seg");
    }

    #[test]
    fn segment_names_sort_lexically_in_sequence_order() {
        let mut names: Vec<_> = [18_004u64, 11_392, 4_608]
            .into_iter()
            .map(|s| segment_name(Seq::new(s)))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "00000000000004608.seg",
                "00000000000011392.seg",
                "00000000000018004.seg"
            ]
        );
    }

    #[test]
    fn the_trailing_rate_counts_only_the_last_hour() {
        let mut r = ByteRate::new();
        let now = 1_762_171_200_000i64;
        r.record(now, 3_600);
        assert!((r.per_second(now) - 1.0).abs() < 1e-9);
        // Two hours later the bucket is stale and does not count.
        assert_eq!(r.per_second(now + 2 * 60 * 60_000), 0.0);
    }

    #[test]
    fn the_trailing_rate_sums_the_minutes_inside_the_hour() {
        let mut r = ByteRate::new();
        let now = 1_762_171_200_000i64;
        for i in 0..10 {
            r.record(now + i * 60_000, 360);
        }
        let at = now + 9 * 60_000;
        assert!(
            (r.per_second(at) - 1.0).abs() < 1e-9,
            "{}",
            r.per_second(at)
        );
    }
}
