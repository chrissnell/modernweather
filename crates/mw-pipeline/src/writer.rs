//! `writer:timescale` — the spool cursor that puts readings in `observations`.
//!
//! # Where the three pieces live, and why none of them is in the same crate as another
//!
//! | Piece | Crate | Why there |
//! |---|---|---|
//! | `ReadingSink`, the handle trait | `mw-component` | §1.3 rule 1: a handle trait lives with the model |
//! | `TimescaleWriter`, the `INSERT` | `mw-store` | the only crate that may name `sqlx` |
//! | `Sink`, `SpooledSink`, this spec | `mw-pipeline` | the `OnReading` path and the cursor |
//!
//! `mw-store` and `mw-pipeline` are both in the `service` layer, so `ci/layers.toml`
//! rejects a dependency either way between them — which is what makes the handle trait
//! load-bearing rather than ceremony. W3 §2a.1 sketches `TimescaleSink` as a struct
//! implementing `mw_pipeline::Sink` directly; that cannot be built, because the `impl`
//! would have to be in one of the two crates and both are forbidden the other. The trait
//! in the layer below is the shape the layer rule leaves, and the same seam
//! `mw_station::plan` records for `Registrar`.
//!
//! # `Durable`, and what that actually buys
//!
//! `Trigger::OnReading { All, Durable, 5 s }` (W2a §4.1). `Durable` means a spool cursor
//! named `writer:timescale`, so:
//!
//! - a batch the writer failed is **not acknowledged**, and replays — which is only safe
//!   because the statement is `ON CONFLICT (station_id, time) DO UPDATE`;
//! - the cursor's position survives a restart, so a database that was down for an hour is
//!   an hour of spool depth rather than an hour of missing history;
//! - nothing upstream notices. `admit` never waits on this component (**D-30a**), so a
//!   stalled writer shows up as `mw_spool_lag_envelopes{cursor="writer:timescale"}` rising
//!   and not as a station dropping frames.
//!
//! The 5 s deadline is W3 §2.4's: "one round trip for 256 rows against a local TimescaleDB
//! is single-digit milliseconds; 5 s is three orders of magnitude of slack, so a deadline
//! miss means the server is hung rather than slow."

use std::sync::Arc;
use std::time::Duration;

use mw_component::{
    BoxFuture, Builder, Capability, ComponentId, ComponentIdError, ComponentSpec, Cx, Need,
    ReadingFilter, ReadingPolicy, ReadingSink, Trigger,
};
use mw_types::Envelope;

use crate::metrics as m;
use crate::sink::{Acked, Sink, SpooledSink};

/// The component's id, which is also the name of its cursor file in the spool.
pub const ID: &str = "writer:timescale";

/// The per-batch deadline (W3 §2.4).
pub const DEADLINE: Duration = Duration::from_secs(5);

/// How long a stop waits for an in-flight transaction (W3 §3.6).
///
/// The default is 10 s and §3.6 names this component as one of the two that need more:
/// aborting mid-`COMMIT` is safe — the transaction rolls back and the cursor has not
/// advanced — but it costs the batch a replay, and the 15 s is what makes that rare rather
/// than routine on every `systemctl restart`.
pub const GRACE: Duration = Duration::from_secs(15);

/// Declare `writer:timescale`.
///
/// Emitted only by a node that has a weather database configured: a node with no
/// `weather_database` row has no `db:postgres`, so this component's hard need would have no
/// provider and `plan()` would refuse the whole configuration (**D-44**, W7 §8.2's
/// satellite). The registrar is where that choice is made, because the registrar is what
/// reads the row.
pub fn spec() -> Result<ComponentSpec, ComponentIdError> {
    let id = ComponentId::new(ID)?;
    Ok(ComponentSpec::new(
        id.clone(),
        Trigger::OnReading {
            // Everything in the spool. A collector stores its satellites' readings as well
            // as its own, which is what makes `NotFrom(peer)` the *uplink's* filter and
            // `All` this one's (**D-47**).
            filter: ReadingFilter::All,
            policy: ReadingPolicy::Durable,
            deadline: DEADLINE,
        },
        Builder::on_reading(move |cx| {
            let id = id.clone();
            async move {
                // Resolved **once**, in `build` (W2a Decision 21). After this line the
                // body has no way to ask whether the database is still there, so an outage
                // reaches it as a lifecycle state — `Waiting { on: ReadingStore }` — and
                // not as an error branch at a call site.
                let store = cx.hard::<Arc<dyn ReadingSink>>()?;
                Ok(SpooledSink::new(id, StoreSink { store }))
            }
        }),
    )
    .requires([Need::hard_write(Capability::ReadingStore)])
    .grace(GRACE))
}

