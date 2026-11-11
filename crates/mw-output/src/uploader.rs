//! [`Uploader`] — the one component body all three rate-limited outputs share.
//!
//! > *The shared body is `mw_output::uploader::Uploader<E: OutputEncoder>`: it resolves the
//! > encoder's declared windows through `WindowedView`, encodes, and sends through the one
//! > `ServiceBuilder` stack of §4.2.*
//! > — `docs/service-architecture.md` §9.3
//!
//! Four things happen per occurrence and nothing else:
//!
//! 1. **Hold.** If a service answered `429` with a `Retry-After` that has not expired, the
//!    occurrence is not attempted. W2a Decision 59: *"a service telling us to slow down
//!    actually slows us down, instead of being retried on the next fixed tick."*
//! 2. **Resolve.** `encoder.required()` against `WindowedView`, for this station.
//! 3. **Encode.** The encoder reads back exactly what it declared.
//! 4. **Deliver**, through the layer stack below.
//!
//! There is no SQL here and there cannot be: `ci/layers.toml` forbids this crate `sqlx` and
//! `mw-store`, which is W2a Decision 50 made structural rather than left as a review
//! preference.
//!
//! # The layer order, and why the retry is a loop
//!
//! **`Retry` outside `RateLimit`, `Timeout` innermost** (**D-57**). Reversed, the first
//! failure spends one permit and then hammers the service as fast as the retry policy
//! allows — which is precisely the burst the rate limit exists to prevent, and it is worst
//! exactly when the remote service is already struggling.
//!
//! The ordering is kept and the top layer is not `tower::retry::Retry`, because it cannot
//! be: `Retry<P, S>` requires `S: Clone` and `tower::limit::RateLimit` is not `Clone`.
//! `crate::retry`'s module documentation has the whole argument and the amendment it is
//! recorded as. What this body does instead is drive the attempt loop itself:
//!
//! ```text
//!   loop {
//!       stack.ready().await        // <-- the limiter's permit is taken HERE, per attempt
//!       stack.call(payload.clone())
//!       policy.consider(&error)    // ClassPolicy, over `E: Classify`
//!   }
//! ```
//!
//! `RateLimit::poll_ready` is where a permit is spent, so a retry *waits for the limiter*
//! rather than bypassing it — which is the behaviour D-57 states in words.
//!
//! [`AttemptTimeout`] is innermost and is this crate's own rather than
//! `tower::timeout::Timeout`, for one reason: `tower`'s erases the error to `BoxError`, and
//! §4.2's entire mechanism is that the retry decision is read from `Classify` on a **typed**
//! error. A layer that turned `UploadError::RateLimited { retry_after }` into a
//! `Box<dyn Error>` would make the policy above it unable to honour the header.
//!
//! # The deadline is the outer bound, and `plan()` checks the arithmetic
//!
//! §4b.5: *"the tick deadline is the outer bound on the whole stack, so
//! `deadline >= retries x (attempt_timeout + rate_limit_period)` must hold, and it is
//! checked in `plan()` alongside `deadline <= period`."* [`budget_fits`] is that check and
//! `crate::plan` is what calls it, so *"a retry budget that cannot fit inside the deadline
//! is a configuration error, not a mysterious stream of `deadline_exceeded`"*.

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use mw_component::{
    BoxFuture, ComponentError, ComponentId, Cx, Declaration, Resolved, Tick, WindowedView,
};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use tokio::time::Instant;
use tower::limit::RateLimit;
use tower::{Service, ServiceBuilder, ServiceExt};
use tracing::{debug, info, warn};

use crate::encoder::{Observation, OutputEncoder};
use crate::error::{EncodeError, UploadError};
use crate::metrics as m;
use crate::retry::ClassPolicy;

/// How long one attempt may take.
///
/// Ten seconds against a public HTTPS endpoint, which is generous for a request whose body
/// is empty and whose response is a line. It is the **attempt's** bound, not the
/// occurrence's: the occurrence's is the trigger's deadline.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The limiter's period — one delivery per this long.
///
/// Three seconds, which is not the upload interval and is not meant to be. The interval is
/// structural (**D-30b**: a component that does not wake more often than allowed needs no
/// queue); this is the floor **between attempts of one occurrence**, so that a retry
/// budget of two cannot become three requests in a hundred milliseconds against a service
/// that has just failed.
pub const LIMIT_PERIOD: Duration = Duration::from_secs(3);

