//! `db:postgres` — the component that owns the weather pool (W2a §4.1, Decision 34).
//!
//! # Why this is a component and not three lines in `main`
//!
//! It is **split from `writer:timescale`**, and W2a §4.1 says why: "pool availability is
//! then independent of write throughput, so what the uploaders wait on is *is the database
//! reachable*, not *is the writer keeping up*". That split is the structural half of the
//! **D-27** fix — in the Go tree, `ValidateTimescaleDBConfig` runs inside four
//! constructors, so PWS, Weather Underground and the snow cache **fail to construct** when
//! the database is down and stay dead until the process restarts.
//!
//! # Readiness is explicit and distinct from `Running` (W2a Decision 22)
//!
//! This component is `Running` from the moment its task starts, and the capability is
//! usable only once the probe has succeeded and the handle is published. A consumer with a
//! hard need sits in `Waiting { on: ReadingStore, provider: db:postgres }` until then,
//! which is a sentence an operator can act on, and goes back to it if the handle is
//! revoked.
//!
//! # The constraint this item was handed, and why it is not redundant
//!
//! **D-62**'s version assertion runs twice. The boot sequence asserts it after the SQLite
//! pool opens and before `plan()` (`modernweather::boot`), and may legitimately **skip**
//! it: a server that is unreachable or slow is logged and started past, because refusing
//! to boot because TimescaleDB is down is the failure **D-27** rejects. That carve-out is
//! only safe while something re-asserts, and this is the item that owns the pool — so the
//! assertion happens again here, on this pool's own first connection, and this time a
//! server that is too old is a `Failed` component rather than a warning.
//!
//! The two halves are the same comparison, [`crate::version::check`], over the same three
//! facts, because the figures live in one place and `ci/sql/assert-timescaledb-version.sql`
//! is read by that module's own tests so the two halves of **D-42** cannot drift.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{
    Builder, Capability, ComponentError, ComponentId, ComponentSpec, Cx, Provision, Publisher,
    ReadingSink, Serve, StationRegistry, Trigger,
};
use mw_types::{Class, Classify, DisplayChain};
use secrecy::ExposeSecret as _;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::version::{self, VersionError};
use crate::writer::TimescaleWriter;

/// The component's id. A constant because three other places name it: the spec, the log
/// lines, and `writer:timescale`'s `Waiting { provider }`.
pub const ID: &str = "db:postgres";

/// How many connections the pool holds.
///
/// Four, against the four things that will hold one concurrently once the read path lands:
/// the writer's batch transaction, `window:rollup`'s minute query, `snapshot:warm`'s seed
/// and one `/span`. Not a round number chosen for comfort — the measured load is ~4
/// stations at 0.5 Hz and the writer's statement is one round trip, so a pool sized for
/// throughput would be sized for a load this system does not have. An idle connection
/// against TimescaleDB is a backend process.
const POOL_SIZE: u32 = 4;

/// How long a connection attempt may take before it is a failure to retry.
///
/// Matched to the boot sequence's `PROBE_DEADLINE` for the same reason it exists: a
/// blackholed address answers only when the kernel gives up, which is minutes, and a
/// component parked in `Starting` for minutes is indistinguishable from one that is hung.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the pool is probed once it is published.
const PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// How many consecutive probe failures revoke the handle.
///
/// W2a §3.4: revoke when the capability is **genuinely unusable** — "a dead connection
/// pool, not a single failed query". Three at ten seconds is half a minute of silence,
/// which is longer than a PostgreSQL restart takes and shorter than the uploaders' own
/// periods, so a server bounce does not move every consumer through `Waiting` and back.
const FAILURES_BEFORE_REVOKE: u32 = 3;

