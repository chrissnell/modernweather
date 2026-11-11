//! The Davis Vantage Pro/Pro2 driver: the conversation, and what the component is called.
//!
//! **This file is the reason `Session` exists.** The prompt's pipeline was
//! `Transport -> Codec -> Identity`, and W2a §4a.2's answer is that it cannot express
//! Davis: before a console emits anything you have to *wake* it, then ask for a run of
//! packets, and when the run ends you have to ask again. That is request/response
//! conversation over the transport, it is impure, and W3 **D-82** names it as the level that
//! would have caught the Davis "stops polling after one batch" defect — which no amount of
//! golden-testing [`mw_codec::DavisLoop`] can see, because the codec decoded every packet it
//! was given perfectly and there was only ever one batch of them.
//!
//! ```text
//!   open()        "\n"  ──▶                              up to three times
//!                       ◀──  "\n\r"                      the console is awake
//!
//!   next_frame()  "LOOP 20\n" ──▶
//!                       ◀──  0x06                        the console accepted it
//!                       ◀──  "LOO" + 96 bytes + CRC-16   packet 1 of 20
//!                 ...
//!                       ◀──  packet 20 of 20
//!   next_frame()  "LOOP 20\n" ──▶                        ** the re-issue **
//! ```
//!
//! # Where this disagrees with the Go driver
//!
//! **1. The run length is counted, so the re-issue is structural.** `GetDavisLoopPackets`
//! loops `n` times and *returns*; `GetLoopPackets` only calls it again on an **error**,
//! and on a clean return it `return`s from the goroutine — so a Davis console that
//! delivers twenty good packets stops being read, permanently, with no error and no log
//! line. That is the defect by name. Here the run length is state on the session
//! ([`DavisSession::remaining`]) and `next_frame` issues the command whenever it reaches
//! zero, so "the run ended" and "ask again" are the same branch.
//!
//! **2. The acknowledgement is *searched for*, not assumed to be the next byte.**
//! `sendData` reads exactly one byte and requires `0x06`. The wake handshake immediately
//! before it answers `\n\r` per newline sent, and Go sends **three** newlines and then reads
//! once — so whether the `LOOP` command's acknowledgement is the first byte of that read
//! depends on how the three answers happened to be buffered. [`DavisSession::issue`] skips
//! `\n` and `\r` while looking for the acknowledgement and refuses anything else by name.
//!
//! **3. A resynchronisation is counted.** The Go scanner searches for `LOO` and silently
//! advances past whatever was before it. Bytes discarded while looking for a header are
//! counted ([`DavisSession::discarded`]) and reported by the driver once per run, because on
//! a serial line at the wrong rate *every* byte is discarded and the only other symptom is
//! a station that reports nothing.
//!
//! **4. Every wait has a deadline.** The serial path in Go has none at all —
//! `SetReadDeadline` is called only when `config.Hostname` is set (`station.go:376`) — so a
//! console that stops talking on a serial line hangs the read forever, and the component
//! is neither failed nor running. Each of the three waits here has its own timeout and its
//! own sentence.

use std::time::Duration;

use tokio::time::Instant;

use mw_codec::Frame;
use mw_component::{BoxFuture, ComponentId, ComponentIdError};

use crate::session::{ByteStream, Session, SessionError};

/// The `type` column value this driver serves.
pub const STATION_TYPE: &str = "davis";

/// `<ACK>` — what the console answers a command it accepted.
pub const ACK: u8 = 0x06;

/// `<NAK>` — "resend", which the console answers a command it did not understand.
pub const NAK: u8 = 0x15;

/// How many packets one `LOOP` command asks for.
///
/// The Go driver's twenty, with its own comment for why: *"The Davis API supports more but
/// tends to be flaky and 20 is a safe bet for each LOOP run."* Kept rather than raised,
/// because a longer run is a longer window in which a lost packet costs a whole run.
pub const RUN_LENGTH: usize = 20;

/// How many times [`DavisSession::open`] will try to wake the console.
///
/// Go's `maxTries`, and its three newlines.
pub const WAKE_ATTEMPTS: usize = 3;

/// What an awake console answers.
pub const WAKE_RESPONSE: &[u8] = b"\n\r";

/// How long one wake attempt waits.
///
/// Longer than Go's 1.2-second sleep between newlines, because this waits for the answer
/// rather than sleeping and hoping: a console on a 2400-baud line takes a moment.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait for a command's acknowledgement.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the next packet of a run.
///
/// A Vantage emits a LOOP packet every two seconds and `cmd/davis-emulator` -- vendored at
/// `tests/emulators/davis` -- emits one every
/// 1.5 s with an optional 3-8 s "hang", so this is generous against both and still an order
/// of magnitude below the component's own grace period. The Go serial path has no deadline
/// at all, which is this module's fourth disagreement.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// The component id of one station's LOOP conversation.
pub fn station_id(station_key: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("station:davis:{station_key}"))
}

