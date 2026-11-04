//! The on-disk encodings of docs/collector.md §3.2, §1.3 and §3.4, as pure functions.
//!
//! Separated from the I/O deliberately: "truncate the segment at the first bad record" is
//! a property of the *framing*, and the test that matters — a segment truncated at every
//! byte offset of its last record — is a test of these functions and not of the
//! filesystem. Everything here is little-endian and CRC32C-protected, and nothing here
//! opens a file.

use crc32c::crc32c;
use mw_types::{OriginId, Seq};

/// `"MWSEG\0\0\0"` — a segment file's first eight bytes.
pub const SEGMENT_MAGIC: [u8; 8] = *b"MWSEG\0\0\0";

/// `"MWSPOOL\0"` — the manifest's first eight bytes.
pub const MANIFEST_MAGIC: [u8; 8] = *b"MWSPOOL\0";

/// The framing version. `format_version` covers the framing; protobuf's own compatibility
/// rules cover the payload, which is what makes an in-place upgrade with a non-empty spool
/// safe (W7 **D-11**).
pub const FORMAT_VERSION: u16 = 1;

/// A segment header: magic, version, reserved, `first_seq`, `created_at`, CRC.
pub const SEGMENT_HEADER_BYTES: usize = 32;

/// A record's fixed prefix: `len`, `seq`, `crc`.
pub const RECORD_HEADER_BYTES: usize = 16;

/// One cursor slot. Two per file, written alternately (§3.4).
pub const CURSOR_SLOT_BYTES: usize = 64;

/// The bytes of a slot the CRC covers — everything but the CRC itself.
pub const CURSOR_SLOT_BODY_BYTES: usize = CURSOR_SLOT_BYTES - 4;

/// How far apart the two slots sit.
///
/// 4096, not 64. §3.4's reasoning is "a crash can leave one slot torn; the CRC finds it and
/// the other slot is a valid earlier position" — and that is only true if a single torn
/// sector cannot damage both. Adjacent 64-byte slots share sector 0 of the file, so the
/// stated mechanism did not hold: the consequence was bounded (a cursor with both slots
/// torn restarts from nothing and replays, which the dedup gate and the writer's upsert
/// absorb) but the claim was wrong. One constant buys it.
pub const CURSOR_SLOT_STRIDE: usize = 4096;

/// A whole cursor file: two slots, one per 4 KiB sector.
pub const CURSOR_FILE_BYTES: usize = CURSOR_SLOT_STRIDE + CURSOR_SLOT_BYTES;

/// Where slot `i` begins.
pub const fn cursor_slot_offset(slot: usize) -> usize {
    slot * CURSOR_SLOT_STRIDE
}

/// This node's own row in the origin table. Always present, always index 0.
pub const SELF_ORIGIN_ID: u16 = 0;

/// The most origins one node's intern table may hold (W7 **D-8**'s `MAX_ORIGINS`).
///
/// 1024 at 34 bytes each is a 35 KiB manifest. The cap exists because the table grows on
/// first sight of an unseen `NodeId`, which is growth an untrusted peer drives, and it is
/// loud rather than silent because reaching it means either a fleet far larger than this is
/// designed for or a misbehaving peer.
pub const MAX_ORIGINS: usize = 1024;

// --- segment header -------------------------------------------------------------------

/// A segment's header, written once at creation and `fsync`ed before the first record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// The `seq` of this segment's first record. Also its file name.
    pub first_seq: Seq,
    /// When it was created, unix milliseconds.
    pub created_at: i64,
}