/// The [`Sink`] over a [`ReadingSink`] handle.
///
/// Thin on purpose: everything about *what* is written is `mw-store`'s, and everything
/// about the cursor and the partial ack is [`SpooledSink`]'s. What is left here is the
/// translation between the two error vocabularies and the two counters.
struct StoreSink {
    store: Arc<dyn ReadingSink>,
}

impl Sink for StoreSink {
    type Error = mw_component::StoreError;

    fn deliver<'a>(
        &'a mut self,
        batch: &'a [Envelope],
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<Acked, Self::Error>> {
        Box::pin(async move {
            let stored = self.store.store(batch).await?;
            m::rows_written(stored.rows);
            if stored.rows < stored.stored as u64 {
                // Two envelopes shared a `(station_id, time)`. Expected for a frame
                // carrying several observations with no station timestamp, and worth a
                // number: a station whose every batch collapses to one row is storing one
                // reading per batch and nothing else would say so.
                m::rows_collapsed(stored.stored as u64 - stored.rows);
            }
            Ok(Acked(stored.stored))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_is_a_durable_on_reading_component_over_every_station() {
        let spec = spec().expect("`writer:timescale` is a valid component id");
        assert_eq!(spec.id.as_str(), ID);
        match &spec.trigger {
            Trigger::OnReading {
                filter,
                policy,
                deadline,
            } => {
                assert_eq!(*filter, ReadingFilter::All);
                assert_eq!(*policy, ReadingPolicy::Durable);
                assert_eq!(*deadline, DEADLINE);
            }
            other => panic!("expected OnReading, got {other:?}"),
        }
        assert_eq!(
            spec.requires,
            vec![Need::hard_write(Capability::ReadingStore)],
            "W2a §4.1b writes it as ReadingStore H W"
        );
        assert!(spec.provides.is_empty(), "the writer provides nothing");
        assert!(
            spec.grace > mw_component::DEFAULT_GRACE,
            "§3.6 names this component as one of the two needing more than the default"
        );
    }

    /// The cursor's file name comes from the component id, so the id is what a restart
    /// resumes from (W7 §3: "the cursor identity is the component id"). A rename would
    /// silently start a new cursor at the tail and skip whatever the old one had not
    /// acknowledged.
    #[test]
    fn the_id_is_the_cursor_name() {
        let id = ComponentId::new(ID).expect("valid");
        assert_eq!(id.as_str(), "writer:timescale");
    }

    /// The adapter's own behaviour, which needs a **real `tokio` runtime** and therefore
    /// cannot run under `--cfg loom`.
    ///
    /// That flag applies to every crate in the graph, and `tokio` gates its whole `net`
    /// module behind `#![cfg(not(loom))]` — so `runtime::Builder::build` asserts the I/O
    /// driver is disabled and a `#[tokio::test]` panics before its body runs. The same
    /// reason `tests/spool.rs`, `tests/fanout.rs` and `tests/ingress.rs` each carry
    /// `#![cfg(not(loom))]` at the top; `.github/workflows/pr.yml`'s loom job explains the
    /// flag's reach in full.
    ///
    /// Nothing here is a concurrency model, so nothing is lost by excluding it: this
    /// module asserts the arithmetic between `Stored` and `Acked`, which is sequential.
    #[cfg(not(loom))]
    mod adapter {
        use super::*;

        /// A `ReadingSink` that answers whatever the test wants, so the adapter's arithmetic
        /// is assertable without a database.
        #[derive(Debug)]
        struct FakeStore(Result<mw_component::Stored, &'static str>);

        impl ReadingSink for FakeStore {
            fn store<'a>(
                &'a self,
                batch: &'a [Envelope],
            ) -> BoxFuture<'a, Result<mw_component::Stored, mw_component::StoreError>> {
                let answer = match &self.0 {
                    Ok(stored) => Ok(mw_component::Stored {
                        // `stored` is a prefix length, so it only means anything against the
                        // batch it was given.
                        stored: stored.stored.min(batch.len()),
                        rows: stored.rows,
                    }),
                    Err(why) => Err(mw_component::StoreError::unavailable(
                        std::io::Error::other(*why),
                    )),
                };
                Box::pin(async move { answer })
            }
        }

        fn cx() -> Cx {
            let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
            Cx::new(
                ComponentId::new(ID).expect("valid"),
                tokio_util::sync::CancellationToken::new(),
                mw_component::Generation::ZERO,
                rx,
            )
        }

        fn envelope(seq: u64) -> Envelope {
            let at =
                chrono::DateTime::from_timestamp(1_767_225_600 + seq as i64, 0).expect("in range");
            Envelope {
                station: mw_types::StationKey::new("backyard"),
                observed_at: at,
                received_at: at,
                provenance: mw_types::Provenance::admitted(
                    mw_types::OriginId::new([4; 16]),
                    mw_types::Seq::new(seq),
                ),
                reading: std::sync::Arc::new(mw_types::Reading::default()),
            }
        }

        /// **The whole batch is acknowledged, not the row count.** Two envelopes that
        /// collapsed to one row are both durably handled, and a cursor that stopped at the row
        /// count would replay the second one forever.
        #[tokio::test]
        async fn the_ack_is_the_envelope_count_and_not_the_row_count() {
            let batch: Vec<Envelope> = (0..4).map(envelope).collect();
            let mut sink = StoreSink {
                store: Arc::new(FakeStore(Ok(mw_component::Stored { stored: 4, rows: 1 }))),
            };
            let Acked(n) = sink
                .deliver(&batch, &cx())
                .await
                .expect("the fake store succeeds");
            assert_eq!(n, 4, "four envelopes handled, even though they are one row");
        }

        /// A store failure becomes a `ComponentError` naming the action, which is what the
        /// supervisor turns into `Failed` + `Backoff` with the cursor where it was (§2.3).
        #[tokio::test]
        async fn a_store_failure_names_the_action_and_keeps_its_chain() {
            let batch = vec![envelope(1)];
            let mut sink = StoreSink {
                store: Arc::new(FakeStore(Err("connection reset by peer"))),
            };
            let e = sink
                .deliver(&batch, &cx())
                .await
                .expect_err("the fake store fails");
            let chain = mw_types::DisplayChain(&e).to_string();
            assert!(chain.contains("could not be reached"), "{chain}");
            assert!(chain.contains("connection reset by peer"), "{chain}");
            let as_component: mw_component::ComponentError = e.into();
            assert_eq!(
                mw_types::DisplayChain(&as_component).to_string(),
                "store a batch of readings: the weather database could not be reached: \
                 connection reset by peer"
            );
        }

        /// An empty batch is a no-op rather than a statement. The fan-out does not deliver
        /// one, but a filter applied downstream can produce one (`ReadingBatch::is_empty`).
        #[tokio::test]
        async fn an_empty_batch_acknowledges_nothing_and_asks_for_nothing() {
            let mut sink = StoreSink {
                store: Arc::new(FakeStore(Ok(mw_component::Stored::default()))),
            };
            let Acked(n) = sink.deliver(&[], &cx()).await.expect("no-op");
            assert_eq!(n, 0);
        }
    }
}
