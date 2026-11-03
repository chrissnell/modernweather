//! The supervisor's own durable state: `component_schedule`, and nothing else.
//!
//! **Why this is a separate handle rather than part of `ConfigStore`.** W3 §3.1 (**D-41**)
//! makes the SQLite pool a root resource owned by `main`, which hands a [`ScheduleStore`]
//! over `component_schedule` **alone** to the supervisor and the full handle to
//! `config:sqlite`. The supervisor needs `Misfire::RunLatest` to survive a restart, so
//! something has to persist the latest occurrence per component -- and routing that need
//! through `ConfigStore` would mean the supervisor depending on a component it supervises,
//! which is the `AppReloader` cycle in a new coat.
//!
//! The narrowness is therefore the point, and it is a property of this type rather than a
//! convention: every statement below names `component_schedule`, so "the supervisor never
//! sees `ConfigStore`" is true by construction. There is deliberately no foreign key from
//! that table to `stations` either -- the migration says so where the table is defined.
//!
//! A failure here is **not fatal**: the worst consequence of a lost occurrence row is that
//! an idempotent `Calendar` occurrence is made up twice or not at all (W2a Decision 14),
//! which is why [`mw_component::ScheduleStoreError`] classifies as `Retryable` and the
//! supervisor logs and carries on.

use chrono::{DateTime, NaiveDate, Utc};
use mw_component::{
    BoxFuture, ComponentId, OccurrenceKey, OccurrenceRecord, Outcome, ScheduleStore,
    ScheduleStoreError,
};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// [`ScheduleStore`] over `component_schedule`.
#[derive(Debug, Clone)]
pub struct SqliteScheduleStore {
    pool: SqlitePool,
}

impl SqliteScheduleStore {
    /// Narrow the configuration pool to this one table.
    pub fn new(pool: SqlitePool) -> Self {
        SqliteScheduleStore { pool }
    }

    async fn read(&self, component: Option<&ComponentId>) -> Result<Vec<OccurrenceRecord>, Error> {
        let sql = "SELECT component_id, local_date, slot_index, completed_at, outcome \
                   FROM component_schedule";
        let rows = match component {
            Some(id) => {
                sqlx::query(&format!("{sql} WHERE component_id = ?1"))
                    .bind(id.as_str())
                    .fetch_all(&self.pool)
                    .await
            }
            None => sqlx::query(sql).fetch_all(&self.pool).await,
        }
        .map_err(Error::Query)?;
        rows.iter().map(row_to_record).collect()
    }

    async fn write(&self, record: &OccurrenceRecord) -> Result<(), Error> {
        // `%Y` emits `+12345` and `-0001` outside years 1-9999, which the column's
        // `CHECK (local_date GLOB '[0-9][0-9][0-9][0-9]-...')` rejects -- so the INSERT
        // would fail as a bare "querying component_schedule failed". Unreachable from a
        // `Calendar` occurrence, and this module's error promises to name the column.
        let local_date = record.key.local_date.format("%Y-%m-%d").to_string();
        if !local_date.is_char_boundary(4) || local_date.len() != 10 {
            return Err(Error::Column {
                component: record.key.component.as_str().to_owned(),
                column: "local_date",
                value: local_date,
            });
        }
        // One row per component, not a history: a second occurrence replaces the first.
        sqlx::query(
            "INSERT INTO component_schedule \
                 (component_id, local_date, slot_index, completed_at, outcome) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (component_id) DO UPDATE SET \
                 local_date = excluded.local_date, \
                 slot_index = excluded.slot_index, \
                 completed_at = excluded.completed_at, \
                 outcome = excluded.outcome",
        )
        .bind(record.key.component.as_str())
        .bind(local_date)
        .bind(i64::from(record.key.slot))
        // `completed_at INTEGER` is whole seconds, so a sub-second instant is truncated
        // on the way in and a read-back is not bit-identical to what was written. That is
        // the right granularity for "when did this occurrence finish" and it is stated
        // because the round-trip test's fixture is a whole second, which would otherwise
        // hide it.
        .bind(record.completed_at.timestamp())
        .bind(record.outcome.as_str())
        .execute(&self.pool)
        .await
        .map_err(Error::Query)?;
        Ok(())
    }
}

impl ScheduleStore for SqliteScheduleStore {
    fn latest(
        &self,
        component: &ComponentId,
    ) -> BoxFuture<'_, Result<Option<OccurrenceRecord>, ScheduleStoreError>> {
        let component = component.clone();
        Box::pin(async move {
            let mut rows = self
                .read(Some(&component))
                .await
                .map_err(ScheduleStoreError::backend)?;
            Ok(rows.pop())
        })
    }

    fn all(&self) -> BoxFuture<'_, Result<Vec<OccurrenceRecord>, ScheduleStoreError>> {
        Box::pin(async move { self.read(None).await.map_err(ScheduleStoreError::backend) })
    }

    fn record(&self, record: &OccurrenceRecord) -> BoxFuture<'_, Result<(), ScheduleStoreError>> {
        let record = record.clone();
        Box::pin(async move {
            self.write(&record)
                .await
                .map_err(ScheduleStoreError::backend)
        })
    }
}

/// Why a `component_schedule` row could not be read or written.
///
/// All three reach the supervisor as `ScheduleStoreError`, which classifies `Retryable`
/// (`mw_component::schedule`). For `Query` that is literal; for the two parse failures it
/// is "the caller's response is the same" rather than "retrying helps" -- the supervisor
/// logs and carries on either way, because the worst consequence is that an idempotent
/// `Calendar` occurrence is made up twice or not at all (W2a Decision 14). Returning
/// `Fatal` would invite a future caller to stop the daemon over one unreadable row.
///
/// Three variants rather than one, because a corrupt row and an unreachable database are
/// different problems and the `STRICT` table's `CHECK`s mean the first should be
/// unreachable: `local_date` is checked against `date()` and `outcome` against a five-word
/// list. A row that still fails to parse is this build disagreeing with its own schema,
/// and the message says which column.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("querying component_schedule failed")]
    Query(#[source] sqlx::Error),

    #[error("component_schedule row for `{component}` has an unreadable {column}: `{value}`")]
    Column {
        component: String,
        column: &'static str,
        value: String,
    },

    #[error("component_schedule holds `{component}`, which is not a component id")]
    Component {
        component: String,
        #[source]
        source: mw_component::ComponentIdError,
    },
}

fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<OccurrenceRecord, Error> {
    let id: String = row.get("component_id");
    let component = ComponentId::new(&id).map_err(|source| Error::Component {
        component: id.clone(),
        source,
    })?;
    let date: String = row.get("local_date");
    let local_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(|_| Error::Column {
        component: id.clone(),
        column: "local_date",
        value: date,
    })?;
    let slot: i64 = row.get("slot_index");
    let slot = u32::try_from(slot).map_err(|_| Error::Column {
        component: id.clone(),
        column: "slot_index",
        value: slot.to_string(),
    })?;
    let completed: i64 = row.get("completed_at");
    let completed_at =
        DateTime::<Utc>::from_timestamp(completed, 0).ok_or_else(|| Error::Column {
            component: id.clone(),
            column: "completed_at",
            value: completed.to_string(),
        })?;
    let outcome: String = row.get("outcome");
    let outcome = Outcome::parse(&outcome).ok_or_else(|| Error::Column {
        component: id.clone(),
        column: "outcome",
        value: outcome,
    })?;
    Ok(OccurrenceRecord {
        key: OccurrenceKey::new(component, local_date, slot),
        completed_at,
        outcome,
    })
}
