//! The numbers W7 §7 and docs/service-architecture.md §5.10 say this crate owns.
//!
//! One function per fact, so that every call site names a metric that exists and the
//! label set is decided here rather than at twelve call sites. All names carry the `mw_`
//! prefix `mw-obs` establishes — §5.10's table writes it and W7 §7's does not, and one
//! namespace for one process's metrics is worth more than two sections' literal text.
//!
//! **Cardinality is bounded by construction** (§5.10). Every label here is a component id,
//! a station key that identity resolution has already bounded, or a closed enum — and the
//! review rule is that a new one must be justifiable as "there are N of these and N is in
//! the configuration". The one place that was false is recorded on [`accepted`]: on the
//! `accept` path the station key is a peer's claim until M6-4's registry check lands, so
//! that path does not label with it.
//!
//! # Which named metrics are deliberately not here
//!
//! Stated rather than left to a grep, because an operator writing an alert from W7 §7 or
//! §5.10 needs to know:
//!
//! - **`origin` on `mw_readings_ingested_total` and `mw_ingest_duplicates_total`** (§5.10,
//!   §7). Omitted for now: `origin` is bounded by `MAX_ORIGINS`, a *constant*, not by the
//!   configuration — so it passes the letter of the cardinality rule and fails its test
//!   ("there are N of these and N is in the configuration"). The per-origin breakdown
//!   belongs with the peer registry that makes N configured, which is M6-4's.
//! - **W7 §7's spellings `admit_rejected_total` and `ingest_rejected_total`** are both
//!   [`rejected`], i.e. `mw_readings_rejected_total{reason}` — §5.10's name, with §5.10's
//!   `mw_` prefix. One series for one event; an alert written against either §7 spelling
//!   will not match, and that is a documentation gap in §7 rather than a second counter.
//! - **`mw_readings_dropped_total{reason="rejected"}`** (§5.10). A rejection is already
//!   counted once, by reason, on `mw_readings_rejected_total`; a second series under a
//!   coarser label is how two dashboards come to disagree.
//! - **Everything per-peer in W7 §7's uplink and collector tables** — `uplink_*`,
//!   `ingest_envelopes_total`, `peer_last_seen_seconds` and the rest. Those belong to the
//!   uplink and the gRPC receiver, which are M6-4's.

use mw_component::{ComponentId, Reject};

use crate::spool::CursorState;

