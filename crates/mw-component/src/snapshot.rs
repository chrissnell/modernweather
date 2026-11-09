//! [`LatestSnapshot`] — the pre-serialised current conditions the `/latest` path serves.
//!
//! W3 §2a.2's shape, with the structural half of **D-35** in the types: `api:legacy` is
//! handed `Arc<dyn LatestSnapshot>` and never a [`crate::Store`], so the `/latest` handler
//! *cannot* query. Not "does not" — there is no handle in it to reach for, and
//! `ci/layers.toml` additionally forbids that crate `sqlx` and `mw-store`, so the two
//! mechanisms agree.
//!
//! # Keyed per `(website, station)` (**D-31**)
//!
//! The payload genuinely depends on the website, so a station-only key would serve the
//! wrong bytes: `getSnowBaseDistance(website)` feeds `snowdepth`, the primary-device
//! selection is per website, and `/latest?station=` may name any configured device while
//! `/snow` always reports the website's snow device. W3 §2a.4 sizes the worst case at the
//! cross product — 6 websites x 5 devices in W1's capture rig, ~5 KB each, **150 KB** —
//! and therefore declines an eviction policy, a TTL and an LRU. There is none here.
//!
//! # What is pre-serialised, and when (**D-33**)
//!
//! **JSON eagerly, once per reading-version. Every other encoding lazily, on first
//! request.** The argument is not CPU: at the measured volume there are ~10x more readings
//! than requests, so pre-serialising is *more* total work, and both choices are far under a
//! second of CPU a day (W3 §2a.3 corrects the prompt's arithmetic). What it buys is tail
//! latency — serialisation leaves the request path entirely — and an **identity for the
//! bytes**, which is what [`ETag`] needs and what a per-request payload does not have.
//!
//! The lazy half is [`Snapshot::alternate`], one [`OnceLock`] per [`Encoding`]. It has no
//! consumer in the tree today and the mechanism is still here, because it is the half
//! D-33 is actually about: `format=msgpack` is **not implemented** (`docs/reconciliation.md`
//! §6.3 narrowed the frozen surface to the five endpoints iOS calls), and protobuf's
//! consumer is `mw-grpc`. A renderer declares which encodings it can produce
//! ([`SnapshotRender::encodings`]) and the rest answer
//! [`RenderError::EncodingUnsupported`], so an encoding nobody implements is a named
//! refusal rather than a silently empty body.
//!
//! # Why the bytes come from a renderer this crate does not contain
//!
//! `/latest`'s body is the frozen 133-key DTO, and `mw-api-legacy` **exclusively** owns it
//! — the duplicate key pairs, the `mm`→inch split, `omitempty`, and the workspace's only
//! zero-fill, `mw_api_legacy::zf` (W3 D-9, D-10, enforced by `ci/layers.toml`'s
//! `one-zero-fill` rule — which is a substring scan that does not know what a comment is, so
//! this paragraph names that function rather than quoting its body).
//! `snapshot:latest` lives in `mw-pipeline`, which is a *lower* layer and may not name that
//! crate. So the renderer is a trait here and its implementation arrives with the item that
//! owns the DTO, which is §1.3 rule 1 — the same arrangement [`crate::ReadingStore`] has
//! with `mw-store`, with the binary as the one place the two meet.
//!
//! # Why [`Snapshot`] keeps its [`SnapshotSource`]
//!
//! Because the lazy encodings need something to render *from*, and re-deriving it would
//! mean keeping the reading somewhere else anyway. W3 §2a.4's per-key budget already counts
//! it: "the enriched `Reading` ~1 KB". Keeping it is also what makes the degradation story
//! in **D-36** expressible — the window-derived values that were folded in are still
//! readable, so "the last good value" is a fact about this object rather than a claim about
//! a previous one.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use mw_types::{Class, Classify, Reading, StationKey, WebsiteId};
use smallvec::SmallVec;
use tokio::sync::watch;

use crate::capability::Capability;
use crate::control::Generation;
use crate::cx::CapabilityHandle;
use crate::ingress::StationMeta;
use crate::window::Resolved;

/// Pre-serialised bytes, cloned by refcount.
///
/// `Arc<[u8]>` rather than `bytes::Bytes`, which is what W3 §2a.2 writes. The property the
/// design asks for is the one both have — "a `Bytes` clone is a refcount bump", so the
/// handler's last step is not a copy — and `bytes` is not in **D-32**'s dependency table,
/// so adopting it would be an amendment for a type alias. `mw-api-legacy` may wrap one in a
/// `Bytes` for `axum` at O(1) (`Bytes::from_owner`) when it needs to.
pub type Payload = Arc<[u8]>;

/// Which key a snapshot belongs to: the website it is rendered for, and the station it is
/// about (**D-31**).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotKey {
    /// `config.website.id`.
    pub website: WebsiteId,
    /// The station the reading came from.
    pub station: StationKey,
}

impl SnapshotKey {
    /// One key.
    pub fn new(website: WebsiteId, station: StationKey) -> Self {
        Self { website, station }
    }
}

impl fmt::Display for SnapshotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.website, self.station)
    }
}

/// An encoding of one snapshot's payload.
///
/// A closed enum rather than a content-type string: the set is a property of this program,
/// and the renderer's support for each one is then a total match rather than a lookup that
/// can miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Encoding {
    /// The frozen `/latest` body. **Eager** — rendered once per reading-version (**D-33**).
    Json,
    /// `format=msgpack`. Lazy, and currently unimplemented by any renderer: the frozen
    /// surface was narrowed to the five endpoints iOS calls, and msgpack is not one of them
    /// (`docs/reconciliation.md` §6.3).
    MsgPack,
    /// The protobuf `Reading` the gRPC services share. Lazy; `mw-grpc` is its consumer.
    Proto,
}