impl SegmentHeader {
    /// Encode the 32 bytes.
    pub fn encode(&self) -> [u8; SEGMENT_HEADER_BYTES] {
        let mut out = [0u8; SEGMENT_HEADER_BYTES];
        out[0..8].copy_from_slice(&SEGMENT_MAGIC);
        out[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        // out[10..12] is `reserved`, left zero.
        out[12..20].copy_from_slice(&self.first_seq.get().to_le_bytes());
        out[20..28].copy_from_slice(&self.created_at.to_le_bytes());
        let crc = crc32c(&out[0..28]);
        out[28..32].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode the 32 bytes, or say what is wrong with them.
    pub fn decode(bytes: &[u8]) -> Result<Self, HeaderProblem> {
        if bytes.len() < SEGMENT_HEADER_BYTES {
            return Err(HeaderProblem::Short { len: bytes.len() });
        }
        if bytes[0..8] != SEGMENT_MAGIC {
            return Err(HeaderProblem::NotASegment);
        }
        let crc = u32::from_le_bytes(bytes[28..32].try_into().expect("four bytes"));
        if crc != crc32c(&bytes[0..28]) {
            return Err(HeaderProblem::BadCrc);
        }
        let version = u16::from_le_bytes(bytes[8..10].try_into().expect("two bytes"));
        if version != FORMAT_VERSION {
            return Err(HeaderProblem::Version { found: version });
        }
        Ok(SegmentHeader {
            first_seq: Seq::new(u64::from_le_bytes(
                bytes[12..20].try_into().expect("eight bytes"),
            )),
            created_at: i64::from_le_bytes(bytes[20..28].try_into().expect("eight bytes")),
        })
    }
}

/// Why a segment header could not be read.
///
/// Distinct from a bad *record*, and the difference is the whole of the recovery policy: a
/// torn record truncates the segment, because that is what a power loss mid-append looks
/// like. A segment whose **header** is unreadable was never validly created, so truncating
/// it to zero records would silently discard every record behind it, and that is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderProblem {
    /// Fewer than 32 bytes in the file.
    #[error("the segment is {len} bytes, shorter than its own 32-byte header")]
    Short {
        /// How many bytes there were.
        len: usize,
    },
    /// The magic is not `MWSEG`.
    #[error("the file does not begin with a segment magic")]
    NotASegment,
    /// The header CRC does not match.
    #[error("the segment header's CRC32C does not match")]
    BadCrc,
    /// A framing version this build does not know.
    #[error("the segment is framing version {found}, and this build writes {FORMAT_VERSION}")]
    Version {
        /// The version in the file.
        found: u16,
    },
}

// --- records --------------------------------------------------------------------------

/// Encode one record: `len`, `seq`, `crc(seq ‖ payload)`, payload.
///
/// The record carries its own length and CRC rather than relying on the file length,
/// which is exactly what makes a torn tail recoverable: the file length after a power loss
/// is whatever the page cache managed, and the only way to know where the valid records
/// stop is to have each one say.
pub fn encode_record(seq: Seq, payload: &[u8], into: &mut Vec<u8>) {
    let len = u32::try_from(payload.len()).expect("a record payload is bounded by the batch limit");
    into.extend_from_slice(&len.to_le_bytes());
    into.extend_from_slice(&seq.get().to_le_bytes());
    into.extend_from_slice(&record_crc(seq, payload).to_le_bytes());
    into.extend_from_slice(payload);
}

/// How many bytes `encode_record` will write.
pub fn record_bytes(payload_len: usize) -> usize {
    RECORD_HEADER_BYTES + payload_len
}

/// CRC32C over `seq ‖ payload`.
///
/// The `seq` is inside the CRC so that a record header whose length and sequence are
/// intact but whose payload is torn is still detected, and so that two adjacent records
/// cannot be confused by a write that landed at the wrong offset.
fn record_crc(seq: Seq, payload: &[u8]) -> u32 {
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&seq.get().to_le_bytes());
    buf.extend_from_slice(payload);
    crc32c(&buf)
}

/// One record, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record<'a> {
    /// Its sequence number.
    pub seq: Seq,
    /// Its payload — a protobuf-encoded `collector.v1.Envelope`.
    pub payload: &'a [u8],
}

/// What a forward scan of a segment's records found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan<'a> {
    /// The records that validated, oldest first.
    pub records: Vec<Record<'a>>,
    /// How many bytes of the segment are valid, header included. A file longer than this
    /// has a torn tail and is truncated here.
    pub valid_bytes: usize,
    /// Why the scan stopped, when it stopped before the end of the file.
    pub torn: Option<Torn>,
}

