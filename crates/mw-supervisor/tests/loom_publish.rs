//! The `loom` half of W3 **D-85**: exhaustive interleavings of the one published snapshot.
//!
//! Compiled only under `--cfg loom`, which is what the `loom interleavings` job in
//! `.github/workflows/pr.yml` sets. That job has existed since W8 and has had nothing to
//! run; this is the first thing it runs.
//!
//! # What loom checks, and what it does not
//!
//! `arc-swap` is **not** loom-instrumented, so a model built over the real cell would
//! explore exactly one interleaving and report success — which is the `G-1` shape this
//! work item exists to close, in miniature. So under `--cfg loom`
//! [`mw_supervisor::Publication`] is a `loom::sync::RwLock<Arc<T>>` instead, and what loom
//! checks is the **protocol**: that no interleaving of a writer's commits against
//! concurrent readers lets a reader observe a state table whose rows disagree with the
//! generation they came from. A publisher that committed the generation and the rows as
//! two stores fails that; the one here cannot, because one commit is one store.
//!
//! The implementation of the cell itself is checked by the other job: ThreadSanitizer runs
//! the real `ArcSwap` under 64 real readers in `concurrency.rs`. The two are complementary
//! — loom is exhaustive over a model, TSan observes a real run — which is W3 §8.4's own
//! reason for having both.
//!
//! The deterministic demonstration that this property is **not** free lives beside the
//! cell, in `publish.rs`'s `a_split_publisher_is_observably_inconsistent`: a two-store
//! publisher is inconsistent between its stores with no concurrency at all.
//!
//! The sentinel is [`mw_supervisor::StateTable::is_coherent`] — rows agreeing *exactly*
//! with the generation they were published with — and not the production predicate
//! `is_fully_applied`, whose `>=` is blind to a stale generation beside fresh rows. Which
//! of the two cells a split publisher writes first decides which direction it tears in,
//! so a one-directional sentinel catches a split publisher in only one of the two
//! orderings.

#![cfg(loom)]

use std::sync::Arc;

use chrono::Utc;
use mw_component::{ComponentId, ComponentStatus, Counters, Generation, ScheduleState, State};
use mw_supervisor::publish::{Publication, StateTable};

/// Two readers rather than 64: loom is exhaustive, so the question is whether *any*
/// interleaving violates the property, and two concurrent readers against one writer
/// already generate every ordering of a load against a store. Sixty-four would multiply
/// the state space for no new shape.
const READERS: usize = 2;

/// Two commits. Each additional commit multiplies the interleavings; two is enough for a
/// reader to be able to straddle one.
const COMMITS: u64 = 2;

fn status(id: &str, applied: u64) -> ComponentStatus {
    ComponentStatus {
        id: ComponentId::new(id).expect("a literal id"),
        trigger: "every",
        state: State::Running { degraded: vec![] },
        since: Utc::now(),
        applied_generation: Generation::new(applied),
        schedule: ScheduleState::default(),
        counters: Counters::default(),
        last_error: None,
    }
}

fn commit(generation: u64) -> Arc<StateTable> {
    Arc::new(StateTable {
        generation: Generation::new(generation),
        components: vec![
            status("db:postgres", generation),
            status("wu:backyard", generation),
        ],
    })
}

#[test]
fn no_interleaving_of_a_commit_and_a_read_tears() {
    loom::model(|| {
        let published = Arc::new(Publication::new(commit(0)));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let published = Arc::clone(&published);
                loom::thread::spawn(move || {
                    let first = published.load();
                    assert!(
                        first.is_coherent(),
                        "a reader observed a mixture of two commits"
                    );
                    let second = published.load();
                    assert!(second.is_coherent());
                    assert!(
                        second.generation >= first.generation,
                        "a reader's successive loads went backwards: {} after {}",
                        second.generation,
                        first.generation
                    );
                })
            })
            .collect();

        let writer = {
            let published = Arc::clone(&published);
            loom::thread::spawn(move || {
                for generation in 1..=COMMITS {
                    published.store(commit(generation));
                }
            })
        };

        for reader in readers {
            reader.join().expect("a reader panicked");
        }
        writer.join().expect("the writer panicked");

        let settled = published.load();
        assert_eq!(settled.generation, Generation::new(COMMITS));
        assert!(settled.is_coherent());
    });
}
