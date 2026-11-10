//! [`StreamStation`] — the `Serve` body every streaming station runs in.
//!
//! One body for Davis and Campbell, because the only thing that differs between them is
//! the conversation and the codec, and both of those are values. What the body does is the
//! four-part decomposition of W2a §4a.2 turned the right way up:
//!
//! ```text
//!   build   open the transport        -> BuildError::Resource, so Backoff (Decision 3)
//!   serve   session.open(io)          -> the wake handshake, or nothing
//!           loop { next_frame -> codec.decode -> ingress.admit }
//!           return on cancellation, or on a session failure -> Backoff
//! ```
//!
//! # There is no reconnect loop here, and that is the difference
//!
//! The Go Davis and Campbell drivers each carry two: an outer one that closes the
//! connection and calls `Connect` on an error, and `Connect` itself, which loops forever
//! with a 5- or 30-second sleep and a `connecting` mutex to stop two of them running at
//! once. Between them they produce four behaviours an operator cannot see: a reconnect that
//! is skipped because another is "in progress", a sleep that is not interruptible by
//! shutdown on one path, a `logger.Fatal` on a missing device path at startup, and — the
//! one that matters — `GetLoopPackets` returning from its goroutine when
//! `GetDavisLoopPackets` returns **successfully**, which stops a healthy station for good.
//!
//! Here a connection is a *build*, so a station whose console is unplugged is
//! `Backoff { until }` with the reason in `ComponentStatus::last_error`, and the retry rate
//! is the supervisor's one exponential-backoff-with-jitter implementation (**D-27**) rather
//! than a per-driver constant. A session failure returns from [`StreamStation::run`], which
//! the supervisor treats the same way. The whole of the reconnect policy is therefore
//! outside this file, which is why there is no `connecting` flag in it.
//!
//! # What is counted
//!
//! `mw_station_frames_total{component, outcome}` per frame
//! ([`crate::metrics::FrameOutcome`]) and `mw_station_resync_bytes_total{component}` for the
//! bytes a session dropped looking for a frame boundary. The second exists for one reason:
//! a serial line whose rate does not match the console's produces nothing but
//! resynchronisation, and `crate::stream` cannot set the rate (see that module). Without a
//! number, "the line rate is wrong" and "the console is unplugged" look the same from
//! outside — one is `Backoff` and the other is a `Running` component with a rising
//! `undecodable` count and no explanation.

use std::fmt;
use std::sync::Arc;

use chrono::Utc;
use mw_codec::{Codec, CodecError, Frame};
use mw_component::{BoxFuture, BuildError, ComponentError, ComponentId, Cx, Ingress, Serve};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use tracing::{debug, error, info, warn};

use crate::identity::StationIdentity;
use crate::metrics::{self as m, FrameOutcome};
use crate::session::{ByteStream, Session};
use crate::transport::Transport;

/// Which conversation a streaming station has.
///
/// A value rather than a `Box<dyn Session>`, because the builder a `ComponentSpec` carries
/// is called **once per start attempt** and a trait object cannot be cloned. So the plan
/// holds the choice and the build makes the session — which also means a reconnect starts a
/// session with no state from the last one, rather than reusing a buffer with half a packet
/// in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Conversation {
    /// Wake the console, ask for a run of `LOOP` packets, re-issue when the run ends.
    DavisLoop {
        /// How many packets one command asks for.
        run_length: usize,
    },
    /// Newline-delimited records, with no command at all.
    Lines,
}

impl Conversation {
    /// A fresh session for one connection.
    #[must_use]
    pub fn session(self) -> Box<dyn Session> {
        match self {
            Conversation::DavisLoop { run_length } => {
                Box::new(crate::davis::DavisSession::new(run_length))
            }
            Conversation::Lines => Box::new(crate::lines::Lines::new()),
        }
    }
}

/// One connected station, mid-conversation.
pub struct StreamStation {
    io: Box<dyn ByteStream>,
    session: Box<dyn Session>,
    codec: Arc<dyn Codec>,
    station: StationKey,
    ingress: Arc<dyn Ingress>,
}

impl fmt::Debug for StreamStation {
    /// The station, the conversation and the codec. Not the `Ingress` handle, which would
    /// print a spool.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamStation")
            .field("station", &self.station)
            .field("session", &self.session.name())
            .field("codec", &self.codec.name())
            .field("io", &self.io)
            .finish_non_exhaustive()
    }
}

impl StreamStation {
    /// Open the transport and build the conversation over it.
    ///
    /// This is a station's **build**: a device path that does not exist and a bridge that is
    /// not listening are both `BuildError::Resource`, so they are `Backoff` with a readable
    /// reason rather than a startup abort (W2a Decision 3) — which is what
    /// `logger.Fatal("must provide either network hostname+port or serial device in
    /// config")` is in the Go driver.
    pub async fn connect(
        transport: &Transport,
        conversation: Conversation,
        codec: Arc<dyn Codec>,
        station: StationKey,
        ingress: Arc<dyn Ingress>,
    ) -> Result<Self, BuildError> {
        let io = crate::stream::open(transport, station.as_str())
            .await
            .map_err(|e| e.build(format!("the transport for station `{station}`")))?;
        Ok(Self::over(io, conversation, codec, station, ingress))
    }