/// Declare `db:postgres`.
///
/// `url` is the configured connection string, which carries a password and is therefore a
/// `SecretString` from the moment it is read (**D-33**). The registry handle travels the
/// same way `mw_station`'s ingress does — through the registrar rather than off the
/// capability bus — because `config:sqlite` has no work item and the binary owns the
/// snapshot as a root resource (`crate::registry`'s module documentation has the whole of
/// that note).
pub fn spec(
    url: secrecy::SecretString,
    registry: Arc<dyn StationRegistry>,
) -> Result<ComponentSpec, mw_component::ComponentIdError> {
    let id = ComponentId::new(ID)?;
    // Before the closure takes the URL, because the closure takes it.
    let digest = mw_component::ConfigDigest::of(url_digest_input(&url));
    Ok(ComponentSpec::new(
        id,
        Trigger::Service,
        Builder::serve(move |cx| {
            let url = url.clone();
            let registry = Arc::clone(&registry);
            async move {
                let publisher = cx.publisher::<Arc<dyn ReadingSink>>()?;
                Ok(Pool {
                    url,
                    registry,
                    publisher,
                })
            }
        }),
    )
    // No needs at all: W2a §4.1's table gives `db:postgres` none, which is what makes it a
    // root of the ordering graph and therefore unable to participate in a cycle.
    .provides([Provision::of(Capability::ReadingStore)])
    // The URL is the only configuration this component closes over, so it is the whole of
    // the digest: a re-plan after the operator points the daemon at another server
    // restarts it, and a re-plan that changed something else does not.
    .config_digest(digest)
    // Longer than the default 10 s, because a stop must not interrupt the writer's
    // in-flight transaction (§3.6 names `writer:timescale` and the uplink as the two that
    // need more). The pool is closed on the way out, and closing it while a consumer still
    // holds a connection is what this grace buys time to avoid.
    .grace(Duration::from_secs(15)))
}

/// What goes into the spec's [`mw_component::ConfigDigest`].
///
/// A separate function with a name, because the obvious thing — hashing the `SecretString`
/// — does not compile and should not: `secrecy` deliberately implements neither `Hash` nor
/// `PartialEq` (**D-33**) so that a credential cannot be compared in non-constant time by
/// accident. The digest is compared within one process by the plan diff, never logged and
/// never persisted, which is what makes exposing the secret to the hasher the right trade
/// rather than a leak.
fn url_digest_input(url: &secrecy::SecretString) -> &str {
    url.expose_secret()
}

/// The body: the pool, and the publish/revoke loop over it.
struct Pool {
    url: secrecy::SecretString,
    registry: Arc<dyn StationRegistry>,
    publisher: Publisher<Arc<dyn ReadingSink>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

impl Serve for Pool {
    fn serve(
        self: Box<Self>,
        cx: Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(self.run(cx))
    }
}

impl Pool {
    async fn run(self: Box<Self>, cx: Cx) -> Result<(), ComponentError> {
        // Connect, assert the version, publish. A connection failure here is the ordinary
        // case on a reboot and is retried by the supervisor's backoff; a version failure
        // is not retried, because no amount of waiting makes a server newer.
        let pool = match self.connect(&cx).await {
            Ok(pool) => pool,
            Err(outcome) => return outcome,
        };

        let writer = match TimescaleWriter::new(pool.clone(), Arc::clone(&self.registry)) {
            Ok(writer) => writer,
            Err(e) => {
                pool.close().await;
                // A build/registry disagreement, not a database problem. Fatal: the next
                // attempt binds the same columns.
                return Err(ComponentError::tick("build the batch writer", e));
            }
        };
        info!(
            columns = writer.columns(),
            pool = POOL_SIZE,
            "the weather database is usable"
        );
        self.publisher
            .publish(Arc::new(writer) as Arc<dyn ReadingSink>);

        // The probe loop. The handle stays published while the pool answers, is revoked
        // after `FAILURES_BEFORE_REVOKE` consecutive silences, and is republished on the
        // first success after that -- `Publisher::publish` replaces, so a reconnected pool
        // needs no revoke first.
        let mut failures = 0u32;
        let mut published = true;
        loop {
            // The one await a `Service` owns inside its own loop, which A-W6-1 makes
            // explicitly not a second clock: the supervisor owns *when this component
            // runs*, not what it waits on while running.
            tokio::select! {
                () = cx.cancelled() => break,
                () = tokio::time::sleep(PROBE_INTERVAL) => {}
            }
            match sqlx::query("SELECT 1").execute(&pool).await {
                Ok(_) => {
                    if !published {
                        info!(
                            after_failures = failures,
                            "the weather database is answering again"
                        );
                        let writer = TimescaleWriter::new(pool.clone(), Arc::clone(&self.registry))
                            .map_err(|e| ComponentError::tick("rebuild the batch writer", e))?;
                        self.publisher
                            .publish(Arc::new(writer) as Arc<dyn ReadingSink>);
                        published = true;
                    }
                    failures = 0;
                }
                Err(e) => {
                    failures += 1;
                    if published && failures >= FAILURES_BEFORE_REVOKE {
                        // Not a `Failed` component: **D-27** is that a database outage
                        // degrades this daemon rather than stopping it. The consumers with
                        // a hard need go to `Waiting`, the spool keeps accepting readings,
                        // and `/latest` keeps serving from the snapshot (§2a.6).
                        error!(
                            err = %DisplayChain(&e),
                            failures,
                            "the weather database has not answered {failures} probes, so \
                             the store handle is withdrawn. Readings keep spooling and \
                             nothing is lost; writer:timescale is now Waiting.",
                        );
                        self.publisher.revoke();
                        published = false;
                    } else {
                        warn!(
                            err = %DisplayChain(&e),
                            failures,
                            "a weather database probe failed"
                        );
                    }
                }
            }
        }

        // Cancelled. Revoke before closing, so no consumer can hold a connection from a
        // pool that is going away -- §3.6's "a consumer must never observe a revoked
        // capability" is the supervisor's ordering, and this is the half this component
        // owes it.
        self.publisher.revoke();
        pool.close().await;
        info!("the weather database pool is closed");
        Ok(())
    }

