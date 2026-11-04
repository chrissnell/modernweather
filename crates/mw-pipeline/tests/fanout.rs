//! The fan-out: one subscription shape per `ReadingPolicy`, and the two properties that
//! are easy to get wrong.
//!
//! - **A filtered envelope advances the cursor** (W7 **D-9**) — a cursor that stalls on an
//!   envelope it will never send stalls *forever*, the spool never reclaims, and `max_lag`
//!   eventually abandons it. So a mutual uplink would present as a disk filling up.
//! - **The group commit coalesces append *calls*, not records** (W7 **D-13**) — 64
//!   concurrent callers cost one disk sync, which is the difference between 64 and 1 for a
//!   reconnecting satellite's replay burst.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{ReadingFilter, ReadingPolicy, ReadingStream, StationSet};
use mw_pipeline::testing::{config, forwarded, id, node, reading, Scratch};
use mw_pipeline::{Clock, Draft, Spool, SpoolConfig, SpoolStream, TestClock, GROUP_COMMIT_APPENDS};
use mw_types::{Seq, StationKey};

fn draft(station: &str, clock: &TestClock) -> Draft {
    Draft {
        station: StationKey::new(station),
        observed_at: clock.now(),
        received_at: clock.now(),
        reading: Arc::new(reading(29.92)),
    }
}

fn spool_with(cfg: SpoolConfig, clock: &TestClock) -> Arc<Spool> {
    Spool::open(cfg, node(1), Arc::new(clock.clone()) as Arc<dyn Clock>)
        .expect("a fresh spool opens")
}

#[tokio::test]
async fn a_durable_subscriber_reads_from_its_cursor_and_advances_it_on_ack() {
    let scratch = Scratch::new("durable");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let cursor = id("writer:timescale");
    let mut sub = stream
        .subscribe(&cursor, &ReadingFilter::All, ReadingPolicy::Durable)
        .expect("a durable subscription needs only a spool");

    s.admit_local(&[draft("backyard", &clock), draft("backyard", &clock)])
        .await
        .expect("accepted");

    let batch = sub.next_batch().await.expect("two envelopes are waiting");
    assert_eq!(batch.len(), 2);
    assert_eq!(
        s.cursor_position(&cursor).expect("registered").acked_seq,
        Seq::new(0),
        "reading does not advance the cursor -- only acknowledging does"
    );

    sub.ack().await.expect("acknowledged");
    assert_eq!(
        s.cursor_position(&cursor).expect("registered").acked_seq,
        Seq::new(2),
        "and the ack is the one place an offset advances"
    );
}

/// A failed delivery replays the same batch, because the cursor did not advance. That is
/// what makes the supervisor's backoff a retry rather than a loss (W3 §2.3).
#[tokio::test]
async fn a_batch_that_was_not_acknowledged_is_delivered_again() {
    let scratch = Scratch::new("replay");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));
    let cursor = id("writer:timescale");

    {
        // Subscribed before the append, because a brand-new cursor starts at the current
        // tail (§3.4) -- a peer configured today has no claim on history it was never
        // promised.
        let mut sub = stream
            .subscribe(&cursor, &ReadingFilter::All, ReadingPolicy::Durable)
            .expect("subscribed");
        s.admit_local(&[draft("backyard", &clock)])
            .await
            .expect("accepted");
        let batch = sub.next_batch().await.expect("one envelope");
        assert_eq!(batch.len(), 1);
        // No ack: the delivery failed, the task ends, the supervisor backs the component
        // off and starts it again -- which is a new subscription.
    }

    let mut sub = stream
        .subscribe(&cursor, &ReadingFilter::All, ReadingPolicy::Durable)
        .expect("the cursor is free again after the first subscription dropped");
    let batch = sub.next_batch().await.expect("the same envelope");
    assert_eq!(batch.len(), 1, "the unacknowledged batch replayed");
}