impl Encoding {
    /// Every encoding, eager first.
    pub const ALL: &'static [Encoding] = &[Encoding::Json, Encoding::MsgPack, Encoding::Proto];

    /// The lazily-produced ones, in [`Snapshot`]'s cell order.
    pub const LAZY: &'static [Encoding] = &[Encoding::MsgPack, Encoding::Proto];

    /// [`Encoding::ALL`] is [`Encoding::LAZY`] plus the one eager encoding — checked at
    /// compile time, because [`Snapshot`]'s cell array is sized from `LAZY` and
    /// [`Snapshot::alternate`] indexes it by position in `LAZY` after rejecting the eager
    /// one. A new variant added to `ALL` and forgotten in `LAZY` is a panic at the first
    /// request for it; this makes it a build failure instead.
    const _ALL_IS_LAZY_PLUS_EAGER: () = assert!(Self::ALL.len() == Self::LAZY.len() + 1);

    /// Whether this encoding is produced when the snapshot is built rather than on demand.
    pub const fn is_eager(self) -> bool {
        matches!(self, Encoding::Json)
    }

    /// The internal name, for a log field and a metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Encoding::Json => "json",
            Encoding::MsgPack => "msgpack",
            Encoding::Proto => "proto",
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `hash(serialiser_version, config_generation)` — the part of an [`ETag`] that is not the
/// reading (W3 §2a.5).
///
/// Folding the configuration generation in is what makes a website rename or a changed snow
/// base distance invalidate a cached response even though the reading has not moved. The
/// renderer's own revision is folded in for the same reason one level down: a changed
/// serialiser produces different bytes for the same reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadRev(u64);

impl PayloadRev {
    /// Fold a renderer revision and a configuration generation into one value.
    ///
    /// FNV-1a, the same hash [`crate::stable_hash`] uses, because what is needed is a
    /// stable identity across processes and not a cryptographic one: an attacker who can
    /// choose a configuration generation can already change the payload.
    pub fn of(serialiser: u64, generation: Generation) -> Self {
        use std::hash::Hasher as _;
        let mut h = crate::id::Fnv1a::default();
        h.write(&serialiser.to_le_bytes());
        h.write(&generation.get().to_le_bytes());
        Self(h.finish())
    }

    /// Fold in the instant the window-derived values were resolved at.
    ///
    /// **The gap this closes.** W3 §2a.5 defines `payload_rev` as
    /// `hash(serialiser_version, config_generation)` and calls the tag a **strong**
    /// validator on the grounds that "the bytes for a given value are byte-identical by
    /// construction". They are not, because the payload is also a function of the
    /// window-derived values — wind gust, rain rate, the calendar and multi-day rain
    /// totals, the storm total — and those are re-resolved on every fold, independently of
    /// `observed_at`.
    ///
    /// So two snapshots can share a station and an `observed_at` and differ in their bytes:
    /// a station re-reporting an instant it already sent (which is why the writer's
    /// `ON CONFLICT (station_id, time) DO UPDATE` exists), `snapshot:warm`'s seed followed
    /// by the same reading arriving through `OnReading`, or a `Degraded`→`Fresh` transition
    /// on an unchanged last reading. Without this, a client that sent `If-None-Match` would
    /// get a `304` and keep the older rain totals until the next *distinct* reading.
    ///
    /// Latent today — no shipped client sends `If-None-Match`, because `Cache-Control` is
    /// frozen at `no-store` — and fixed here rather than later because M4-2 is what starts
    /// emitting the header. **The tag's four-part shape is unchanged**; what gains an input
    /// is `payload_rev`, which is the field §2a.5 already describes as a hash of everything
    /// that is not the reading. Recorded as **A-M4-1-3**.
    pub fn folding(self, windows_as_of: DateTime<Utc>) -> Self {
        use std::hash::Hasher as _;
        let mut h = crate::id::Fnv1a::default();
        h.write(&self.0.to_le_bytes());
        h.write(&windows_as_of.timestamp_millis().to_le_bytes());
        Self(h.finish())
    }

    /// The value, for the `ETag` text and for a test.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PayloadRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A **strong** entity tag over one snapshot's bytes (W3 §2a.5).
///
/// `"l1-<station>-<observed_at_unix_ms>-<payload_rev>"`, quoted, strong rather than weak:
/// the bytes for a given `(reading, windows, configuration, serialiser)` are byte-identical
/// by construction, which is the definition of a strong validator. **`windows` is in that
/// tuple and W3 §2a.5's own version of this sentence omits it** — see
/// [`PayloadRev::folding`], which is where it gets folded in and why.
///
/// **The website is deliberately not in the tag**, even though the snapshot is keyed by it.
/// An entity tag is scoped to the URI it was served from, and two websites are two
/// authorities, so the same tag on `a.example.com/latest` and `b.example.com/latest` names
/// two different resources and cannot be confused by a conforming cache. What *would* be a
/// collision is two payloads for one URI, and `payload_rev` covers that.
///
/// **It cannot fail conformance, and that is a checked property rather than a hope.**
/// `contract/conformance` iterates each fixture's `assert.headers` as an allowlist and
/// never enumerates a response's headers, so an additive `ETag` is invisible to the suite
/// by construction — the same reason `Date` has never been a problem. What must not happen
/// is "fixing" `Cache-Control`, which is frozen at `no-cache, no-store, must-revalidate`:
/// a client honouring `no-store` never sends `If-None-Match`, so the 304 simply never fires
/// for the iOS app as it ships, and that is fine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ETag(Box<str>);

impl ETag {
    /// The tag for one station's reading at one payload revision.
    ///
    /// The station key is escaped, because nothing validates its character set: a
    /// `StationKey` wraps arbitrary text and `config.stations.station_key` is
    /// `TEXT NOT NULL UNIQUE` with no `CHECK` on its bytes. RFC 9110's `etagc` is
    /// `%x21 / %x23-7E / obs-text` — every printable byte except `"` — so a key containing
    /// a quote would produce a syntactically invalid entity tag, and one containing CR or
    /// LF would make `HeaderValue::from_str` fail in the handler. On `/latest` that is a
    /// 5xx on a frozen route, which is the family of outcome **D-48** exists to make
    /// unobservable. Escaping here makes it unrepresentable instead of handled.
    pub fn new(station: &StationKey, observed_at: DateTime<Utc>, rev: PayloadRev) -> Self {
        Self(
            format!(
                "\"l1-{station}-{ms}-{rev}\"",
                station = etagc(station.as_str()),
                ms = observed_at.timestamp_millis(),
            )
            .into(),
        )
    }

