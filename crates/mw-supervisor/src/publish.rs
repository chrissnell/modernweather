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
    /// Not what the UI asks — W5 compares only the components a write *affects* — but it
    /// is what the concurrency test asserts, because the writer builds every table so that
    /// the invariant holds, and a torn read is then visible as a violation.
    pub fn is_fully_applied(&self) -> bool {
        self.components
            .iter()
            .all(|c| c.applied_generation >= self.generation)
    }
}

/// A single-cell publication: one atomic store, one atomic load.
#[derive(Debug)]
pub struct Publication<T> {
    cell: Cell<T>,
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
        Self { cell }
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

        generation.store(Arc::new(Generation::new(2)));
        let settled = StateTable {
            generation: *generation.load(),
            components: (*rows.load()).clone(),
        };
        assert!(settled.is_fully_applied());
    }
}
