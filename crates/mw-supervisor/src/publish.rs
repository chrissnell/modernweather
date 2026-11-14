//! The one published snapshot of supervisor state, and the cell it is published through.
//!
//! **The property.** A reader takes one atomic load and holds an `Arc` for as long as it
//! needs, so it sees one generation or the next and never a mixture. This is the direct
//! replacement for the measured race the Go version dies on: `ReloadWebsiteConfig`
//! replaces the website list, the device list, the device-name map, the devices-by-website
//! map and the TLS configuration wholesale, as plain unsynchronised fields, while
//! in-flight requests read them — and a concurrent map read and write in Go is an
//! unrecoverable fatal error, so saving a website while serving traffic can kill the
//! process (**D-28**).
//!
//! **What makes it hold is that one commit is one store.** Everything a reader must see
//! consistently lives inside one [`StateTable`], which is immutable once published. If the
//! generation and the component rows were two cells, a reader could observe a new table
//! with an old generation — and W5 §2.2's "is this change live" comparison would then be
//! answered from a mixture of two commits. That is the property the concurrency tests
//! check, and it is a property of the *protocol* rather than of `arc-swap`.
//!
//! # `loom` and `arc-swap`
//!
//! Under `--cfg loom` the cell is a `loom::sync::RwLock<Arc<T>>` rather than an
//! `ArcSwap<T>`, and that substitution is deliberate rather than a convenience:
//! `arc-swap` is not loom-instrumented, so loom cannot see inside it and a model built
//! over it would explore exactly one interleaving while reporting success. What loom
//! checks here is the **protocol** — that no interleaving of one writer's commits against
//! concurrent readers yields a reader that sees an inconsistent table — and a split
//! publisher fails it. What checks `arc-swap` itself is the ThreadSanitizer job, which
//! runs the real cell under real threads. The two are complementary, which is also W3
//! §8.4's reason for having both.

use std::sync::Arc;

use mw_component::{ComponentId, ComponentStatus, Generation};

// One without the other is always a mistake, and the error it produces otherwise is four
// "cannot infer type" messages about a type parameter nobody wrote.
#[cfg(all(loom, not(feature = "loom")))]
compile_error!(
    "`--cfg loom` needs `--features mw-supervisor/loom`: the cell substitution in this \
     module links loom from the library, which a dev-dependency cannot do. The `loom` job \
     in .github/workflows/pr.yml passes both."
);

/// Everything a reader of supervisor state must see consistently.
///
/// Published as one immutable `Arc`. The `generation` is the newest **committed**
/// configuration; each component's own `applied_generation` is the generation the running
/// instance was built from, so a change is live exactly when every affected component's
/// `applied_generation >= generation` (W5 §2.2).
#[derive(Debug, Clone, Default)]
pub struct StateTable {
    /// The newest committed configuration generation.
    pub generation: Generation,
    /// Every planned component's status, in plan order.
    pub components: Vec<ComponentStatus>,
}

impl StateTable {
    /// One component's status.
    pub fn get(&self, id: &ComponentId) -> Option<&ComponentStatus> {
        self.components.iter().find(|c| c.id == *id)
    }

    /// Whether every component's `applied_generation` has caught up with `generation`.
    ///
    /// The production reading of W5 §2.2, and deliberately **one-directional**: a
    /// component can legitimately be *behind* the committed generation (it has not been
    /// restarted yet) and can legitimately be *ahead* of it only in the instant between
    /// a plan being applied and the next publication, which never leaves this process.
    ///
    /// This is **not** the torn-read sentinel. `>=` cannot see a reader that observed a
    /// stale `generation` beside fresh rows, which is one of the two ways a split
    /// publication tears — so a test that used this would miss half of what it claims to
    /// check. [`StateTable::is_coherent`] is the sentinel.
    pub fn is_fully_applied(&self) -> bool {
        self.components
            .iter()
            .all(|c| c.applied_generation >= self.generation)
    }

    /// Whether this table's rows agree **exactly** with the generation they were published
    /// with — the torn-read sentinel.
    ///
    /// A property of how the writer builds a table rather than a field the production path
    /// reads: the concurrency tests commit every row at the table's own generation, so any
    /// disagreement in *either* direction means the reader saw a mixture of two commits.
    /// That is what makes the assertion "no torn read" rather than "no crash" — `ArcSwap`
    /// would not crash even if the design were wrong.
    pub fn is_coherent(&self) -> bool {
        self.components
            .iter()
            .all(|c| c.applied_generation == self.generation)
    }
}

/// A single-cell publication: one atomic store, one atomic load.
#[derive(Debug)]
pub struct Publication<T> {
    cell: Cell<T>,
    /// Bumped by every [`Publication::store`], so a reader can wait for the next one.
    ///
    /// **This is what lets the management UI push component state without a clock.**
    /// Without it an observer has to poll, which would mean a second timer in a process
    /// whose entire §3.3 argument is that there is exactly one -- and polling the state
    /// table is strictly worse than being told, because the thing a reader wants to know
    /// is precisely "has it changed".
    ///
    /// A **version counter** rather than a `Notify`, and the difference is a race rather
    /// than a preference: a `Notify` wakes only the waiters registered at the moment of
    /// the store, so a reader that is between "read the table" and "wait for the next
    /// change" misses one. A `watch` receiver carries its own seen-marker, so a change
    /// that lands in that window is still pending when the reader gets there.
    ///
    /// The counter and not the table: the value is already shared through the `Arc` the
    /// cell hands out, and a second copy of it in a channel would be a second answer to
    /// what the current state is. A woken reader re-reads through [`Publication::load`].
    ///
    /// Absent under `--cfg loom`, which models the cell's memory ordering and links a
    /// `loom` runtime rather than tokio's.
    #[cfg(not(loom))]
    version: tokio::sync::watch::Sender<u64>,
}