/// Describe and zero everything this crate owns.
///
/// Called once from the boot sequence. A Prometheus counter does not exist in the
/// exposition until something touches it, so a daemon that has rejected no readings and a
/// daemon whose metrics are broken look identical to a scraper — and an alert on
/// `rate(...)` over a series that appears halfway through a window reads as a step change
/// rather than as the first increment.
pub fn register() {
    metrics::describe_counter!(
        "mw_readings_ingested_total",
        "Readings sealed into an envelope and durably appended"
    );
    metrics::describe_counter!(
        "mw_readings_rejected_total",
        "Frames and envelopes refused before anything was stored, by reason"
    );
    metrics::describe_counter!(
        "mw_readings_dropped_total",
        "Readings lost rather than refused, by reason"
    );
    metrics::describe_counter!(
        "mw_snapshot_published_total",
        "Current-conditions snapshots rendered, per website and station"
    );
    metrics::describe_gauge!(
        "mw_snapshot_age_seconds",
        "How old each key's newest folded reading is"
    );
    metrics::describe_gauge!(
        "mw_snapshot_degraded",
        "1 while a key's window-derived values are served from their last good value"
    );
    metrics::describe_counter!(
        "mw_snapshot_render_failed_total",
        "Payloads that could not be serialised, per website and station"
    );
    metrics::describe_gauge!("mw_spool_depth_bytes", "How much the spool holds on disk");
    metrics::describe_gauge!(
        "mw_spool_depth_envelopes",
        "The spool's tail, as a count of appended records"
    );
    metrics::describe_gauge!(
        "mw_spool_lag_envelopes",
        "How far behind each cursor is, in records"
    );
    metrics::describe_gauge!(
        "mw_spool_oldest_unacked_age_seconds",
        "How long each cursor has been behind"
    );
    metrics::describe_gauge!(
        "mw_spool_time_remaining_seconds",
        "When the ceiling arrives at the trailing-hour admit rate"
    );
    metrics::describe_gauge!(
        "mw_spool_origins",
        "Rows in the spool manifest's origin table"
    );
    metrics::describe_gauge!("mw_spool_cursor_state", "1 for the state each cursor is in");
    metrics::describe_counter!("mw_spool_appends_total", "Append calls committed");
    metrics::describe_counter!("mw_spool_fsyncs_total", "Group commits -- fsyncs performed");
    metrics::describe_counter!(
        "mw_spool_truncated_total",
        "Segments whose torn tail was discarded at start-up"
    );
    metrics::describe_counter!(
        "mw_spool_segments_reclaimed_total",
        "Segments deleted after every active cursor acked past them"
    );
    metrics::describe_counter!(
        "mw_spool_record_undecodable_total",
        "Records behind a valid CRC that this build could not decode"
    );
    metrics::describe_counter!(
        "mw_spool_cursor_abandoned_total",
        "Cursors given up on because they were behind longer than max_lag"
    );
    metrics::describe_counter!(
        "mw_spool_cursor_skipped_envelopes_total",
        "Records reclaimed before a re-activated cursor read them"
    );
    metrics::describe_counter!(
        "mw_ingest_duplicates_total",
        "Envelopes the dedup gate already had"
    );
    metrics::describe_counter!(
        "mw_enrich_on_accept_total",
        "Envelopes a peer sent that the enrich stage still had a column to fill"
    );
    metrics::describe_counter!(
        "mw_observations_rows_total",
        "Rows the writer put in `observations`"
    );
    metrics::describe_counter!(
        "mw_observations_rows_collapsed_total",
        "Envelopes that shared a (station, instant) with another in the same batch"
    );
    metrics::describe_counter!(
        "mw_ingest_window_overrun_total",
        "Sequence numbers that fell out of an origin's dedup window"
    );
    metrics::describe_counter!(
        "mw_sink_partial_ack_total",
        "Deliveries where a sink handled a strict prefix of the batch"
    );
    metrics::describe_histogram!(
        "mw_sink_delivery_seconds",
        "How long one delivery to a durable sink took, end to end"
    );
    metrics::describe_counter!(
        "mw_stream_lagged_total",
        "Envelopes a Lossy subscriber fell too far behind to receive"
    );
    metrics::describe_counter!(
        "mw_rollup_buckets_read_total",
        "1-minute aggregate buckets window:rollup has read into its ring"
    );
    metrics::describe_counter!(
        "mw_rollup_pieces_summed_total",
        "Aggregate-level range sums window:rollup has asked the store for"
    );
    metrics::describe_counter!(
        "mw_rain_rollovers_total",
        "Station-local day boundaries that have moved"
    );

    // Zeroed, so "nothing has gone wrong yet" is an assertion rather than an absence.
    metrics::counter!("mw_spool_appends_total").absolute(0);
    metrics::counter!("mw_spool_fsyncs_total").absolute(0);
    metrics::counter!("mw_spool_truncated_total").absolute(0);
    metrics::counter!("mw_spool_record_undecodable_total").absolute(0);
    metrics::counter!("mw_spool_segments_reclaimed_total").absolute(0);
    metrics::counter!("mw_spool_cursor_skipped_envelopes_total").absolute(0);
    metrics::counter!("mw_ingest_duplicates_total").absolute(0);
    metrics::counter!("mw_ingest_window_overrun_total").absolute(0);
    metrics::counter!("mw_rollup_buckets_read_total").absolute(0);
    metrics::counter!("mw_rollup_pieces_summed_total").absolute(0);
    // Zeroed for a reason this one specifically needs: a daily rollover increments once a
    // day per station, so a scraper that found no series could not tell "it has not been a
    // day yet" from "the component never started".
    metrics::counter!("mw_rain_rollovers_total").absolute(0);
    for reason in Reject::ALL {
        metrics::counter!("mw_readings_rejected_total", "reason" => reason.as_str()).absolute(0);
    }
    for reason in DROPPED_REASONS {
        metrics::counter!("mw_readings_dropped_total", "reason" => *reason).absolute(0);
    }
}

