//! Level 2 of W3 **D-82**'s three: a `Session` against a recorded transcript.
//!
//! **This is the level that matters**, and the plan says so: it is what would have caught
//! the Davis "stops polling after one batch" defect. A codec golden test cannot see that
//! bug, because the codec decoded every packet it was given perfectly and there was only
//! ever one batch of them; and an emulator test sees it only if the emulator is driven for
//! longer than one batch, which is a property of the test's patience rather than of its
//! design.
//!
//! WeatherLink Live has **no session** — both its transports deliver one frame per read
//! (W2a §4a.2 lists the inbound drivers as exactly the ones that need none) — so what this
//! file proves is the harness and the contract rather than a particular protocol's
//! conversation. **The level-2 artefact for the driver this item actually ships is
//! `tests/wll_driver.rs`**, whose `every_datagram_is_admitted_and_not_only_the_first` and
//! `every_poll_response_is_admitted_and_not_only_the_first` are the same assertion over a
//! recorded sequence of transport output. Said here because "the transcript level" and "the
//! level for this driver" are two files, and a reader who found only this one could
//! reasonably conclude the delivered driver had no level-2 coverage. The fixture below is a deliberately minimal request/response session in
//! the shape Davis needs: wake, issue a command, read a run of `n` frames, re-issue. It is
//! a **test fixture and not the Davis driver**, which is M6-1's; what it is here for is to
//! demonstrate that `Transcript` fails a session that stops re-issuing, so that when that
//! driver arrives the level it needs already exists and has been seen failing.

use mw_codec::Frame;
use mw_component::BoxFuture;
use mw_station::session::{ByteStream, NoSession, Session, SessionError};
use mw_station::testing::Transcript;

/// A request/response session over a run of `n` frames, in the shape W2a §4a.2 describes.
struct RunOf {
    /// How many frames one command yields.
    n: usize,
    /// How many of the current run have been read.
    seen: usize,
    /// Whether to re-issue when the run ends. `false` reproduces the Davis defect.
    reissue: bool,
    /// Whether the command has been issued at all.
    issued: bool,
}

impl RunOf {
    fn new(n: usize, reissue: bool) -> Self {
        Self {
            n,
            seen: 0,
            reissue,
            issued: false,
        }
    }
}

const WAKE: &[u8] = b"\n";
const AWAKE: &[u8] = b"\n\r";

impl Session for RunOf {
    fn name(&self) -> &'static str {
        "test-run-of"
    }

    fn open<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            io.write_all(WAKE)
                .await
                .map_err(|e| SessionError::io("waking the station", e))?;
            let mut buf = [0u8; 8];
            let n = io
                .read(&mut buf)
                .await
                .map_err(|e| SessionError::io("waking the station", e))?;
            if &buf[..n] != AWAKE {
                return Err(SessionError::Unexpected {
                    what: "waking the station",
                });
            }
            // The run is "over" before it starts, so the first `next_frame` issues the
            // command. That is the state machine Davis needs and the one its driver got
            // wrong in the other direction.
            self.seen = self.n;
            Ok(())
        })
    }

    fn next_frame<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<Frame, SessionError>> {
        Box::pin(async move {
            if self.seen >= self.n && (self.reissue || !self.issued) {
                io.write_all(format!("LOOP {}\n", self.n).as_bytes())
                    .await
                    .map_err(|e| SessionError::io("issuing a command", e))?;
                self.issued = true;
                self.seen = 0;
            }
            let mut buf = vec![0u8; 256];
            let n = io
                .read(&mut buf)
                .await
                .map_err(|e| SessionError::io("reading a frame", e))?;
            if n == 0 {
                return Err(SessionError::Closed {
                    what: "reading a frame",
                });
            }
            buf.truncate(n);
            self.seen += 1;
            Ok(Frame::new(buf))
        })
    }
}