#[cfg(not(loom))]
type Cell<T> = arc_swap::ArcSwap<T>;
#[cfg(loom)]
type Cell<T> = loom::sync::RwLock<Arc<T>>;

impl<T> Publication<T> {
    /// Publish an initial value.
    pub fn new(value: Arc<T>) -> Self {
        #[cfg(not(loom))]
        let cell = arc_swap::ArcSwap::new(value);
        #[cfg(loom)]
        let cell = loom::sync::RwLock::new(value);
        Self {
            cell,
            #[cfg(not(loom))]
            version: tokio::sync::watch::Sender::new(0),
        }
    }

    /// A receiver that fires on every [`Publication::store`].
    ///
    /// Held across iterations by its one consumer -- the management API's live stream --
    /// which is what makes the wait race-free; a receiver created afresh for each wait
    /// would mark the current version as seen and miss the change it was created to
    /// notice.
    #[cfg(not(loom))]
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// Read the current value. One load; the `Arc` keeps it alive for the caller.
    pub fn load(&self) -> Arc<T> {
        #[cfg(not(loom))]
        {
            self.cell.load_full()
        }
        #[cfg(loom)]
        {
            Arc::clone(&self.cell.read().expect("publication lock poisoned"))
        }
    }

    /// Replace the current value. **One commit is one call**; there is deliberately no
    /// method that updates part of it.
    pub fn store(&self, value: Arc<T>) {
        #[cfg(not(loom))]
        {
            self.cell.store(value);
            // After the store, so a reader woken by this always loads the value that woke
            // it rather than the one before it.
            self.version.send_modify(|v| *v = v.wrapping_add(1));
        }
        #[cfg(loom)]
        {
            *self.cell.write().expect("publication lock poisoned") = value;
        }
    }
}

// Not under `--cfg loom`: these drive the cell directly, and a loom primitive touched
// outside a `loom::model` panics by design. The loom model lives in
// `tests/loom_publish.rs`.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use chrono::Utc;
    use mw_component::{Counters, ScheduleState, State};

    fn status(id: &str, applied: u64) -> ComponentStatus {
        ComponentStatus {
            id: ComponentId::new(id).unwrap(),
            trigger: "every",
            state: State::Running { degraded: vec![] },
            since: Utc::now(),
            applied_generation: Generation::new(applied),
            schedule: ScheduleState::default(),
            counters: Counters::default(),
            last_error: None,
        }
    }

    fn table(generation: u64) -> Arc<StateTable> {
        Arc::new(StateTable {
            generation: Generation::new(generation),
            components: vec![
                status("wu:backyard", generation),
                status("db:postgres", generation),
            ],
        })
    }

    #[test]
    fn a_published_table_is_internally_consistent() {
        let published = Publication::new(table(1));
        assert!(published.load().is_fully_applied());
        assert!(published.load().is_coherent());
        published.store(table(2));
        let seen = published.load();
        assert_eq!(seen.generation, Generation::new(2));
        assert!(seen.is_fully_applied());
        assert!(seen
            .get(&ComponentId::new("wu:backyard").unwrap())
            .is_some());
    }

    /// The property is not free, and this is the evidence: a publisher that commits the
    /// generation and the rows as **two** stores exposes an inconsistent table between
    /// them, with no concurrency at all.
    ///
    /// Keeping this test beside the real cell is what makes the concurrency tests
    /// meaningful rather than decorative — they assert the absence of exactly this, and a
    /// reader has to be able to see that the assertion can fail.
    #[test]
    fn a_split_publisher_is_observably_inconsistent() {
        // Two cells, which is the mistake: the rows and the number that describes them.
        let rows = Publication::new(Arc::new(vec![status("wu:backyard", 1)]));
        let generation = Publication::new(Arc::new(Generation::new(1)));

        // Commit generation 2, one cell at a time, and look between the two stores.
        rows.store(Arc::new(vec![status("wu:backyard", 2)]));
        let torn = StateTable {
            generation: *generation.load(),
            components: (*rows.load()).clone(),
        };
        assert_eq!(torn.generation, Generation::new(1));
        assert_eq!(torn.components[0].applied_generation, Generation::new(2));
        assert!(
            torn.components[0].applied_generation > torn.generation,
            "a component cannot have applied a generation that has not been committed"
        );
        // The sentinel the concurrency tests use catches it in this direction, which is
        // the one `is_fully_applied`'s `>=` is blind to.
        assert!(!torn.is_coherent());
        assert!(
            torn.is_fully_applied(),
            "which is exactly why the production predicate is not the sentinel"
        );

        generation.store(Arc::new(Generation::new(2)));
        let settled = StateTable {
            generation: *generation.load(),
            components: (*rows.load()).clone(),
        };
        assert!(settled.is_fully_applied());
    }
}
