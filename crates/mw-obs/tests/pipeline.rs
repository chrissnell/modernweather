//! The stack as it is actually assembled: suppression with whole-pipeline reach, two
//! rings, and a record built from typed values rather than from re-parsed text.
//!
//! An integration test rather than a unit test because the claims are about `Layered`:
//! that `event_enabled` ANDs across the stack (§5.2), that a span's fields reach an event
//! that did not repeat them (§5.4), and that the access stream does not evict the
//! application stream (§5.7). None of those is observable from one layer.

use std::sync::Arc;

use mw_obs::{
    Dedupe, DedupeLayer, LogRings, Policy, RingLayer, Stream, Value, ACCESS_TARGET, DEDUPE,
    DEDUPE_TARGET,
};
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

/// The stack, minus the two layers that would write to the process's stderr and claim its
/// one metrics recorder.
fn stack(rings: Arc<LogRings>) -> (impl tracing::Subscriber + Send + Sync, Arc<Dedupe>) {
    stack_with(rings, DEDUPE)
}

fn stack_with(
    rings: Arc<LogRings>,
    policy: Policy,
) -> (impl tracing::Subscriber + Send + Sync, Arc<Dedupe>) {
    let dedupe = Arc::new(Dedupe::new(policy));
    let subscriber = tracing_subscriber::registry()
        .with(DedupeLayer::new(Arc::clone(&dedupe)))
        .with(RingLayer::app(Arc::clone(&rings)))
        .with(RingLayer::access(rings));
    (subscriber, dedupe)
}

#[test]
fn a_record_carries_the_enclosing_span_fields_without_the_event_repeating_them() {
    let rings = Arc::new(LogRings::new());
    let (subscriber, _dedupe) = stack(Arc::clone(&rings));
    tracing::subscriber::with_default(subscriber, || {
        let span =
            tracing::info_span!("deliver", component = "aprs:backyard", station = "backyard");
        let _entered = span.enter();
        warn!(
            metric = "out_temp",
            value = -412.0,
            reason = "below_range",
            disposition = "dropped",
            "reading discarded"
        );
    });

    let records = rings.ring(Stream::App).snapshot();
    assert_eq!(records.len(), 1, "{records:#?}");
    let r = &records[0];
    assert_eq!(r.message, "reading discarded");
    assert_eq!(r.spans, vec!["deliver"]);
    assert_eq!(
        r.field("station"),
        Some(&Value::Str("backyard".into())),
        "§5.4: the span's station is on the event"
    );
    assert_eq!(
        r.field("component"),
        Some(&Value::Str("aprs:backyard".into()))
    );
    assert_eq!(
        r.field("value"),
        Some(&Value::F64(-412.0)),
        "the ring stores the typed value, not a formatted one"
    );
    assert_eq!(r.field("disposition"), Some(&Value::Str("dropped".into())));
}

#[test]
fn suppression_reaches_every_sink_and_the_first_three_are_not_delayed() {
    let rings = Arc::new(LogRings::new());
    let (subscriber, dedupe) = stack(Arc::clone(&rings));
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..200 {
            warn!(port = "/dev/ttyUSB0", "serial port unavailable");
        }
    });

    assert_eq!(
        rings.ring(Stream::App).len(),
        usize::try_from(DEDUPE.burst).unwrap(),
        "the ring sees the burst and nothing after it: `event_enabled` is ANDed \
         across the stack, so one layer's `false` suppresses every sink"
    );
    let summaries = dedupe.sweep_at(std::time::Instant::now() + DEDUPE.summary_every);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert_eq!(summaries[0].suppressed, 197);
}