/// Why a record scan stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Torn {
    /// Fewer than 16 bytes left, so there is not even a record header.
    PartialHeader,
    /// The header's `len` runs past the end of the file.
    LengthPastEnd {
        /// The length it claimed.
        claimed: u32,
        /// How many payload bytes were actually there.
        available: usize,
    },
    /// The header and payload are both present and the CRC does not match them.
    BadCrc {
        /// The sequence number the torn record claimed.
        seq: Seq,
    },
}

impl std::fmt::Display for Torn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Torn::PartialHeader => f.write_str("a partial record header"),
            Torn::LengthPastEnd { claimed, available } => write!(
                f,
                "a record claiming {claimed} payload bytes with {available} in the file"
            ),
            Torn::BadCrc { seq } => write!(f, "a CRC32C mismatch on the record at seq {seq}"),
        }
    }
}

/// Scan a segment's records forward from the end of its header.
///
/// Stops at the first record that is short, over-long or fails its CRC, and reports both
/// the records before it and where the valid bytes end. Everything before a bad record is
/// valid and everything after is discarded (W7 **D-12**) — which is the correct response
/// to the only way this happens in practice, a power loss mid-append.
pub fn scan_records(segment: &[u8]) -> Result<Scan<'_>, HeaderProblem> {
    SegmentHeader::decode(segment)?;
    let mut records = Vec::new();
    let mut at = SEGMENT_HEADER_BYTES;
    loop {
        let remaining = segment.len() - at;
        if remaining == 0 {
            return Ok(Scan {
                records,
                valid_bytes: at,
                torn: None,
            });
        }
        if remaining < RECORD_HEADER_BYTES {
            return Ok(Scan {
                records,
                valid_bytes: at,
                torn: Some(Torn::PartialHeader),
            });
        }
        let len = u32::from_le_bytes(segment[at..at + 4].try_into().expect("four bytes"));
        let seq = Seq::new(u64::from_le_bytes(
            segment[at + 4..at + 12].try_into().expect("eight bytes"),
        ));
        let crc = u32::from_le_bytes(segment[at + 12..at + 16].try_into().expect("four bytes"));
        let body = at + RECORD_HEADER_BYTES;
        let available = segment.len() - body;
        let Ok(len_usize) = usize::try_from(len) else {
            return Ok(Scan {
                records,
                valid_bytes: at,
                torn: Some(Torn::LengthPastEnd {
                    claimed: len,
                    available,
                }),
            });
        };
        if len_usize > available {
            return Ok(Scan {
                records,
                valid_bytes: at,
                torn: Some(Torn::LengthPastEnd {
                    claimed: len,
                    available,
                }),
            });
        }
        let payload = &segment[body..body + len_usize];
        if crc != record_crc(seq, payload) {
            return Ok(Scan {
                records,
                valid_bytes: at,
                torn: Some(Torn::BadCrc { seq }),
            });
        }
        records.push(Record { seq, payload });
        at = body + len_usize;
    }
}

// --- manifest -------------------------------------------------------------------------

/// One row of the manifest's origin table (docs/collector.md §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    /// The dense local index. Never reused, like a metric column (**D-21**).
    pub origin_id: u16,
    /// The globally unique node identity the wire carries.
    pub node_id: OriginId,
    /// When this node first saw that one, unix milliseconds.
    pub first_seen: i64,
    /// The highest `seq` accepted from that origin — §4.4's dedup high water.
    ///
    /// Here rather than in a file of its own so that **one** `fsync` covers both the
    /// origin table and the dedup state, and so that the manifest can never claim a high
    /// water the segments do not contain (§3.3's ordering).
    pub hw_seq: u64,
}

/// A row: `origin_id`, `node_id`, `first_seen`, `hw_seq`.
const ORIGIN_ROW_BYTES: usize = 2 + 16 + 8 + 8;

/// Everything before the origin table: magic, version, node id, count.
const MANIFEST_HEADER_BYTES: usize = 8 + 2 + 16 + 2;

/// The spool's manifest: this node's identity, the origin table, the dedup high water.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// This node's own `NodeId`.
    pub node_id: OriginId,
    /// The intern table, by `origin_id`.
    pub origins: Vec<Origin>,
}