/// §5.10's named list for `mw_readings_dropped_total`.
///
/// `rejected` is deliberately **not** here even though §5.10 lists it: a rejection is
/// counted once, by reason, on `mw_readings_rejected_total`, and a second series that
/// double-counts the same event under a coarser label is how two dashboards come to
/// disagree. The three that remain are losses rather than refusals.
const DROPPED_REASONS: &[&str] = &["spool_full", "lagged", "out_of_range"];

/// A frame or an envelope refused before anything was stored.
pub fn rejected(reason: Reject, n: u64) {
    metrics::counter!("mw_readings_rejected_total", "reason" => reason.as_str()).increment(n);
}

/// Readings lost at the spool's ceiling (W3 §2.7).
///
/// `n` is the number of **readings**, not of batches: §5.10 defines this series as a
/// "dropped-reading count by reason", and a refused 256-envelope batch counted as 1 would
/// under-report by 256×.
pub fn dropped_spool_full(n: u64) {
    metrics::counter!("mw_readings_dropped_total", "reason" => "spool_full").increment(n);
}

/// Readings sealed and committed on the `admit` path, per station.
///
/// `station` is safe to label with **here and only here**: `admit` has already passed the
/// key through `StationRegistry::resolve`, so its values come from the configuration, which
/// is what §5.10's cardinality rule asks ("there are N of these and N is in the
/// configuration"). The `accept` path uses [`accepted`] instead, because there the key is
/// still a peer's claim.
pub fn ingested(station: &str, n: u64) {
    metrics::counter!("mw_readings_ingested_total", "station" => station.to_string()).increment(n);
}

/// Readings accepted from a peer — **unlabelled**.
///
/// §5.10: "A label whose values come from a request … is forbidden". On the `accept` path
/// the `station_key` is whatever the peer put in the envelope: the check that would bound
/// it is `station_key` resolving in `StationRegistry`, which is W7 §4.4 step 5 and M6-4's
/// seam. Labelling with it would let one peer mint unbounded series. When M6-4 lands that
/// check, this can become [`ingested`] and the two paths merge again.
pub fn accepted(station: &str) {
    let _ = station;
    metrics::counter!("mw_readings_ingested_total", "station" => "peer").increment(1);
}

/// Envelopes arriving from a peer that the enrich stage still had something to write.
///
/// **Expected to be zero, and informative when it is not.** A satellite running this build
/// enriched the envelope before it ever reached the wire (W3 §2.5 consequence 3), so a
/// non-zero rate means either a satellite older than this collector or a column this build
/// derives and that one did not — which is the version skew W7 §5.2 puts `software` on the
/// peer row for. Not an error: the collector filling it is the design working.
pub fn enriched_on_accept(n: u64) {
    metrics::counter!("mw_enrich_on_accept_total").increment(n);
}

/// Rows the writer wrote. Against `mw_readings_ingested_total` this is the write path's
/// end-to-end accounting: the two differ by the spool's lag and by the collapsing below.
pub fn rows_written(n: u64) {
    metrics::counter!("mw_observations_rows_total").increment(n);
}

/// Envelopes that shared a `(station_id, time)` with another in the same batch, and were
/// therefore written as one row.
///
/// Not a loss in the spool's sense -- both are durably handled, and a sequential upsert
/// would have left the same row standing -- but it **is** a reading that is not in the
/// database, and the unique index on `(station_id, time)` means nothing else could be. A
/// station whose rate here matches its ingest rate is a station reporting several
/// observations per frame with no timestamp on any of them.
pub fn rows_collapsed(n: u64) {
    metrics::counter!("mw_observations_rows_collapsed_total").increment(n);
}

/// 1-minute buckets one `window:rollup` tick read.
///
/// In the steady state this is **one per station per tick**, because the refresh is
/// incremental. A rate that matches the ring size is a component that is re-reading the
/// whole hour every minute, which means its ring is being thrown away -- a restart loop, or
/// a bucket stream whose instants are going backwards.
pub fn rollup_buckets_read(n: u64) {
    metrics::counter!("mw_rollup_buckets_read_total").increment(n);
}