/// One delivery attempt, as a transport.
///
/// Consumes `self` so an implementation can move what it needs into the future, and is
/// `Clone` because the stack clones it per attempt — which for `crate::frame::SharedSink`
/// and `crate::https::HttpUpload` is an `Arc` bump.
///
/// **This is the seam the tests stub.** Every scheduling, retry, rate-limiting and
/// pushback property below is asserted against a recording implementation of this trait,
/// because the item's instruction is explicit: *a test that actually posts to CWOP or
/// Weather Underground is a test that pollutes a public network with fabricated
/// observations from the owner's callsign.*
pub trait Transmit: Clone + Send + Sync + 'static {
    /// What it sends.
    type Payload: Clone + Send + 'static;

    /// Send it, once.
    fn transmit(self, payload: Self::Payload) -> BoxFuture<'static, Result<(), UploadError>>;
}

/// A [`Transmit`] as a `tower::Service`.
///
/// `poll_ready` is always ready: the readiness this stack has to express is the limiter's,
/// and a transport that opens a connection per attempt has no other.
#[derive(Debug, Clone)]
pub struct Attempt<T>(T);

impl<T> Attempt<T> {
    /// Wrap a transport.
    pub fn new(transport: T) -> Self {
        Self(transport)
    }
}

impl<T: Transmit> Service<T::Payload> for Attempt<T> {
    type Response = ();
    type Error = UploadError;
    type Future = BoxFuture<'static, Result<(), UploadError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, payload: T::Payload) -> Self::Future {
        self.0.clone().transmit(payload)
    }
}

/// Bound one attempt, keeping the error typed.
///
/// `tower::timeout::Timeout` would do this and would erase the error to `BoxError`; see
/// this module's documentation for why that is not available here.
#[derive(Debug, Clone)]
pub struct AttemptTimeout<S> {
    inner: S,
    after: Duration,
    target: crate::error::Target,
}

impl<S> AttemptTimeout<S> {
    /// Bound `inner`'s calls at `after`.
    pub fn new(inner: S, after: Duration, target: crate::error::Target) -> Self {
        Self {
            inner,
            after,
            target,
        }
    }
}

impl<S, Req> Service<Req> for AttemptTimeout<S>
where
    S: Service<Req, Response = (), Error = UploadError>,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = UploadError;
    type Future = BoxFuture<'static, Result<(), UploadError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let call = self.inner.call(request);
        let after = self.after;
        let target = self.target.clone();
        Box::pin(async move {
            match tokio::time::timeout(after, call).await {
                Ok(result) => result,
                Err(_) => Err(UploadError::Timeout { target, after }),
            }
        })
    }
}

/// The layer stack, named so that the body can hold it.
///
/// Outside in: the limiter, then the attempt timeout, then the transport. The retry is the
/// loop in [`Uploader::deliver`], which sits outside all of it.
pub type Stack<T> = RateLimit<AttemptTimeout<Attempt<T>>>;

/// Build the stack for one component.
pub fn stack<T: Transmit>(
    transport: T,
    target: crate::error::Target,
    attempt_timeout: Duration,
    limit_period: Duration,
) -> Stack<T> {
    ServiceBuilder::new()
        // `RateLimit` is the outermost layer this builder owns, and the retry loop is
        // outside it -- which is D-57's order.
        .layer(tower::limit::RateLimitLayer::new(1, limit_period))
        .service(AttemptTimeout::new(
            Attempt::new(transport),
            attempt_timeout,
            target,
        ))
}

/// Whether a retry budget fits inside an occurrence's deadline (§4b.5).
///
/// `deadline >= (retries + 1) x (attempt_timeout + limit_period)`. The `+ 1` is the first
/// attempt, which §4b.5's `retries x ...` leaves implicit and which a check that omitted it
/// would admit a budget whose *first* attempt already exceeds the deadline.
pub fn budget_fits(
    deadline: Duration,
    retries: u8,
    attempt_timeout: Duration,
    limit_period: Duration,
) -> bool {
    let per_attempt = attempt_timeout.saturating_add(limit_period);
    let attempts = u32::from(retries) + 1;
    deadline >= per_attempt.saturating_mul(attempts)
}