impl Manifest {
    /// A manifest for a fresh spool: this node interned as `origin_id` 0, no peers.
    ///
    /// **This node is a row in its own origin table, and that is what discharges §3.6
    /// step 4.** Recovery needs the local append position when the spool is empty — "or
    /// `MANIFEST`'s node seq if the spool is empty" — and §1.3's declared layout has
    /// nowhere else to put it. Giving the node row 0 makes `hw_seq` mean one thing for
    /// every row: *the highest `seq` this node has accepted from that origin*, which for
    /// the node itself is the tail of its own log, because `admit` mints the next append
    /// position and `accept` consumes one too (W7 **D-4**).
    ///
    /// The consequence for [`MAX_ORIGINS`] is one slot: 1023 peers, not 1024.
    pub fn new(node_id: OriginId, at: i64) -> Self {
        Self {
            node_id,
            origins: vec![Origin {
                origin_id: SELF_ORIGIN_ID,
                node_id,
                first_seen: at,
                hw_seq: 0,
            }],
        }
    }

    /// The row for `node`, if it has been interned.
    pub fn find(&self, node: OriginId) -> Option<&Origin> {
        self.origins.iter().find(|o| o.node_id == node)
    }

    /// The row for `node`, mutably.
    pub fn find_mut(&mut self, node: OriginId) -> Option<&mut Origin> {
        self.origins.iter_mut().find(|o| o.node_id == node)
    }

    /// Intern an unseen `NodeId`, or return the row it already has.
    ///
    /// The only place the origin table grows (docs/collector.md §1.3), and therefore the
    /// only place [`MAX_ORIGINS`] can be reached. `None` means the table is full, which is
    /// a counted rejection and not a silent drop: reaching 1024 means either a fleet far
    /// larger than designed for or a misbehaving peer, and both want saying out loud.
    pub fn intern(&mut self, node: OriginId, at: i64) -> Option<u16> {
        if let Some(found) = self.find(node) {
            return Some(found.origin_id);
        }
        if self.origins.len() >= MAX_ORIGINS {
            return None;
        }
        // Dense and never reused: the next index is the count, and a row is never removed.
        let origin_id = u16::try_from(self.origins.len()).expect("MAX_ORIGINS is 1024");
        self.origins.push(Origin {
            origin_id,
            node_id: node,
            first_seen: at,
            hw_seq: 0,
        });
        Some(origin_id)
    }

    /// The tail of this node's own log — the highest local append position.
    pub fn tail_seq(&self) -> Seq {
        Seq::new(self.find(self.node_id).map_or(0, |o| o.hw_seq))
    }

    /// Record a new tail.
    pub fn set_tail_seq(&mut self, seq: Seq) {
        let node = self.node_id;
        if let Some(row) = self.find_mut(node) {
            row.hw_seq = seq.get();
        }
    }