#[test]
fn the_access_stream_lands_in_its_own_ring_and_cannot_evict_the_application_one() {
    let rings = Arc::new(LogRings::with_capacities(8, 16));
    let (subscriber, _dedupe) = stack(Arc::clone(&rings));
    tracing::subscriber::with_default(subscriber, || {
        warn!(sink = "writer:timescale", "sink delivery failed");
        // §5.7's measured asymmetry: `/latest` is 93% of ~19,000 requests, so the access
        // stream is the one that overflows -- which is only true because the access target
        // is exempt from repetition control. One loop, one callsite: a token bucket over
        // it would cap this at three records and the ring would never fill.
        for i in 0..40 {
            tracing::info!(
                target: ACCESS_TARGET,
                class = "ignorable",
                route = "/latest",
                status = 200,
                i,
                "request"
            );
        }
    });

    let app = rings.ring(Stream::App).snapshot();
    assert_eq!(app.len(), 1, "{app:#?}");
    assert_eq!(app[0].message, "sink delivery failed");
    assert_eq!(
        rings.ring(Stream::App).dropped(),
        0,
        "an afternoon of traffic must not evict the one ERROR the operator wants"
    );

    let access = rings.ring(Stream::Access);
    assert_eq!(access.len(), 16, "bounded at its own capacity");
    assert_eq!(access.dropped(), 24);
    let snapshot = access.snapshot();
    assert!(snapshot[0].is_gap(), "a gap record precedes the backfill");
    assert_eq!(snapshot[0].field("dropped"), Some(&Value::U64(24)));
    assert_eq!(
        snapshot[0].field("stream"),
        Some(&Value::Str("access".into())),
        "the UI shows which stream lost records"
    );
}

#[test]
fn merged_reads_as_one_screen_with_one_clock() {
    let rings = Arc::new(LogRings::new());
    let (subscriber, _dedupe) = stack(Arc::clone(&rings));
    tracing::subscriber::with_default(subscriber, || {
        info!(target: ACCESS_TARGET, route = "/latest", status = 500, "request");
        warn!(sink = "writer:timescale", "sink delivery failed");
        info!(target: ACCESS_TARGET, route = "/snow", status = 200, "request");
    });

    let merged = rings.merged();
    assert_eq!(merged.len(), 3, "{merged:#?}");
    assert!(
        merged.windows(2).all(|w| w[0].ts <= w[1].ts),
        "merged in timestamp order"
    );
    assert_eq!(
        merged.iter().map(|r| r.stream).collect::<Vec<_>>(),
        vec![Stream::Access, Stream::App, Stream::Access],
        "correlating a 500 with the query that failed is the whole job, so the two \
         streams interleave in one view"
    );
}

#[test]
fn the_suppression_summary_is_an_event_like_any_other_and_is_not_itself_suppressed() {
    let rings = Arc::new(LogRings::new());
    // A zero-length summary window, because the alternative is a sixty-second test. The
    // arithmetic over a real window is `sweep_at`'s unit test; what this covers is the
    // emission: that `sweep` produces a real event, through the real stack, carrying the
    // count -- and that the summaries do not throttle each other.
    let (subscriber, dedupe) = stack_with(
        Arc::clone(&rings),
        Policy {
            summary_every: std::time::Duration::ZERO,
            ..DEDUPE
        },
    );
    tracing::subscriber::with_default(subscriber, || {
        for station in ["backyard", "roof"] {
            for _ in 0..10 {
                warn!(station, port = "/dev/ttyUSB0", "serial port unavailable");
            }
        }
        dedupe.sweep();
    });

    let summaries: Vec<_> = rings
        .ring(Stream::App)
        .snapshot()
        .into_iter()
        .filter(|r| r.target == DEDUPE_TARGET)
        .collect();
    assert_eq!(
        summaries.len(),
        2,
        "one per key, and the second is not silenced by the first: {summaries:#?}"
    );
    for summary in &summaries {
        assert_eq!(summary.message, "repeated event suppressed");
        assert_eq!(
            summary.field("suppressed"),
            Some(&Value::U64(7)),
            "ten occurrences, three of them emitted"
        );
        assert!(
            summary.field("callsite").is_some(),
            "the operator is told where they came from: {summary:#?}"
        );
    }
}
