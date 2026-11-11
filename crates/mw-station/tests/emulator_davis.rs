//! Level 3 of W3 **D-82**'s three, for Davis: the whole driver against
//! `cmd/davis-emulator` from the pinned reference commit.
//!
//! **Ignored by default, and that is not a skip.** `cargo test` reports these as `ignored`,
//! which is visible; `cargo xtask emulator-tests` — the `driver-emulators` gate's command —
//! runs them with `--ignored` and asserts that exactly the declared set of them **passed**,
//! so a gate that ran nothing fails. `tests/emulators/manifest.toml` declares the set by
//! name.
//!
//! # What this level adds over the two below it
//!
//! | Property | Where it comes from |
//! |---|---|
//! | an **independently written** oracle | `tests/emulators/davis` is verbatim from the reference tree, written years before this programme (**D-40**, **A-W8-4**) |
//! | a separate process, a different language | the emulator is Go |
//! | real sockets | `tcp_out` straight at the emulator's port |
//! | a real **character device** | `socat` bridges a pty to the same port (**D-83**) |
//! | **more than one batch** | the assertion the Davis defect is about |
//! | corrupted frames, bad CRCs, truncation, hangs, dropped connections | the emulator's own `-flaky` family |
//!
//! The last row is why this emulator is the strongest oracle in the programme. Its flaky
//! modes were written to break the Go driver and they are enabled here at rates high enough
//! to fire within a test: what is asserted is that the driver **keeps producing readings**
//! through them, which no golden test and no transcript can say.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::DavisLoop;
use mw_component::{ComponentId, Cx, Generation, Ingress};
use mw_station::driver::{Conversation, StreamStation};
use mw_station::testing::{GoEmulator, PtyBridge, RecordingIngress};
use mw_station::transport::Transport;
use mw_types::StationKey;
use tokio_util::sync::CancellationToken;

/// The emulator directory, which is also the manifest's `path`.
const EMULATOR: &str = "davis";

/// How many packets one command asks for in these tests.
///
/// Small, because the emulator sends one every 1.5 s: two runs of three is nine seconds of
/// packets, which is enough to prove the re-issue and short enough to be a test.
const RUN_LENGTH: usize = 3;

/// How long to wait for a given number of readings.
///
/// A bound rather than a measurement, and generous on purpose: the emulator's flaky modes
/// include a 3-8 second hang and a 5-15 second slow response, so a run that hits several of
/// them legitimately takes a minute. A test that failed for being slow would be a flaky
/// test about flaky hardware.
const DEADLINE: Duration = Duration::from_secs(240);

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

