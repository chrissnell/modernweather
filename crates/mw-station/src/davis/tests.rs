//! The Davis conversation against a recorded transcript — W3 **D-82**'s middle level.
//!
//! The assertions that matter are the ones about the **second** run: a session that
//! delivered twenty packets and then stopped asking would pass every test about the first
//! one, and that is the defect by name.

use super::*;
use crate::testing::Transcript;
use std::time::Duration;

/// A valid LOOP record carrying `out_temp` so that a test can tell two apart.
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

/// The outside temperature a packet carries, through the real codec — so these tests
/// assert on a decoded value rather than on a byte offset they chose themselves.
fn out_temp(frame: &Frame) -> Option<f32> {
    use mw_codec::Codec;
    mw_codec::DavisLoop::default()
        .decode(frame)
        .expect("the transcript's packets are valid")[0]
        .values
        .out_temp
}

fn session(run_length: usize) -> DavisSession {
    DavisSession::new(run_length).with_deadlines(
        Duration::from_millis(50),
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
}

/// **The test this file exists for.** Two runs of two packets, and the transcript asserts
/// that the command was written **twice**: `Transcript::scripted` fails the write when the
/// session writes something the script did not expect, and leaves
/// `Transcript::exhausted` false when a scripted write never happens.
#[tokio::test]
async fn the_command_is_reissued_when_a_run_ends() {
    let mut io = Transcript::scripted(
        [
            WAKE_RESPONSE.to_vec(),
            vec![ACK],
            packet(700),
            packet(701),
            // The second run: the acknowledgement and two more packets.
            vec![ACK],
            packet(702),
            packet(703),
        ],
        [
            b"\n".to_vec(),
            b"LOOP 2\n".to_vec(),
            // ** the re-issue **
            b"LOOP 2\n".to_vec(),
        ],
    );
    let mut session = session(2);
    session.open(&mut io).await.expect("the console wakes");

    let mut temps = Vec::new();
    for _ in 0..4 {
        let frame = session.next_frame(&mut io).await.expect("a packet");
        temps.push(out_temp(&frame));
    }
    assert_eq!(
        temps,
        vec![Some(70.0), Some(70.1), Some(70.2), Some(70.3)],
        "four packets across two runs, in order"
    );
    assert!(
        io.exhausted(),
        "the session did not write every command the script required"
    );
    assert_eq!(
        io.written(),
        &[b"\n".to_vec(), b"LOOP 2\n".to_vec(), b"LOOP 2\n".to_vec()]
    );
    assert_eq!(session.remaining(), 0, "the second run is spent too");
}

#[tokio::test]
async fn the_run_length_is_what_the_command_asks_for() {
    let mut io = Transcript::reading([WAKE_RESPONSE.to_vec(), vec![ACK], packet(700)]);
    let mut session = session(RUN_LENGTH);
    session.open(&mut io).await.expect("wakes");
    session.next_frame(&mut io).await.expect("a packet");
    assert_eq!(session.remaining(), RUN_LENGTH - 1);
    assert_eq!(io.written()[1], b"LOOP 20\n");
    // A run length of zero would be a command the console refuses and a timer that never
    // advances; it is clamped rather than refused, because the only caller is this crate.
    assert_eq!(DavisSession::new(0).remaining_run_length(), 1);
}

/// The wake handshake's retry path, as the end of a stream rather than as silence.
#[tokio::test]
async fn a_console_that_hangs_up_during_the_handshake_is_a_closed_session() {
    let mut io = Transcript::scripted(
        [Vec::new(), WAKE_RESPONSE.to_vec()],
        [b"\n".to_vec(), b"\n".to_vec()],
    );
    let mut session = session(2);
    // An empty read is end-of-stream to the transcript, so this exercises the *closed*
    // path rather than the silent one; the silent one has its own test below.
    let e = session.open(&mut io).await.expect_err("the stream ended");
    assert!(matches!(e, SessionError::Closed { .. }), "{e}");
}

#[tokio::test]
async fn a_console_that_never_answers_is_silent_and_says_for_how_long() {
    #[derive(Debug)]
    struct Quiet;
    impl crate::session::ByteStream for Quiet {
        fn read<'a>(
            &'a mut self,
            _buf: &'a mut [u8],
        ) -> mw_component::BoxFuture<'a, std::io::Result<usize>> {
            Box::pin(std::future::pending())
        }
        fn write_all<'a>(
            &'a mut self,
            _bytes: &'a [u8],
        ) -> mw_component::BoxFuture<'a, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    let mut io = Quiet;
    let mut session = session(2);
    let e = session.open(&mut io).await.expect_err("nothing answered");
    let SessionError::Silent { seconds, what } = e else {
        panic!("expected a silent console, got {e}");
    };
    assert!(what.contains("waking"), "{what}");
    assert_eq!(seconds, 0, "the test's deadline is sub-second");
}

