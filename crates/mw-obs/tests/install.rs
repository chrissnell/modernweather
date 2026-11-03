//! `install` itself, in a test binary of its own.
//!
//! Its own file because both things it claims are process-global: the `tracing` subscriber
//! and the `metrics` recorder. One test per process is the only honest way to assert that
//! claiming them works, that claiming them twice is reported rather than ignored, and that
//! the handles it hands back are wired to the layers it installed.

use mw_obs::{Obs, ObsError, Options, Requested, Shape, Stream};

#[test]
fn install_claims_the_process_globals_once_and_reports_a_second_attempt() {
    let options = Options {
        directive: Some("info,mw_obs=trace".into()),
        ring_directive: None,
        format: Some(Requested::Json),
    };
    let obs: Obs = mw_obs::install(&options).expect("the first install claims the globals");
    assert_eq!(obs.shape(), Shape::Json, "the explicit request wins (§5.3)");
    assert_eq!(obs.directive(), "info,mw_obs=trace");

    // The subscriber is live: an event reaches the ring the handle hands back.
    tracing::warn!(station = "backyard", "reading discarded");
    let records = obs.rings().ring(Stream::App).snapshot();
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(records[0].message, "reading discarded");

    // The log handle is the one the reload layer holds.
    let log = obs.log();
    assert_eq!(log.current(), "info,mw_obs=trace");
    log.set("warn").expect("a valid directive is adopted");
    assert_eq!(log.current(), "warn");
    tracing::info!("below the new level");
    assert_eq!(
        obs.rings().ring(Stream::App).snapshot().len(),
        1,
        "the reload took effect for the whole pipeline"
    );
    log.reset()
        .expect("reset returns to the starting directive");

    // The recorder is installed and its own counters are described and zeroed, so a
    // scrape of a daemon that has done nothing is still a payload.
    let payload = obs.metrics().render();
    for expected in [
        "mw_ring_dropped_total{stream=\"app\"} 0",
        "mw_ring_dropped_total{stream=\"access\"} 0",
        "mw_dedupe_evicted_total 0",
    ] {
        assert!(
            payload.contains(expected),
            "{expected} missing from {payload}"
        );
    }

    // The binary's loop tick drives both periodic jobs; neither may panic.
    obs.tick();

    let again = mw_obs::install(&options).expect_err("the globals are claimed");
    assert!(
        matches!(
            again,
            ObsError::SubscriberInstalled { .. } | ObsError::RecorderInstalled { .. }
        ),
        "{again:?} should name the global that was already taken"
    );
}
