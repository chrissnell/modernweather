//! The identity newtypes docs/service-architecture.md §1.2 lists for this crate.
//!
//! Four of the five here exist because something in the component model has to name a
//! station, a website or a node without being able to name the table it came from. They
//! are newtypes rather than aliases for the ordinary reason — `fn get(id: i64, website:
//! i64)` has two arguments that cannot be told apart at a call site, and the Go tree's
//! `stationName string` threaded through forty signatures is what that grows into.
//!
//! The split between [`StationId`] and [`StationKey`] is the configuration schema's, not
//! an invention here: `stations.id` is the surrogate integer the FROZEN contract returns
//! (`/stationinfo` emits `stations[].id` as a number) and `stations.station_key` is the
//! text domain identity that identity resolution produces (W2a §4a.6). Both are needed,
//! they are not interchangeable, and conflating them is how a text name reached a field
//! three shipped clients parse as an integer.

use std::fmt;

/// The surrogate integer identity of a station — `config.stations.id`.
///
/// `Copy`, because `SnapshotKey` is (docs/service-architecture.md §2a.2) and the hot path
/// keys a map with it per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StationId(i64);

impl StationId {
    /// Wrap a surrogate id read from configuration.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    /// The underlying integer, for the three frozen fields that emit it as a number.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for StationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The text domain identity of a station — `config.stations.station_key`.
///
/// Produced by identity resolution and by nothing else: there is no envelope without one
/// (W2a §4a.6), which is the structural reason an unauthenticated reading cannot be
/// stored.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StationKey(Box<str>);

impl StationKey {
    /// Wrap a resolved station key.
    pub fn new(key: impl Into<Box<str>>) -> Self {
        Self(key.into())
    }

    /// The key as text, for a log field, a metric label or a query parameter.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity of one configured public website — `config.website.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WebsiteId(i64);

impl WebsiteId {
    /// Wrap a website id read from configuration.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    /// The underlying integer.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for WebsiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The identity of a node in a multi-hop topology — `config.node.node_id`, 16 bytes.
///
/// Opaque and fixed-width because it is compared, carried in a provenance path and used
/// for loop prevention (W7 §2); it is never parsed and never rendered to an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OriginId([u8; 16]);

impl OriginId {
    /// Wrap the 16 bytes stored in `config.node.node_id`.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes, for the wire and for the spool record.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for OriginId {
    /// Lower-case hex, no separators. An operator never sees this; a log line might.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A per-`(origin, station)` monotonic sequence number.
///
/// Gaps are visible and reuse is not, which is what makes a replayed spool prefix
/// recognisable rather than merely idempotent (W7 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    /// The first sequence number a station's stream carries.
    pub const FIRST: Self = Self(1);

    /// Wrap a sequence number read from a record or a wire message.
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The underlying counter.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next sequence number. Saturating, so an exhausted counter repeats rather than
    /// wrapping to a value the cursor has already acked — at 1 reading/second that is
    /// 584 billion years away, and the saturating form is still the correct one to write.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_id_renders_as_hex() {
        let id = OriginId::new([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ]);
        assert_eq!(id.to_string(), "0123456789abcdef0011223344556677");
    }

    #[test]
    fn seq_saturates_rather_than_wrapping() {
        assert_eq!(Seq::new(u64::MAX).next().get(), u64::MAX);
        assert_eq!(Seq::FIRST.next().get(), 2);
    }
}
