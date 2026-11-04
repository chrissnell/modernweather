//! [`Session`] and [`ByteStream`] — the impure fourth part of W2a §4a.2.
//!
//! The prompt's pipeline is `Transport -> Codec -> Identity`, and W2a's answer is that it
//! cannot express Davis: before a console emits a LOOP packet you must wake it (a newline,
//! await `\n\r`, retry) and send `LOOP n`, then re-issue once the run of `n` ends. That is
//! request/response conversation over the transport, it is impure by nature, and putting it
//! in the codec would cost the one property worth protecting. So there is a fourth part,
//! and it is this one.
//!
//! **What a session is for, stated as a test.** A `Session` is testable against a *recorded
//! transcript* — bytes in, bytes expected out — with no hardware, because the only thing it
//! touches is a [`ByteStream`]. `crate::testing::Transcript` is that double, and W3
//! **D-82** calls this level the one that matters: it is what would have caught the Davis
//! "stops polling after one batch" defect, which no amount of golden-testing the codec can
//! see, because the codec decoded every packet it was given perfectly and there was only
//! ever one batch of them.
//!
//! **WeatherLink Live needs no session**, and that is why [`NoSession`] exists rather than
//! an `Option`. Its UDP datagrams and its HTTP responses are each one frame per read, with
//! no conversation at all (W2a §4a.2 lists the inbound drivers as exactly the ones that need
//! none) — so the identity session is a real implementation of the trait and the driver has
//! one code path instead of two.

use mw_codec::Frame;
use mw_component::BoxFuture;

/// A bidirectional byte stream, as a trait object.
///
/// Narrower than `AsyncRead + AsyncWrite` on purpose. A session needs "give me more bytes"
/// and "send these bytes"; it does not need `poll_read`'s buffer discipline, a split, a
/// shutdown or a seek. The narrower surface is what makes the recorded-transcript double
/// twenty lines instead of a mock of `tokio`'s traits.
///
/// Dyn-compatible, so `Session`'s methods can take `&mut dyn ByteStream` and one session
/// implementation serves a serial port, a TCP connection and a transcript. That is also why
/// the methods return [`BoxFuture`] rather than being `async fn`.
pub trait ByteStream: Send {
    /// Read some bytes. `Ok(0)` is end of stream.
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>>;

    /// Write all of `bytes`.
    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>>;
}

/// Why a session could not produce a frame.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The stream failed or ended.
    #[error("{what}")]
    Io {
        /// What the session was doing, in words that name the action (§5.9).
        what: &'static str,
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// The stream ended where the protocol expected more.
    #[error("the station closed the connection while {what}")]
    Closed {
        /// What the session was doing.
        what: &'static str,
    },

    /// The station answered, and not with what the protocol says.
    ///
    /// The Davis wake handshake is the case: a console that answers anything but `\n\r` has
    /// not woken, and retrying is the protocol's own answer rather than an error.
    #[error("the station answered unexpectedly while {what}")]
    Unexpected {
        /// What the session was doing.
        what: &'static str,
    },

    /// A frame did not arrive inside the protocol's own window.
    #[error("the station did not answer within {seconds}s while {what}")]
    Silent {
        /// What the session was doing.
        what: &'static str,
        /// The window, in seconds.
        seconds: u64,
    },
}

impl SessionError {
    /// Wrap a stream failure, naming the action.
    pub fn io(what: &'static str, source: std::io::Error) -> Self {
        SessionError::Io { what, source }
    }
}

impl mw_types::Classify for SessionError {
    /// Every variant is [`mw_types::Class::RETRYABLE`], and the uniformity is the decision.
    /// A session failure means "this conversation is over"; the answer is always to open a
    /// new one, and which of the four it was changes the log line and not the action. A
    /// `Fatal` here would be a driver that stays dead until the process restarts, which is
    /// the behaviour **D-27** exists to remove.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::RETRYABLE
    }
}

/// The protocol conversation over a transport: impure, stateful, and the only part of a
/// driver that writes bytes.
pub trait Session: Send {
    /// A stable name for a log field — `davis-loop`, `none`.
    fn name(&self) -> &'static str;

    /// Bring the stream to the point where frames can be read: a wake handshake, a login, a
    /// command. Called once per connection, and again after a reconnect.
    fn open<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<(), SessionError>>;

    /// Read the next frame, re-issuing whatever the protocol needs re-issued.
    ///
    /// **This is where the Davis defect lived.** A `next_frame` that returned the last
    /// packet of a `LOOP n` run and then never re-issued the command would look exactly
    /// like a working driver for `n` packets, which is why the transcript test asserts on
    /// the *second* batch and not only on the first.
    fn next_frame<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<Frame, SessionError>>;
}

/// The identity session: one read is one frame, and nothing is ever written.
///
/// For every transport where the station or the response is already framed — a UDP
/// datagram, an HTTP response body, an inbound request. Not an `Option<Box<dyn Session>>`,
/// because a driver with two code paths has two code paths to test.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSession;

/// The largest frame the identity session will assemble.
///
/// A WeatherLink Live datagram is under 2 KiB and a full `current_conditions` body with a
/// leaf/soil station is under 8; the Go receiver's buffer is 4096 and silently truncates a
/// larger datagram, which would be a decode failure with no cause. Sized so that a frame
/// this build cannot read is an error that says so.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

impl Session for NoSession {
    fn name(&self) -> &'static str {
        "none"
    }

    fn open<'a>(
        &'a mut self,
        _io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }

    fn next_frame<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<Frame, SessionError>> {
        Box::pin(async move {
            let mut buf = vec![0u8; MAX_FRAME_BYTES];
            let n = io
                .read(&mut buf)
                .await
                .map_err(|source| SessionError::io("reading a frame", source))?;
            if n == 0 {
                return Err(SessionError::Closed {
                    what: "reading a frame",
                });
            }
            buf.truncate(n);
            Ok(Frame::new(buf))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Transcript;
    use mw_types::{Class, Classify};

    #[tokio::test]
    async fn the_identity_session_turns_each_read_into_one_frame() {
        let mut io = Transcript::reading(["{\"a\":1}", "{\"a\":2}"]);
        let mut session = NoSession;
        session.open(&mut io).await.expect("nothing to open");
        assert_eq!(
            session.next_frame(&mut io).await.expect("a frame").bytes(),
            b"{\"a\":1}"
        );
        assert_eq!(
            session.next_frame(&mut io).await.expect("a frame").bytes(),
            b"{\"a\":2}"
        );
        // ... and the end of the transcript is the end of the stream, not an empty frame.
        assert!(matches!(
            session.next_frame(&mut io).await,
            Err(SessionError::Closed { .. })
        ));
        assert_eq!(session.name(), "none");
    }

    #[test]
    fn every_session_failure_is_retryable() {
        assert_eq!(
            SessionError::io("waking the console", std::io::Error::other("hung up")).class(),
            Class::RETRYABLE
        );
        assert_eq!(
            SessionError::Silent {
                what: "awaiting a LOOP packet",
                seconds: 5
            }
            .class(),
            Class::RETRYABLE
        );
        assert_eq!(
            SessionError::Unexpected {
                what: "waking the console"
            }
            .to_string(),
            "the station answered unexpectedly while waking the console"
        );
    }
}