/// The wake-and-`LOOP` conversation.
#[derive(Debug)]
pub struct DavisSession {
    run_length: usize,
    remaining: usize,
    buffer: Vec<u8>,
    discarded: u64,
    wake_timeout: Duration,
    ack_timeout: Duration,
    frame_timeout: Duration,
}

impl Default for DavisSession {
    fn default() -> Self {
        Self::new(RUN_LENGTH)
    }
}

impl DavisSession {
    /// How many bytes to read at a time. A packet is 99 and arrives in as many pieces as
    /// the line produces.
    const READ_BYTES: usize = 512;

    /// How many bytes may be **discarded** looking for one header before the session gives
    /// up.
    ///
    /// Two packets' worth. Reaching it means the console is emitting something that is not
    /// LOOP records — the symptom of a line rate that does not match — and the session
    /// failing is what turns that into a restart with a message rather than a driver that
    /// reads garbage for as long as the process runs.
    ///
    /// **Counted as bytes dropped, not as bytes held.** The buffer cannot grow:
    /// [`DavisSession::take_packet`] trims it to at most ninety-eight bytes every time it
    /// returns without a frame, so a bound on `buffer.len()` was unreachable and the
    /// restart this comment describes never happened — `next_frame` read garbage forever
    /// while the resync counter climbed. The delta is what the bound has to be on.
    const MAX_RESYNC_BYTES: u64 = 2 * mw_codec::LOOP_BYTES as u64;

    /// How many bytes of noise may precede the wake answer.
    ///
    /// The console answers a newline with two bytes and nothing else, so the answer is at
    /// the front of what arrives. The bound is here because `\n\r` is also the **trailer of
    /// every LOOP record** (bytes 95 and 96): without it, a console still streaming a run
    /// from before the line dropped satisfies the handshake with a packet's own trailer and
    /// the driver proceeds as though a console that never answered had.
    ///
    /// Noise is tolerated rather than refused outright because a reconnect reads whatever
    /// the kernel had buffered first. An attempt that refuses it *drops* it — the buffer is
    /// cleared before each newline — so the next newline is the one that meets a quiet
    /// line, which is what `WAKE_ATTEMPTS` is for. What this bound does not catch is a
    /// buffer that happens to start ninety-three bytes into a record; the fresh
    /// acknowledgement `issue` requires is what catches that.
    const MAX_WAKE_NOISE: usize = 8;

    /// A session that asks for `run_length` packets at a time.
    #[must_use]
    pub fn new(run_length: usize) -> Self {
        Self {
            run_length: run_length.max(1),
            remaining: 0,
            buffer: Vec::new(),
            discarded: 0,
            wake_timeout: WAKE_TIMEOUT,
            ack_timeout: ACK_TIMEOUT,
            frame_timeout: FRAME_TIMEOUT,
        }
    }

    /// The same session with every deadline scaled down, for a test that would otherwise
    /// take fifteen seconds to prove a deadline exists.
    #[must_use]
    pub fn with_deadlines(mut self, wake: Duration, ack: Duration, frame: Duration) -> Self {
        self.wake_timeout = wake;
        self.ack_timeout = ack;
        self.frame_timeout = frame;
        self
    }