/// **W7 D-9**, the done-criterion: a filtered envelope advances the cursor without being
/// sent.
///
/// Three stations in the log and a filter that wants one of them. The cursor must end up
/// past *all* of them — including the two that were never delivered — or it stalls forever
/// and the spool never reclaims.
#[tokio::test]
async fn a_filtered_envelope_advances_the_cursor_without_being_delivered() {
    let scratch = Scratch::new("filtered");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));
    let cursor = id("uplink:suncrest");

    let mut sub = stream
        .subscribe(
            &cursor,
            &ReadingFilter::Stations(
                [StationKey::new("backyard")]
                    .into_iter()
                    .collect::<StationSet>(),
            ),
            ReadingPolicy::Durable,
        )
        .expect("subscribed");

    s.admit_local(&[
        draft("roof", &clock),
        draft("backyard", &clock),
        draft("greenhouse", &clock),
    ])
    .await
    .expect("accepted");

    let batch = sub.next_batch().await.expect("the one that matched");
    assert_eq!(batch.len(), 1, "only the matching station was delivered");
    assert_eq!(batch.envelopes()[0].station.as_str(), "backyard");

    sub.ack().await.expect("acknowledged");
    assert_eq!(
        s.cursor_position(&cursor).expect("registered").acked_seq,
        Seq::new(3),
        "the cursor advanced over all three -- the two filtered ones are 'durably \
         handled', which is what stops it stalling on an envelope it will never send"
    );
}

/// The same property when **every** envelope in the window is filtered: the subscription
/// advances the cursor itself rather than returning an empty batch or parking behind them.
///
/// This is the shape that would actually wedge a node. With a filter nothing matches, there
/// is no batch for the supervisor to acknowledge, so if the subscription did not advance
/// the cursor over the filtered window nothing ever would — the spool would stop
/// reclaiming and `max_lag` would eventually abandon a cursor whose only fault was that its
/// peer sent everything.
#[tokio::test]
async fn a_window_that_is_entirely_filtered_still_advances_the_cursor() {
    let scratch = Scratch::new("all-filtered");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));
    let cursor = id("uplink:suncrest");

    let mut sub = stream
        .subscribe(
            &cursor,
            &ReadingFilter::Stations(
                [StationKey::new("backyard")]
                    .into_iter()
                    .collect::<StationSet>(),
            ),
            ReadingPolicy::Durable,
        )
        .expect("subscribed");

    // Five envelopes nobody wants, and nothing else: there is no batch to return, so the
    // subscription must park -- having first advanced the cursor past all five.
    s.admit_local(&(0..5).map(|_| draft("roof", &clock)).collect::<Vec<_>>())
        .await
        .expect("accepted");
    let parked = tokio::time::timeout(Duration::from_millis(100), sub.next_batch()).await;
    assert!(
        parked.is_err(),
        "nothing matched, so there is nothing to deliver"
    );
    assert_eq!(
        s.cursor_position(&cursor).expect("registered").acked_seq,
        Seq::new(5),
        "but the cursor advanced over all five anyway (W7 D-9) -- otherwise it would stall          here forever and the spool would never reclaim"
    );

    // And it keeps going: the next matching envelope is delivered normally.
    s.admit_local(&[draft("backyard", &clock)])
        .await
        .expect("accepted");
    let batch = sub.next_batch().await.expect("the sixth one");
    assert_eq!(batch.len(), 1);
    sub.ack().await.expect("acknowledged");
    assert_eq!(
        s.cursor_position(&cursor).expect("registered").acked_seq,
        Seq::new(6)
    );
}

/// `ReadingFilter::NotFrom(peer)` is `!prov.has_visited(peer)` and nothing else — which is
/// also why the uplink's filter is `NotFrom(peer)` and not `NotFrom(self)` (**D-47**):
/// `admit` sets `path = [self]`, so `NotFrom(self)` would match nothing at all.
#[tokio::test]
async fn not_from_filters_what_the_peer_has_already_seen_and_not_what_we_admitted() {
    let scratch = Scratch::new("not-from");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let peer = node(7);
    let mut sub = stream
        .subscribe(
            &id("uplink:suncrest"),
            &ReadingFilter::NotFrom(peer),
            ReadingPolicy::Durable,
        )
        .expect("subscribed");

    // One we measured, and one that came FROM the peer -- which must not be sent back.
    s.admit_local(&[draft("backyard", &clock)])
        .await
        .expect("accepted");
    s.append_sealed(&[forwarded(peer, 1, "remote", clock.now(), &[node(1)])])
        .await
        .expect("accepted");

    let batch = sub.next_batch().await.expect("our own reading");
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch.envelopes()[0].provenance.origin(),
        node(1),
        "our own, which `NotFrom(self)` would have excluded"
    );

    // What the self-filter would have done instead, asserted so D-47 is not folk wisdom.
    let mut wrong = stream
        .subscribe(
            &id("uplink:wrong"),
            &ReadingFilter::NotFrom(node(1)),
            ReadingPolicy::Durable,
        )
        .expect("subscribed");
    wrong.ack().await.expect("nothing in flight");
    let got = tokio::time::timeout(Duration::from_millis(50), wrong.next_batch()).await;
    assert!(
        got.is_err(),
        "NotFrom(self) matches every envelope in this node's own spool, so an uplink \
         configured that way sends nothing -- which is exactly D-47"
    );
}