    /// The header value, quotes included.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether an `If-None-Match` value matches this tag.
    ///
    /// RFC 9110 §13.1.2 as written, in three parts, because the obvious implementation gets
    /// two of them wrong:
    ///
    /// 1. **Weak comparison is mandatory** — *"a recipient MUST use the weak comparison
    ///    function when comparing entity tags for `If-None-Match`"* — so a `W/` prefix on
    ///    either side is stripped before comparing. §8.8.1 permits a transforming proxy to
    ///    weaken a tag it forwards, and exact string equality would make that tag never
    ///    match, costing a `304` that was available.
    /// 2. **`*` matches whenever a snapshot exists**, which is what W3 §2a.5 reproduces.
    /// 3. **The list is split on commas *outside* the quoted strings.** An entity tag is a
    ///    quoted string and may contain a comma, so `split(',')` is not a tokenizer. The
    ///    tags this type produces cannot contain one — [`ETag::new`] escapes the station
    ///    key — but the value being parsed came from a client, so it is not this type's
    ///    tags that have to be comma-free.
    ///
    /// Every deviation fails *safe* in the same direction: a missed `304`, never a wrongly
    /// issued one. That is why this is worth being exact about rather than approximate —
    /// the failure a loose parser produces is invisible.
    pub fn matches(&self, if_none_match: &str) -> bool {
        let want = strip_weak(self.as_str());
        candidates(if_none_match).any(|c| c == "*" || strip_weak(c) == want)
    }
}

impl fmt::Display for ETag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `text`, with every byte RFC 9110's `etagc` does not permit percent-escaped.
///
/// `etagc = "!" / %x23-7E / obs-text`, i.e. printable ASCII except `"`, plus the high
/// bytes. `%` itself is escaped so the mapping is injective — otherwise `a%41` and `aA`
/// would produce the same tag, which is the one thing a validator may not do.
fn etagc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'%' => out.push_str("%25"),
            b'"' => out.push_str("%22"),
            0x21 | 0x23..=0x7e => out.push(char::from(b)),
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// An entity tag with its `W/` weakness prefix removed, for RFC 9110's weak comparison.
fn strip_weak(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// The members of an `If-None-Match` value, splitting on commas **outside** quotes.
fn candidates(header: &str) -> impl Iterator<Item = &str> {
    let mut in_quotes = false;
    // `split` with a stateful closure: the quote flag flips on every `"`, so a comma seen
    // while it is set is part of a tag rather than a separator.
    header
        .split(move |c: char| {
            if c == '"' {
                in_quotes = !in_quotes;
            }
            c == ',' && !in_quotes
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Whether a snapshot's window-derived values are current (**D-36**).
///
/// **This never reaches the frozen body.** `/latest` has no field for it and inventing one
/// is a contract change, so the indicator goes out through additive response headers
/// (`X-MW-Snapshot-Age`, `X-MW-Degraded`), through `mw_snapshot_age_seconds` and
/// `mw_component_state`, and through the management UI — never through the bytes a shipped
/// client parses. The public website shows nothing (**D-38**).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Every value folded in is current.
    Fresh,
    /// One or more window-derived field groups are being served from their last good value.
    ///
    /// The reading itself, wind chill, heat index, AQI and the snow depth are *unaffected*
    /// by a database outage, because `snapshot:latest` folds arrivals in memory. What
    /// degrades is what a component maintains: wind gust and the rain rate from
    /// `window:rollup`, the rainfall windows from the same, the snow totals from `snow:*`.
    Degraded {
        /// Which capabilities are not answering. Four inline, which is every capability a
        /// snapshot can depend on.
        missing: SmallVec<[Capability; 4]>,
        /// When the degradation began — the oldest `as_of` behind the stale values, not
        /// when it was noticed.
        since: DateTime<Utc>,
    },
}

impl Staleness {
    /// Whether this is [`Staleness::Fresh`].
    pub fn is_fresh(&self) -> bool {
        matches!(self, Staleness::Fresh)
    }

    /// The capabilities that are not answering, empty when fresh.
    ///
    /// For `X-MW-Degraded`'s comma-separated value and for the management UI's sentence.
    pub fn missing(&self) -> &[Capability] {
        match self {
            Staleness::Fresh => &[],
            Staleness::Degraded { missing, .. } => missing,
        }
    }
}

/// The `config.website` columns a payload depends on (**D-31**).
///
/// D-31's argument for keying per website is that three things in the payload depend on the
/// website, and these are those three plus the identity: the **primary-device selection**
/// (which station `/latest` answers for when the request names none), the **snow device**
/// (`/snow`'s `stationname` is always the website's, even when `?station=` named something
/// else), and the website's own name.
///
/// The snow **base distance** is not here, and that is a schema change rather than an
/// omission: W3 §2a.4 writes `getSnowBaseDistance(website)`, and W4 put the column on
/// `config.stations` instead, so it reaches a renderer through
/// [`SnapshotSource::meta`]. The per-website dependency it was an example of survives —
/// two websites bound to two different snow devices read two different base distances.
#[derive(Debug, Clone, PartialEq)]
pub struct WebsiteView {
    /// `config.website.id`.
    pub id: WebsiteId,
    /// `config.website.name`.
    pub name: String,
    /// `config.website.primary_station_key`, resolved. `None` is a website with no primary
    /// device, which is legal: the column is nullable and `ON DELETE SET NULL`.
    pub primary_station: Option<StationKey>,
    /// `config.website.snow_enabled`.
    pub snow_enabled: bool,
    /// `config.website.snow_station_key`. The schema's `CHECK` makes this `Some` whenever
    /// [`WebsiteView::snow_enabled`] is set, so the pair cannot disagree.
    pub snow_station: Option<StationKey>,
}

/// Everything a renderer needs to produce one key's payload.
///
/// Owned rather than borrowed, because [`Snapshot`] keeps it for the lazy encodings. The
/// fields are the four inputs W3 §2a.2's handler sketch implies and nothing else: a
/// renderer that needed a fifth would be a renderer that queries.
#[derive(Debug, Clone)]
pub struct SnapshotSource {
    /// Which website and station this is for.
    pub key: SnapshotKey,
    /// The website's configured bindings — the part of the payload that is per website
    /// rather than per station, and therefore the reason for **D-31**'s key.
    pub website: WebsiteView,
    /// The newest enriched reading folded in. **R1**'s derived values are already columns
    /// on it (`wind_chill`, `heat_index`, the AQIs, `snow_depth`), because the enrich stage
    /// computed them at ingest.
    pub reading: Reading,
    /// The station's static configuration — coordinates for the sun and moon, the snow base
    /// distance for `snowdepth` (**D-31**'s per-website payload reads it per website).
    ///
    /// Read from `config.stations` rather than off the reading (W4 assumption 24): a
    /// station's latitude is configuration, and taking it from a frame would make it
    /// whatever the device last claimed.
    pub meta: StationMeta,
    /// The window-derived values, as the provider last resolved them.
    ///
    /// Empty when `WindowedView` was unavailable at build — which is a node whose database
    /// has never been up, and is the cold-start case rather than the degraded one.
    pub windows: Resolved,
}

/// One key's pre-serialised current conditions.
///
/// Immutable: a new reading produces a new `Snapshot` and an [`arc_swap`]-style pointer
/// write replaces it, so a reader never waits on the writer (**D-28**). The lazy encodings
/// are the one piece of interior mutability, and they are write-once.
///
/// [`arc_swap`]: https://docs.rs/arc-swap
pub struct Snapshot {
    key: SnapshotKey,
    etag: ETag,
    observed_at: DateTime<Utc>,
    rendered_at: DateTime<Utc>,
    staleness: Staleness,
    json: Payload,
    /// One cell per [`Encoding::LAZY`] member, in that order.
    ///
    /// Sized from the list rather than written as `2`, so that adding a fourth
    /// [`Encoding`] cannot turn [`Snapshot::alternate`]'s carefully-worded `expect` into a
    /// plain out-of-bounds panic one line later.
    lazy: [OnceLock<Payload>; Encoding::LAZY.len()],
    render: Arc<dyn SnapshotRender>,
    source: SnapshotSource,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("key", &self.key)
            .field("etag", &self.etag)
            .field("observed_at", &self.observed_at)
            .field("staleness", &self.staleness)
            .field("json_bytes", &self.json.len())
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// Render one snapshot. **The JSON is produced here**, which is the eager half of
    /// **D-33**; the rest is produced on first request.
    ///
    /// `Err` only from the renderer: the caller is a component tick, and a renderer that
    /// cannot encode a reading is a `Fatal` disagreement between the DTO and the registry
    /// rather than something to retry per reading.
    pub fn render(
        source: SnapshotSource,
        staleness: Staleness,
        rendered_at: DateTime<Utc>,
        rev: PayloadRev,
        render: Arc<dyn SnapshotRender>,
    ) -> Result<Self, RenderError> {
        let json = render.render(Encoding::Json, &source)?;
        let observed_at = source.reading.time;
        // **Here rather than at the caller**, so that no component can produce a snapshot
        // whose tag does not cover the window values it rendered from. `PayloadRev::folding`
        // has why that matters.
        let rev = rev.folding(source.windows.as_of());
        Ok(Self {
            etag: ETag::new(&source.key.station, observed_at, rev),
            key: source.key.clone(),
            observed_at,
            rendered_at,
            staleness,
            json,
            lazy: [const { OnceLock::new() }; Encoding::LAZY.len()],
            render,
            source,
        })
    }

    /// Which website and station this is for.
    pub fn key(&self) -> &SnapshotKey {
        &self.key
    }

    /// The strong validator for these bytes.
    pub fn etag(&self) -> &ETag {
        &self.etag
    }

    /// The instant of the newest reading folded in — what W1's 10-minute recency rule is
    /// measured against.
    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    /// When this snapshot was rendered.
    pub fn rendered_at(&self) -> DateTime<Utc> {
        self.rendered_at
    }

    /// Whether the window-derived values are current (**D-36**).
    pub fn staleness(&self) -> &Staleness {
        &self.staleness
    }

    /// The eagerly-rendered `/latest` body. A clone is a refcount bump.
    pub fn json(&self) -> Payload {
        Arc::clone(&self.json)
    }

    /// The rendering inputs, for a surface that needs a value rather than the bytes —
    /// `/stationinfo`'s per-website table, the management UI's rendering of `Staleness`.
    pub fn source(&self) -> &SnapshotSource {
        &self.source
    }

    /// One encoding's bytes, rendering it on first request (**D-33**'s lazy half).
    ///
    /// [`Encoding::Json`] is already rendered and returns immediately. A losing racer
    /// renders a second time and discards its result, which is the cost of `OnceLock`
    /// without a lock on the read path — and the read path is the one that matters here.
    pub fn alternate(&self, encoding: Encoding) -> Result<Payload, RenderError> {
        if encoding.is_eager() {
            return Ok(self.json());
        }
        let cell = &self.lazy[Encoding::LAZY
            .iter()
            .position(|e| *e == encoding)
            .expect("every non-eager Encoding is in Encoding::LAZY")];
        if let Some(bytes) = cell.get() {
            return Ok(Arc::clone(bytes));
        }
        let bytes = self.render.render(encoding, &self.source)?;
        Ok(Arc::clone(cell.get_or_init(|| bytes)))
    }

    /// How old the snapshot's reading is at `now`, for `X-MW-Snapshot-Age` and
    /// `mw_snapshot_age_seconds`.
    ///
    /// Negative when a station's clock is ahead of this one, which is reported rather than
    /// clamped: a station reporting from the future is a configuration problem an operator
    /// should see, and a clamp to zero is how it stays invisible.
    pub fn age_seconds(&self, now: DateTime<Utc>) -> f64 {
        (now - self.observed_at).num_milliseconds() as f64 / 1000.0
    }
}

/// Produces one key's bytes from its [`SnapshotSource`].
///
/// Implemented by `mw-api-legacy`, which exclusively owns the frozen DTO; see the module
/// documentation for why it is not implemented here.
pub trait SnapshotRender: Send + Sync + 'static {
    /// A number that changes when the bytes this renderer produces for a given source
    /// change. Folded into [`PayloadRev`], so a serialiser change invalidates an `ETag`.
    fn revision(&self) -> u64;

    /// Which encodings this renderer can produce. [`Encoding::Json`] must be among them.
    fn encodings(&self) -> &'static [Encoding];

    /// Render one encoding.
    fn render(&self, encoding: Encoding, source: &SnapshotSource) -> Result<Payload, RenderError>;
}

/// Why a payload could not be rendered.
///
/// Both variants are `Fatal` and for the same reason the rest of this crate's error enums
/// are classified at the type: each is a disagreement between the renderer and the registry
/// or between a caller and the renderer's declared set, so the next attempt on the next
/// reading answers the same thing.
///
/// **Known gap — the classification changes nothing on its own**, and it is worth saying
/// rather than discovering. `From<RenderError>` into a `ComponentError` goes through
/// `ComponentError::tick`, whose own `Classify` is unconditionally `Retryable` and
/// deliberately does not consult the boxed source ([`crate::error`]'s comment explains
/// why), and `mw_supervisor::task::interpret` branches only on `Ignorable`. So a component
/// that returned one of these would be retried regardless. `snapshot:latest` therefore does
/// **not** return it: `mw_pipeline::snapshot` counts a render failure per key and keeps the
/// previous bytes, because taking the component down would revoke `LatestSnapshot` and
/// unmount a frozen route table. Recorded the way [`StoreError::Rejected`] records its own
/// identical divergence, and for the same reason: the alternative is a comment that reads
/// as a behavioural promise.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// An encoding the renderer does not produce.
    #[error(
        "this build's snapshot renderer does not produce `{encoding}`. It declares \
         [{declared}]; `format=msgpack` in particular is deliberately not implemented, \
         because docs/reconciliation.md §6.3 narrowed the frozen surface to the five \
         endpoints the iOS client calls and msgpack is not one of them."
    )]
    EncodingUnsupported {
        /// What was asked for.
        encoding: Encoding,
        /// What the renderer declares, comma separated.
        declared: String,
    },