    /// A station over a stream somebody else opened, for a test that drives a transcript or
    /// a `socat` pty rather than a device.
    pub fn over(
        io: Box<dyn ByteStream>,
        conversation: Conversation,
        codec: Arc<dyn Codec>,
        station: StationKey,
        ingress: Arc<dyn Ingress>,
    ) -> Self {
        Self {
            io,
            session: conversation.session(),
            codec,
            station,
            ingress,
        }
    }

    /// Run the conversation until it ends.
    ///
    /// Returns `Ok(())` **only** on cancellation. A session failure is an `Err`, which the
    /// supervisor turns into `Backoff` and then a fresh [`StreamStation::connect`] — so
    /// "reconnect" is a state transition rather than a loop in this file.
    pub async fn run(self: Box<Self>, cx: Cx) -> Result<(), ComponentError> {
        let id = cx.id().clone();
        let Self {
            mut io,
            mut session,
            codec,
            station,
            ingress,
        } = *self;
        // Zero every series, so a station that has never framed a packet is
        // distinguishable from a station whose metrics are broken.
        m::arm(&id);
        m::arm_resync(&id);

        session.open(&mut *io).await.map_err(|e| {
            warn!(
                %station,
                session = session.name(),
                error = %DisplayChain(&e),
                "a station conversation could not be opened"
            );
            ComponentError::tick("opening a station conversation", e)
        })?;
        info!(
            %station,
            session = session.name(),
            codec = codec.name(),
            "a station conversation is open"
        );

        let mut resynced = 0u64;
        loop {
            let next = tokio::select! {
                // Biased so that a cancelled component does not admit one more frame
                // before noticing: shutdown is ranked, and a reading admitted after the
                // spool's cursors have been told to stop is a reading nobody drains.
                biased;
                () = cx.cancelled() => {
                    debug!(%station, "a station conversation was cancelled");
                    return Ok(());
                }
                next = session.next_frame(&mut *io) => next,
            };
            // Before the frame is handled, and before an error returns: the bytes a
            // session dropped are what an operator reads for a line that is producing
            // something other than frames, and the failing read is exactly when that
            // matters most.
            let discarded = session.discarded();
            if discarded > resynced {
                m::resync(&id, discarded - resynced);
                resynced = discarded;
            }
            let frame = match next {
                Ok(frame) => frame,
                Err(e) => {
                    warn!(
                        %station,
                        session = session.name(),
                        resync_bytes = resynced,
                        error = %DisplayChain(&e),
                        "a station conversation ended; the supervisor will reconnect"
                    );
                    return Err(ComponentError::tick("reading a station frame", e));
                }
            };
            admit(&id, &codec, &station, &ingress, &frame).await?;
        }
    }
}

impl Serve for StreamStation {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(self.run(cx))
    }
}

/// Decode one frame and admit what it carried.
///
/// Returns `Err` only for a failure that should end the component. A frame that did not
/// decode, and a reading admission refused as `Ignorable`, are **counted drops**: a console
/// on a marginal cable sends a bad packet every so often, and a driver that exited for one
/// would spend its life in `Backoff`.
///
/// A free function rather than a method because [`StreamStation::run`] destructures itself
/// to hold the session and the stream as two independent borrows across the `select!`.
async fn admit(
    id: &ComponentId,
    codec: &Arc<dyn Codec>,
    station: &StationKey,
    ingress: &Arc<dyn Ingress>,
    frame: &Frame,
) -> Result<(), ComponentError> {
    let batch = match codec.decode(frame) {
        Ok(batch) => batch,
        Err(e) => {
            let outcome = match &e {
                // "The station's own configuration says this reading is a fault" is a
                // different number from "these bytes are not a frame", and the snow gauge's
                // base distance is the configuration that produces it (DEV-9).
                CodecError::Implausible { .. } => FrameOutcome::Implausible,
                _ => FrameOutcome::Undecodable,
            };
            m::frame(id, outcome);
            warn!(
                %station,
                codec = codec.name(),
                outcome = %outcome,
                error = %DisplayChain(&e),
                "a station frame did not become a reading"
            );
            return Ok(());
        }
    };
    // `Fixed`: configuration says which station, so the frame presents nothing and
    // `StationRegistry::resolve` answers from the receiving component (`crate::identity`).
    // Every streaming station driver is `Fixed` -- the wire is the credential (W2a §4a.6).
    let meta = StationIdentity::Fixed.frame_meta(id, None, Utc::now());
    match ingress.admit(&batch, &meta).await {
        Ok(admitted) => {
            m::frame(id, FrameOutcome::Decoded);
            debug!(
                %station,
                accepted = admitted.accepted,
                rejected = admitted.rejected,
                "admitted a station frame"
            );
            Ok(())
        }
        Err(e) => {
            m::frame(id, FrameOutcome::Refused);
            match e.class() {
                // A station pointed at the wrong endpoint is a counted signal, not a
                // failure of the component that received it (W2a §4.4).
                Class::Ignorable => {
                    debug!(
                        %station,
                        error = %DisplayChain(&e),
                        "a station reading was refused"
                    );
                    Ok(())
                }
                Class::Fatal => {
                    error!(
                        %station,
                        error = %DisplayChain(&e),
                        "the spool could not be written, so this node cannot durably accept \
                         readings"
                    );
                    Err(ComponentError::tick("admitting a station reading", e))
                }
                _ => Err(ComponentError::tick("admitting a station reading", e)),
            }
        }
    }
}

#[cfg(test)]
mod tests;
