//! Level 3 of W3 **D-82**'s three, for Campbell Scientific: the whole driver against
//! `cmd/campbell-emulator` from the pinned reference commit.
//!
//! `../../tests/emulators/campbell/README.md` has what the emulator is and why it is
//! vendored; `emulator_davis.rs`'s header has why these tests are `#[ignore]`d and what
//! that is not.
//!
//! # The two things this level proves that the levels below it cannot
//!
//! **1. One read is not one record, on a real socket.** The emulator is a
//! `json.Encoder` writing into a TCP connection, so the kernel chooses the segment
//! boundaries: a record arrives in pieces, two records arrive together, and the `Lines`
//! session is what makes either of those one frame. A transcript can *script* that; only
//! this can produce it.
//!
//! **2. N-20's transport is the one under test.** The owner's Campbell is on USB serial and
//! the daemon is moving into Kubernetes, so the production path is `TcpOut` against a
//! `ser2net` bridge — which is exactly what
//! [`the_driver_reads_a_datalogger_over_tcp`] is, with the emulator standing in for the
//! bridge. The serial case is tested too, over a `socat`-bridged pty, because the station
//! is physically on a serial line and the bridge is what is new.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::CampbellAscii;
use mw_component::{ComponentId, Cx, Generation, Ingress};
use mw_station::driver::{Conversation, StreamStation};
use mw_station::testing::{Admission, GoEmulator, PtyBridge, RecordingIngress};
use mw_station::transport::Transport;
use mw_types::StationKey;
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "campbell";

/// The emulator's output period for these tests. Its default is two seconds, which would
/// make five readings a ten-second test for no gain.
const INTERVAL: &str = "100ms";

const DEADLINE: Duration = Duration::from_secs(90);

fn cx(id: &str) -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    (
        Cx::new(
            ComponentId::new(id).expect("a legal component id"),
            token.clone(),
            Generation::ZERO,
            rx,
        ),
        token,
    )
}

async fn collect(transport: Transport, want: usize) -> Vec<Admission> {
    let ingress = RecordingIngress::accepting();
    let station = Box::new(
        StreamStation::connect(
            &transport,
            Conversation::Lines,
            Arc::new(CampbellAscii::default()),
            StationKey::new("suncrest:meadow"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the driver connects to the emulator"),
    );
    let (context, token) = cx("station:campbell:suncrest:meadow");
    // `clippy.toml` forbids `spawn` in the daemon and this test is the supervisor; see
    // `emulator_davis.rs` for the same note.
    #[allow(clippy::disallowed_methods)]
    let driving = tokio::task::spawn(async move { station.run(context).await });

    let deadline = tokio::time::Instant::now() + DEADLINE;
    while ingress.admissions().len() < want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} of {want} readings arrived in {DEADLINE:?}",
            ingress.admissions().len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), driving).await;
    ingress.admissions()
}

/// Every reading carries the ten keys the datalogger's program outputs, in the registry's
/// units, and the values **move**.
fn assert_live(admissions: &[Admission], want: usize) {
    assert!(
        admissions.len() >= want,
        "expected at least {want} readings, got {}",
        admissions.len()
    );
    let temps: Vec<f32> = admissions
        .iter()
        .map(|a| {
            a.batch[0]
                .values
                .out_temp
                .expect("`airtemp_f` is in every packet the emulator writes")
        })
        .collect();
    for temp in &temps {
        assert!(
            (-60.0..=160.0).contains(temp),
            "{temp} degF is not a temperature; the key mapping is wrong"
        );
    }
    assert!(
        temps.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same temperature: {temps:?}"
    );
    for admission in admissions {
        assert_eq!(admission.receiver, "station:campbell:suncrest:meadow");
        assert_eq!(admission.presented_key, None, "a Fixed identity");
        let r = &admission.batch[0].values;
        // The keys the emulator's own generator always produces a non-zero value for, which
        // are therefore always on the wire.
        assert!(r.out_humidity.is_some(), "`rh`");
        assert!(r.barometer.is_some(), "`baro`");
        assert!(r.extra_temp1.is_some(), "`baro_temp_f`");
        assert!(r.wind_speed.is_some(), "`wind_s`");
        assert!(r.station_battery_voltage.is_some(), "`batt_volt`");

        // **DEV-8, against the pinned oracle.** Every field of the emulator's packet struct
        // is `omitempty`, so a value that happens to be zero is **not on the wire at all**:
        // `slr_w` and `slr_mj` are absent between dusk and dawn, `rain_in` is absent 98% of
        // the time, and `wind_d` is absent whenever the generator picks due north. The Go
        // driver stores `0.0` for each of those, which is indistinguishable from a sensor
        // that reported zero; this stores NULL. What is asserted is the shape of that
        // correction rather than a particular hour's values:
        //
        //  - the two solar keys are derived from one number, so they arrive or are absent
        //    **together**;
        //  - and an absent key is `None`, never `Some(0.0)` -- which is the whole
        //    difference between the two implementations and is checkable at any hour.
        assert_eq!(
            r.solar_watts.is_some(),
            r.solar_joules.is_some(),
            "`slr_w` and `slr_mj` are one value in the emulator's generator: {r:?}"
        );
        for (key, value) in [
            ("slr_w", r.solar_watts),
            ("slr_mj", r.solar_joules),
            ("rain_in", r.rain_incremental),
            ("wind_d", r.wind_dir),
        ] {
            assert_ne!(
                value,
                Some(0.0),
                "`{key}` is `omitempty`, so a zero is an absent key and not a stored zero"
            );
        }
        // The enrich stage's three, including the one only this driver ever computed
        // (DEV-9's `potentialsolarwatts`).
        assert_eq!(r.wind_chill, None);
        assert_eq!(r.heat_index, None);
        assert_eq!(r.potential_solar_watts, None);
    }
}