/// Run the real driver against a transport until `want` readings have been admitted,
/// **reconnecting the way the supervisor would**.
///
/// The reconnection is the point rather than a convenience. A conversation that fails is an
/// `Err` out of `StreamStation::run`, which the supervisor turns into `Backoff` and then a
/// fresh `connect` (`mw_station::driver`) — so a test that ran the body once would be
/// testing half the design. It matters most against `-flaky`: a run whose packets are all
/// corrupted ends on the frame deadline, exactly as a real console on a bad cable does, and
/// what has to be true is that the **station** keeps producing readings across those
/// reconnections.
///
/// There is no `spawn` here, which is also deliberate: the body is driven by a `select!`
/// against a poll of the ingress, so "enough readings arrived" cancels the component in the
/// same task rather than racing a handle.
async fn collect(
    transport: Transport,
    want: usize,
    run_length: usize,
) -> Vec<mw_station::testing::Admission> {
    /// How many conversations one call may need. Generous against the flaky modes, and
    /// bounded so that a driver which fails to connect at all is a failure rather than a
    /// hang.
    const ATTEMPTS: usize = 40;

    let ingress = RecordingIngress::accepting();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut failures = Vec::new();

    for attempt in 0..ATTEMPTS {
        if ingress.admissions().len() >= want {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} of {want} readings arrived in {DEADLINE:?} over {attempt} \
             conversation(s); the last few ended: {failures:?}",
            ingress.admissions().len()
        );
        let station = match StreamStation::connect(
            &transport,
            Conversation::DavisLoop { run_length },
            Arc::new(DavisLoop::default()),
            StationKey::new("suncrest:backyard"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        {
            Ok(station) => Box::new(station),
            Err(e) => {
                // The emulator drops connections on purpose in the flaky modes, so a
                // refused connect is a `Backoff` and not a failure.
                failures.push(mw_types::DisplayChain(&e).to_string());
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let (context, token) = cx("station:davis:suncrest:backyard");
        let enough = {
            let ingress = Arc::clone(&ingress);
            let token = token.clone();
            async move {
                while ingress.admissions().len() < want {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                token.cancel();
            }
        };
        tokio::select! {
            outcome = station.run(context) => {
                if let Err(e) = outcome {
                    failures.push(mw_types::DisplayChain(&e).to_string());
                }
            }
            () = enough => {}
            // **The deadline has to be inside the select, not only above it.** One
            // conversation that neither ends nor produces `want` readings never returns to
            // the top of the loop, so the check there cannot fire -- and `cargo xtask
            // emulator-tests` runs this through `Command::output()` with no timeout of its
            // own, so the gate would hang rather than go red. `emulator_campbell.rs` and
            // `emulator_snowgauge.rs` already bound the equivalent wait; this was the
            // outlier.
            () = tokio::time::sleep_until(deadline) => {
                failures.push(format!(
                    "one conversation neither ended nor produced {want} readings within \
                     {DEADLINE:?}"
                ));
                break;
            }
        }
    }

    assert!(
        ingress.admissions().len() >= want,
        "only {} of {want} readings arrived over {ATTEMPTS} conversations: {failures:?}",
        ingress.admissions().len()
    );
    ingress.admissions()
}

/// Every reading carries a plausible outside temperature, and the values **move** — which
/// is what distinguishes a driver that is reading the console from one that is replaying
/// the first packet it ever saw.
fn assert_live(admissions: &[mw_station::testing::Admission], want: usize) {
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
                .expect("the emulator always reports an outside temperature")
        })
        .collect();
    for temp in &temps {
        assert!(
            (-60.0..=160.0).contains(temp),
            "{temp} is not a temperature a Vantage reports; the field offsets are wrong"
        );
    }
    assert!(
        temps.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same temperature: {temps:?}"
    );
    for admission in admissions {
        assert_eq!(admission.receiver, "station:davis:suncrest:backyard");
        assert_eq!(
            admission.presented_key, None,
            "a Fixed identity presents nothing; the receiver resolves it"
        );
        // The console's whole packet, not one field: a barometer and a humidity prove the
        // 99-byte structure was read at the right offsets end to end.
        let r = &admission.batch[0].values;
        assert!(
            matches!(r.barometer, Some(b) if (25.0..=33.0).contains(&b)),
            "barometer: {:?}",
            r.barometer
        );
        assert!(
            matches!(r.out_humidity, Some(h) if (0.0..=100.0).contains(&h)),
            "humidity: {:?}",
            r.out_humidity
        );
        assert!(
            matches!(r.station_battery_voltage, Some(v) if v > 0.0),
            "the console battery: {:?}",
            r.station_battery_voltage
        );
    }
}

/// **The test the whole item is about.** Two runs of `RUN_LENGTH` packets over TCP, which
/// means the driver re-issued `LOOP n` when the first run ended — the thing
/// `GetLoopPackets` does not do.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn the_driver_keeps_reading_past_the_end_of_a_run_over_tcp() {
    let (_emulator, port) = GoEmulator::serving_tcp(EMULATOR, &[]).await;
    let admissions = collect(
        Transport::TcpOut {
            host: "127.0.0.1".into(),
            port,
            tls: None,
        },
        RUN_LENGTH + 2,
        RUN_LENGTH,
    )
    .await;
    assert_live(&admissions, RUN_LENGTH + 2);
}

/// The same driver over a **character device**, which is W3 D-83's requirement: `socat`
/// bridges a pty to the emulator's port, so the framing, the wake handshake and the
/// re-issue all happen over a tty rather than over a socket.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn the_driver_wakes_the_console_and_reads_it_over_a_serial_line() {
    let (_emulator, port) = GoEmulator::serving_tcp(EMULATOR, &[]).await;
    let bridge = PtyBridge::spawn("davis", port).await.expect(
        "`socat` is a test dependency of this item: W3 D-83 requires the serial path to be \
         exercised over a real character device, and bridging one to the pinned emulator's \
         port is how",
    );
    let admissions = collect(
        Transport::Serial {
            dev: bridge.device().to_path_buf(),
            baud: 19_200,
        },
        RUN_LENGTH + 2,
        RUN_LENGTH,
    )
    .await;
    assert_live(&admissions, RUN_LENGTH + 2);
}