#[tokio::test]
async fn something_on_the_line_that_is_not_a_davis_console_is_refused_by_name() {
    let mut io = Transcript::reading([
        b"login: ".to_vec(),
        b"Password: ".to_vec(),
        b"nope".to_vec(),
    ]);
    let mut session = session(2);
    let e = session.open(&mut io).await.expect_err("not a console");
    assert!(matches!(e, SessionError::Unexpected { .. }), "{e}");
    assert_eq!(
        e.to_string(),
        "the station answered unexpectedly while waking a Davis console"
    );
}

/// The second disagreement with the Go driver: the acknowledgement is searched for.
#[tokio::test]
async fn the_acknowledgement_is_found_behind_the_handshakes_own_answer() {
    let mut io = Transcript::reading([
        WAKE_RESPONSE.to_vec(),
        // What Go's one-byte read would have seen: the leftover answers to the second and
        // third newlines, and only then the acknowledgement.
        b"\n\r\n\r\x06".to_vec(),
        packet(700),
    ]);
    let mut session = session(2);
    session.open(&mut io).await.expect("wakes");
    let frame = session.next_frame(&mut io).await.expect("a packet");
    assert_eq!(out_temp(&frame), Some(70.0));
    assert_eq!(
        session.discarded(),
        WAKE_RESPONSE.len() as u64 + 4,
        "the handshake's own answer and the four noise bytes are both counted"
    );
}

#[tokio::test]
async fn a_resend_is_a_refusal_the_console_made_and_is_named_as_one() {
    let mut io = Transcript::reading([WAKE_RESPONSE.to_vec(), vec![NAK]]);
    let mut session = session(2);
    session.open(&mut io).await.expect("wakes");
    let e = session.next_frame(&mut io).await.expect_err("refused");
    assert!(matches!(e, SessionError::Unexpected { .. }), "{e}");
    assert!(e.to_string().contains("asking a Davis console"), "{e}");
}

/// The framing property: a packet arriving in pieces is one frame, and so is a read
/// carrying two.
#[tokio::test]
async fn a_packet_split_across_reads_is_one_frame_and_two_in_one_read_are_two() {
    let first = packet(700);
    let second = packet(701);
    let mut both = first.clone();
    both.extend_from_slice(&second);
    let mut io = Transcript::reading([
        WAKE_RESPONSE.to_vec(),
        vec![ACK],
        // One packet in three pieces.
        first[..10].to_vec(),
        first[10..60].to_vec(),
        first[60..].to_vec(),
        // Two packets in one read.
        both,
    ]);
    let mut session = session(4);
    session.open(&mut io).await.expect("wakes");
    let mut temps = Vec::new();
    for _ in 0..3 {
        let frame = session.next_frame(&mut io).await.expect("a packet");
        temps.push(out_temp(&frame));
    }
    assert_eq!(temps, vec![Some(70.0), Some(70.0), Some(70.1)]);
    assert_eq!(
        session.discarded(),
        WAKE_RESPONSE.len() as u64,
        "the handshake's answer, and nothing resynchronised after it"
    );
}

