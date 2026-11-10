//! The driver level: a conversation, a codec and an admission, over a transcript and over
//! a `socat` pty pair.
//!
//! W3 **D-82**'s three levels are the codec against captured frames (`mw-codec`'s golden
//! tests), the session against a recorded transcript (`crate::davis::tests`,
//! `crate::lines::tests`) and the **whole driver** against a pinned emulator
//! (`tests/emulator.rs`). This file is the seam between the middle and the top: it drives
//! the real session and the real codec into a recording `Ingress`, with no emulator and no
//! Go toolchain, so that "the driver admits every frame of the second run" is asserted here
//! as well as there.

use super::*;
use crate::testing::{PtyPair, RecordingIngress, Transcript};
use mw_component::Reject;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn cx(id: &str) -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
    (
        Cx::new(
            ComponentId::new(id).expect("a legal component id"),
            token.clone(),
            mw_component::Generation::ZERO,
            rx,
        ),
        token,
    )
}

/// A valid LOOP record carrying `out_temp`, so a test can tell two apart.
fn packet(tenths: i16) -> Vec<u8> {
    let mut bytes = vec![0u8; mw_codec::LOOP_BYTES];
    bytes[..3].copy_from_slice(&mw_codec::LOOP_HEADER);
    bytes[12..14].copy_from_slice(&tenths.to_le_bytes());
    bytes[95] = b'\n';
    bytes[96] = b'\r';
    let crc = mw_codec::crc16(&bytes[..mw_codec::LOOP_BYTES - 2]);
    bytes[mw_codec::LOOP_BYTES - 2..].copy_from_slice(&crc.to_be_bytes());
    bytes
}