    /// Encode the whole manifest, CRC last.
    pub fn encode(&self) -> Vec<u8> {
        let count = u16::try_from(self.origins.len()).expect("MAX_ORIGINS is 1024");
        let mut out =
            Vec::with_capacity(MANIFEST_HEADER_BYTES + self.origins.len() * ORIGIN_ROW_BYTES + 4);
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(self.node_id.as_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        for o in &self.origins {
            out.extend_from_slice(&o.origin_id.to_le_bytes());
            out.extend_from_slice(o.node_id.as_bytes());
            out.extend_from_slice(&o.first_seen.to_le_bytes());
            out.extend_from_slice(&o.hw_seq.to_le_bytes());
        }
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode a manifest.
    ///
    /// A CRC failure here is **not** recoverable by truncation and the spool refuses to
    /// start on it (§3.6 step 1): a lost origin table would mis-dedup, because `hw_seq`
    /// read as zero makes every envelope a peer ever sent look new.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestProblem> {
        if bytes.len() < MANIFEST_HEADER_BYTES + 4 {
            return Err(ManifestProblem::Short { len: bytes.len() });
        }
        if bytes[0..8] != MANIFEST_MAGIC {
            return Err(ManifestProblem::NotAManifest);
        }
        let body = bytes.len() - 4;
        let crc = u32::from_le_bytes(bytes[body..].try_into().expect("four bytes"));
        if crc != crc32c(&bytes[..body]) {
            return Err(ManifestProblem::BadCrc);
        }
        let version = u16::from_le_bytes(bytes[8..10].try_into().expect("two bytes"));
        if version != FORMAT_VERSION {
            return Err(ManifestProblem::Version { found: version });
        }
        let node_id = OriginId::new(bytes[10..26].try_into().expect("sixteen bytes"));
        let count = usize::from(u16::from_le_bytes(
            bytes[26..28].try_into().expect("two bytes"),
        ));
        if count > MAX_ORIGINS {
            // `intern` can never produce this, so a manifest claiming it is corrupt or
            // hand-edited. Refusing is the same answer as a CRC failure and for the same
            // reason (§3.6 step 1): a wrong origin table mis-dedups.
            return Err(ManifestProblem::TooManyOrigins { origins: count });
        }
        let expected = MANIFEST_HEADER_BYTES + count * ORIGIN_ROW_BYTES + 4;
        if bytes.len() != expected {
            return Err(ManifestProblem::Truncated {
                origins: count,
                expected,
                found: bytes.len(),
            });
        }
        let mut origins = Vec::with_capacity(count);
        for i in 0..count {
            let at = MANIFEST_HEADER_BYTES + i * ORIGIN_ROW_BYTES;
            origins.push(Origin {
                origin_id: u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes")),
                node_id: OriginId::new(bytes[at + 2..at + 18].try_into().expect("sixteen bytes")),
                first_seen: i64::from_le_bytes(
                    bytes[at + 18..at + 26].try_into().expect("eight bytes"),
                ),
                hw_seq: u64::from_le_bytes(
                    bytes[at + 26..at + 34].try_into().expect("eight bytes"),
                ),
            });
        }
        Ok(Manifest { node_id, origins })
    }
}

/// Why a manifest could not be read. Every variant refuses the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManifestProblem {
    /// Too short to hold a header and a CRC.
    #[error("the manifest is {len} bytes, shorter than an empty one")]
    Short {
        /// How many bytes there were.
        len: usize,
    },
    /// The magic is not `MWSPOOL`.
    #[error("the file does not begin with a manifest magic")]
    NotAManifest,
    /// The CRC does not match.
    #[error("the manifest's CRC32C does not match")]
    BadCrc,
    /// A framing version this build does not know.
    #[error("the manifest is framing version {found}, and this build writes {FORMAT_VERSION}")]
    Version {
        /// The version in the file.
        found: u16,
    },
    /// More origins than [`MAX_ORIGINS`], which `intern` cannot produce.
    #[error("the manifest claims {origins} origins, more than the {MAX_ORIGINS} allowed")]
    TooManyOrigins {
        /// The count it claimed.
        origins: usize,
    },
    /// The origin count and the file length disagree.
    #[error(
        "the manifest claims {origins} origins, which needs {expected} bytes, and it is {found}"
    )]
    Truncated {
        /// The count it claimed.
        origins: usize,
        /// How long that makes the file.
        expected: usize,
        /// How long it is.
        found: usize,
    },
}

// --- cursor slots ---------------------------------------------------------------------

/// Whether a cursor is still a consumer, or has been given up on.
///
/// An `Abandoned` cursor is ignored for reclamation (§3.5), which is the whole point: a
/// cursor that will never catch up must not pin segments forever, and the alternative to
/// abandoning it is the ceiling — a node-wide failure attributed to whichever station
/// happened to admit last rather than to the peer that caused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    /// A live consumer. Its position holds segments.
    Active,
    /// Past `max_lag`. Its component is `Failed { CursorAbandoned }` and its segments are
    /// reclaimed.
    Abandoned,
}

impl CursorState {
    const fn code(self) -> u8 {
        match self {
            CursorState::Active => 0,
            CursorState::Abandoned => 1,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(CursorState::Active),
            1 => Some(CursorState::Abandoned),
            _ => None,
        }
    }

    /// The metric label and log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            CursorState::Active => "active",
            CursorState::Abandoned => "abandoned",
        }
    }
}