/// The shared component body.
///
/// One per uploader component. Holds the encoder, the station it is for, the resolved
/// `WindowedView` handle and the layer stack; everything else is per occurrence.
pub struct Uploader<E: OutputEncoder, T: Transmit<Payload = E::Payload>> {
    id: ComponentId,
    encoder: E,
    station: StationKey,
    windows: Arc<dyn WindowedView>,
    stack: Stack<T>,
    /// How many retries one occurrence may make.
    budget: u8,
    /// The limiter's external "not before" signal — a `Retry-After` the service gave us,
    /// held **across occurrences**, which is what makes the pushback mean anything.
    not_before: Option<Instant>,
}

impl<E: OutputEncoder, T: Transmit<Payload = E::Payload>> std::fmt::Debug for Uploader<E, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uploader")
            .field("id", &self.id)
            .field("service", &self.encoder.service())
            .field("station", &self.station)
            .field("declared", &self.encoder.required().len())
            .field("held", &self.not_before.is_some())
            .finish_non_exhaustive()
    }
}

impl<E: OutputEncoder, T: Transmit<Payload = E::Payload>> Uploader<E, T> {
    /// Assemble a body. `crate::plan`'s builder is the only caller outside tests.
    pub fn new(
        id: ComponentId,
        encoder: E,
        station: StationKey,
        windows: Arc<dyn WindowedView>,
        stack: Stack<T>,
        budget: u8,
    ) -> Self {
        Self {
            id,
            encoder,
            station,
            windows,
            stack,
            budget,
            not_before: None,
        }
    }

    /// Which service this uploads to.
    pub fn service(&self) -> &'static str {
        self.encoder.service()
    }

    /// Whether a service's pushback is still in force.
    pub fn held_until(&self) -> Option<Instant> {
        self.not_before
    }

    /// Resolve this encoder's declarations.
    fn resolve(&self) -> Result<Resolved, EncodeError> {
        let declarations: &[Declaration] = self.encoder.required();
        self.windows
            .resolve(&self.station, declarations)
            .map_err(|source| EncodeError::Window {
                station: self.station.clone(),
                source,
            })
    }

    /// Deliver one payload, retrying per [`ClassPolicy`].
    async fn deliver(&mut self, payload: E::Payload) -> Result<(), UploadError> {
        // Both are copied out before the loop, because `self.stack.ready()` borrows `self`
        // mutably for as long as the readied service is alive.
        let service = self.encoder.service();
        let id = self.id.clone();
        let mut policy = ClassPolicy::new(self.budget);
        loop {
            // **The permit is taken here**, once per attempt, which is what makes the
            // retry sit outside the limiter (D-57).
            let error = match self.stack.ready().await {
                // `RateLimit::poll_ready` delegates to the inner service, which is always
                // ready, so this arm is unreachable today. Handled rather than unwrapped
                // because a readiness failure is still a delivery failure, and a
                // `poll_ready` that can fail is what a future layer here would add.
                Err(e) => e,
                Ok(ready) => match ready.call(payload.clone()).await {
                    Ok(()) => {
                        m::attempt(&id, service, m::ATTEMPT_OK);
                        return Ok(());
                    }
                    Err(e) => e,
                },
            };
            m::attempt(&id, service, error.reason());
            match policy.consider(&error) {
                None => return Err(error),
                Some(delay) => {
                    warn!(
                        component = %id,
                        service,
                        reason = error.reason(),
                        retry_in_s = delay.as_secs(),
                        remaining = policy.remaining(),
                        err = %DisplayChain(&error),
                        "an upload attempt failed; retrying"
                    );
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    }

    /// Record a service's pushback, so the next occurrence does not attempt before it.
    fn hold(&mut self, error: &UploadError) {
        if let Class::Retryable { after: Some(after) } = error.class() {
            let until = Instant::now() + after;
            self.not_before = Some(until);
            info!(
                component = %self.id,
                service = self.encoder.service(),
                hold_s = after.as_secs(),
                "the service asked us to slow down; occurrences before then will be skipped"
            );
        }
    }
}

impl<E: OutputEncoder, T: Transmit<Payload = E::Payload>> Tick for Uploader<E, T> {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let service = self.encoder.service();

            // 1. The hold. A service that asked us to slow down is obeyed between
            //    occurrences, not only inside one.
            if let Some(until) = self.not_before {
                if Instant::now() < until {
                    m::occurrence(&self.id, service, m::Outcome::Held);
                    debug!(
                        component = %self.id,
                        service,
                        "skipping this occurrence: the service's `Retry-After` has not expired"
                    );
                    return Ok(());
                }
                self.not_before = None;
            }

            // 2. Resolve, and 3. encode. Both failures are classified beside their own
            //    definitions; an `Ignorable` one is an absence rather than a failed tick.
            let resolved = match self.resolve() {
                Ok(resolved) => resolved,
                Err(e) => return self.encode_failure(e, service),
            };
            let payload = {
                let observation = Observation::new(service, &self.station, &resolved);
                match self.encoder.encode(&observation) {
                    Ok(payload) => payload,
                    Err(e) => return self.encode_failure(e, service),
                }
            };

            // 4. Deliver, with cancellation observed at the await point (**D-24**): there
            //    is no stop method anywhere in the abstraction, and a cancelled upload is
            //    a stop rather than a failure.
            tokio::select! {
                biased;
                () = cx.cancelled() => {
                    m::occurrence(&self.id, service, m::Outcome::Cancelled);
                    Ok(())
                }
                result = self.deliver(payload) => match result {
                    Ok(()) => {
                        m::occurrence(&self.id, service, m::Outcome::Sent);
                        debug!(component = %self.id, service, "uploaded");
                        Ok(())
                    }
                    Err(e) => {
                        self.hold(&e);
                        m::occurrence(&self.id, service, m::Outcome::Failed);
                        warn!(
                            component = %self.id,
                            service,
                            reason = e.reason(),
                            err = %DisplayChain(&e),
                            "the upload failed for this period"
                        );
                        Err(ComponentError::tick(
                            format!("upload to {service}"),
                            e,
                        ))
                    }
                }
            }
        })
    }
}

