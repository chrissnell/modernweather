//! [`TimescaleStore`] — the one handle `db:postgres` publishes, over one `PgPool`.
//!
//! `mw_component::store`'s module documentation has the whole of why there is one handle
//! and not two; this is the twelve lines that implement it. The two halves stay two types
//! with two files — [`crate::TimescaleWriter`] owns the `INSERT` and **D-56**'s
//! projections, [`crate::TimescaleReader`] owns the three reads over the aggregate
//! hierarchy — and this delegates. Nothing is added: a method here would be a method on
//! neither half's query set, which is the merge **D-30** is against.
//!
//! What a consumer does with the handle is **upcast it to the half it declared**:
//! `writer:timescale` keeps an `Arc<dyn ReadingSink>` and `window:rollup` keeps an
//! `Arc<dyn ReadingStore>`, so after `build` neither can reach the other's methods even
//! though both resolved `Capability::ReadingStore`. That is the type-level form of W2a
//! §4.1b's `H W` against `H`.

use std::sync::Arc;

use mw_component::{
    BoxFuture, Bucket, BucketQuery, Bucketed, ReadingSink, ReadingStore, SeriesQuery, StoreError,
    Stored, TotalsQuery,
};
use mw_types::Envelope;
use sqlx::PgPool;

use crate::reader::TimescaleReader;
use crate::writer::{TimescaleWriter, UnplaceableDimensions};

/// Both halves of the weather pool.
#[derive(Debug)]
pub struct TimescaleStore {
    writer: TimescaleWriter,
    reader: TimescaleReader,
}

impl TimescaleStore {
    /// Build both halves over one pool and the registry the projections are copied from.
    ///
    /// Fails only where [`TimescaleWriter::new`] does — the dimension-set assertion, which
    /// is a disagreement between the writer and the registry rather than anything about the
    /// database. The reader is infallible: everything it needs is generated.
    pub fn new(
        pool: PgPool,
        registry: Arc<dyn mw_component::StationRegistry>,
    ) -> Result<Self, UnplaceableDimensions> {
        Ok(Self {
            writer: TimescaleWriter::new(pool.clone(), registry)?,
            reader: TimescaleReader::new(pool),
        })
    }

    /// How many columns the write half binds, for `db:postgres`'s log line.
    pub fn write_columns(&self) -> usize {
        self.writer.columns()
    }

    /// How many columns one 1-minute bucket carries, for the same log line.
    pub fn read_columns(&self) -> usize {
        self.reader.bucket_columns()
    }
}

impl ReadingSink for TimescaleStore {
    fn store<'a>(&'a self, batch: &'a [Envelope]) -> BoxFuture<'a, Result<Stored, StoreError>> {
        self.writer.store(batch)
    }
}

impl ReadingStore for TimescaleStore {
    fn buckets<'a>(&'a self, q: BucketQuery) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>> {
        self.reader.buckets(q)
    }

    fn totals<'a>(&'a self, q: TotalsQuery) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
        self.reader.totals(q)
    }

    fn series<'a>(&'a self, q: SeriesQuery) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>> {
        self.reader.series(q)
    }
}