    /// The renderer refused the source — a metric whose value the DTO cannot express, a
    /// serialiser failure.
    #[error("the current conditions for `{key}` could not be serialised: {reason}")]
    NotSerialisable {
        /// Which key.
        key: SnapshotKey,
        /// What the renderer said. A string rather than a `#[source]`, because the renderer
        /// lives in a crate this one may not name and a boxed error would make this enum
        /// un-`PartialEq` for one caller's benefit.
        reason: String,
    },
}

impl Classify for RenderError {
    fn class(&self) -> Class {
        Class::Fatal
    }
}

/// Every key's current snapshot, as one immutable map.
///
/// `BTreeMap` rather than `HashMap`, which is what W3 §2a.2 writes. The map is at most a
/// few dozen entries so the lookup cost is indistinguishable, and ordered iteration is what
/// `/stationinfo` and the portal want from [`LatestSnapshot::table`]: a rendered table whose
/// row order changes between requests is one nobody can diff.
#[derive(Debug, Clone, Default)]
pub struct SnapshotTable(BTreeMap<SnapshotKey, Arc<Snapshot>>);

impl SnapshotTable {
    /// An empty table — a freshly started process, and what `snapshot:warm` finishes with
    /// when `ReadingStore` is unavailable (**D-37**).
    pub fn new() -> Self {
        Self::default()
    }