/// **N-20's production path**: `tcp_out` at what is a `ser2net` bridge in the field.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn the_driver_reads_a_datalogger_over_tcp() {
    let (_emulator, port) = GoEmulator::serving_tcp(EMULATOR, &["-interval", INTERVAL]).await;
    let admissions = collect(
        Transport::TcpOut {
            host: "127.0.0.1".into(),
            port,
            tls: None,
        },
        5,
    )
    .await;
    assert_live(&admissions, 5);
}

/// The same driver over a character device, which is where the station physically is.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn the_driver_reads_a_datalogger_over_a_serial_line() {
    let (_emulator, port) = GoEmulator::serving_tcp(EMULATOR, &["-interval", INTERVAL]).await;
    let bridge = PtyBridge::spawn("campbell", port).await.expect(
        "`socat` is a test dependency of this item: W3 D-83 requires the serial path to be \
         exercised over a real character device",
    );
    let admissions = collect(
        Transport::Serial {
            dev: bridge.device().to_path_buf(),
            baud: 19_200,
        },
        5,
    )
    .await;
    assert_live(&admissions, 5);
}

/// The framing property, made observable: at the emulator's **fastest** output period the
/// records arrive faster than the reads, so several land in one read — and the count is
/// still one reading per record. A driver that treated a read as a frame would admit fewer
/// readings than the emulator wrote, and would also admit some that failed to parse.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn records_that_arrive_together_are_still_one_reading_each() {
    let (_emulator, port) = GoEmulator::serving_tcp(EMULATOR, &["-interval", "1ms"]).await;
    let admissions = collect(
        Transport::TcpOut {
            host: "127.0.0.1".into(),
            port,
            tls: None,
        },
        200,
    )
    .await;
    assert!(admissions.len() >= 200);
    // Every one of them decoded: the session never handed the codec half a record, which is
    // the whole of what `Lines` is for. A failure here is a rising `undecodable` count, and
    // a recording ingress sees only what was admitted -- so the assertion is that there are
    // as many readings as records and that each is complete.
    for admission in &admissions {
        assert_eq!(admission.batch.len(), 1);
        let r = &admission.batch[0].values;
        assert!(
            r.out_temp.is_some() && r.out_humidity.is_some() && r.barometer.is_some(),
            "a partial record was decoded as a whole one: {r:?}"
        );
    }
    // And the values move across two hundred records, which is what says the driver is
    // reading the stream rather than re-admitting one record.
    let temps: Vec<Option<f32>> = admissions
        .iter()
        .map(|a| a.batch[0].values.out_temp)
        .collect();
    assert!(
        temps.windows(2).any(|pair| pair[0] != pair[1]),
        "two hundred records carried one temperature"
    );
}

/// A datalogger that hangs up is a reconnect, not a failure the station stays in. The
/// emulator closes the connection when its write fails, and killing it is the same event
/// from this side.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_datalogger_that_hangs_up_ends_the_conversation_retryably() {
    let (emulator, port) = GoEmulator::serving_tcp(EMULATOR, &["-interval", INTERVAL]).await;
    let ingress = RecordingIngress::accepting();
    let station = Box::new(
        StreamStation::connect(
            &Transport::TcpOut {
                host: "127.0.0.1".into(),
                port,
                tls: None,
            },
            Conversation::Lines,
            Arc::new(CampbellAscii::default()),
            StationKey::new("suncrest:meadow"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the driver connects"),
    );
    let (context, _token) = cx("station:campbell:suncrest:meadow");
    #[allow(clippy::disallowed_methods)]
    let driving = tokio::task::spawn(async move { station.run(context).await });

    // One reading first, so that the conversation is established before it is broken.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while ingress.admissions().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no reading arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(emulator);

    let e = tokio::time::timeout(DEADLINE, driving)
        .await
        .expect("the driver noticed the connection was gone")
        .expect("the task did not panic")
        .expect_err("the datalogger hung up");
    assert_eq!(
        mw_types::Classify::class(&e),
        mw_types::Class::RETRYABLE,
        "a reconnect, not a give-up (D-27): {}",
        mw_types::DisplayChain(&e)
    );
}