impl<E: OutputEncoder, T: Transmit<Payload = E::Payload>> Uploader<E, T> {
    /// What an encode or resolve failure means for the occurrence.
    ///
    /// `Ignorable` is the station having nothing to say, which is counted and is not a
    /// failed tick; everything else is a declaration or configuration error and fails the
    /// occurrence so that it is visible in `last_error` and in the component's state.
    fn encode_failure(
        &self,
        error: EncodeError,
        service: &'static str,
    ) -> Result<(), ComponentError> {
        if error.class() == Class::Ignorable {
            m::occurrence(&self.id, service, m::Outcome::NoData);
            debug!(
                component = %self.id,
                service,
                station = %self.station,
                "nothing to upload this period"
            );
            return Ok(());
        }
        m::occurrence(&self.id, service, m::Outcome::Failed);
        warn!(
            component = %self.id,
            service,
            err = %DisplayChain(&error),
            "could not build this period's upload"
        );
        Err(ComponentError::tick(
            format!("build this period's upload for {service}"),
            error,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use mw_component::{Declaration, Generation, Resolved, WindowError};
    use mw_types::{Agg, MetricKey};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use crate::encoder::CURRENT;
    use crate::error::Target;

    /// A declaration table of one, so a test encoder is three lines.
    const ONLY: Declaration = (MetricKey::OutTemp, CURRENT, Agg::Mean);

    /// An encoder whose payload is the temperature it was handed.
    #[derive(Debug, Clone)]
    struct TestEncoder;

    impl OutputEncoder for TestEncoder {
        type Payload = String;

        fn service(&self) -> &'static str {
            "test"
        }

        fn required(&self) -> &[Declaration] {
            &[ONLY]
        }

        fn encode(&self, observation: &Observation<'_>) -> Result<String, EncodeError> {
            match observation.value(ONLY)? {
                Some(v) => Ok(format!("t={v}")),
                None => Err(EncodeError::NothingToSend {
                    service: "test",
                    station: observation.station().clone(),
                    declared: observation.declared(),
                }),
            }
        }
    }

    /// A transport that records what it was handed and answers from a script.
    #[derive(Debug, Clone)]
    struct Recorder {
        sent: Arc<Mutex<Vec<String>>>,
        answers: Arc<Mutex<std::collections::VecDeque<Result<(), UploadError>>>>,
        at: Arc<Mutex<Vec<Instant>>>,
    }

    impl Recorder {
        fn new<I: IntoIterator<Item = Result<(), UploadError>>>(answers: I) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                answers: Arc::new(Mutex::new(answers.into_iter().collect())),
                at: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("not poisoned").clone()
        }

        fn attempts(&self) -> Vec<Instant> {
            self.at.lock().expect("not poisoned").clone()
        }
    }

    impl Transmit for Recorder {
        type Payload = String;

        fn transmit(self, payload: String) -> BoxFuture<'static, Result<(), UploadError>> {
            Box::pin(async move {
                self.sent.lock().expect("not poisoned").push(payload);
                self.at.lock().expect("not poisoned").push(Instant::now());
                self.answers
                    .lock()
                    .expect("not poisoned")
                    .pop_front()
                    // An exhausted script is a success, so a test that only cares about the
                    // first answer does not have to write the rest.
                    .unwrap_or(Ok(()))
            })
        }
    }