/// One slot of a cursor file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSlot {
    /// Monotonic, incremented on every write. The valid slot with the higher generation
    /// is the live one.
    pub generation: u64,
    /// Every record at or below this `seq` is durably handled.
    pub acked_seq: u64,
    /// When, unix milliseconds. The input to
    /// `mw_spool_oldest_unacked_age_seconds` and to `max_lag`.
    pub acked_at: i64,
    /// Active, or abandoned.
    pub state: CursorState,
}

impl CursorSlot {
    /// A cursor that has acknowledged nothing.
    pub fn initial(at: i64) -> Self {
        Self {
            generation: 1,
            acked_seq: 0,
            acked_at: at,
            state: CursorState::Active,
        }
    }

    /// Encode the 64 bytes.
    pub fn encode(&self) -> [u8; CURSOR_SLOT_BYTES] {
        let mut out = [0u8; CURSOR_SLOT_BYTES];
        out[0..8].copy_from_slice(&self.generation.to_le_bytes());
        out[8..16].copy_from_slice(&self.acked_seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.acked_at.to_le_bytes());
        out[24] = self.state.code();
        // out[25..60] is `reserved`, left zero.
        let crc = crc32c(&out[0..CURSOR_SLOT_BODY_BYTES]);
        out[CURSOR_SLOT_BODY_BYTES..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode a slot, or `None` if it is torn.
    ///
    /// `None` rather than an error: a torn slot is the **expected** outcome of a crash
    /// mid-write, the other slot is then a valid earlier position, and the cost is one
    /// replay rather than a loss. An error type here would make the caller decide
    /// something that has only one right answer.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CURSOR_SLOT_BYTES {
            return None;
        }
        let crc = u32::from_le_bytes(
            bytes[CURSOR_SLOT_BODY_BYTES..CURSOR_SLOT_BYTES]
                .try_into()
                .expect("four bytes"),
        );
        if crc != crc32c(&bytes[0..CURSOR_SLOT_BODY_BYTES]) {
            return None;
        }
        // A zeroed slot has a matching CRC of zero over zeroed bytes only by coincidence;
        // generation 0 is what actually distinguishes "never written" from "written", and
        // it is reserved for that.
        if u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")) == 0 {
            return None;
        }
        Some(CursorSlot {
            generation: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            acked_seq: u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")),
            acked_at: i64::from_le_bytes(bytes[16..24].try_into().expect("eight bytes")),
            state: CursorState::from_code(bytes[24])?,
        })
    }
}

