//! §5's live data: one SSE endpoint, several channels.
//!
//! # Why one endpoint rather than one per channel
//!
//! HTTP/1.1 caps a browser at six connections per origin, and this UI is served from a
//! Rust binary that may well be on plaintext HTTP/1.1 on a LAN -- so six is enough until
//! the operator opens a second tab. The load-bearing reason is the other one: one
//! connection means **one** reconnect path, **one** backfill path and **one** gap story,
//! instead of three that drift apart.
//!
//! # What replaces the polling
//!
//! The current system runs two three-second polling loops and the management UI must not
//! reintroduce the pattern (§5.6). So nothing on the client asks for status: component
//! state, log records and readings are pushed. The **server** observes its own component
//! set on a tick, which is not the same thing -- an in-process read of
//! [`ComponentControl::components`] costs a lock, and it happens once for every connected
//! browser rather than once per browser per three seconds.
//!
//! Two of the three channels are genuinely event-driven and need no tick at all: log
//! records arrive through [`crate::host::LogSource::tap`] on the emitting thread, and a
//! reading arrives when the station produces one.
//!
//! # Identity, resume and the gap
//!
//! Every live frame carries a monotonic `id:`. On reconnect a browser's `EventSource`
//! sends `Last-Event-ID` automatically; a client that re-creates the connection itself --
//! which the UI does, because `EventSource`'s own reconnect interval is not §5.2's
//! schedule -- passes `?last_event_id=` instead. Both are accepted, and the resume rules
//! are the same either way:
//!
//! | `last_event_id` | What the connection gets |
//! |---|---|
//! | absent | the full backfill of §5.3, then `caught_up` |
//! | known | every frame after it, then `caught_up` |
//! | evicted | a `gap`, then the full backfill, then `caught_up` |
//!
//! **Backfill frames carry no `id:`.** That is deliberate: an id is a position in the live
//! sequence, and giving a replayed snapshot a fresh one would let a client resume from a
//! point that never existed in the stream. A client's resume point therefore only ever
//! advances past frames that were actually broadcast.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::response::sse::{Event, KeepAlive, Sse};
use mw_component::ComponentControl;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt as _};

use crate::admin::ConfigAdmin;
use crate::display::Vocabulary;
use crate::host::{LogEntry, LogSink, LogSource, Readings, Stream as LogStream};
use crate::runtime::ComponentView;

/// The channels a connection may subscribe to.
///
/// A closed set rather than a free string: a typo in `?channels=` is a connection that
/// silently receives nothing, which is the most expensive possible way to misspell a word.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// §2.3's component state: a snapshot, then deltas.
    Components,
    /// The merged log rings of §1.8.
    Logs,
    /// The latest reading per station, for the Home tiles and §0.7 step 3.
    Readings,
}

impl Channel {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "components" => Some(Channel::Components),
            "logs" => Some(Channel::Logs),
            "readings" => Some(Channel::Readings),
            _ => None,
        }
    }

    /// Parse a `?channels=a,b` list. An unknown name is an error rather than a silent
    /// drop, for the reason on the type.
    pub fn parse_list(list: &str) -> Result<BTreeSet<Channel>, String> {
        let mut out = BTreeSet::new();
        for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match Channel::parse(part) {
                Some(c) => {
                    out.insert(c);
                }
                None => return Err(part.to_string()),
            }
        }
        if out.is_empty() {
            out.insert(Channel::Components);
        }
        Ok(out)
    }
}

/// What a channel change submits.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ChannelsInput {
    /// The channels this connection should receive from now on.
    pub channels: Vec<String>,
}

/// What a channel change answers with.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ChannelsView {
    /// The channels now in force, which is what the client asked for when it succeeded.
    pub channels: Vec<String>,
}

/// One frame on the wire.
#[derive(Debug, Clone)]
pub struct Frame {
    /// `None` on a backfill frame, for the reason in the module documentation.
    pub id: Option<u64>,
    /// The SSE `event:` field.
    pub event: &'static str,
    /// The serialised payload.
    pub data: String,
    /// Which channel it belongs to, so a connection can filter without parsing `data`.
    pub channel: Channel,
}