    /// A transport that never returns, for the timeout and cancellation cases.
    #[derive(Debug, Clone)]
    struct Hangs;

    impl Transmit for Hangs {
        type Payload = String;

        fn transmit(self, _payload: String) -> BoxFuture<'static, Result<(), UploadError>> {
            Box::pin(async move {
                std::future::pending::<()>().await;
                unreachable!("pending never resolves")
            })
        }
    }

    /// A `WindowedView` that answers one declaration, or refuses.
    #[derive(Debug)]
    struct View(Result<Option<f64>, WindowError>);

    impl WindowedView for View {
        fn resolve(
            &self,
            _station: &StationKey,
            want: &[Declaration],
        ) -> Result<Resolved, WindowError> {
            let value = self.0.clone()?;
            let at = "2026-11-11T05:00:00Z".parse().expect("an instant");
            Ok(Resolved::new(at, want.iter().map(|d| (*d, value))))
        }
    }

    fn id() -> ComponentId {
        ComponentId::new("wu:backyard").expect("a component id")
    }

    fn cx() -> (Cx, CancellationToken) {
        let token = CancellationToken::new();
        let (_tx, rx) = watch::channel(Generation::ZERO);
        (Cx::new(id(), token.clone(), Generation::ZERO, rx), token)
    }

    fn uploader<T: Transmit<Payload = String>>(
        view: View,
        transport: T,
        budget: u8,
    ) -> Uploader<TestEncoder, T> {
        Uploader::new(
            id(),
            TestEncoder,
            StationKey::new("backyard"),
            Arc::new(view),
            stack(
                transport,
                Target::peer("test", 1),
                Duration::from_secs(10),
                LIMIT_PERIOD,
            ),
            budget,
        )
    }

    #[tokio::test]
    async fn an_occurrence_resolves_encodes_and_sends() {
        let recorder = Recorder::new([Ok(())]);
        let mut body = uploader(View(Ok(Some(72.5))), recorder.clone(), 2);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect("the occurrence succeeds");
        assert_eq!(recorder.sent(), vec!["t=72.5".to_string()]);
        assert_eq!(body.service(), "test");
        assert!(format!("{body:?}").contains("wu:backyard"));
    }

    /// A station that has reported nothing is an absence, not a failed tick — and nothing
    /// is sent.
    #[tokio::test]
    async fn a_station_with_no_values_sends_nothing_and_does_not_fail() {
        let recorder = Recorder::new([]);
        let mut body = uploader(View(Ok(None)), recorder.clone(), 2);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect("an absence is not a failure");
        assert!(recorder.sent().is_empty());
    }

    /// A declaration the registry cannot resolve is a configuration error and fails the
    /// occurrence, because the alternative is an uploader that silently sends nothing
    /// forever.
    #[tokio::test]
    async fn an_unresolvable_declaration_fails_the_occurrence() {
        let refusal = WindowError::NotDeclared {
            metric: MetricKey::OutTemp,
            window: CURRENT,
            agg: Agg::Mean,
        };
        let recorder = Recorder::new([]);
        let mut body = uploader(View(Err(refusal)), recorder.clone(), 2);
        let (cx, _token) = cx();
        let e = body.tick(&cx).await.expect_err("a declaration error");
        assert!(
            DisplayChain(&e).to_string().contains("registry.toml"),
            "{e}"
        );
        assert!(recorder.sent().is_empty());
    }

    /// **D-57's property.** Three attempts, and each one waits for its own permit — so the
    /// second and third are at least one limiter period apart rather than immediate.
    #[tokio::test(start_paused = true)]
    async fn each_retry_takes_its_own_permit_from_the_limiter() {
        let transient = || {
            Err(UploadError::Connect {
                what: "could not connect to",
                target: Target::peer("test", 1),
                source: std::io::Error::other("refused"),
            })
        };
        let recorder = Recorder::new([transient(), transient(), Ok(())]);
        let mut body = uploader(View(Ok(Some(1.0))), recorder.clone(), 2);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect("the third attempt succeeds");

        let at = recorder.attempts();
        assert_eq!(at.len(), 3, "two retries after the first attempt");
        // Reversed -- `RateLimit` outside `Retry` -- every retry would happen inside one
        // permit and these gaps would be zero.
        assert!(
            at[1].duration_since(at[0]) >= LIMIT_PERIOD,
            "the first retry did not wait for a permit: {:?}",
            at[1].duration_since(at[0])
        );
        assert!(
            at[2].duration_since(at[1]) >= LIMIT_PERIOD,
            "the second retry did not wait for a permit: {:?}",
            at[2].duration_since(at[1])
        );
    }

    /// The budget is finite, so a service that is down does not consume the occurrence's
    /// whole deadline.
    #[tokio::test(start_paused = true)]
    async fn the_budget_bounds_the_attempts() {
        let transient = || {
            Err(UploadError::Connect {
                what: "could not connect to",
                target: Target::peer("test", 1),
                source: std::io::Error::other("refused"),
            })
        };
        let recorder = Recorder::new([transient(), transient(), transient(), transient()]);
        let mut body = uploader(View(Ok(Some(1.0))), recorder.clone(), 2);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect_err("every attempt failed");
        assert_eq!(recorder.sent().len(), 3, "one attempt plus two retries");
    }

    /// A 4xx is not retried at all: a bad API key is not fixed by trying again, and the
    /// period is the retry.
    #[tokio::test(start_paused = true)]
    async fn a_rejected_upload_is_not_retried() {
        let recorder = Recorder::new([Err(UploadError::Rejected {
            target: Target::peer("test", 1),
            status: 401,
            body: "Invalid ID".to_string(),
        })]);
        let mut body = uploader(View(Ok(Some(1.0))), recorder.clone(), 5);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect_err("rejected");
        assert_eq!(recorder.sent().len(), 1, "a 4xx is fatal for the tick");
    }

    /// **W2a Decision 59.** A `Retry-After` is honoured *between* occurrences: the next
    /// tick is skipped rather than retried on the fixed schedule.
    #[tokio::test(start_paused = true)]
    async fn a_retry_after_holds_the_next_occurrence() {
        let limited = || {
            Err(UploadError::RateLimited {
                target: Target::peer("test", 1),
                retry_after: Some(Duration::from_secs(600)),
            })
        };
        // Budget zero, so the `429` ends the occurrence immediately and the hold is what
        // the test is about.
        let recorder = Recorder::new([limited(), Ok(())]);
        let mut body = uploader(View(Ok(Some(1.0))), recorder.clone(), 0);
        let (cx, _token) = cx();

        body.tick(&cx).await.expect_err("rate limited");
        assert_eq!(recorder.sent().len(), 1);
        assert!(body.held_until().is_some(), "the hold was recorded");

        // The next occurrence is skipped and is **not** a failure: the component is
        // healthy and the service asked for time.
        body.tick(&cx)
            .await
            .expect("a held occurrence is not a failure");
        assert_eq!(recorder.sent().len(), 1, "nothing was attempted");

        // Once the hold expires, the next occurrence goes.
        tokio::time::advance(Duration::from_secs(601)).await;
        body.tick(&cx).await.expect("the hold has expired");
        assert_eq!(recorder.sent().len(), 2);
        assert!(body.held_until().is_none(), "the hold was cleared");
    }

    /// A `429` with no header does not hold: there is nothing to honour, and holding for a
    /// guess would be this program inventing a backoff the service did not ask for.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_with_no_header_does_not_hold() {
        let recorder = Recorder::new([Err(UploadError::RateLimited {
            target: Target::peer("test", 1),
            retry_after: None,
        })]);
        let mut body = uploader(View(Ok(Some(1.0))), recorder, 0);
        let (cx, _token) = cx();
        body.tick(&cx).await.expect_err("rate limited");
        assert!(body.held_until().is_none());
    }

    /// The timeout is innermost, so it bounds an **attempt**: a transport that hangs is
    /// retried rather than consuming the occurrence.
    #[tokio::test(start_paused = true)]
    async fn the_timeout_bounds_an_attempt_and_not_the_sequence() {
        let mut body = Uploader::new(
            id(),
            TestEncoder,
            StationKey::new("backyard"),
            Arc::new(View(Ok(Some(1.0)))),
            stack(
                Hangs,
                Target::peer("test", 1),
                Duration::from_secs(2),
                LIMIT_PERIOD,
            ),
            1,
        );
        let (cx, _token) = cx();
        let started = Instant::now();
        let e = body.tick(&cx).await.expect_err("every attempt timed out");
        // `ComponentError::Tick`'s `Display` is the action it names (§5.9), and the
        // timeout is the cause behind it.
        assert_eq!(e.to_string(), "upload to test");
        assert!(
            DisplayChain(&e)
                .to_string()
                .contains("exceeded its 2 s timeout"),
            "{}",
            DisplayChain(&e)
        );
        // Two attempts at two seconds each, separated by a permit: well under the sum of a
        // whole-sequence timeout and well over one attempt's.
        let elapsed = Instant::now().duration_since(started);
        assert!(elapsed >= Duration::from_secs(4), "{elapsed:?}");
    }

    /// Cancellation is observed at the await point (**D-24**) and is a stop rather than a
    /// failure.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_occurrence_is_not_a_failure() {
        let mut body = Uploader::new(
            id(),
            TestEncoder,
            StationKey::new("backyard"),
            Arc::new(View(Ok(Some(1.0)))),
            stack(
                Hangs,
                Target::peer("test", 1),
                Duration::from_secs(600),
                LIMIT_PERIOD,
            ),
            0,
        );
        let (cx, token) = cx();
        token.cancel();
        body.tick(&cx).await.expect("a stop is not a failure");
    }

    /// §4b.5's arithmetic, which `crate::plan` checks so that a budget that cannot fit is
    /// a configuration error rather than a stream of `deadline_exceeded`.
    #[test]
    fn a_retry_budget_either_fits_the_deadline_or_it_does_not() {
        let attempt = Duration::from_secs(10);
        let period = Duration::from_secs(3);
        // Three attempts at 13 s each needs 39 s.
        assert!(budget_fits(Duration::from_secs(39), 2, attempt, period));
        assert!(!budget_fits(Duration::from_secs(38), 2, attempt, period));
        // The first attempt counts, which a check written as `retries x per_attempt` would
        // miss: one retry is two attempts.
        assert!(!budget_fits(Duration::from_secs(13), 1, attempt, period));
        assert!(budget_fits(Duration::from_secs(26), 1, attempt, period));
        // No retries still needs room for the one attempt.
        assert!(!budget_fits(Duration::from_secs(12), 0, attempt, period));
        assert!(budget_fits(Duration::from_secs(13), 0, attempt, period));
    }

    /// The default attempt timeout and limiter period, pinned: they are what `plan()`'s
    /// deadline arithmetic is computed from.
    #[test]
    fn the_defaults_are_the_ones_the_deadline_is_computed_from() {
        assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(LIMIT_PERIOD, Duration::from_secs(3));
    }
}