/// Range sums one `window:rollup` tick asked for.
///
/// Bounded by the decomposition rather than by the window: a local year is about four
/// hundred *buckets* but only a handful of pieces, and this counts the pieces. It rises
/// sharply at a local new year, when the year window stops being one day long.
pub fn rollup_pieces_summed(n: u64) {
    metrics::counter!("mw_rollup_pieces_summed_total").increment(n);
}

/// One station-local day boundary moved.
///
/// **One per station per local day, including the days that are 23 and 25 hours long.** A
/// value of two for one local date would be fall-back's second pass through local midnight
/// having been counted, which is what the occurrence key exists to prevent.
pub fn rain_rolled_over() {
    metrics::counter!("mw_rain_rollovers_total").increment(1);
}

/// One current-conditions snapshot rendered into the table.
///
/// Three series, because an operator asks three different questions of a snapshot:
///
/// - `mw_snapshot_published_total{website,station}` — is the fold happening at all? In the
///   steady state this is one per website per reading, so a station at a 2-second cadence
///   bound to two websites is one per second.
/// - `mw_snapshot_age_seconds{website,station}` — §2a.6's gauge, and the number behind the
///   `X-MW-Snapshot-Age` header. It is the **reading's** age rather than the render's,
///   which is what W1's 10-minute recency rule is measured against.
///
///   **It is the age at the last fold, and it does not climb on its own.** This is written
///   only when a snapshot is published, so a station that stops reporting freezes its gauge
///   instead of crossing 600 — which is the one transition an operator would want to alert
///   on. Said here rather than left to be discovered: until something sweeps every key on a
///   timer, alert on `rate(mw_snapshot_published_total)` going flat, which *does* detect a
///   station that has gone quiet. The sweep's natural home is the scrape path, and that is
///   M4-3's (`http:listener`) — recorded on this function rather than in a work item,
///   because the number is here and the gap is a property of the number.
///   `mw_snapshot_degraded` has the same shape for the same reason.
/// - `mw_snapshot_degraded{website,station}` — **D-36**'s indicator as a number, 1 while the
///   window-derived values are being served from their last good value. This is the series
///   an alert fires on, because the frozen body cannot carry the fact and must not.
///
/// The labels are a website id and a station key, both bounded by the configuration (§5.10)
/// — W3 §2a.4's worst case is 30 pairs.
pub fn snapshot_published(
    key: &mw_component::SnapshotKey,
    observed_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    fresh: bool,
) {
    let website = key.website.to_string();
    let station = key.station.as_str().to_string();
    metrics::counter!(
        "mw_snapshot_published_total",
        "website" => website.clone(),
        "station" => station.clone(),
    )
    .increment(1);
    metrics::gauge!(
        "mw_snapshot_age_seconds",
        "website" => website.clone(),
        "station" => station.clone(),
    )
    .set((now - observed_at).num_milliseconds() as f64 / 1000.0);
    metrics::gauge!(
        "mw_snapshot_degraded",
        "website" => website,
        "station" => station,
    )
    .set(if fresh { 0.0 } else { 1.0 });
}