    /// Open the pool and re-assert **D-62** on its first connection.
    ///
    /// `Err` carries the outcome to return: a version that is wrong is a component error,
    /// and so is a connection that could not be made.
    ///
    /// **Both are `ComponentError::tick`, and that is deliberate rather than lazy.**
    /// `Tick`'s own class is `Retryable`, so the supervisor records `Failed` and schedules
    /// `Backoff` for a server that is too old exactly as it does for one that is down —
    /// and W2a **Decision 30** forbids permanent give-up, so that is the right answer: a
    /// component that gave up on PostgreSQL 15 would have to be restarted by hand after
    /// the operator upgraded the server. What distinguishes the two for the person reading
    /// the log is the cause chain and `mw_store::version`'s own `Classify`, which is why
    /// the `VersionError` is carried rather than flattened into a string.
    async fn connect(&self, cx: &Cx) -> Result<PgPool, Result<(), ComponentError>> {
        let connecting = PgPoolOptions::new()
            .max_connections(POOL_SIZE)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect(self.url.expose_secret());
        let pool = tokio::select! {
            () = cx.cancelled() => return Err(Ok(())),
            pool = connecting => pool,
        };
        let pool = match pool {
            Ok(pool) => pool,
            Err(e) => {
                return Err(Err(ComponentError::tick(
                    "connect to the weather database",
                    version::VersionError::Unreachable {
                        redacted: version::redact(self.url.expose_secret()),
                        source: e,
                    },
                )))
            }
        };
        match self.assert_version(&pool).await {
            Ok(()) => Ok(pool),
            Err(e) => {
                pool.close().await;
                Err(Err(ComponentError::tick(
                    "assert the weather database version (D-62)",
                    e,
                )))
            }
        }
    }