    /// How many packets are left in the current run. Zero means the next
    /// [`Session::next_frame`] re-issues the command.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining
    }

    /// How many packets one command asks for.
    #[must_use]
    pub fn remaining_run_length(&self) -> usize {
        self.run_length
    }

    /// How many bytes have been discarded while resynchronising on a packet header.
    ///
    /// Non-zero on a healthy console too — the acknowledgement's trailing noise, a
    /// half-packet left over from a previous connection — so it is a *rate* an operator
    /// reads rather than a condition. See this module's third disagreement.
    #[must_use]
    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    /// The command that asks for a run.
    fn command(&self) -> Vec<u8> {
        format!("LOOP {}\n", self.run_length).into_bytes()
    }

    /// Ask for a run and wait for the acknowledgement.
    async fn issue(&mut self, io: &mut dyn ByteStream) -> Result<(), SessionError> {
        const WHAT: &str = "asking a Davis console for a run of LOOP packets";
        // **Whatever is on the line now is not an answer to a command that has not been
        // sent.** Without this, a `0x15` left over from the wake handshake or from a
        // previous run is read as this command's refusal and kills the session, and a
        // leftover `0x06` is read as its acknowledgement — so the driver believes a run is
        // outstanding that the console never agreed to. A stale byte is roughly a one in
        // three chance of a `0x15` per residual packet, which made a reconnect against a
        // bridge that replays its buffer an unexplained `Backoff` loop. `sendData` reads
        // one byte with no such guard; this is the second disagreement.
        self.drop_buffered();
        io.write_all(&self.command())
            .await
            .map_err(|source| SessionError::io(WHAT, source))?;
        // One deadline for the whole acknowledgement, not one per read: a console
        // trickling a byte at a time renewed a per-read deadline forever, which is
        // sixteen minutes of waiting at `ACK_TIMEOUT` and `MAX_RESYNC_BYTES`.
        let until = Instant::now() + self.ack_timeout;
        // How many bytes have been skipped looking for the acknowledgement, bounded so
        // that a console answering something else forever is a session failure rather than
        // a loop.
        let mut skipped = 0u64;
        loop {
            while !self.buffer.is_empty() {
                let byte = self.buffer.remove(0);
                match byte {
                    ACK => {
                        self.remaining = self.run_length;
                        return Ok(());
                    }
                    // "Resend", which the console answers a command it did not
                    // understand. Named rather than lumped in with the noise below,
                    // because it means the console heard and refused.
                    NAK => return Err(SessionError::Unexpected { what: WHAT }),
                    // The wake handshake's `\n\r`, and whatever the previous run left on
                    // the line. The one byte `sendData` reads could be any of these, which
                    // is this module's second disagreement.
                    _ => {
                        self.discarded += 1;
                        skipped += 1;
                        if skipped > Self::MAX_RESYNC_BYTES {
                            return Err(SessionError::Unexpected { what: WHAT });
                        }
                    }
                }
            }
            self.fill(io, until, self.ack_timeout, WHAT).await?;
        }
    }

    /// Drop whatever is buffered, counting it, because it belongs to a previous phase.
    ///
    /// The count is the point: these bytes are exactly what an operator reads
    /// `mw_station_resync_bytes_total` for, and dropping them silently would make a line
    /// that is producing something other than packets look like a healthy one.
    fn drop_buffered(&mut self) {
        self.discarded += self.buffer.len() as u64;
        self.buffer.clear();
    }

    /// Read until there is something in the buffer, or `until` passes.
    ///
    /// `until` is the caller's **whole-phase** deadline and `budget` is only what the
    /// failure reports, so every read in one wake, one acknowledgement or one frame shares
    /// one clock. A deadline per read is not the same bound: it is renewed by every byte
    /// that arrives, so a console emitting continuous garbage never reaches it at all.
    async fn fill(
        &mut self,
        io: &mut dyn ByteStream,
        until: Instant,
        budget: Duration,
        what: &'static str,
    ) -> Result<(), SessionError> {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(SessionError::Silent {
                what,
                seconds: budget.as_secs(),
            });
        }
        let mut chunk = vec![0u8; Self::READ_BYTES];
        let read = tokio::time::timeout(left, io.read(&mut chunk));
        let n = match read.await {
            Ok(Ok(n)) => n,
            Ok(Err(source)) => return Err(SessionError::io(what, source)),
            Err(_) => {
                return Err(SessionError::Silent {
                    what,
                    seconds: budget.as_secs(),
                })
            }
        };
        if n == 0 {
            return Err(SessionError::Closed { what });
        }
        self.buffer.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// One whole packet out of the buffer, if there is one.
    ///
    /// Resynchronises on the three-byte header, which is the only framing the protocol has:
    /// a LOOP record is ninety-nine bytes with no terminator.
    fn take_packet(&mut self) -> Option<Frame> {
        let header = mw_codec::LOOP_HEADER;
        loop {
            let at = self
                .buffer
                .windows(header.len())
                .position(|window| window == header);
            match at {
                // A whole packet at the front.
                Some(0) if self.buffer.len() >= mw_codec::LOOP_BYTES => {
                    return Some(Frame::new(
                        self.buffer
                            .drain(..mw_codec::LOOP_BYTES)
                            .collect::<Vec<u8>>(),
                    ));
                }
                // The start of one: more bytes are needed and none may be dropped.
                Some(0) => return None,
                // A header further in: everything before it is noise. Looped rather than
                // returning, because the bytes after it may already be a whole packet and
                // the caller's next step is to WAIT for more.
                Some(at) => {
                    self.buffer.drain(..at);
                    self.discarded += at as u64;
                }
                // No header anywhere. Everything but the last two bytes is noise -- a
                // header can straddle a read boundary -- and dropping it is what keeps the
                // buffer from growing on a line that never produces one.
                None => {
                    if self.buffer.len() >= header.len() {
                        let drop = self.buffer.len() - (header.len() - 1);
                        self.buffer.drain(..drop);
                        self.discarded += drop as u64;
                    }
                    return None;
                }
            }
        }
    }
}