/// **The driver-level version of the Davis defect.** Two runs of two packets: four
/// admissions, not two, and the transcript asserts the command was written twice.
#[tokio::test]
async fn a_davis_station_admits_every_packet_of_the_second_run() {
    let io = Transcript::scripted(
        [
            crate::davis::WAKE_RESPONSE.to_vec(),
            vec![crate::davis::ACK],
            packet(700),
            packet(701),
            vec![crate::davis::ACK],
            packet(702),
            packet(703),
        ],
        [b"\n".to_vec(), b"LOOP 2\n".to_vec(), b"LOOP 2\n".to_vec()],
    );
    let ingress = RecordingIngress::accepting();
    let station = Box::new(StreamStation::over(
        Box::new(io),
        Conversation::DavisLoop { run_length: 2 },
        Arc::new(mw_codec::DavisLoop::default()),
        StationKey::new("suncrest:backyard"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (cx, _token) = cx("station:davis:suncrest:backyard");

    // The transcript ends, so the session reports the stream closed and the body returns
    // an error -- which is what the supervisor turns into a reconnect.
    let e = station.run(cx).await.expect_err("the transcript ran out");
    assert_eq!(e.class(), Class::RETRYABLE, "a reconnect, not a give-up");

    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 4, "four packets across two runs");
    let temps: Vec<Option<f32>> = admissions
        .iter()
        .map(|a| a.batch[0].values.out_temp)
        .collect();
    assert_eq!(temps, vec![Some(70.0), Some(70.1), Some(70.2), Some(70.3)]);
    for admission in &admissions {
        assert_eq!(admission.receiver, "station:davis:suncrest:backyard");
        assert_eq!(
            admission.presented_key, None,
            "a Fixed identity presents nothing; the receiver resolves it"
        );
    }
}

#[tokio::test]
async fn a_campbell_station_admits_a_record_split_across_reads() {
    let io = Transcript::reading([
        &b"{\"airtemp_f\":68.4,"[..],
        &b"\"rh\":41.5}\n{\"airtemp_f\":68.9,\"rh\":42.0}\n"[..],
    ]);
    let ingress = RecordingIngress::accepting();
    let station = Box::new(StreamStation::over(
        Box::new(io),
        Conversation::Lines,
        Arc::new(mw_codec::CampbellAscii::default()),
        StationKey::new("suncrest:meadow"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (cx, _token) = cx("station:campbell:suncrest:meadow");
    station.run(cx).await.expect_err("the transcript ran out");

    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 2);
    assert_eq!(admissions[0].batch[0].values.out_temp, Some(68.4));
    assert_eq!(admissions[1].batch[0].values.out_temp, Some(68.9));
}

/// A bad frame is a counted drop and the conversation continues, which is the property a
/// console on a marginal cable needs.
#[tokio::test]
async fn a_frame_that_does_not_decode_does_not_end_the_conversation() {
    let mut corrupt = packet(700);
    corrupt[20] ^= 0xFF;
    let io = Transcript::reading([
        crate::davis::WAKE_RESPONSE.to_vec(),
        vec![crate::davis::ACK],
        corrupt,
        packet(701),
    ]);
    let ingress = RecordingIngress::accepting();
    let station = Box::new(StreamStation::over(
        Box::new(io),
        Conversation::DavisLoop { run_length: 4 },
        Arc::new(mw_codec::DavisLoop::default()),
        StationKey::new("suncrest:backyard"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (cx, _token) = cx("station:davis:suncrest:backyard");
    station.run(cx).await.expect_err("the transcript ran out");
    assert_eq!(
        ingress.admissions().len(),
        1,
        "the corrupted packet was dropped and the next one was admitted"
    );
}

/// An `Ignorable` refusal — a station nothing is configured for — is counted and survived.
#[tokio::test]
async fn an_unknown_station_is_counted_and_the_conversation_continues() {
    let io = Transcript::reading([
        crate::davis::WAKE_RESPONSE.to_vec(),
        vec![crate::davis::ACK],
        packet(700),
        packet(701),
    ]);
    let ingress = RecordingIngress::refusing(Reject::UnknownStation);
    let station = Box::new(StreamStation::over(
        Box::new(io),
        Conversation::DavisLoop { run_length: 4 },
        Arc::new(mw_codec::DavisLoop::default()),
        StationKey::new("suncrest:backyard"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (cx, _token) = cx("station:davis:suncrest:backyard");
    let e = station.run(cx).await.expect_err("the transcript ran out");
    // The conversation ended because the transcript ran out, NOT because of the refusals.
    assert!(
        e.to_string().contains("reading a station frame"),
        "the refusals should not have ended it: {e}"
    );
    assert_eq!(ingress.admissions().len(), 0);
}

#[tokio::test]
async fn a_cancelled_conversation_returns_without_an_error() {
    // A stream that answers the handshake and then never speaks again, so the only way out
    // is cancellation.
    #[derive(Debug)]
    struct HandshakeThenQuiet(Option<Vec<u8>>);
    impl ByteStream for HandshakeThenQuiet {
        fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
            Box::pin(async move {
                match self.0.take() {
                    Some(bytes) => {
                        buf[..bytes.len()].copy_from_slice(&bytes);
                        Ok(bytes.len())
                    }
                    None => std::future::pending().await,
                }
            })
        }
        fn write_all<'a>(&'a mut self, _bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    let station = Box::new(StreamStation::over(
        Box::new(HandshakeThenQuiet(Some(b"{\"airtemp_f\":68.4}\n".to_vec()))),
        Conversation::Lines,
        Arc::new(mw_codec::CampbellAscii::default()),
        StationKey::new("suncrest:meadow"),
        RecordingIngress::accepting() as Arc<dyn Ingress>,
    ));
    let (cx, token) = cx("station:campbell:suncrest:meadow");
    // `spawn` is forbidden in the daemon (`clippy.toml`: everything goes through
    // `mw_supervisor::task`, §3.2) and this test is the supervisor: a `Serve` body runs
    // until it is cancelled, so driving it and cancelling it at the same time is the only
    // way to exercise the loop. The same exemption `tests/wll_driver.rs` takes.
    #[allow(clippy::disallowed_methods)]
    let run = tokio::task::spawn(async move { station.run(cx).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the body observed cancellation")
        .expect("the task did not panic");
    assert!(result.is_ok(), "cancellation is not a failure: {result:?}");
}

/// A build failure rather than a startup abort: the Go driver's `logger.Fatal`.
#[tokio::test]
async fn a_station_whose_device_does_not_exist_is_a_build_failure() {
    let e = StreamStation::connect(
        &Transport::Serial {
            dev: std::path::PathBuf::from("/dev/there-is-no-such-port"),
            baud: 19_200,
        },
        Conversation::DavisLoop { run_length: 20 },
        Arc::new(mw_codec::DavisLoop::default()),
        StationKey::new("suncrest:backyard"),
        RecordingIngress::accepting() as Arc<dyn Ingress>,
    )
    .await
    .expect_err("no such device");
    let rendered = DisplayChain(&e).to_string();
    assert!(rendered.contains("suncrest:backyard"), "{rendered}");
    assert!(rendered.contains("there-is-no-such-port"), "{rendered}");
}

/// **The whole driver over a real character device**, which is W3 D-83's level without the
/// Go toolchain: a `socat` pty pair, the real `DavisSession`, the real codec, and an
/// emulator written in the test itself.
#[tokio::test]
async fn a_davis_station_over_a_pty_pair_wakes_the_console_and_admits_two_runs() {
    let Some(pair) = PtyPair::spawn("driver-davis").await else {
        eprintln!("skipping: socat is not on PATH");
        return;
    };
    let ingress = RecordingIngress::accepting();
    let station = Box::new(
        StreamStation::connect(
            &Transport::Serial {
                dev: pair.daemon_side().to_path_buf(),
                baud: 19_200,
            },
            Conversation::DavisLoop { run_length: 2 },
            Arc::new(mw_codec::DavisLoop::default()),
            StationKey::new("suncrest:backyard"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the pty opens"),
    );

    // The console side: answer each newline, acknowledge each command, send two packets.
    let mut console = crate::stream::open(
        &Transport::Serial {
            dev: pair.device_side().to_path_buf(),
            baud: 19_200,
        },
        "the emulated console",
    )
    .await
    .expect("the other end opens");

    let (cx, token) = cx("station:davis:suncrest:backyard");
    // The same exemption, for the same reason: this test is the supervisor and the
    // console on the other end of the pty at once.
    #[allow(clippy::disallowed_methods)]
    let driving = tokio::task::spawn(async move { station.run(cx).await });

    let mut buf = [0u8; 256];
    let mut temp = 700i16;
    for run in 0..2 {
        if run == 0 {
            // The wake handshake.
            let n = console.read(&mut buf).await.expect("a newline");
            assert_eq!(&buf[..n], b"\n");
            console
                .write_all(crate::davis::WAKE_RESPONSE)
                .await
                .expect("answers");
        }
        // The command, which arrives as `LOOP 2\n`.
        let mut command = Vec::new();
        while !command.ends_with(b"\n") {
            let n = console.read(&mut buf).await.expect("a command");
            command.extend_from_slice(&buf[..n]);
        }
        assert_eq!(command, b"LOOP 2\n", "run {run}");
        console.write_all(&[crate::davis::ACK]).await.expect("acks");
        for _ in 0..2 {
            console.write_all(&packet(temp)).await.expect("a packet");
            temp += 1;
        }
    }

    // Wait for all four admissions, then stop the component.
    for _ in 0..500 {
        if ingress.admissions().len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), driving)
        .await
        .expect("the body observed cancellation")
        .expect("the task did not panic");
    assert!(result.is_ok(), "{result:?}");

    let temps: Vec<Option<f32>> = ingress
        .admissions()
        .iter()
        .map(|a| a.batch[0].values.out_temp)
        .collect();
    assert_eq!(
        temps,
        vec![Some(70.0), Some(70.1), Some(70.2), Some(70.3)],
        "two runs over a real character device, and the command was re-issued"
    );
}

#[tokio::test]
async fn a_conversation_is_a_value_and_makes_a_fresh_session_each_time() {
    let davis = Conversation::DavisLoop { run_length: 20 };
    assert_eq!(davis.session().name(), "davis-loop");
    assert_eq!(Conversation::Lines.session().name(), "lines");
    // Two sessions from one conversation share no state, which is what makes a reconnect
    // start clean.
    assert_eq!(davis.session().discarded(), 0);
}