    /// **D-62**, on this pool's own connection.
    async fn assert_version(&self, pool: &PgPool) -> Result<(), VersionError> {
        let mut conn = pool.acquire().await.map_err(|source| {
            // An acquire failure here is the connection and not the statement, which is
            // the distinction `VersionError`'s `Classify` reads.
            VersionError::Unreachable {
                redacted: version::redact(self.url.expose_secret()),
                source,
            }
        })?;
        let observed = version::observe(&mut conn).await?;
        let toolkit = match version::check(&observed) {
            Ok(toolkit) => toolkit,
            Err(e) => {
                // Logged here as well as returned, because the boot sequence's assertion
                // is allowed to have been skipped -- so for a server that was down at
                // startup this is the *first* place the operator is told, and the message
                // is the whole value of the assertion.
                if e.class() == Class::Fatal {
                    error!(
                        err = %DisplayChain(&e),
                        "the weather database is not one this build can use. The startup \
                         assertion (D-62) is allowed to be skipped for a server that did \
                         not answer in time (D-27), which is why this check exists here \
                         as well -- and why this is the first message about it."
                    );
                }
                return Err(e);
            }
        };
        if !toolkit {
            warn!("timescaledb_toolkit is not installed; stats_agg and rollup are unavailable");
        }
        info!(server = %observed, "database version asserted on the pool's own connection");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mw_component::{CapabilityAccess, CapabilityPublisher};
    use std::any::Any;
    use std::sync::Mutex;

    /// A capability bus that records rather than routing, so the publish/revoke half of
    /// this component is assertable without a supervisor.
    #[derive(Debug, Default)]
    struct RecordingBus(Mutex<Vec<(Capability, bool)>>);

    impl CapabilityPublisher for RecordingBus {
        fn publish(
            &self,
            _provider: &ComponentId,
            capability: Capability,
            _handle: Arc<dyn Any + Send + Sync>,
        ) {
            self.0
                .lock()
                .expect("not poisoned")
                .push((capability, true));
        }

        fn revoke(&self, _provider: &ComponentId, capability: Capability) {
            self.0
                .lock()
                .expect("not poisoned")
                .push((capability, false));
        }
    }

    /// The component's body, built the way the supervisor builds it.
    fn body(url: &str, cancelled: bool) -> (Box<Pool>, Arc<RecordingBus>, Cx) {
        let bus = Arc::new(RecordingBus::default());
        let token = tokio_util::sync::CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
        let cx = Cx::new(
            ComponentId::new(ID).expect("valid"),
            token,
            mw_component::Generation::ZERO,
            rx,
        );
        let access = CapabilityAccess::new(
            Arc::from(Vec::<mw_component::Need>::new()),
            Arc::from(vec![Capability::ReadingStore]),
            Vec::new(),
            Arc::clone(&bus) as Arc<dyn CapabilityPublisher>,
        );
        let build = mw_component::BuildCx::new(cx.clone(), access);
        let publisher = build
            .publisher::<Arc<dyn ReadingSink>>()
            .expect("the spec declares this provision");
        (
            Box::new(Pool {
                url: secrecy::SecretString::from(url.to_string()),
                registry: Arc::new(crate::ConfigRegistry::new(
                    crate::RegistrySnapshot::default(),
                )),
                publisher,
            }),
            bus,
            cx,
        )
    }

    /// **The password never reaches the message** (**D-33**), and an unreachable server is
    /// `Retryable` rather than `Fatal` (**D-27**) — a reboot is the ordinary case and the
    /// supervisor's backoff is the retry.
    ///
    /// Port 1 is refused immediately on every platform this builds for, so this is a unit
    /// test rather than a timeout.
    #[tokio::test]
    async fn an_unreachable_server_is_retryable_and_its_url_is_redacted() {
        let (pool, bus, cx) = body("postgresql://mw:hunter2@127.0.0.1:1/weather", false);
        let e = pool
            .serve(cx)
            .await
            .expect_err("nothing is listening on port 1");
        let chain = DisplayChain(&e).to_string();
        assert!(
            chain.contains("connect to the weather database"),
            "the action is named (§5.9): {chain}"
        );
        assert!(chain.contains("127.0.0.1:1"), "{chain}");
        assert!(
            !chain.contains("hunter2"),
            "the configured URL carries a password and this message is logged: {chain}"
        );
        assert!(
            bus.0.lock().expect("not poisoned").is_empty(),
            "nothing was published, so a consumer stays in Waiting rather than being \
             handed a pool that does not work"
        );
    }

    /// A stop during startup is an ordered exit, not a failure: the component returns
    /// `Ok` and publishes nothing. The supervisor cancels in reverse topological order and
    /// a `Failed` here would put a spurious failure in the last thing an operator reads
    /// (§3.6).
    #[tokio::test]
    async fn a_cancelled_start_publishes_nothing_and_returns_ok() {
        // An address that would take the kernel minutes to give up on, so the test is
        // asserting the cancellation arm and not a race with a refusal.
        let (pool, bus, cx) = body("postgresql://mw@192.0.2.1:5432/weather", true);
        pool.serve(cx)
            .await
            .expect("a cancelled start is not a failure");
        assert!(bus.0.lock().expect("not poisoned").is_empty());
    }

    #[test]
    fn the_spec_provides_the_store_and_requires_nothing() {
        let spec = spec(
            secrecy::SecretString::from("postgresql://localhost/mw".to_string()),
            Arc::new(crate::ConfigRegistry::new(
                crate::RegistrySnapshot::default(),
            )),
        )
        .expect("`db:postgres` is a valid component id");
        assert_eq!(spec.id.as_str(), ID);
        assert!(spec.requires.is_empty(), "a root of the ordering graph");
        assert_eq!(
            spec.provides,
            vec![Provision::of(Capability::ReadingStore)],
            "W2a §4.1 also gives it ForecastStore, AlertStore and SnowCache; those arrive \
             with the items that read them (M5, M6-3)"
        );
        assert!(matches!(spec.trigger, Trigger::Service));
        assert!(
            spec.grace > mw_component::DEFAULT_GRACE,
            "§3.6: the writer's in-flight transaction needs more than the default"
        );
    }

    /// The digest is what makes a re-plan restart this component when the operator points
    /// it at a different server, and not otherwise.
    #[test]
    fn the_digest_follows_the_configured_url() {
        let of = |url: &str| {
            spec(
                secrecy::SecretString::from(url.to_string()),
                Arc::new(crate::ConfigRegistry::new(
                    crate::RegistrySnapshot::default(),
                )),
            )
            .expect("valid")
            .config_digest
        };
        assert_eq!(of("postgresql://a/mw"), of("postgresql://a/mw"));
        assert_ne!(of("postgresql://a/mw"), of("postgresql://b/mw"));
    }
}