/// Pick the live slot of a cursor file: the valid one with the higher generation.
///
/// Returns which slot index it was, so the next write goes to the other one.
pub fn live_slot(file: &[u8]) -> Option<(usize, CursorSlot)> {
    let slot = |i: usize| {
        let at = cursor_slot_offset(i);
        file.get(at..at + CURSOR_SLOT_BYTES)
            .and_then(CursorSlot::decode)
            .map(|s| (i, s))
    };
    let a = slot(0);
    let b = slot(1);
    match (a, b) {
        (Some(a), Some(b)) if b.1.generation > a.1.generation => Some(b),
        (Some(a), Some(_)) => Some(a),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> OriginId {
        OriginId::new([n; 16])
    }

    fn segment_with(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = SegmentHeader {
            first_seq: Seq::FIRST,
            created_at: 1_762_171_200_000,
        }
        .encode()
        .to_vec();
        for (i, p) in payloads.iter().enumerate() {
            encode_record(Seq::new(i as u64 + 1), p, &mut out);
        }
        out
    }

    #[test]
    fn a_segment_header_round_trips_and_is_thirty_two_bytes() {
        let h = SegmentHeader {
            first_seq: Seq::new(18_004),
            created_at: -1,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), 32);
        assert_eq!(SegmentHeader::decode(&bytes), Ok(h));
    }

    #[test]
    fn a_flipped_bit_in_a_segment_header_is_a_crc_failure_and_not_a_wrong_first_seq() {
        let mut bytes = SegmentHeader {
            first_seq: Seq::FIRST,
            created_at: 0,
        }
        .encode();
        bytes[12] ^= 0x01;
        assert_eq!(SegmentHeader::decode(&bytes), Err(HeaderProblem::BadCrc));
    }

    #[test]
    fn records_round_trip_in_order() {
        let seg = segment_with(&[b"one", b"two", b"three"]);
        let scan = scan_records(&seg).expect("a valid header");
        assert_eq!(scan.torn, None);
        assert_eq!(scan.valid_bytes, seg.len());
        assert_eq!(
            scan.records,
            vec![
                Record {
                    seq: Seq::new(1),
                    payload: b"one"
                },
                Record {
                    seq: Seq::new(2),
                    payload: b"two"
                },
                Record {
                    seq: Seq::new(3),
                    payload: b"three"
                },
            ]
        );
    }

    /// **The power-loss simulation of M3-2's test strategy, at the format level.** A
    /// segment truncated at *every* byte offset of its last record must yield the two
    /// records before it, and must never yield a third.
    #[test]
    fn truncating_at_every_byte_of_the_last_record_keeps_exactly_the_prefix() {
        let seg = segment_with(&[b"first", b"second", b"the one that was being written"]);
        let last_starts = seg.len() - record_bytes(30);
        for cut in last_starts..=seg.len() {
            let scan = scan_records(&seg[..cut]).expect("the header is intact");
            if cut == seg.len() {
                assert_eq!(scan.records.len(), 3, "the whole segment is valid");
                assert_eq!(scan.torn, None);
                continue;
            }
            assert_eq!(
                scan.records.len(),
                2,
                "cut at {cut}: the two complete records survive and the torn one does not"
            );
            assert_eq!(
                scan.valid_bytes, last_starts,
                "cut at {cut}: the valid bytes end where the torn record began"
            );
            // A cut exactly at the record boundary is not a tear: the file ends where a
            // record ends, which is what a segment looks like between appends. Every cut
            // *inside* the record is one.
            assert_eq!(
                scan.torn.is_some(),
                cut > last_starts,
                "cut at {cut}: a tear is reported iff the cut is inside the record"
            );
        }
    }

    /// The other half: not a truncation but a corruption, which is what a bad block looks
    /// like. The record is complete and its CRC does not match it.
    #[test]
    fn a_corrupt_payload_truncates_at_that_record_rather_than_being_read() {
        let mut seg = segment_with(&[b"first", b"second"]);
        let last = seg.len() - 6;
        seg[last] ^= 0xff;
        let scan = scan_records(&seg).expect("the header is intact");
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.torn, Some(Torn::BadCrc { seq: Seq::new(2) }));
    }

    /// A length field that claims more than the file holds — the other way a torn append
    /// presents, when the header landed and the payload did not.
    #[test]
    fn a_length_past_the_end_of_the_file_is_a_tear_and_not_a_panic() {
        let mut seg = segment_with(&[b"first"]);
        let at = SEGMENT_HEADER_BYTES;
        seg[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let scan = scan_records(&seg).expect("the header is intact");
        assert_eq!(scan.records.len(), 0);
        assert!(matches!(scan.torn, Some(Torn::LengthPastEnd { .. })));
    }

    #[test]
    fn an_empty_segment_scans_to_no_records_and_no_tear() {
        let seg = segment_with(&[]);
        let scan = scan_records(&seg).expect("the header is intact");
        assert!(scan.records.is_empty());
        assert_eq!(scan.torn, None);
        assert_eq!(scan.valid_bytes, SEGMENT_HEADER_BYTES);
    }

    #[test]
    fn a_manifest_round_trips_with_its_origin_table() {
        let m = Manifest {
            node_id: node(7),
            origins: vec![
                Origin {
                    origin_id: 0,
                    node_id: node(1),
                    first_seen: 10,
                    hw_seq: 4711,
                },
                Origin {
                    origin_id: 1,
                    node_id: node(2),
                    first_seen: 20,
                    hw_seq: 0,
                },
            ],
        };
        assert_eq!(Manifest::decode(&m.encode()), Ok(m));
    }

    #[test]
    fn an_empty_manifest_round_trips() {
        let m = Manifest::new(node(7), 1_762_171_200_000);
        assert_eq!(Manifest::decode(&m.encode()), Ok(m));
    }

    /// §3.6 step 1: a manifest CRC failure refuses the start, because a lost origin table
    /// reads every `hw_seq` as zero and makes every envelope a peer ever sent look new.
    #[test]
    fn a_corrupt_manifest_is_an_error_and_not_an_empty_origin_table() {
        let mut bytes = Manifest {
            node_id: node(7),
            origins: vec![Origin {
                origin_id: 0,
                node_id: node(1),
                first_seen: 10,
                hw_seq: 4711,
            }],
        }
        .encode();
        let at = bytes.len() - 8;
        bytes[at] ^= 0x01;
        assert_eq!(Manifest::decode(&bytes), Err(ManifestProblem::BadCrc));
    }

    /// `intern` cannot produce more than `MAX_ORIGINS` rows, so a manifest claiming more
    /// is corrupt or hand-edited — and a wrong origin table mis-dedups, which is why §3.6
    /// step 1 refuses rather than repairs.
    #[test]
    fn a_manifest_claiming_more_origins_than_the_bound_is_refused() {
        let mut bytes = Manifest::new(node(7), 0).encode();
        // Claim 2000 origins. The length check would catch it too; this asserts the bound
        // is checked first, so the error names the real problem.
        let count = u16::try_from(MAX_ORIGINS + 976).expect("fits a u16");
        bytes[26..28].copy_from_slice(&count.to_le_bytes());
        let body = bytes.len() - 4;
        let crc = crc32c(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Manifest::decode(&bytes),
            Err(ManifestProblem::TooManyOrigins {
                origins: MAX_ORIGINS + 976
            })
        );
    }

    #[test]
    fn a_cursor_slot_round_trips_and_is_sixty_four_bytes() {
        let s = CursorSlot {
            generation: 9,
            acked_seq: 4711,
            acked_at: 1_762_171_200_000,
            state: CursorState::Abandoned,
        };
        let bytes = s.encode();
        assert_eq!(bytes.len(), CURSOR_SLOT_BYTES);
        assert_eq!(CursorSlot::decode(&bytes), Some(s));
    }

    /// The torn-write-safe pattern, as the only thing it has to do: a crash leaves one
    /// slot unreadable and the other is a valid earlier position.
    #[test]
    fn a_torn_slot_loses_to_the_intact_one_whichever_half_it_is() {
        let older = CursorSlot {
            generation: 4,
            acked_seq: 100,
            acked_at: 1,
            state: CursorState::Active,
        };
        let newer = CursorSlot {
            generation: 5,
            acked_seq: 200,
            acked_at: 2,
            state: CursorState::Active,
        };

        let mut file = [0u8; CURSOR_FILE_BYTES];
        let (at0, at1) = (cursor_slot_offset(0), cursor_slot_offset(1));
        assert!(
            at1 >= 4096,
            "the two slots must not share a sector, or a single torn write can damage both"
        );
        file[at0..at0 + CURSOR_SLOT_BYTES].copy_from_slice(&older.encode());
        file[at1..at1 + CURSOR_SLOT_BYTES].copy_from_slice(&newer.encode());
        assert_eq!(live_slot(&file), Some((1, newer)), "the higher generation");

        // Tear the newer one: the older position is still valid, which costs a replay.
        let mut torn = file;
        torn[at1 + 9] ^= 0xff;
        assert_eq!(live_slot(&torn), Some((0, older)));

        // Tear the older one instead.
        let mut torn = file;
        torn[at0 + 9] ^= 0xff;
        assert_eq!(live_slot(&torn), Some((1, newer)));

        // Both torn: there is no position, so the cursor starts over from the manifest.
        let mut torn = file;
        torn[at0 + 9] ^= 0xff;
        torn[at1 + 9] ^= 0xff;
        assert_eq!(live_slot(&torn), None);
    }

    #[test]
    fn a_never_written_cursor_file_has_no_live_slot() {
        assert_eq!(live_slot(&[0u8; CURSOR_FILE_BYTES]), None);
    }
}