/// Recompute `mw_snapshot_age_seconds` and `mw_snapshot_degraded` for every published key.
///
/// **M4-1's constraint 2, and the scrape path is where it belongs.** Both gauges are written
/// by [`snapshot_published`], which runs only when a fold happens -- so a station that stops
/// reporting leaves its age gauge at whatever it was when the last reading arrived, and the
/// 600-second recency rule the frozen contract applies is never crossed in the metric. An
/// alert on `mw_snapshot_age_seconds > 600` would therefore never fire for the failure it
/// exists to catch.
///
/// Sweeping here rather than on a timer is the decision M4-1 recorded: a timer would be a
/// second clock (§3.3 allows one) and would cost work on a daemon nobody is scraping, while
/// the scrape is exactly the moment the number is read. The cost is one iteration of the
/// published table -- W3 §2a.4 bounds it at the cross product of websites and stations, 30
/// keys in W1's rig -- per scrape.
///
/// **`now` is a parameter** so that the sweep is testable without a clock, which is the same
/// shape [`snapshot_published`] already has.
///
/// A key with no snapshot yet is skipped rather than reported as age zero: a gauge that
/// claimed a fresh reading for a station that has never reported would be worse than an
/// absent series, and `/latest`'s cold-start answer is a `404` for the same reason.
pub fn sweep_snapshot_ages(state: &crate::SnapshotState, now: chrono::DateTime<chrono::Utc>) {
    // `LatestSnapshot::table()` rather than an inherent accessor, which is what M4-1's
    // constraint names: the sweep reads the same published table every request reads.
    use mw_component::LatestSnapshot as _;
    for (key, snapshot) in state.table().iter() {
        let website = key.website.to_string();
        let station = key.station.as_str().to_string();
        metrics::gauge!(
            "mw_snapshot_age_seconds",
            "website" => website.clone(),
            "station" => station.clone(),
        )
        .set(snapshot.age_seconds(now));
        metrics::gauge!(
            "mw_snapshot_degraded",
            "website" => website,
            "station" => station,
        )
        .set(if snapshot.staleness().is_fresh() {
            0.0
        } else {
            1.0
        });
    }
}

/// One key's payload could not be serialised, so its previous bytes stay published.
///
/// A counter rather than a component failure, because taking `snapshot:latest` down would
/// revoke `LatestSnapshot` and unmount a frozen route table — `crate::snapshot`'s fold has
/// the whole chain. **Any non-zero rate here is a build defect**, not an operational
/// condition: the renderer and the registry disagree about a metric, so the same reading
/// will fail the same way on the next fold.
pub fn snapshot_render_failed(key: &mw_component::SnapshotKey) {
    metrics::counter!(
        "mw_snapshot_render_failed_total",
        "website" => key.website.to_string(),
        "station" => key.station.as_str().to_string(),
    )
    .increment(1);
}

/// Envelopes the dedup gate already had (W7 §4.4). Expected and non-zero in a diamond.
pub fn duplicates(n: u64) {
    metrics::counter!("mw_ingest_duplicates_total").increment(n);
}

/// Sequence numbers that fell out of an origin's window, so the database backstop is the
/// only remaining dedup for them.
pub fn window_overrun(n: u64) {
    metrics::counter!("mw_ingest_window_overrun_total").increment(n);
}

/// One committed append call, carrying how many records it coalesced.
pub fn spool_appended(records: usize) {
    metrics::counter!("mw_spool_appends_total").increment(1);
    metrics::histogram!("mw_spool_append_records").record(records as f64);
}

/// One **group commit**. The ratio against `mw_spool_appends_total` is the coalescing,
/// which is the one thing that makes a replay burst cheap (W7 **D-13**).
///
/// Named `fsyncs` for W7 §7's spelling, and it is not a syscall count: one commit is
/// `sync_data` on the segment plus, whenever the manifest changed, that file's own
/// write-fsync-rename-fsyncdir. The ratio is what the metric is for.
pub fn spool_fsynced() {
    metrics::counter!("mw_spool_fsyncs_total").increment(1);
}

/// A record behind a valid CRC32C that this build could not decode (§3.6 step 5).
///
/// Separate from `mw_spool_truncated_total` because it is not corruption: it is a schema
/// or validation difference between the build that wrote the record and the build reading
/// it, which is exactly the case **D-11** says protobuf's compatibility rules cover. A
/// non-zero value means an in-place upgrade met a record it cannot interpret — worth
/// knowing, and not a reason to refuse the start.
pub fn spool_record_undecodable() {
    metrics::counter!("mw_spool_record_undecodable_total").increment(1);
}

/// A segment's torn tail was discarded (W7 **D-12**).
pub fn spool_truncated() {
    metrics::counter!("mw_spool_truncated_total").increment(1);
}

/// Segments deleted because every active cursor had acked past them.
pub fn spool_reclaimed(n: usize) {
    metrics::counter!("mw_spool_segments_reclaimed_total").increment(n as u64);
}

