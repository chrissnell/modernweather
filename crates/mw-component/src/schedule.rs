//! The occurrence key, and the one-table store the supervisor keeps it in.
//!
//! **Why a store trait here.** W3 §3.1 makes the SQLite pool a root resource owned by
//! `main`, which hands a `ScheduleStore` over `component_schedule` **alone** to the
//! supervisor and the full handle to `config:sqlite`. The supervisor never sees
//! `ConfigStore` and `config:sqlite` never sees `component_schedule`; routing either need
//! through the other would be the `AppReloader` cycle in a new coat. The trait lives in
//! this crate because `mw-store` implements it and the two crates are in the same layer,
//! so the only place they can meet is below them both.

use chrono::{DateTime, NaiveDate, Utc};

use crate::body::BoxFuture;
use crate::error::Outcome;
use crate::id::ComponentId;

/// Which occurrence of a schedule this is.
///
/// Keyed on **local date and slot** rather than on an instant, and that is what makes
/// fall-back's second pass recognisable as already completed (W2a Decision 13): an
/// ambiguous local time happens twice, and firing a daily rollover twice would
/// double-count it. It is also what makes `Misfire::RunLatest` idempotent and "did this
/// occurrence already run" answerable at all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OccurrenceKey {
    /// Whose occurrence.
    pub component: ComponentId,
    /// The date in the component's resolved zone.
    pub local_date: NaiveDate,
    /// Which slot of that day, as a position in the declared times.
    pub slot: u32,
}

impl OccurrenceKey {
    /// An occurrence key.
    pub fn new(component: ComponentId, local_date: NaiveDate, slot: u32) -> Self {
        Self {
            component,
            local_date,
            slot,
        }
    }
}

/// The most recent occurrence of one component — one row, not a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceRecord {
    /// Which occurrence.
    pub key: OccurrenceKey,
    /// When it completed.
    pub completed_at: DateTime<Utc>,
    /// How it went.
    pub outcome: Outcome,
}

/// The supervisor's own durable state: the latest `Calendar` occurrence per component.
///
/// Narrow on purpose — one table, three operations. A store that could read configuration
/// would be a second path to the configuration database, and the separation between this
/// and `ConfigStore` is the whole reason the pool is owned by `main`.
pub trait ScheduleStore: Send + Sync + 'static {
    /// The latest recorded occurrence for one component, if any.
    fn latest(
        &self,
        component: &ComponentId,
    ) -> BoxFuture<'_, Result<Option<OccurrenceRecord>, ScheduleStoreError>>;

    /// Every recorded occurrence, which the supervisor loads once at startup so that
    /// misfire handling does not need a query per component.
    fn all(&self) -> BoxFuture<'_, Result<Vec<OccurrenceRecord>, ScheduleStoreError>>;

    /// Record an occurrence, replacing this component's previous row.
    fn record(&self, record: &OccurrenceRecord) -> BoxFuture<'_, Result<(), ScheduleStoreError>>;
}

/// Why the schedule store could not answer.
///
/// A failure here must not stop the supervisor: the worst consequence of a lost occurrence
/// row is that a missed `Calendar` occurrence is made up twice or not at all, and every
/// `Calendar` user in this system is idempotent (W2a Decision 14). So the supervisor logs
/// and carries on rather than treating this as fatal.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleStoreError {
    /// The underlying store failed.
    #[error("the component schedule could not be read or written")]
    Backend {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl ScheduleStoreError {
    /// Wrap a store failure.
    pub fn backend<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        ScheduleStoreError::Backend {
            source: Box::new(source),
        }
    }
}