impl Frame {
    fn to_event(&self) -> Event {
        let mut event = Event::default().event(self.event).data(&self.data);
        if let Some(id) = self.id {
            event = event.id(id.to_string());
        }
        event
    }
}

/// How many live frames are kept for `Last-Event-ID` resume.
///
/// Sized against the thing that actually fills it: a daemon logging at a steady few
/// records a second, reconnecting on §5.2's worst case of 15 seconds plus jitter. 2,048 is
/// two orders of magnitude of headroom on that, and the `gap` path exists for the case it
/// is not -- which is the point of having an explicit one rather than a bigger buffer.
const REPLAY: usize = 2_048;

/// The §5.4 record a client gets when its resume point has been evicted.
#[derive(Debug, Clone, serde::Serialize)]
struct Gap {
    kind: &'static str,
    stream: String,
    dropped: u64,
    ts: chrono::DateTime<chrono::Utc>,
}

/// §5.3's boundary marker.
#[derive(Debug, Clone, serde::Serialize)]
struct CaughtUp {
    kind: &'static str,
    ts: chrono::DateTime<chrono::Utc>,
}

/// The first frame on every connection: who the server thinks this connection is.
#[derive(Debug, Clone, serde::Serialize)]
struct Hello {
    stream_id: String,
    generation: u64,
}

/// §5.3's component backfill: a **full snapshot**, so the client never has to reconstruct
/// state from a partial event history.
#[derive(Debug, Clone, serde::Serialize)]
struct ComponentSnapshot<'a> {
    kind: &'static str,
    generation: u64,
    components: &'a [ComponentView],
}

#[derive(Debug, Clone, serde::Serialize)]
struct ComponentDelta<'a> {
    kind: &'static str,
    generation: u64,
    component: &'a ComponentView,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ComponentRemoved {
    kind: &'static str,
    generation: u64,
    id: String,
}

/// The hub: the broadcast, the replay ring, and the per-connection subscriptions.
pub struct Hub {
    tx: broadcast::Sender<Arc<Frame>>,
    replay: Mutex<VecDeque<Arc<Frame>>>,
    next_id: AtomicU64,
    /// How many frames the replay ring has evicted, for the `gap` a resume reports.
    evicted: AtomicU64,
    /// Per-connection channel subscriptions, so §5.1's in-stream change needs no reconnect.
    subscriptions: Mutex<BTreeMap<String, BTreeSet<Channel>>>,
    connections: AtomicU64,
    generation: AtomicU64,
    /// Bumped when every open connection should end and reconnect.
    ///
    /// **This is what keeps a save from taking ten seconds.** An SSE response is an
    /// in-flight request that never finishes, and `mw-http`'s graceful shutdown waits for
    /// in-flight requests -- so a configuration change that restarts `http:listener`
    /// (which adding or removing a station does, by M7-1's stated cost) would wait out the
    /// whole drain deadline on a browser that is doing exactly what it was told to do, log
    /// two warnings, and refuse connections for the moment in between.
    ///
    /// So a generation change ends every stream instead. The client's answer to a stream
    /// that ends is already written and already tested: §5.2's one reconnect, a `gap` if
    /// applicable, and an `INFO` line -- because "a server restart is not an error" is the
    /// same situation seen from the other side.
    epoch: AtomicU64,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// An empty hub. Nothing is observed until [`Hub::observe`] is spawned.
    pub fn new() -> Arc<Self> {
        // The channel is bounded: a browser that stops reading must not be able to grow
        // this process's memory without limit. An overrun is a `RecvError::Lagged` on that
        // one connection, which [`connect`] turns into a `gap` -- the same record an
        // operator would see from a ring overflow, and the same sentence.
        let (tx, _) = broadcast::channel(1_024);
        Arc::new(Hub {
            tx,
            replay: Mutex::new(VecDeque::with_capacity(REPLAY.min(256))),
            next_id: AtomicU64::new(1),
            evicted: AtomicU64::new(0),
            subscriptions: Mutex::new(BTreeMap::new()),
            connections: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        })
    }