/// The third disagreement: resynchronisation is counted.
#[tokio::test]
async fn bytes_dropped_to_find_a_header_are_counted() {
    let mut prefixed = b"xyzzy".to_vec();
    prefixed.extend_from_slice(&packet(700));
    let mut io = Transcript::reading([WAKE_RESPONSE.to_vec(), vec![ACK], prefixed]);
    let mut session = session(2);
    session.open(&mut io).await.expect("wakes");
    let frame = session.next_frame(&mut io).await.expect("a packet");
    assert_eq!(out_temp(&frame), Some(70.0));
    assert_eq!(session.discarded(), WAKE_RESPONSE.len() as u64 + 5);
}

/// A line at the wrong rate produces bytes that are never a header, and the session ends
/// rather than buffering for as long as the process runs.
#[tokio::test]
async fn a_line_that_never_produces_a_header_fails_the_session() {
    let noise = vec![0x55u8; 256];
    let reads: Vec<Vec<u8>> = std::iter::once(WAKE_RESPONSE.to_vec())
        .chain(std::iter::once(vec![ACK]))
        .chain(std::iter::repeat_n(noise, 8))
        .collect();
    let mut io = Transcript::reading(reads);
    let mut session = session(2);
    session.open(&mut io).await.expect("wakes");
    let e = session.next_frame(&mut io).await.expect_err("never framed");
    assert!(
        matches!(
            e,
            SessionError::Unexpected { .. } | SessionError::Closed { .. }
        ),
        "{e}"
    );
    assert!(
        session.discarded() > 0,
        "the discarded bytes are what an operator sees for a wrong line rate"
    );
}

/// Every wait has a deadline, which the Go serial path does not.
#[tokio::test]
async fn a_console_that_stops_mid_run_is_a_deadline_rather_than_a_hang() {
    #[derive(Debug)]
    struct OneThenQuiet(Option<Vec<u8>>);
    impl crate::session::ByteStream for OneThenQuiet {
        fn read<'a>(
            &'a mut self,
            buf: &'a mut [u8],
        ) -> mw_component::BoxFuture<'a, std::io::Result<usize>> {
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
        fn write_all<'a>(
            &'a mut self,
            _bytes: &'a [u8],
        ) -> mw_component::BoxFuture<'a, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    let mut bytes = WAKE_RESPONSE.to_vec();
    bytes.push(ACK);
    bytes.extend_from_slice(&packet(700));
    let mut io = OneThenQuiet(Some(bytes));
    let mut session = session(4);
    session.open(&mut io).await.expect("wakes");
    session.next_frame(&mut io).await.expect("the one packet");
    let e = session
        .next_frame(&mut io)
        .await
        .expect_err("the console went quiet");
    let SessionError::Silent { what, .. } = e else {
        panic!("expected a deadline, got {e}");
    };
    assert!(what.contains("LOOP packet"), "{what}");
}

#[tokio::test]
async fn a_reopen_forgets_the_previous_connections_run() {
    let mut io = Transcript::reading([
        WAKE_RESPONSE.to_vec(),
        vec![ACK],
        packet(700),
        WAKE_RESPONSE.to_vec(),
    ]);
    let mut session = session(20);
    session.open(&mut io).await.expect("wakes");
    session.next_frame(&mut io).await.expect("a packet");
    assert_eq!(session.remaining(), 19);
    session.open(&mut io).await.expect("wakes again");
    assert_eq!(
        session.remaining(),
        0,
        "the console forgets its LOOP state when the line drops"
    );
}

#[tokio::test]
async fn the_session_names_itself_and_the_ids_are_the_ones_the_design_names() {
    assert_eq!(DavisSession::default().name(), "davis-loop");
    assert_eq!(
        station_id("backyard").expect("a legal id").as_str(),
        "station:davis:backyard"
    );
    assert!(station_id("Back Yard").is_err());
    assert_eq!(STATION_TYPE, "davis");
}
