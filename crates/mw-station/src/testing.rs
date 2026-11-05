//! Test support: a recorded transcript, and an [`Ingress`] that records instead of
//! storing.
//!
//! Public rather than `#[cfg(test)]`, for the reason `mw_pipeline::testing` and
//! `mw_supervisor::testing` are: this crate's integration tests are separate compilation
//! units, and W3 **D-82**'s middle level — "`Session` against a recorded transcript" — has
//! to be reachable from one.
//!
//! Nothing here has a production use. The real `Ingress` is `mw-pipeline`'s over the spool;
//! the real byte streams are a serial port, a TCP connection and a UDP socket.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use mw_component::{
    AdmitError, Admitted, BoxFuture, FrameMeta, Ingress, Reject, StationMeta, StationProjection,
    StationRegistry,
};
use mw_types::{Envelope, OriginId, PartialReading, Seq, StationKey};

use crate::session::ByteStream;

/// A [`ByteStream`] over a script: bytes to hand back, and bytes the session must write.
///
/// **A mismatched write is an error from the stream**, not a panic and not a silent pass.
/// That is what makes this a transcript rather than a stub: a session that stops re-issuing
/// its command does not merely read fewer frames, it fails the script at the point the
/// protocol required a write — which is the shape of the Davis defect W3 D-82 names.
#[derive(Debug)]
pub struct Transcript {
    reads: VecDeque<Vec<u8>>,
    expected: VecDeque<Vec<u8>>,
    written: Vec<Vec<u8>>,
}

impl Transcript {
    /// A transcript that only hands bytes back, for a session that writes nothing.
    pub fn reading<I, B>(reads: I) -> Self
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        Self {
            reads: reads.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            expected: VecDeque::new(),
            written: Vec::new(),
        }
    }

    /// A transcript that also asserts what the session writes, in order.
    pub fn scripted<R, W, B, C>(reads: R, writes: W) -> Self
    where
        R: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
        W: IntoIterator<Item = C>,
        C: AsRef<[u8]>,
    {
        Self {
            reads: reads.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            expected: writes.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            written: Vec::new(),
        }
    }

    /// Everything the session wrote, in order.
    pub fn written(&self) -> &[Vec<u8>] {
        &self.written
    }

    /// How many scripted reads are left.
    pub fn remaining(&self) -> usize {
        self.reads.len()
    }

    /// Whether every scripted write was performed.
    pub fn exhausted(&self) -> bool {
        self.expected.is_empty()
    }
}

impl ByteStream for Transcript {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            let Some(chunk) = self.reads.pop_front() else {
                // The end of the script is the end of the stream, which is what a station
                // that hung up looks like.
                return Ok(0);
            };
            if chunk.len() > buf.len() {
                return Err(std::io::Error::other(format!(
                    "the transcript's next chunk is {} bytes and the session offered a \
                     {}-byte buffer",
                    chunk.len(),
                    buf.len()
                )));
            }
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            self.written.push(bytes.to_vec());
            match self.expected.pop_front() {
                None => Ok(()),
                Some(want) if want == bytes => Ok(()),
                Some(want) => Err(std::io::Error::other(format!(
                    "the transcript expected the session to write {want:?} and it wrote \
                     {bytes:?}"
                ))),
            }
        })
    }
}

/// What one [`RecordingIngress::admit`] was handed.
#[derive(Debug, Clone)]
pub struct Admission {
    /// The readings the codec produced.
    pub batch: Vec<PartialReading>,
    /// Which component received the frame.
    pub receiver: String,
    /// What the frame claimed, for a non-`Fixed` identity.
    pub presented_key: Option<String>,
    /// When the transport observed the frame.
    pub received_at: DateTime<Utc>,
}

/// An [`Ingress`] that records what it was handed.
///
/// `admit` only. `accept` is the peer path and no station driver takes it, so it refuses
/// with a reason rather than recording — a driver that called it would be a bug this double
/// should surface, not absorb.
#[derive(Debug, Default)]
pub struct RecordingIngress {
    admitted: Mutex<Vec<Admission>>,
    refuse: Mutex<Option<Reject>>,
}

impl RecordingIngress {
    /// An ingress that accepts everything.
    pub fn accepting() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An ingress whose identity resolution refuses everything — what an unknown station
    /// looks like from inside a driver.
    pub fn refusing(reason: Reject) -> Arc<Self> {
        Arc::new(Self {
            admitted: Mutex::new(Vec::new()),
            refuse: Mutex::new(Some(reason)),
        })
    }

    /// Every admission, in order.
    pub fn admissions(&self) -> Vec<Admission> {
        self.admitted
            .lock()
            .expect("the recording ingress is not poisoned")
            .clone()
    }

    /// How many readings have been admitted in total.
    pub fn readings(&self) -> usize {
        self.admissions().iter().map(|a| a.batch.len()).sum()
    }
}

impl Ingress for RecordingIngress {
    fn admit<'a>(
        &'a self,
        batch: &'a [PartialReading],
        meta: &'a FrameMeta,
    ) -> BoxFuture<'a, Result<Admitted, AdmitError>> {
        Box::pin(async move {
            if let Some(reason) = *self.refuse.lock().expect("not poisoned") {
                return Err(AdmitError::Rejected(reason));
            }
            let mut log = self.admitted.lock().expect("not poisoned");
            log.push(Admission {
                batch: batch.to_vec(),
                receiver: meta.receiver.as_str().to_string(),
                presented_key: meta.presented_key.clone(),
                received_at: meta.received_at,
            });
            Ok(Admitted {
                accepted: u32::try_from(batch.len()).unwrap_or(u32::MAX),
                rejected: 0,
                seq_high: Seq::new(log.len() as u64),
            })
        })
    }

    fn accept<'a>(
        &'a self,
        _batch: &'a [Envelope],
        _from: OriginId,
    ) -> BoxFuture<'a, Result<mw_component::Accepted, mw_component::AcceptError>> {
        Box::pin(async {
            Err(mw_component::AcceptError::Rejected(
                Reject::UnauthorizedPeer,
            ))
        })
    }
}

/// A [`StationRegistry`] that resolves one claim, for a test that needs the gate itself
/// rather than a driver's view of it.
#[derive(Debug)]
pub struct OneStation {
    claim: String,
    station: StationKey,
}

impl OneStation {
    /// A registry in which `claim` is `station` and nothing else resolves.
    pub fn new(claim: &str, station: &str) -> Arc<Self> {
        Arc::new(Self {
            claim: claim.to_ascii_lowercase(),
            station: StationKey::new(station),
        })
    }
}

impl StationRegistry for OneStation {
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject> {
        match meta.presented_key.as_deref() {
            Some(claim) if claim.to_ascii_lowercase() == self.claim => Ok(self.station.clone()),
            _ => Err(Reject::UnknownStation),
        }
    }

    /// Nothing configured. A driver test asserts what the *codec* decoded, and an enriched
    /// column here would make the expected reading depend on this registry's coordinates
    /// rather than on the frame -- `mw_pipeline::enrich`'s own tests are where the derived
    /// values are asserted.
    fn metadata(&self, _station: &StationKey) -> StationMeta {
        StationMeta::EMPTY
    }

    fn projection(&self, _station: &StationKey) -> Option<StationProjection> {
        None
    }
}