    /// Publish one live frame: assign it an id, keep it for resume, and broadcast it.
    fn publish(&self, event: &'static str, channel: Channel, data: String) {
        let frame = Arc::new(Frame {
            id: Some(self.next_id.fetch_add(1, Ordering::Relaxed)),
            event,
            data,
            channel,
        });
        {
            let mut ring = self.lock_replay();
            if ring.len() == REPLAY {
                ring.pop_front();
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
            ring.push_back(Arc::clone(&frame));
        }
        // An error here means nobody is connected, which is the ordinary case.
        let _ = self.tx.send(frame);
    }

    fn lock_replay(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<Frame>>> {
        self.replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_subs(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeSet<Channel>>> {
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every retained frame strictly after `id`, or `None` if `id` has been evicted.
    fn after(&self, id: u64) -> Option<Vec<Arc<Frame>>> {
        let ring = self.lock_replay();
        let oldest = ring.front().and_then(|f| f.id)?;
        // `id + 1 < oldest` rather than `id < oldest`: resuming from exactly the frame
        // before the oldest retained one loses nothing.
        if id + 1 < oldest {
            return None;
        }
        Some(
            ring.iter()
                .filter(|f| f.id.is_some_and(|fid| fid > id))
                .cloned()
                .collect(),
        )
    }

    /// The channels one connection currently wants.
    fn channels_for(&self, stream_id: &str) -> BTreeSet<Channel> {
        self.lock_subs().get(stream_id).cloned().unwrap_or_default()
    }

    /// §5.1's in-stream change. `Err` names the channel this build does not know.
    pub fn set_channels(&self, stream_id: &str, wanted: &[String]) -> Result<Vec<String>, String> {
        let mut set = BTreeSet::new();
        for name in wanted {
            match Channel::parse(name) {
                Some(c) => {
                    set.insert(c);
                }
                None => return Err(name.clone()),
            }
        }
        let mut subs = self.lock_subs();
        if !subs.contains_key(stream_id) {
            return Err(String::new());
        }
        let names = set.iter().map(channel_name).map(String::from).collect();
        subs.insert(stream_id.to_string(), set);
        Ok(names)
    }

    /// How many connections are open, for `System → Advanced` and for a test.
    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// The current epoch. A connection opened on an older one is over.
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// End every open stream, so the listener can drain.
    ///
    /// The frame is published **after** the bump so that every connection is woken by it
    /// and then sees that it is stale; publishing first would leave an idle connection
    /// waiting for the next heartbeat, twenty seconds away.
    pub fn close_all(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.publish("reload", Channel::Components, "{}".to_string());
    }
}

fn channel_name(c: &Channel) -> &'static str {
    match c {
        Channel::Components => "components",
        Channel::Logs => "logs",
        Channel::Readings => "readings",
    }
}

/// The hub's own tap on the log rings.
///
/// Registered once, for the life of the process, rather than per connection: the rings
/// hand a record to every tap, so one tap per browser would mean N conversions of the same
/// record. The conversion happens once here and the fan-out is the broadcast's job.
struct Tap(Arc<Hub>);

impl LogSink for Tap {
    fn entry(&self, entry: LogEntry) {
        // Serialising on the emitting thread is deliberate: the alternative is holding an
        // `Arc<Record>` and rendering per subscriber, which does the same work N times on
        // the connection's thread instead of once here.
        if let Ok(data) = serde_json::to_string(&entry) {
            self.0.publish("log", Channel::Logs, data);
        }
    }
}

/// Everything the observation task needs, so the task owns no router state.
///
/// Four capability handles, none of them `Debug`, so the manual implementation below says
/// what is useful about it in a log line rather than deriving nothing.
pub struct Sources {
    /// The configuration, for the vocabulary a component view is rendered with.
    ///
    /// Re-read on every tick rather than cached, because D-36 puts the translation on the
    /// server: a station renamed through the API must change the name on every connected
    /// browser's badge at the next tick, and a cached vocabulary is how it would not.
    pub admin: Arc<dyn ConfigAdmin>,
    /// Component state, read on the tick.
    pub control: Arc<dyn ComponentControl>,
    /// The log rings, for the tap.
    pub logs: Arc<dyn LogSource>,
    /// The latest reading per station.
    pub readings: Arc<dyn Readings>,
}

impl std::fmt::Debug for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sources")
            .field("generation", &self.admin.generation())
            .finish_non_exhaustive()
    }
}

/// What one rendering of the component set needs, gathered once per read.
///
/// A struct rather than four arguments threaded through three functions: the four always
/// travel together and two of them (the vocabulary and the awaited set) are derived from
/// the third.
pub struct Rendering {
    /// The vocabulary, built from the configuration.
    pub vocab: Vocabulary,
    /// Which components the newest change is still waiting on (§2.2).
    pub awaiting: crate::admin::Awaiting,
    /// The newest committed generation.
    pub generation: u64,
}

impl std::fmt::Debug for Rendering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendering")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Rendering {
    /// Gather it from a [`ConfigAdmin`].
    pub async fn gather(admin: &dyn ConfigAdmin) -> Result<Self, crate::ApiError> {
        let config = admin.read().await?;
        Ok(Rendering {
            vocab: Vocabulary::new(&config),
            awaiting: admin.awaiting(),
            generation: admin.generation().get(),
        })
    }
}

/// §5.2's keep-alive interval.
///
/// A **comment frame**, which is what §5.2 specifies, and it is `axum`'s own
/// `Sse::keep_alive` that emits it -- so an idle proxy does not reap the connection and a
/// black-holed one is detected in bounded time, without this crate owning a timer. §5.1
/// also lists a `heartbeat` event among the frame types; a comment frame is the better
/// reading of the two, because a typed frame would be a log-less record in a stream whose
/// every other frame carries information.
const KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(20);

/// Observe the process and publish what changed.
///
/// **There is no tick here, and that is the design rather than an optimisation.** W3 §3.3
/// puts one clock in the whole process and §3.2 puts every task in one table, so a loop of
/// this crate's own with an interval and a `spawn` would be a second clock and an
/// untracked task -- which `clippy.toml` refuses by name. What drives this instead is the
/// two places a change is actually committed:
///
/// * the supervisor's state publication ([`ComponentControl::changes`]), which fires at
///   the instant a component's state is committed rather than up to a tick later;
/// * the reading fan-out ([`Readings::changes`]), which fires when a reading arrives.
///
/// So the stream is strictly more current than a poll would have made it, and the process
/// gains nothing it has to account for. The one timer left in §5 is the client's
/// `Backoff` countdown over a server-supplied absolute instant, which §5.6 names as the
/// stated exception.
///
/// Awaited as a process-level future beside `supervisor.run` rather than spawned, which is
/// how this daemon already runs its other long-lived loops, and returns when `cancel`
/// resolves so that `main`'s join finishes.
pub async fn observe(
    hub: Arc<Hub>,
    sources: Sources,
    cancel: impl std::future::Future<Output = ()>,
) {
    hub.logs_tap(&sources);
    let mut previous: BTreeMap<String, ComponentView> = BTreeMap::new();
    let mut last_reading: BTreeMap<String, chrono::DateTime<chrono::Utc>> = BTreeMap::new();

    // Taken **once** and held: a receiver created per wait would mark the current version
    // as seen and miss the change it was created to notice.
    let mut component_changes = sources.control.changes();
    let mut reading_changes = sources.readings.changes();

    // `None` from either is a build that does not publish that kind of change -- a test
    // double, or a node with no spool. The stream then serves the snapshot and the
    // `caught_up` boundary and does not follow, which is visible rather than silent.
    if component_changes.is_none() && reading_changes.is_none() {
        tracing::info!(
            "nothing publishes live changes in this build, so the management UI's stream \
             serves its backfill and then stays quiet"
        );
        cancel.await;
        return;
    }

    let mut cancel = std::pin::pin!(cancel);
    loop {
        enum Woke {
            Components,
            Readings,
            Stop,
        }
        // `select!` over two optional receivers. The `pending` arms are what let one of
        // them be absent without a second copy of this loop.
        let woke = tokio::select! {
            () = &mut cancel => Woke::Stop,
            result = async {
                match component_changes.as_mut() {
                    Some(rx) => rx.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => {
                if result { Woke::Components } else { Woke::Stop }
            }
            result = async {
                match reading_changes.as_mut() {
                    Some(rx) => rx.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => {
                if result { Woke::Readings } else { Woke::Stop }
            }
        };

        match woke {
            Woke::Stop => return,
            Woke::Components => {
                let Ok(rendering) = Rendering::gather(sources.admin.as_ref()).await else {
                    // A configuration read that failed is a database that is unreachable,
                    // and the log already says so through the tap. Keeping the last good
                    // state on screen -- dimmed by the client, because the stream itself
                    // is still up -- beats replacing it with nothing.
                    continue;
                };
                let generation = rendering.generation;
                let before = hub.generation.swap(generation, Ordering::Relaxed);
                // A committed change restarts components, and adding or removing a station
                // or a website restarts **every** component -- including the one listener.
                // Ending the streams here is what lets it stop; see `Hub::epoch`.
                if before != 0 && before != generation {
                    hub.close_all();
                }
                let view = crate::runtime::components(
                    &sources.control.components(),
                    generation,
                    &rendering.awaiting,
                    &rendering.vocab,
                    chrono::Utc::now(),
                );
                let mut seen = BTreeSet::new();
                for component in view.components {
                    seen.insert(component.id.clone());
                    let changed = previous
                        .get(&component.id)
                        .is_none_or(|old| !same(old, &component));
                    if changed {
                        if let Ok(data) = serde_json::to_string(&ComponentDelta {
                            kind: "delta",
                            generation,
                            component: &component,
                        }) {
                            hub.publish("component", Channel::Components, data);
                        }
                        previous.insert(component.id.clone(), component);
                    }
                }
                let gone: Vec<String> = previous
                    .keys()
                    .filter(|id| !seen.contains(*id))
                    .cloned()
                    .collect();
                for id in gone {
                    previous.remove(&id);
                    if let Ok(data) = serde_json::to_string(&ComponentRemoved {
                        kind: "removed",
                        generation,
                        id,
                    }) {
                        hub.publish("component", Channel::Components, data);
                    }
                }
            }
            Woke::Readings => {
                // The vocabulary, for `station_name`. Read here rather than cached for
                // D-36's reason: a station renamed through the API changes the name on
                // every connected browser's tile at the next reading.
                let rendering = Rendering::gather(sources.admin.as_ref()).await.ok();
                for mut reading in sources.readings.latest() {
                    let unchanged = last_reading
                        .get(&reading.station_key)
                        .is_some_and(|ts| *ts == reading.ts);
                    if unchanged {
                        continue;
                    }
                    last_reading.insert(reading.station_key.clone(), reading.ts);
                    if let Some(rendering) = &rendering {
                        reading.station_name = rendering.vocab.station_name(&reading.station_key);
                    }
                    if let Ok(data) = serde_json::to_string(&reading) {
                        hub.publish("reading", Channel::Readings, data);
                    }
                }
            }
        }
    }
}

impl Hub {
    /// Register the log tap exactly once, from [`observe`].
    fn logs_tap(self: &Arc<Self>, sources: &Sources) {
        sources
            .logs
            .tap(Arc::new(Tap(Arc::clone(self))) as Arc<dyn LogSink>);
    }
}

/// Whether two renderings of the same component say the same thing.
///
/// Compared on the **rendered** view rather than on `ComponentStatus`, because the view is
/// what the client holds: a schedule's `next_run` moving is a real change the UI shows,
/// and a counter that only counts up is not worth a frame a second on its own.
fn same(a: &ComponentView, b: &ComponentView) -> bool {
    a.state == b.state
        && a.sentence == b.sentence
        && a.applied_generation == b.applied_generation
        && a.pending == b.pending
        && a.last_error == b.last_error
        && a.notes == b.notes
        && a.degraded == b.degraded
        && a.schedule.last_run == b.schedule.last_run
        && a.schedule.last_outcome == b.schedule.last_outcome
        && a.schedule.next_run == b.schedule.next_run
        && a.counters.runs_total == b.counters.runs_total
        && a.counters.failed_total == b.counters.failed_total
}

/// Removes a connection's subscription when its stream is dropped.
///
/// A `Drop` guard rather than cleanup in the handler, because an SSE connection ends by
/// the client going away: there is no code path on the server that "finishes" it, so the
/// only reliable moment is the stream being dropped.
struct Subscription {
    hub: Arc<Hub>,
    id: String,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.lock_subs().remove(&self.id);
        self.hub.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Everything a request handler needs to open a connection.
///
/// A second struct rather than fields on [`crate::routes::Mgmt`] so that a hub can be
/// absent: a build or a test that does not want live data constructs the router without
/// one, and the route answers `501` rather than the process carrying a tick it never uses.
pub struct Live {
    /// The hub.
    pub hub: Arc<Hub>,
    /// The log rings, for the backfill.
    pub logs: Arc<dyn LogSource>,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").field("hub", &self.hub).finish()
    }
}

/// The log backfill, read through [`Live`] so the hub owns no source.
pub fn log_backfill(live: &Live, stream: LogStream) -> crate::host::LogsView {
    let mut view = live.logs.read(stream, BACKFILL);
    // Newest last is what §5.3 specifies and what the ring already returns; stated here
    // because the one thing a log view cannot recover from is being handed in the wrong
    // order and told it is right.
    view.entries.sort_by_key(|e| e.ts);
    view
}

/// What one connection asked for.
#[derive(Debug, Clone)]
pub struct Request {
    /// The channels, already parsed.
    pub channels: BTreeSet<Channel>,
    /// Which log ring, for the backfill and the live filter.
    pub log_stream: LogStream,
    /// The resume point, from `Last-Event-ID` or `?last_event_id=`.
    pub last_event_id: Option<u64>,
}

/// How many log records a connection backfills. §5.3's number; the full ring is the paged
/// `GET /api/v1/logs`, which is a different question with a different shape.
const BACKFILL: usize = 500;

/// Open one connection.
///
/// The returned response is an infinite stream: the backfill, then `caught_up`, then every
/// live frame on a subscribed channel.
pub fn connect(
    live: &Live,
    control: &dyn ComponentControl,
    rendering: &Rendering,
    request: Request,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let generation = rendering.generation;
    let hub = Arc::clone(&live.hub);
    // An opaque, per-connection id. Not a secret and not a UUID: it names one open
    // connection to the client that opened it, so the only property it needs is that two
    // live connections do not collide -- which the counter gives and the mix makes
    // unguessable enough that a stale client cannot stumble onto another's.
    let stream_id = format!(
        "{:x}",
        (hub.connections
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(2_654_435_761))
            ^ hub.next_id.load(Ordering::Relaxed)
    );
    hub.lock_subs()
        .insert(stream_id.clone(), request.channels.clone());
    let guard = Subscription {
        hub: Arc::clone(&hub),
        id: stream_id.clone(),
    };

    let mut prologue: Vec<Frame> = Vec::new();
    let hello = serde_json::to_string(&Hello {
        stream_id: stream_id.clone(),
        generation,
    })
    .unwrap_or_else(|_| "{}".to_string());
    prologue.push(Frame {
        id: None,
        event: "hello",
        data: hello,
        channel: Channel::Components,
    });

    // The three resume cases of the module table, in order.
    let replayed = request.last_event_id.and_then(|id| hub.after(id));
    match (request.last_event_id, replayed) {
        (Some(_), Some(frames)) => {
            prologue.extend(frames.into_iter().map(|f| (*f).clone()));
        }
        (Some(_), None) => {
            // The resume point is gone. Say so, then start again -- in that order, so the
            // hole is reported before the data that follows it rather than inferred from a
            // jump in the ids.
            let dropped = hub.evicted.load(Ordering::Relaxed);
            prologue.push(gap_frame("live", dropped));
            prologue.extend(backfill(live, control, rendering, &request));
        }
        (None, _) => {
            prologue.extend(backfill(live, control, rendering, &request));
        }
    }

    prologue.push(Frame {
        id: None,
        event: "caught_up",
        data: serde_json::to_string(&CaughtUp {
            kind: "caught_up",
            ts: chrono::Utc::now(),
        })
        .unwrap_or_else(|_| "{}".to_string()),
        channel: Channel::Components,
    });

    let live = BroadcastStream::new(hub.tx.subscribe());
    let hub_for_filter = Arc::clone(&hub);
    let hub_for_filter2 = Arc::clone(&hub);
    let id_for_filter = stream_id;
    let opened_at_epoch = hub.epoch();
    // `take_while` rather than a check inside `filter_map`: the stream has to **end**, not
    // merely stop yielding, or the response stays in flight and the drain still waits.
    let tail = live
        .take_while(move |_| hub_for_filter.epoch() == opened_at_epoch)
        .filter_map(move |item| {
            // Hold the guard inside the stream so the subscription outlives exactly the
            // connection.
            let _ = &guard;
            match item {
                Ok(frame) => {
                    let wanted = hub_for_filter2.channels_for(&id_for_filter);
                    if wanted.contains(&frame.channel) {
                        Some(Ok(frame.to_event()))
                    } else {
                        None
                    }
                }
                // §5.4 again, from the other direction: this browser could not keep up. The
                // same record and the same sentence as a ring overflow, because to the
                // operator it is the same fact.
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(gap_frame("live", n).to_event()))
                }
            }
        });

    let head = tokio_stream::iter(
        prologue
            .into_iter()
            .map(|f| Ok(f.to_event()))
            .collect::<Vec<_>>(),
    );

    Sse::new(head.chain(tail)).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}

fn gap_frame(stream: &str, dropped: u64) -> Frame {
    Frame {
        id: None,
        event: "gap",
        data: serde_json::to_string(&Gap {
            kind: "gap",
            stream: stream.to_string(),
            dropped,
            ts: chrono::Utc::now(),
        })
        .unwrap_or_else(|_| "{}".to_string()),
        channel: Channel::Logs,
    }
}

/// §5.3's backfill: the component snapshot, then the log ring's current contents.
fn backfill(
    live: &Live,
    control: &dyn ComponentControl,
    rendering: &Rendering,
    request: &Request,
) -> Vec<Frame> {
    let generation = rendering.generation;
    let mut out = Vec::new();
    if request.channels.contains(&Channel::Components) {
        let view = crate::runtime::components(
            &control.components(),
            generation,
            &rendering.awaiting,
            &rendering.vocab,
            chrono::Utc::now(),
        );
        if let Ok(data) = serde_json::to_string(&ComponentSnapshot {
            kind: "snapshot",
            generation,
            components: &view.components,
        }) {
            out.push(Frame {
                id: None,
                event: "component",
                data,
                channel: Channel::Components,
            });
        }
    }
    if request.channels.contains(&Channel::Logs) {
        let logs = log_backfill(live, request.log_stream);
        if logs.dropped > 0 {
            out.push(gap_frame(log_stream_name(request.log_stream), logs.dropped));
        }
        for entry in logs.entries {
            if let Ok(data) = serde_json::to_string(&entry) {
                out.push(Frame {
                    id: None,
                    event: "log",
                    data,
                    channel: Channel::Logs,
                });
            }
        }
    }
    out
}

fn log_stream_name(stream: LogStream) -> &'static str {
    match stream {
        LogStream::App => "app",
        LogStream::Access => "access",
        LogStream::Both => "both",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_channel_is_an_error_rather_than_a_silent_drop() {
        assert!(Channel::parse_list("components,logs").is_ok());
        assert_eq!(Channel::parse_list("components,logz").unwrap_err(), "logz");
    }

    /// An empty list is `components`, which is what every page needs: a connection that
    /// asked for nothing is a client bug, and answering it with no frames at all makes the
    /// bug look like a server that stopped.
    #[test]
    fn an_empty_channel_list_is_components() {
        let parsed = Channel::parse_list("").expect("empty parses");
        assert_eq!(
            parsed.into_iter().collect::<Vec<_>>(),
            vec![Channel::Components]
        );
    }

    /// The resume contract: a retained id replays what follows it, and an evicted one is
    /// `None` so the caller emits a gap instead of silently starting from the ring's
    /// oldest frame -- which would look like a successful resume that lost records.
    #[test]
    fn a_resume_point_is_either_replayed_or_reported_as_a_gap() {
        let hub = Hub::new();
        for i in 0..5u64 {
            hub.publish("log", Channel::Logs, format!("{{\"n\":{i}}}"));
        }
        let after = hub.after(2).expect("id 2 is retained");
        assert_eq!(after.len(), 3, "ids 3, 4 and 5 follow id 2");
        assert_eq!(after[0].id, Some(3));

        // Evict everything, then a resume from the start must be a gap.
        for i in 0..REPLAY as u64 {
            hub.publish("log", Channel::Logs, format!("{{\"n\":{i}}}"));
        }
        assert!(hub.after(1).is_none(), "id 1 has been evicted");
    }

    /// Backfill frames carry no id, so a client's resume point never advances past a frame
    /// that was not broadcast.
    #[test]
    fn a_published_frame_has_an_id_and_a_backfill_frame_does_not() {
        let hub = Hub::new();
        hub.publish("log", Channel::Logs, "{}".to_string());
        assert_eq!(hub.lock_replay().front().and_then(|f| f.id), Some(1));
        assert_eq!(gap_frame("app", 3).id, None);
    }

    /// §5.1's in-stream change refuses a channel this build does not know rather than
    /// quietly narrowing the subscription, and refuses an unknown connection rather than
    /// creating one -- a `POST` naming a stream that does not exist is a stale client.
    #[test]
    fn setting_channels_refuses_an_unknown_name_and_an_unknown_connection() {
        let hub = Hub::new();
        hub.lock_subs()
            .insert("abc".to_string(), BTreeSet::from([Channel::Components]));
        assert_eq!(
            hub.set_channels("abc", &["components".into(), "logs".into()])
                .expect("both are known"),
            vec!["components".to_string(), "logs".to_string()]
        );
        assert_eq!(
            hub.set_channels("abc", &["nope".into()]).unwrap_err(),
            "nope"
        );
        assert_eq!(
            hub.set_channels("missing", &["logs".into()]).unwrap_err(),
            ""
        );
    }

    /// The delta filter: a component whose counters merely counted up is not a frame, and
    /// one whose state or sentence changed is.
    #[test]
    fn only_a_visible_change_is_a_frame() {
        let a = view("running", "Running.", 7, 0);
        let mut b = a.clone();
        assert!(same(&a, &b));
        b.counters.lagged_total += 1;
        assert!(
            same(&a, &b),
            "a counter the UI does not render is not a change"
        );
        b.sentence = "Running, but forecasts aren't available.".to_string();
        assert!(!same(&a, &b));
    }

    fn view(state: &str, sentence: &str, generation: u64, failed: u64) -> ComponentView {
        ComponentView {
            id: "wu:backyard".to_string(),
            display: crate::display::Display {
                name: "Weather Underground".to_string(),
                qualifier: Some("Backyard".to_string()),
                section: crate::display::Section::Share,
                href: "/share/weather-underground".to_string(),
            },
            state: state.to_string(),
            badge: crate::display::Badge {
                label: "Running".to_string(),
                tone: "success".to_string(),
            },
            sentence: sentence.to_string(),
            waiting_on: Vec::new(),
            degraded: Vec::new(),
            since: chrono::Utc::now(),
            retry_at: None,
            applied_generation: generation,
            pending: false,
            schedule: crate::runtime::ScheduleView {
                kind: "every".to_string(),
                sentence: None,
                period_s: Some(60),
                last_run: None,
                last_outcome: None,
                next_run: None,
            },
            last_error: None,
            counters: crate::runtime::CountersView {
                runs_total: 1,
                failed_total: failed,
                skipped_total: 0,
                deadline_exceeded_total: 0,
                misfired_total: 0,
                restart_total: 0,
                lagged_total: 0,
                stop_timeout_total: 0,
            },
            notes: Vec::new(),
        }
    }
}