/// **The emulator's flaky hardware, at rates that fire.** Dropped bytes, corrupted bytes,
/// deliberately wrong CRCs and truncated packets, all at 20%: the driver has to
/// resynchronise on the next `LOO` header, drop what it cannot check, and keep producing
/// readings. A decoder that trusted the length or the header alone would store a packet
/// whose CRC does not match, and `assert_live`'s range checks are what would catch that.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_flaky_console_costs_frames_and_not_the_station() {
    let (_emulator, port) = GoEmulator::serving_tcp(
        EMULATOR,
        &[
            "-flaky",
            "-drop-rate",
            "0.2",
            "-corrupt-rate",
            "0.2",
            "-bad-crc-rate",
            "0.2",
            "-truncate-rate",
            "0.2",
            // **Explicitly zero**, and that is not tidiness: `-flaky` turns the whole
            // family on and each of these has a non-zero DEFAULT in the emulator's own
            // flags (2%, 1%, 2%, 1%). They end the *conversation* rather than a frame --
            // a dropped connection, a 3-8 second hang, a 5-15 second delay, an ignored
            // command -- and what the supervisor does with those has its own tests below.
            // Left at their defaults they make this test's failures ambiguous and its
            // duration a minute of sleeping.
            "-disconnect-rate",
            "0",
            "-hang-rate",
            "0",
            "-slow-rate",
            "0",
            "-no-response-rate",
            "0",
        ],
    )
    .await;
    let admissions = collect(
        Transport::TcpOut {
            host: "127.0.0.1".into(),
            port,
            tls: None,
        },
        3,
        // **One packet per command**, which is not an optimisation. With a longer run, a
        // packet the driver drops leaves it waiting for a frame the console has already
        // finished sending -- which ends the conversation on the frame deadline after
        // fifteen seconds. That is the right behaviour and is what the reconnection in
        // `collect` covers; asking for one packet at a time instead exercises the
        // **re-issue** on every single packet, which is the path under test here, and does
        // it in seconds rather than minutes.
        1,
    )
    .await;
    assert_live(&admissions, 3);
}

/// **The connection-level flakiness**, which is the other half: the emulator drops the
/// connection mid-transmission at 30% and hangs at 10%. Every one of those ends the
/// *conversation*, and what has to be true is that the **station** keeps producing
/// readings — which is `Backoff` plus a fresh `connect`, and is what `collect` reproduces.
///
/// The Go driver's answer to the same events is two nested reconnect loops, a `connecting`
/// mutex that makes a reconnect skippable while another is "in progress", and a sleep that
/// one of the two paths does not interrupt on shutdown.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_console_that_drops_the_connection_is_reconnected_to() {
    let (_emulator, port) = GoEmulator::serving_tcp(
        EMULATOR,
        &[
            "-flaky",
            "-disconnect-rate",
            "0.3",
            "-hang-rate",
            "0.1",
            "-hang-min",
            "1",
            "-hang-max",
            "2",
            // The frame-level modes off, so a failure here is about the connection.
            "-drop-rate",
            "0",
            "-corrupt-rate",
            "0",
            "-bad-crc-rate",
            "0",
            "-truncate-rate",
            "0",
            "-slow-rate",
            "0",
            "-no-response-rate",
            "0",
        ],
    )
    .await;
    let admissions = collect(
        Transport::TcpOut {
            host: "127.0.0.1".into(),
            port,
            tls: None,
        },
        3,
        1,
    )
    .await;
    assert_live(&admissions, 3);
}

/// A console that ignores commands is a station the driver reconnects to rather than one
/// it hangs on. `-no-response-rate 1.0` makes the emulator ignore **every** command,
/// including the wake, so the conversation can never open — and the driver has to come back
/// with a deadline rather than waiting forever, which is what the Go serial path does.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_console_that_never_answers_ends_the_conversation_with_a_deadline() {
    let (_emulator, port) =
        GoEmulator::serving_tcp(EMULATOR, &["-flaky", "-no-response-rate", "1.0"]).await;
    let ingress = RecordingIngress::accepting();
    let station = Box::new(
        StreamStation::connect(
            &Transport::TcpOut {
                host: "127.0.0.1".into(),
                port,
                tls: None,
            },
            Conversation::DavisLoop {
                run_length: RUN_LENGTH,
            },
            Arc::new(DavisLoop::default()),
            StationKey::new("suncrest:backyard"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the port accepts the connection"),
    );
    let (context, _token) = cx("station:davis:suncrest:backyard");
    // The wake handshake has three attempts, each with its own deadline, so this returns
    // rather than hanging -- which is the property the Go driver's serial path does not
    // have (`SetReadDeadline` is called only when a hostname is configured).
    let e = tokio::time::timeout(DEADLINE, station.run(context))
        .await
        .expect("the driver gave up rather than hanging")
        .expect_err("the console never answered");
    let rendered = mw_types::DisplayChain(&e).to_string();
    assert!(
        rendered.contains("waking a Davis console"),
        "the failure should name the handshake: {rendered}"
    );
    assert!(ingress.admissions().is_empty());
    // Retryable, which is what makes the supervisor reconnect instead of giving up
    // (**D-27**).
    assert_eq!(mw_types::Classify::class(&e), mw_types::Class::RETRYABLE);
}
