//! [`Lines`] — the session for a device that streams newline-delimited records.
//!
//! Campbell Scientific's, and nothing else today. The datalogger is programmed in CRBasic
//! by whoever installed it and this one's program writes one JSON object per output
//! interval, terminated, continuously, with no command and no response — so the whole
//! session is framing.
//!
//! **W2a §4a.5 writes Campbell's session as "command/response", and the installed program
//! does not use it.** That is the *transport's* capability rather than this program's: a
//! Campbell datalogger will answer PakBus commands over the same line, and nothing in this
//! build sends one. Recorded because a reader comparing the table against this file will
//! otherwise think one of them is wrong.
//!
//! # Why framing needs a session at all
//!
//! Because [`NoSession`](crate::session::NoSession) is wrong here, and wrong in a way that
//! only shows up in production. One `read` is not one record: a 300-byte JSON object
//! arrives over a 19 200-baud line in whatever chunks the UART's FIFO produces, and a
//! TCP connection can deliver two records in one segment or half of one. A driver that
//! treated a read as a frame would decode a truncated object as a parse failure and the
//! other half as a second one — on a *correctly functioning* station. The Go driver gets
//! this right by wrapping the connection in `bufio.Scanner` (`campbell/station.go:180`),
//! and this is that scanner.
//!
//! # The bound, and what happens at it
//!
//! A line longer than [`Lines::MAX_LINE_BYTES`] is **not** a line this session will
//! assemble, and the session fails rather than growing: a device that never sends a
//! newline is indistinguishable from a device sending one enormous record, and both end
//! with the driver holding every byte the station has ever sent. `bufio.Scanner`'s own
//! answer is the same shape — `ErrTooLong` at 64 KiB — and the Go driver treats it as a
//! reconnect, which is what a session failure is here (**D-27**).

use mw_codec::Frame;
use mw_component::BoxFuture;

use crate::session::{ByteStream, Session, SessionError};

/// A newline-delimited frame reader.
///
/// Holds the bytes read past the end of the last frame, which is the whole of its state and
/// the reason it is a `Session` rather than a function.
#[derive(Debug, Default)]
pub struct Lines {
    buffer: Vec<u8>,
    discarded: u64,
}

impl Lines {
    /// The longest line this session will assemble.
    ///
    /// `bufio.Scanner`'s default maximum, which is also what the Go driver uses, so a record
    /// one implementation refuses the other refuses too.
    ///
    /// **Measured on the line and not on the buffer, which is not the same bound.** A
    /// check made only before each read left room for one more `READ_BYTES` after it, so a
    /// line of up to 69 631 bytes was assembled and *returned* where `ErrTooLong` refuses
    /// anything past 65 536 — one implementation accepting a record the other refuses is
    /// the one thing this constant exists to prevent. The buffer may still hold one read
    /// past the bound before the session gives up, which is bounded and is not what a
    /// device can grow.
    pub const MAX_LINE_BYTES: usize = 64 * 1024;

    /// How many bytes to read at a time.
    ///
    /// A record is a few hundred bytes and arrives in several pieces; the buffer is sized
    /// for the pieces rather than for the record.
    const READ_BYTES: usize = 4096;

    /// A session with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many bytes are held past the end of the last frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// The next complete line in the buffer, consumed.
    fn take_line(&mut self) -> Option<Frame> {
        let at = self.buffer.iter().position(|b| *b == b'\n')?;
        let mut line: Vec<u8> = self.buffer.drain(..=at).collect();
        // The terminator, and a `\r` before it: a datalogger on a serial line writes
        // CRLF, and `\r` inside a JSON document is not whitespace the parser accepts
        // everywhere.
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(Frame::new(line))
    }
}