/// `max_lag` fired on a cursor.
pub fn spool_cursor_abandoned(cursor: &ComponentId) {
    metrics::counter!("mw_spool_cursor_abandoned_total", "cursor" => cursor.as_str().to_string())
        .increment(1);
}

/// Records reclaimed before a re-activated cursor read them (§3.6 step 6).
pub fn spool_cursor_skipped(n: u64) {
    metrics::counter!("mw_spool_cursor_skipped_envelopes_total").increment(n);
}

/// How much is on disk, and how many records deep the log is.
pub fn spool_depth(bytes: u64, envelopes: u64) {
    metrics::gauge!("mw_spool_depth_bytes").set(bytes as f64);
    metrics::gauge!("mw_spool_depth_envelopes").set(envelopes as f64);
}

/// Rows in the origin table, against [`crate::spool::MAX_ORIGINS`].
pub fn spool_origins(n: usize) {
    metrics::gauge!("mw_spool_origins").set(n as f64);
}

/// One cursor's lag, age and state.
///
/// One series per state rather than a state *value*, so "how long has this cursor been
/// abandoned" is a query rather than an arithmetic convention (§5.10's
/// `mw_component_state` does the same).
pub fn spool_cursor(cursor: &ComponentId, lag: u64, age_seconds: i64, state: CursorState) {
    let label = cursor.as_str().to_string();
    metrics::gauge!("mw_spool_lag_envelopes", "cursor" => label.clone()).set(lag as f64);
    metrics::gauge!("mw_spool_oldest_unacked_age_seconds", "cursor" => label.clone())
        .set(age_seconds as f64);
    for possible in [CursorState::Active, CursorState::Abandoned] {
        metrics::gauge!(
            "mw_spool_cursor_state",
            "cursor" => label.clone(),
            "state" => possible.as_str(),
        )
        .set(f64::from(u8::from(possible == state)));
    }
}

/// When the ceiling arrives at the trailing-hour rate — the number an operator watches,
/// and the one W5 renders as "about 94 days" (W7 §3.5).
pub fn spool_time_remaining(seconds: f64) {
    metrics::gauge!("mw_spool_time_remaining_seconds").set(seconds);
}

/// A sink handled a strict prefix of a batch — counted **once per delivery**, not once per
/// retry.
///
/// The distinction matters because `SpooledSink` re-offers the suffix every 50 ms while a
/// sink is in backpressure: incrementing there turned this into a 20 Hz rate that measured
/// stall *duration* in 50 ms units, under a name and a description that say it counts
/// deliveries. Duration is `mw_sink_delivery_seconds`' job.
pub fn sink_partial_ack(sink: &ComponentId, handled: usize, of: usize) {
    metrics::counter!("mw_sink_partial_ack_total", "sink" => sink.as_str().to_string())
        .increment(1);
    // §2.3's `cx.metrics().partial_ack(n, batch.len())` wants the shape of the shortfall
    // too, and the honest place for it is the delivery histogram's sibling rather than two
    // discarded arguments. Recorded as a gauge-shaped observation on the counter's own
    // labels so it is one series per sink and not one per prefix length.
    metrics::histogram!("mw_sink_partial_ack_fraction", "sink" => sink.as_str().to_string())
        .record(if of == 0 {
            0.0
        } else {
            handled as f64 / of as f64
        });
}

/// How long one delivery to a durable sink took (§5.10: "distinguishes 'slow' from
/// 'stalled'").
pub fn sink_delivery(sink: &ComponentId, seconds: f64) {
    metrics::histogram!("mw_sink_delivery_seconds", "sink" => sink.as_str().to_string())
        .record(seconds);
}

/// A `Lossy` subscriber fell more than 1024 envelopes behind.
pub fn stream_lagged(subscriber: &ComponentId, n: u64) {
    metrics::counter!("mw_stream_lagged_total", "component" => subscriber.as_str().to_string())
        .increment(n);
    metrics::counter!("mw_readings_dropped_total", "reason" => "lagged").increment(n);
}