    /// One key's snapshot.
    pub fn get(&self, key: &SnapshotKey) -> Option<&Arc<Snapshot>> {
        self.0.get(key)
    }

    /// Insert or replace one key's snapshot, returning what was there.
    pub fn insert(&mut self, snapshot: Arc<Snapshot>) -> Option<Arc<Snapshot>> {
        self.0.insert(snapshot.key().clone(), snapshot)
    }

    /// Every key and its snapshot, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&SnapshotKey, &Arc<Snapshot>)> {
        self.0.iter()
    }

    /// Every snapshot for one station, across the websites it is bound to.
    pub fn for_station<'a>(
        &'a self,
        station: &'a StationKey,
    ) -> impl Iterator<Item = &'a Arc<Snapshot>> {
        self.0
            .iter()
            .filter(move |(key, _)| key.station == *station)
            .map(|(_, snapshot)| snapshot)
    }

    /// How many keys are populated.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing has been folded in yet.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Pre-serialised per-station current conditions (W3 §2a.2).
///
/// The capability `api:legacy` and `web:site:*` require **Hard** and the reason the
/// `/latest` handler performs no queries: this is the only reading-shaped handle they are
/// given, and it answers from memory.
///
/// # Why every method is synchronous
///
/// Each one is a pointer read. [`LatestSnapshot::get`] is an `ArcSwap` load, a map lookup
/// and a refcount bump; `/latest`'s whole job after that is a comparison and a write
/// (**D-34**'s four things). A `BoxFuture` here would be a signature saying the handler
/// might wait, which is the thing this capability exists to make impossible — the same
/// reasoning [`crate::WindowedView`] and [`crate::StationRegistry`] are synchronous for.
pub trait LatestSnapshot: Send + Sync + 'static {
    /// One key's snapshot, or `None` for a key nothing has reported for.
    ///
    /// `None` is what `/latest` turns into its documented
    /// `404 no recent weather data available for this station\n` — the same bytes a reading
    /// outside the 10-minute window produces, which is the answer W2a assumption 26 asked
    /// W1 to pin and which `contract/fixtures/latest-no-recent-data` already covers.
    fn get(&self, key: &SnapshotKey) -> Option<Arc<Snapshot>>;

    /// Changes to one key, for SSE and for the live stream.
    ///
    /// A `watch` rather than a broadcast: a subscriber that falls behind wants the newest
    /// value and not the ones it missed, which is `ReadingPolicy::Latest` one layer up.
    /// Subscribing to a configured key nothing has reported for yet is legal and yields
    /// `None` until something does.
    ///
    /// **`None` for a key that is not in the configuration**, which W3 §2a.2 writes as a
    /// bare receiver and therefore leaves no answer for. The alternatives are both worse
    /// than a named absence: a receiver over a channel whose sender has been dropped hands
    /// the subscriber a closed stream that looks like a shutdown, and one over a sender
    /// kept alive for an unconfigured key is a leak per request. The key set is a pure
    /// function of configuration (**D-31**'s cross product), so this is the same answer
    /// [`LatestSnapshot::get`] gives and the handler has already resolved the station
    /// against the configuration before it gets here.
    fn subscribe(&self, key: &SnapshotKey) -> Option<watch::Receiver<Option<Arc<Snapshot>>>>;

    /// Every key at once, for the surfaces that need the set rather than one entry —
    /// `/stationinfo`, the portal, the management UI's station list.
    fn table(&self) -> Arc<SnapshotTable>;
}