impl Session for Lines {
    fn name(&self) -> &'static str {
        "lines"
    }

    fn open<'a>(
        &'a mut self,
        _io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            // Nothing to negotiate, and the buffer is cleared rather than kept: `open` is
            // called again after a reconnect, and half a record from the previous
            // connection is not the first half of the next one. Counted for the same
            // reason the too-long path counts.
            self.discarded += self.buffer.len() as u64;
            self.buffer.clear();
            Ok(())
        })
    }

    /// Every byte this session dropped: half a record left by a reconnect, and the whole
    /// of a line that ran past the bound.
    ///
    /// Reported rather than left at the default zero, because without it a datalogger
    /// emitting 64 KiB of something that is not a record showed a resync counter of nought
    /// while the session restarted — which is the one case the counter exists for.
    fn discarded(&self) -> u64 {
        self.discarded
    }

    fn next_frame<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<Frame, SessionError>> {
        Box::pin(async move {
            const WHAT: &str = "reading a line from the datalogger";
            loop {
                if let Some(frame) = self.take_line() {
                    // **The bound is on the line, not on the buffer.** A read can carry
                    // the rest of an over-long line and its terminator at once, so a
                    // buffer that was under the bound before the read can yield a line
                    // that is over it -- which `ErrTooLong` refuses and this used to
                    // return. Measured where the length is known.
                    if frame.bytes().len() > Self::MAX_LINE_BYTES {
                        self.discarded += frame.bytes().len() as u64;
                        return Err(SessionError::Unexpected { what: WHAT });
                    }
                    return Ok(frame);
                }
                if self.buffer.len() > Self::MAX_LINE_BYTES {
                    // Discarded rather than kept: the session is over either way, and
                    // holding 64 KiB until the component is dropped is 64 KiB of a device's
                    // mistake. Counted, because `discarded` is what an operator reads for a
                    // device that is producing something other than records.
                    self.discarded += self.buffer.len() as u64;
                    self.buffer.clear();
                    return Err(SessionError::Unexpected { what: WHAT });
                }
                let mut chunk = vec![0u8; Self::READ_BYTES];
                let n = io
                    .read(&mut chunk)
                    .await
                    .map_err(|source| SessionError::io(WHAT, source))?;
                if n == 0 {
                    return Err(SessionError::Closed { what: WHAT });
                }
                self.buffer.extend_from_slice(&chunk[..n]);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Transcript;

    async fn frames(reads: &[&[u8]], want: usize) -> Vec<String> {
        let mut io = Transcript::reading(reads.iter().copied());
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        let mut out = Vec::new();
        for _ in 0..want {
            let frame = session.next_frame(&mut io).await.expect("a frame");
            out.push(String::from_utf8_lossy(frame.bytes()).into_owned());
        }
        out
    }

    /// The property `NoSession` does not have: one read is not one record.
    #[tokio::test]
    async fn a_record_split_across_four_reads_is_one_frame() {
        let got = frames(&[b"{\"airtemp", b"_f\":68.4,", b"\"rh\":41.5}", b"\n"], 1).await;
        assert_eq!(got, vec![r#"{"airtemp_f":68.4,"rh":41.5}"#]);
    }

    /// And the other half of the same property: one read can be two records.
    #[tokio::test]
    async fn two_records_in_one_read_are_two_frames() {
        let got = frames(&[b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n"], 3).await;
        assert_eq!(got, vec![r#"{"a":1}"#, r#"{"b":2}"#, r#"{"c":3}"#]);
    }

    #[tokio::test]
    async fn a_crlf_terminator_is_not_part_of_the_record() {
        let got = frames(&[b"{\"a\":1}\r\n{\"b\":2}\n"], 2).await;
        assert_eq!(got, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    /// An empty line is a frame, which is deliberate: the codec is what decides that
    /// nothing arrived (`mw_codec::CampbellAscii` answers `NoRecords`), and a session that
    /// skipped blank lines would be a second place a record can vanish.
    #[tokio::test]
    async fn a_blank_line_is_an_empty_frame_rather_than_skipped() {
        let got = frames(&[b"\n{\"a\":1}\n"], 2).await;
        assert_eq!(got, vec![String::new(), r#"{"a":1}"#.to_string()]);
    }

    #[tokio::test]
    async fn the_end_of_the_stream_is_a_closed_session_and_not_a_frame() {
        let mut io = Transcript::reading([&b"{\"a\":1}\n"[..]]);
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        assert_eq!(session.next_frame(&mut io).await.expect("a frame").len(), 7);
        // The partial record at the end of a stream is NOT returned: a datalogger that hung
        // up mid-record has not sent a record, and returning the half would be a decode
        // failure attributed to the device rather than to the connection.
        assert!(matches!(
            session.next_frame(&mut io).await,
            Err(SessionError::Closed { .. })
        ));
    }

    #[tokio::test]
    async fn a_partial_record_at_the_end_of_a_connection_is_dropped_not_returned() {
        let mut io = Transcript::reading([&b"{\"a\":1"[..]]);
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        assert!(matches!(
            session.next_frame(&mut io).await,
            Err(SessionError::Closed { .. })
        ));
        assert_eq!(session.buffered(), 6, "the partial record is still held");
        // ... and `open` on the next connection clears it, so the two halves never join.
        session.open(&mut io).await.expect("reopens");
        assert_eq!(session.buffered(), 0);
    }

    #[tokio::test]
    async fn a_device_that_never_sends_a_newline_fails_the_session_rather_than_growing() {
        // Enough 4 KiB chunks to pass the bound, which is what a device with a stuck
        // terminator looks like.
        let chunk = vec![b'x'; Lines::READ_BYTES];
        let reads: Vec<&[u8]> = std::iter::repeat_n(
            chunk.as_slice(),
            Lines::MAX_LINE_BYTES / Lines::READ_BYTES + 1,
        )
        .collect();
        let mut io = Transcript::reading(reads);
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        let e = session.next_frame(&mut io).await.expect_err("too long");
        assert!(matches!(e, SessionError::Unexpected { .. }), "{e}");
        assert_eq!(session.buffered(), 0, "and the bytes are not retained");
        assert!(
            session.discarded() >= Lines::MAX_LINE_BYTES as u64,
            "the dropped bytes are reported rather than left at nought"
        );
    }

    /// A line that arrives complete and over the bound is refused, which is the case a
    /// check made before each read cannot see: the terminator comes in the same read as
    /// the bytes that pass the bound, so the buffer was never over it when it was measured.
    #[tokio::test]
    async fn an_over_long_line_that_arrives_with_its_terminator_is_refused() {
        let mut record = vec![b'x'; Lines::MAX_LINE_BYTES + 1];
        record.push(b'\n');
        // Delivered in `READ_BYTES` pieces, which is how a real device delivers it, with
        // the terminator in the last piece.
        let chunks: Vec<&[u8]> = record.chunks(Lines::READ_BYTES).collect();
        let mut io = Transcript::reading(chunks);
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        let e = session.next_frame(&mut io).await.expect_err("too long");
        assert!(matches!(e, SessionError::Unexpected { .. }), "{e}");
        // And one byte shorter is a line, so the bound is the bound rather than a
        // rejection of long records in general.
        let mut record = vec![b'x'; Lines::MAX_LINE_BYTES];
        record.push(b'\n');
        let chunks: Vec<&[u8]> = record.chunks(Lines::READ_BYTES).collect();
        let mut io = Transcript::reading(chunks);
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        let frame = session
            .next_frame(&mut io)
            .await
            .expect("exactly the bound");
        assert_eq!(frame.bytes().len(), Lines::MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn the_session_writes_nothing() {
        let mut io = Transcript::scripted([&b"{\"a\":1}\n"[..]], Vec::<&[u8]>::new());
        let mut session = Lines::new();
        session.open(&mut io).await.expect("nothing to open");
        session.next_frame(&mut io).await.expect("a frame");
        assert!(io.written().is_empty(), "a line reader sends no commands");
        assert_eq!(session.name(), "lines");
    }
}