impl Session for DavisSession {
    fn name(&self) -> &'static str {
        "davis-loop"
    }

    fn open<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            const WHAT: &str = "waking a Davis console";
            // A new connection, and no run is outstanding -- the console forgets its
            // `LOOP` state when the line drops. What a new connection does **not**
            // guarantee is a quiet line: a serial console keeps streaming into the
            // character device after the previous process let go of it, and a `ser2net`
            // bridge replays what it buffered. So each attempt below clears first.
            self.remaining = 0;
            let mut answered = false;
            for _ in 0..WAKE_ATTEMPTS {
                // Anything already on the line predates this newline and cannot be its
                // answer. Dropping it is also what makes a second attempt useful: the
                // newline just written stops a console that was mid-run, so the attempt
                // after it meets the quiet line this handshake assumes.
                self.drop_buffered();
                io.write_all(b"\n")
                    .await
                    .map_err(|source| SessionError::io(WHAT, source))?;
                let until = Instant::now() + self.wake_timeout;
                match self.fill(io, until, self.wake_timeout, WHAT).await {
                    Ok(()) => answered = true,
                    // No answer to this newline. Another newline is the protocol's own
                    // answer -- a console asleep in its power-saving state misses the
                    // first one -- so a silent attempt is retried rather than returned.
                    Err(SessionError::Silent { .. }) => continue,
                    Err(other) => return Err(other),
                }
                if let Some(at) = self
                    .buffer
                    .windows(WAKE_RESPONSE.len())
                    .position(|window| window == WAKE_RESPONSE)
                    // **Not anywhere in the buffer.** `\n\r` is the trailer of every LOOP
                    // record, so an unbounded search is satisfied by a packet the console
                    // sent before it was asked anything -- see `MAX_WAKE_NOISE`.
                    .filter(|at| *at <= Self::MAX_WAKE_NOISE)
                {
                    // Everything up to and including the answer is the handshake's, and is
                    // consumed; anything after it is not. Draining rather than clearing,
                    // because a read can carry the answer and whatever came next -- and a
                    // `clear` here would silently eat it, which is a bug that only appears
                    // when two things arrive in one read.
                    let through = at + WAKE_RESPONSE.len();
                    self.buffer.drain(..through);
                    // Counted, because these bytes were dropped: `issue` would have skipped
                    // them anyway and the count is what an operator reads for a line that
                    // is producing something other than packets.
                    self.discarded += through as u64;
                    return Ok(());
                }
            }
            Err(if answered {
                // Something is on the line and it is not a Davis console saying hello.
                SessionError::Unexpected { what: WHAT }
            } else {
                SessionError::Silent {
                    what: WHAT,
                    seconds: self.wake_timeout.as_secs() * WAKE_ATTEMPTS as u64,
                }
            })
        })
    }

    fn discarded(&self) -> u64 {
        self.discarded
    }

    fn next_frame<'a>(
        &'a mut self,
        io: &'a mut dyn ByteStream,
    ) -> BoxFuture<'a, Result<Frame, SessionError>> {
        Box::pin(async move {
            const WHAT: &str = "awaiting a Davis LOOP packet";
            // **The re-issue.** One branch, reached whenever the run is spent, which is
            // what makes "stops polling after one batch" unrepresentable here.
            if self.remaining == 0 {
                self.issue(io).await?;
            }
            // Both bounds are on this one frame: `until` is how long it may take and
            // `dropped` is how much noise it may cost. Neither is expressible per read --
            // a line at the wrong rate produces bytes steadily and trims the buffer on
            // every pass, so it renews a per-read deadline and never grows a buffer.
            let until = Instant::now() + self.frame_timeout;
            let dropped = self.discarded;
            loop {
                if let Some(frame) = self.take_packet() {
                    self.remaining -= 1;
                    return Ok(frame);
                }
                if self.discarded - dropped > Self::MAX_RESYNC_BYTES {
                    self.drop_buffered();
                    return Err(SessionError::Unexpected { what: WHAT });
                }
                self.fill(io, until, self.frame_timeout, WHAT).await?;
            }
        })
    }
}

#[cfg(test)]
mod tests;