impl CapabilityHandle for Arc<dyn LatestSnapshot> {
    const CAPABILITY: Capability = Capability::LatestSnapshot;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{MetricKey, Window};

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn key() -> SnapshotKey {
        SnapshotKey::new(WebsiteId::new(1), StationKey::new("backyard"))
    }

    fn website() -> WebsiteView {
        WebsiteView {
            id: WebsiteId::new(1),
            name: "suncrest".to_string(),
            primary_station: Some(StationKey::new("backyard")),
            snow_enabled: false,
            snow_station: None,
        }
    }

    /// A renderer that records what it was asked for, so the eager/lazy split is
    /// observable rather than asserted about.
    #[derive(Debug)]
    struct Counting {
        calls: std::sync::Mutex<Vec<Encoding>>,
        encodings: &'static [Encoding],
    }

    impl Counting {
        fn new(encodings: &'static [Encoding]) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                encodings,
            })
        }

        fn calls(&self) -> Vec<Encoding> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SnapshotRender for Counting {
        fn revision(&self) -> u64 {
            7
        }

        fn encodings(&self) -> &'static [Encoding] {
            self.encodings
        }

        fn render(
            &self,
            encoding: Encoding,
            source: &SnapshotSource,
        ) -> Result<Payload, RenderError> {
            if !self.encodings.contains(&encoding) {
                return Err(RenderError::EncodingUnsupported {
                    encoding,
                    declared: self
                        .encodings
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
            self.calls.lock().unwrap().push(encoding);
            Ok(format!("{encoding}:{}", source.key).into_bytes().into())
        }
    }

    fn source() -> SnapshotSource {
        let mut reading = Reading {
            time: at("2026-11-07T18:30:00Z"),
            station_id: "backyard".to_string(),
            ..Reading::default()
        };
        reading.out_temp = Some(41.5);
        SnapshotSource {
            key: key(),
            website: website(),
            reading,
            meta: StationMeta::EMPTY,
            windows: Resolved::new(at("2026-11-07T18:30:00Z"), []),
        }
    }

    fn snapshot(render: Arc<dyn SnapshotRender>) -> Snapshot {
        Snapshot::render(
            source(),
            Staleness::Fresh,
            at("2026-11-07T18:30:01Z"),
            PayloadRev::of(7, Generation::new(3)),
            render,
        )
        .expect("the renderer declares Json")
    }

    /// **D-33's split, as an observation.** The JSON is rendered when the snapshot is
    /// built; nothing else is until it is asked for, and then exactly once.
    #[test]
    fn json_is_rendered_eagerly_and_every_other_encoding_once_on_first_request() {
        let render = Counting::new(&[Encoding::Json, Encoding::Proto]);
        let snap = snapshot(Arc::clone(&render) as Arc<dyn SnapshotRender>);
        assert_eq!(render.calls(), vec![Encoding::Json]);

        // The eager one is not re-rendered when it is read back.
        assert_eq!(snap.json().as_ref(), b"json:1/backyard");
        assert_eq!(render.calls(), vec![Encoding::Json]);

        let first = snap.alternate(Encoding::Proto).expect("declared");
        let second = snap.alternate(Encoding::Proto).expect("declared");
        assert_eq!(first.as_ref(), b"proto:1/backyard");
        assert_eq!(
            render.calls(),
            vec![Encoding::Json, Encoding::Proto],
            "the OnceLock means a second request renders nothing"
        );
        assert!(Arc::ptr_eq(&first, &second), "the same bytes, not a copy");
    }

    /// An encoding no renderer in this build produces is a named refusal rather than empty
    /// bytes — which is what makes `format=msgpack`'s absence a decision a reader can find.
    #[test]
    fn an_unsupported_encoding_names_itself_and_what_is_declared() {
        let render = Counting::new(&[Encoding::Json]);
        let snap = snapshot(render as Arc<dyn SnapshotRender>);
        let err = snap
            .alternate(Encoding::MsgPack)
            .expect_err("the renderer declares json only");
        assert_eq!(err.class(), Class::Fatal);
        let text = err.to_string();
        assert!(text.contains("msgpack"), "{text}");
        assert!(text.contains("§6.3"), "{text}");
    }

    /// The `ETag` is strong, carries the three things §2a.5 names, and moves when any of
    /// them does.
    #[test]
    fn the_etag_changes_with_the_reading_and_with_the_configuration() {
        let station = StationKey::new("backyard");
        let rev = PayloadRev::of(7, Generation::new(3));
        let tag = ETag::new(&station, at("2026-11-07T18:30:00Z"), rev);
        assert!(tag.as_str().starts_with("\"l1-backyard-1794076200000-"));
        assert!(!tag.as_str().starts_with("W/"), "strong, not weak");

        let later = ETag::new(&station, at("2026-11-07T18:30:02Z"), rev);
        assert_ne!(tag, later, "a new reading is a new entity");

        let renamed = ETag::new(
            &station,
            at("2026-11-07T18:30:00Z"),
            PayloadRev::of(7, Generation::new(4)),
        );
        assert_ne!(
            tag, renamed,
            "a committed configuration change invalidates even when the reading has not \
             moved -- a website rename changes the bytes"
        );

        let reserialised = ETag::new(
            &station,
            at("2026-11-07T18:30:00Z"),
            PayloadRev::of(8, Generation::new(3)),
        );
        assert_ne!(tag, reserialised, "so does a changed serialiser");
    }

    /// `If-None-Match` matching: RFC 9110 §13.1.2's weak comparison, the `*` form, and a
    /// list split on commas outside the quotes.
    #[test]
    fn if_none_match_handles_a_list_the_wildcard_and_weak_comparison() {
        let tag = ETag::new(
            &StationKey::new("backyard"),
            at("2026-11-07T18:30:00Z"),
            PayloadRev::of(7, Generation::new(3)),
        );
        assert!(tag.matches(tag.as_str()));
        assert!(tag.matches("*"), "`*` matches whenever a snapshot exists");
        assert!(tag.matches(&format!("\"l1-other-1-0\", {}", tag.as_str())));
        assert!(!tag.matches("\"l1-other-1-0\""));
        assert!(!tag.matches(""), "no If-None-Match is a normal 200");

        // §13.1.2: weak comparison is MANDATORY for If-None-Match, so a proxy that
        // weakened the tag on its way out still gets its 304 back.
        assert!(
            tag.matches(&format!("W/{}", tag.as_str())),
            "exact string equality would cost a 304 that was available"
        );

        // A comma inside a quoted tag is not a separator. `split(',')` would break this
        // candidate into two halves and match neither.
        let awkward = "\"a,b\"";
        assert!(
            tag.matches(&format!("{awkward}, {}", tag.as_str())),
            "the list is tokenized, not split"
        );
        assert!(!tag.matches(awkward));
    }

    /// **A station key cannot produce a syntactically invalid entity tag.** RFC 9110's
    /// `etagc` forbids `"` and the control bytes, nothing validates
    /// `config.stations.station_key`, and an unescaped quote or CR in a header value is a
    /// 5xx on `/latest` — which is the one family of response **D-48** forbids on a frozen
    /// route.
    #[test]
    fn a_station_key_is_escaped_into_the_tag() {
        let rev = PayloadRev::of(7, Generation::new(3));
        let at_ = at("2026-11-07T18:30:00Z");
        for hostile in [
            "back\"yard",
            "back\r\nyard",
            "back yard",
            "back,yard",
            "back%yard",
        ] {
            let tag = ETag::new(&StationKey::new(hostile), at_, rev);
            let body = tag
                .as_str()
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .expect("quoted exactly once");
            assert!(
                body.bytes()
                    .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b)),
                "`{hostile}` produced a tag outside etagc: {tag}"
            );
            assert!(tag.matches(tag.as_str()), "and it still round-trips");
        }

        // The escape is injective, so two keys cannot collide into one tag.
        assert_ne!(
            ETag::new(&StationKey::new("a%41"), at_, rev),
            ETag::new(&StationKey::new("aA"), at_, rev),
        );
    }

    /// **The tag covers the window-derived values it renders from.** Two snapshots can
    /// share a station and an `observed_at` and still differ in their bytes, because
    /// `Folder::resolve` re-resolves the windows on every fold. Without this, a client that
    /// sent `If-None-Match` would get a 304 and keep the older rain totals.
    #[test]
    fn two_snapshots_at_one_instant_with_different_windows_have_different_tags() {
        let render = Counting::new(&[Encoding::Json]) as Arc<dyn SnapshotRender>;
        let rev = PayloadRev::of(7, Generation::new(3));
        let tags: Vec<ETag> = ["2026-11-07T18:30:00Z", "2026-11-07T18:31:00Z"]
            .into_iter()
            .map(|window_at| {
                let mut src = source();
                // The reading is identical; only the window resolution has moved.
                src.windows = Resolved::new(at(window_at), []);
                Snapshot::render(
                    src,
                    Staleness::Fresh,
                    at("2026-11-07T18:31:05Z"),
                    rev,
                    Arc::clone(&render),
                )
                .expect("rendered")
                .etag()
                .clone()
            })
            .collect();
        assert_eq!(
            tags[0].to_string().split('-').count(),
            4,
            "the four-part shape W3 §2a.5 writes is unchanged; payload_rev gained an input"
        );
        assert_ne!(
            tags[0], tags[1],
            "a refreshed window is a new entity even at an unchanged observed_at"
        );
    }

    /// `Staleness` is readable as a list, which is what `X-MW-Degraded` renders and the
    /// management UI turns into a sentence. The frozen body is not involved.
    #[test]
    fn degraded_names_the_capabilities_that_are_not_answering() {
        let fresh = Staleness::Fresh;
        assert!(fresh.is_fresh());
        assert!(fresh.missing().is_empty());

        let degraded = Staleness::Degraded {
            missing: SmallVec::from_slice(&[Capability::WindowedView]),
            since: at("2026-11-07T18:00:00Z"),
        };
        assert!(!degraded.is_fresh());
        assert_eq!(degraded.missing(), &[Capability::WindowedView]);
    }

    /// The table is ordered, answers per station across websites, and starts empty.
    #[test]
    fn the_table_is_keyed_per_website_and_station() {
        let render = Counting::new(&[Encoding::Json]) as Arc<dyn SnapshotRender>;
        let mut table = SnapshotTable::new();
        assert!(table.is_empty());

        for website in [2, 1] {
            let mut src = source();
            src.key = SnapshotKey::new(WebsiteId::new(website), StationKey::new("backyard"));
            src.website.id = WebsiteId::new(website);
            table.insert(Arc::new(
                Snapshot::render(
                    src,
                    Staleness::Fresh,
                    at("2026-11-07T18:30:01Z"),
                    PayloadRev::of(7, Generation::ZERO),
                    Arc::clone(&render),
                )
                .expect("rendered"),
            ));
        }
        assert_eq!(table.len(), 2, "one entry per website, per D-31");
        let order: Vec<i64> = table.iter().map(|(k, _)| k.website.get()).collect();
        assert_eq!(order, vec![1, 2], "ordered, so a rendered table diffs");
        assert_eq!(
            table.for_station(&StationKey::new("backyard")).count(),
            2,
            "one station, both of its websites"
        );
        assert!(table
            .get(&SnapshotKey::new(
                WebsiteId::new(9),
                StationKey::new("backyard")
            ))
            .is_none());
    }

    /// The age is what `X-MW-Snapshot-Age` carries, and a station reporting from the future
    /// is visible rather than clamped.
    #[test]
    fn the_age_is_signed() {
        let render = Counting::new(&[Encoding::Json]) as Arc<dyn SnapshotRender>;
        let snap = snapshot(render);
        assert_eq!(snap.age_seconds(at("2026-11-07T18:30:30Z")), 30.0);
        assert_eq!(snap.age_seconds(at("2026-11-07T18:29:58Z")), -2.0);
    }

    /// The source survives rendering, which is what the lazy encodings render from and what
    /// `/stationinfo` reads a value out of.
    #[test]
    fn the_rendering_inputs_are_kept() {
        let render = Counting::new(&[Encoding::Json]) as Arc<dyn SnapshotRender>;
        let snap = snapshot(render);
        assert_eq!(snap.source().reading.out_temp, Some(41.5));
        assert_eq!(snap.observed_at(), at("2026-11-07T18:30:00Z"));
        assert!(snap.staleness().is_fresh());
        assert_eq!(
            snap.source().windows.value((
                MetricKey::WindSpeed,
                Window::of(Window::TEN_MINUTES),
                mw_types::Agg::Max
            )),
            Err(crate::WindowError::NotDeclaredInRequest {
                metric: MetricKey::WindSpeed,
                window: Window::of(Window::TEN_MINUTES),
                agg: mw_types::Agg::Max,
            })
        );
    }
}
