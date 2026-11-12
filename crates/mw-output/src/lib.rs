//! Third-party HTTPS integrations: the rate-limited outputs, and the Aeris fetchers.
//!
//! Two halves over one outbound HTTP stack. **Out:** `OutputEncoder` implementations for
//! APRS/CWOP, PWS Weather and Weather Underground over one shared `Uploader<E>` body.
//! **In:** [`aeris`]'s four components, which fetch forecasts and weather alerts.
//!
//! The crate's declared responsibility in `docs/service-architecture.md` §1.2 is the first
//! half alone, and the second arrived with implementation-plan.md M6-3 — recorded as
//! `docs/reconciliation.md` **A-M6-3-2** rather than taken quietly. The short version is
//! that no document assigns the Aeris components a crate, the one thing they need that
//! nothing else in a lower layer has is **an outbound HTTPS request with a public trust
//! anchor**, and `ci/layers.toml` confines `webpki-roots` to this crate on the explicit
//! grounds that a second crate verifying a public endpoint would mean "something had
//! started verifying a public endpoint somewhere the design does not have one". Here is
//! where the design does have one.
//!
//! An encoder is a declared `&[(MetricKey, Window, Agg)]` table and an `encode`; the
//! scheduling, retry, rate limiting, backoff and SQL are all somewhere else
//! (docs/service-architecture.md §9.3). Retry goes **outside** RateLimit so a retry
//! re-enters the limiter, and Timeout is innermost so it bounds an attempt rather than the
//! whole sequence (docs/component-model.md §4b.5).
//!
//! **Forbidden to depend on `sqlx` and `mw-store`** -- W2a decision 50 ("no SQL in the
//! radio component") made structural rather than left as a review preference.
//!
//! # These are scheduled producers, not stream sinks
//!
//! **D-30b**, which replaced D-30 for exactly this reason:
//!
//! > *Rate-limited outputs are **not stream sinks.** They are scheduled producers with a
//! > declared period, and each declares the **windowed values** it needs as
//! > `(Metric, Window, Agg)`, resolved by rolling up aggregate buckets. Rate limiting is
//! > therefore structural — a component that does not wake more often than allowed needs no
//! > queue, no buffer and no coalescing.*
//!
//! The half that makes it a *correctness* decision rather than a structural one is the
//! value: CWOP requires wind direction and speed **averaged** over five minutes with gust
//! as a **peak over ten**, and Weather Underground's field names encode their windows
//! (`windspdmph_avg2m`, `windgustmph_10m`). Coalescing to the newest reading yields an
//! instantaneous sample where an average is required — *"a protocol violation that looks
//! like working software: the packets are accepted, the numbers are wrong, and nothing
//! reports it"* (`DESIGN.md` A-W2a-1). That is the current behaviour and it is what this
//! crate exists to stop.
//!
//! # The shape, in one place
//!
//! ```text
//!   Arc<dyn WindowedView>                 (window:rollup, M3-4)
//!            |
//!            v  resolve(station, encoder.required())
//!   +-------------------+   encode    +------------------+
//!   |     Uploader<E,T> | ----------> |  E: OutputEncoder|   cwop | wu | pws
//!   +-------------------+             +------------------+
//!            |  deliver, retrying
//!            v
//!     RateLimit -> AttemptTimeout -> Attempt<T>        T: Transmit
//!                                       |
//!                            +----------+-----------+
//!                            |                      |
//!                   FrameSink (AprsIs,       HttpUpload (GET over
//!                   KissSerial, KissTcp)     TLS, crate::https)
//! ```
//!
//! # Where each module's argument lives
//!
//! | Module | What it owns |
//! |---|---|
//! | [`encoder`] | the three declaration tables, and the trait that makes them the only thing an encode can read |
//! | [`uploader`] | the shared tick, the layer order, and the `Retry-After` hold |
//! | [`frame`] | two framings over one information field, and the three APRS transports |
//! | [`https`] | the outbound request, which no design document named a crate for |
//! | [`interval`] | the RF beacon floor, as a type |
//! | [`retry`] | the retry decision, over `Classify`, once |
//! | [`plan`] | one component per `uploader` row, and every refusal a bad row earns |
//! | [`error`] | the two error enums and their classifications |
//! | [`form`] | a query string whose `Debug` redacts |
//! | [`metrics`] | the two series this crate owns |
//!
//! # What this crate deliberately does not do
//!
//! - **It does not read the database.** `ci/layers.toml` forbids it `sqlx` and `mw-store`;
//!   `mw-store` owns the `uploader` query and the binary is where the two meet (§1.3 rule
//!   1), which is why [`plan::UploaderRow`] is a copy of the columns rather than a type
//!   from the store.
//! - **It does not schedule itself.** Every cadence is a `Trigger::Every` the supervisor
//!   owns (**D-25**), which is what replaces four hand-rolled tickers, a reconcile loop and
//!   a `map[string]context.CancelFunc`.
//! - **It does not talk to a third party in a test.** The seam is
//!   [`uploader::Transmit`], and every scheduling, retry, rate-limiting and pushback
//!   property is asserted against a recording implementation of it.

pub mod aeris;
pub mod encoder;
pub mod error;
pub mod form;
pub mod frame;
pub mod https;
pub mod interval;
pub mod metrics;
pub mod plan;
pub mod retry;
pub mod uploader;

pub use aeris::{AerisAccount, AerisConfig, AerisPlanError, AerisStation, Span};
pub use encoder::account::Account;
pub use encoder::cwop::Cwop;
pub use encoder::pws::PwsWeather;
pub use encoder::wu::WeatherUnderground;
pub use encoder::{Observation, OutputEncoder};
pub use error::{AerisError, EncodeError, Target, UploadError};
pub use form::Form;
pub use frame::{FrameSink, SharedSink};
pub use https::{Endpoint, EndpointError, HttpUpload};
pub use interval::{RadioInterval, TooFast};
pub use plan::{register, OutputConfig, OutputPlanError, UploaderRow};
pub use retry::ClassPolicy;
pub use uploader::{Transmit, Uploader};