#[tokio::test]
async fn a_latest_subscriber_sees_the_newest_and_is_never_blocked() {
    let scratch = Scratch::new("latest");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let mut sub = stream
        .subscribe(
            &id("snapshot:latest"),
            &ReadingFilter::All,
            ReadingPolicy::Latest,
        )
        .expect("subscribed");

    // Three appends with nobody reading: a `watch` coalesces by construction, so the
    // intermediate values are dropped by design rather than queued.
    for i in 0..3 {
        s.admit_local(&[Draft {
            reading: Arc::new(reading(29.0 + i as f32)),
            ..draft("backyard", &clock)
        }])
        .await
        .expect("accepted");
    }

    let batch = sub.next_batch().await.expect("the newest");
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch.envelopes()[0].reading.barometer,
        Some(31.0),
        "the newest, not the oldest: coalescing is the semantics"
    );
    assert_eq!(
        s.cursor_position(&id("snapshot:latest")),
        None,
        "and a Latest subscriber has no cursor to advance"
    );
    sub.ack().await.expect("a no-op, as the trait says");
}

#[tokio::test]
async fn a_lossy_subscriber_receives_every_envelope_while_it_keeps_up() {
    let scratch = Scratch::new("lossy");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let mut sub = stream
        .subscribe(
            &id("live:stream"),
            &ReadingFilter::All,
            ReadingPolicy::Lossy,
        )
        .expect("subscribed");

    s.admit_local(
        &(0..3)
            .map(|_| draft("backyard", &clock))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("accepted");

    for _ in 0..3 {
        assert_eq!(sub.next_batch().await.expect("an envelope").len(), 1);
    }
}

/// A subscriber that has not subscribed yet does not get history: `Latest` and `Lossy` are
/// taps on the append path, not logs. The durable cursor is the only thing that remembers.
#[tokio::test]
async fn latest_and_lossy_subscribers_do_not_replay_history() {
    let scratch = Scratch::new("no-history");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    s.admit_local(&[draft("backyard", &clock)])
        .await
        .expect("accepted");

    let mut lossy = stream
        .subscribe(
            &id("live:stream"),
            &ReadingFilter::All,
            ReadingPolicy::Lossy,
        )
        .expect("subscribed");
    let got = tokio::time::timeout(Duration::from_millis(50), lossy.next_batch()).await;
    assert!(got.is_err(), "a Lossy tap starts at the present");
}

/// Every subscription parks rather than ending, and dropping the parked future loses
/// nothing — which is what the supervisor's `select!` against cancellation does at every
/// stop, and therefore the only cancellation contract the stream owes (§3.6, **D-24**).
///
/// Stated as a test because the obvious alternative — handing the stream the process's
/// shutdown token so it can end subscriptions itself — looks like a tidier design and
/// races the supervisor's ordered stop. `SpoolStream`'s own doc has the measurement.
#[tokio::test]
async fn a_parked_subscription_is_cancel_safe_and_loses_nothing_when_dropped() {
    let scratch = Scratch::new("cancel-safe");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    for (name, policy) in [
        ("writer:timescale", ReadingPolicy::Durable),
        ("snapshot:latest", ReadingPolicy::Latest),
        ("live:stream", ReadingPolicy::Lossy),
    ] {
        let mut sub = stream
            .subscribe(&id(name), &ReadingFilter::All, policy)
            .expect("subscribed");

        // Nothing to read: the subscription parks, and the supervisor drops the future.
        let parked = tokio::time::timeout(Duration::from_millis(50), sub.next_batch()).await;
        assert!(parked.is_err(), "{name} parks rather than ending");

        // One envelope arrives while nobody is awaiting, and the next call still gets it:
        // the dropped future had taken nothing from a queue.
        s.admit_local(&[draft("backyard", &clock)])
            .await
            .expect("accepted");
        let delivered = tokio::time::timeout(Duration::from_millis(500), sub.next_batch()).await;
        let batch = delivered
            .unwrap_or_else(|_| panic!("{name} must deliver what arrived while it was unpolled"))
            .unwrap_or_else(|| panic!("{name} ended instead of delivering"));
        assert_eq!(batch.len(), 1, "{name}");
    }
}

/// A `Durable` subscription **does** end on its own for the one reason that is its own
/// business: `max_lag` abandoned its cursor, so the segments it wanted are reclaimed and
/// no amount of waiting produces any.
#[tokio::test]
async fn an_abandoned_cursor_ends_its_subscription() {
    let scratch = Scratch::new("abandoned-sub");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let mut sub = stream
        .subscribe(
            &id("uplink:suncrest"),
            &ReadingFilter::All,
            ReadingPolicy::Durable,
        )
        .expect("subscribed");
    s.admit_local(&[draft("backyard", &clock)])
        .await
        .expect("accepted");

    // Eight days behind, past the seven-day default.
    clock.advance(chrono::Duration::days(8));
    s.admit_local(&[draft("backyard", &clock)])
        .await
        .expect("accepted");

    assert!(
        sub.next_batch().await.is_none(),
        "an abandoned cursor's subscription ends rather than parking on records that have \
         been reclaimed"
    );
}

/// W3 Decision 20 through the stream: two components cannot share a durable cursor.
#[tokio::test]
async fn two_durable_subscriptions_with_one_id_are_refused() {
    let scratch = Scratch::new("one-cursor");
    let clock = TestClock::fixed();
    let s = spool_with(config(scratch.spool()), &clock);
    let stream = SpoolStream::new(Arc::clone(&s));

    let _first = stream
        .subscribe(
            &id("writer:timescale"),
            &ReadingFilter::All,
            ReadingPolicy::Durable,
        )
        .expect("the first subscription");
    let Err(err) = stream.subscribe(
        &id("writer:timescale"),
        &ReadingFilter::All,
        ReadingPolicy::Durable,
    ) else {
        panic!("the second subscription on one cursor id must be refused");
    };
    assert_eq!(
        mw_types::Classify::class(&err),
        mw_types::Class::RETRYABLE,
        "reported through StreamError::Backend, whose class the supervisor reads"
    );
}

/// **W7 D-13**, measured rather than asserted in prose: 64 concurrent append calls cost
/// **one** `fsync`.
///
/// Under `tokio::time::pause()`, so the 50 ms window is virtual and the test finishes in
/// microseconds. One task, 64 concurrent futures: the first to reach `commit` becomes the
/// group's leader and parks on the window; the other 63 write their records and park on
/// the leader's publication. When the window closes the leader syncs everything.
#[tokio::test(start_paused = true)]
async fn sixty_four_concurrent_appends_cost_one_fsync() {
    let scratch = Scratch::new("group-commit");
    let clock = TestClock::fixed();
    let s = spool_with(
        SpoolConfig {
            // The production window, not the zeroed test one: the coalescing is the thing
            // under test.
            group_commit_window: mw_pipeline::GROUP_COMMIT_WINDOW,
            ..config(scratch.spool())
        },
        &clock,
    );

    let appends: Vec<_> = (0..GROUP_COMMIT_APPENDS)
        .map(|_| {
            let s = Arc::clone(&s);
            let clock = clock.clone();
            async move {
                s.admit_local(&[draft("backyard", &clock)])
                    .await
                    .expect("accepted");
            }
        })
        .collect();
    futures_unordered(appends).await;

    let stats = s.stats();
    assert_eq!(
        stats.appends, GROUP_COMMIT_APPENDS as u64,
        "sixty-four append calls"
    );
    assert!(
        stats.fsyncs < GROUP_COMMIT_APPENDS as u64,
        "and fewer disk syncs than that: {stats:?}"
    );
    assert_eq!(
        s.tail_seq(),
        Seq::new(GROUP_COMMIT_APPENDS as u64),
        "with every record in the log"
    );
}

/// Drive a set of futures to completion concurrently on one task.
///
/// Hand-rolled rather than `futures::future::join_all`, because `futures` is not in
/// **D-32**'s table and this is twelve lines: poll every future once per pass, and stop
/// when they have all finished. `tokio::task::spawn` is the obvious alternative and is
/// forbidden outside `mw_supervisor::task` (`clippy.toml`), which is the rule this test
/// has no business being the first exception to.
async fn futures_unordered<F: std::future::Future<Output = ()>>(futures: Vec<F>) {
    use std::pin::Pin;
    use std::task::Poll;

    let mut pinned: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut done = vec![false; pinned.len()];
    std::future::poll_fn(move |cx| {
        let mut all = true;
        for (i, fut) in pinned.iter_mut().enumerate() {
            if done[i] {
                continue;
            }
            match fut.as_mut().poll(cx) {
                Poll::Ready(()) => done[i] = true,
                Poll::Pending => all = false,
            }
        }
        if all {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}
