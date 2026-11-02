//! Rate-limited outputs: `OutputEncoder` implementations for APRS/CWOP, PWS Weather and
//! Weather Underground over one shared `Uploader<E>` body.
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
//! The outbound HTTP client is not named by any design document; it arrives with the work
//! item that writes `Uploader`.