#[tokio::test]
async fn a_session_that_re_issues_reads_every_batch() {
    // Two runs of two frames. The transcript requires the command to be written twice, so
    // a session that issues it once fails the script rather than merely reading less.
    let mut io = Transcript::scripted(
        [
            AWAKE.to_vec(),
            b"frame-1".to_vec(),
            b"frame-2".to_vec(),
            b"frame-3".to_vec(),
            b"frame-4".to_vec(),
        ],
        [WAKE.to_vec(), b"LOOP 2\n".to_vec(), b"LOOP 2\n".to_vec()],
    );
    let mut session = RunOf::new(2, true);
    session.open(&mut io).await.expect("the station wakes");
    for expected in ["frame-1", "frame-2", "frame-3", "frame-4"] {
        let frame = session
            .next_frame(&mut io)
            .await
            .unwrap_or_else(|e| panic!("{expected} should arrive: {e}"));
        assert_eq!(frame.bytes(), expected.as_bytes());
    }
    assert!(
        io.exhausted(),
        "the session did not write every scripted command: {:?}",
        io.written()
    );
}

/// The defect, reproduced: a session that stops re-issuing looks perfect for one batch.
#[tokio::test]
async fn a_session_that_stops_re_issuing_fails_the_transcript_and_not_the_first_batch() {
    let mut io = Transcript::scripted(
        [
            AWAKE.to_vec(),
            b"frame-1".to_vec(),
            b"frame-2".to_vec(),
            b"frame-3".to_vec(),
        ],
        [WAKE.to_vec(), b"LOOP 2\n".to_vec(), b"LOOP 2\n".to_vec()],
    );
    let mut session = RunOf::new(2, false);
    session.open(&mut io).await.expect("the station wakes");
    // The first batch is indistinguishable from a working driver.
    for expected in ["frame-1", "frame-2"] {
        assert_eq!(
            session
                .next_frame(&mut io)
                .await
                .expect("the first batch arrives")
                .bytes(),
            expected.as_bytes()
        );
    }
    // The third frame is where the two sessions differ, and the transcript is what says so:
    // the second command was never written.
    let _third = session.next_frame(&mut io).await;
    assert!(
        !io.exhausted(),
        "the transcript should still be expecting the second command"
    );
    assert_eq!(
        io.written(),
        &[WAKE.to_vec(), b"LOOP 2\n".to_vec()],
        "exactly one command was issued, which is the Davis defect"
    );
}

/// A session that writes the wrong bytes is an error from the stream, not a silent pass.
///
/// A *missing* write is a weaker property of this harness and worth naming: it is caught
/// only because the test above asserts `io.exhausted()`, not because `read` refuses to hand
/// back a chunk while a scripted write is outstanding. Making the harness enforce the
/// ordering would turn the Davis defect from assertable into unmissable, and is the first
/// thing to do here when M6-1 brings a driver that needs it.
#[tokio::test]
async fn the_transcript_refuses_a_session_that_says_the_wrong_thing() {
    let mut io = Transcript::scripted([AWAKE.to_vec()], [b"WAKE\r\n".to_vec()]);
    let mut session = RunOf::new(1, true);
    let e = session
        .open(&mut io)
        .await
        .expect_err("the script wanted other bytes");
    assert!(matches!(e, SessionError::Io { .. }), "{e}");
}

/// A station that hangs up mid-conversation is `Closed`, not an empty frame.
#[tokio::test]
async fn the_end_of_the_script_is_the_end_of_the_stream() {
    let mut io = Transcript::reading([b"one".to_vec()]);
    let mut session = NoSession;
    assert_eq!(
        session
            .next_frame(&mut io)
            .await
            .expect("one frame")
            .bytes(),
        b"one"
    );
    assert!(matches!(
        session.next_frame(&mut io).await,
        Err(SessionError::Closed { .. })
    ));
    assert_eq!(io.remaining(), 0);
}

/// WeatherLink Live's actual session, asserted to be the identity: nothing is written to a
/// unit to make it answer a datagram or an HTTP response.
#[tokio::test]
async fn weatherlink_live_writes_nothing_to_its_station() {
    let datagram = br#"{"did":"001D0A700002","ts":1,"conditions":[{"data_structure_type":1,"txid":1,"temp":1.0}]}"#;
    let mut io = Transcript::scripted([datagram.to_vec()], Vec::<Vec<u8>>::new());
    let mut session = NoSession;
    session.open(&mut io).await.expect("nothing to open");
    assert_eq!(
        session
            .next_frame(&mut io)
            .await
            .expect("the datagram is one frame")
            .len(),
        datagram.len()
    );
    assert!(io.written().is_empty(), "a listener writes nothing");
}
