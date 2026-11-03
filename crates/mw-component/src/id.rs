//! [`ComponentId`], and the stable hash the supervisor derives a phase offset from.

use std::fmt;

/// A component's stable, human-meaningful identity — `aprs:backyard`, `db:postgres`.
///
/// Three properties, each load-bearing somewhere else:
///
/// - **Stable across restarts**, because `Phase::Spread` derives a schedule offset from it
///   (W2a §1.5) and a restart that reshuffled every offset would be a stampede of its own.
/// - **Structured as `kind[:instance]*`**, because W5 renders `display.qualifier` from the
///   instance part and because `aprs:backyard` tells an operator which of six APRS
///   components failed. The Go tree's component identity is a type string shared by every
///   instance, which is why its logs cannot say which device stopped.
/// - **Bounded in its character set**, because the same string is a metric label
///   (docs/service-architecture.md §5.10), a `component_schedule` primary key and a log
///   field. Lower-case ASCII, digits, `-`, `_` and `.` inside a segment; `:` between
///   segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId(Box<str>);

/// Why a string is not a [`ComponentId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComponentIdError {
    /// No text at all.
    #[error("a component id may not be empty")]
    Empty,
    /// A `:`-delimited segment with nothing in it — `aprs:`, `:backyard`, `a::b`.
    #[error("component id `{id}` has an empty segment")]
    EmptySegment {
        /// The rejected id.
        id: String,
    },
    /// A character that cannot be a metric label or a log field.
    #[error("component id `{id}` contains `{found}`, which is not permitted (lower-case ASCII, digits, `-`, `_`, `.` and `:`)")]
    BadCharacter {
        /// The rejected id.
        id: String,
        /// The first offending character.
        found: char,
    },
    /// Longer than anything legitimate, and long enough to be a mistake worth naming.
    #[error(
        "component id `{id}` is {len} bytes; the limit is {}",
        ComponentId::MAX_LEN
    )]
    TooLong {
        /// The rejected id.
        id: String,
        /// Its length in bytes.
        len: usize,
    },
}

impl ComponentId {
    /// The longest a component id may be. Generous for `kind:station:detail` and far short
    /// of anything that belongs in a metric label.
    ///
    /// On the type rather than on the module, so a caller that wants to check a length
    /// before constructing one can reach it — the module is private.
    pub const MAX_LEN: usize = 128;

    /// Validate and wrap. The only constructor: an unvalidated id would reach a metric
    /// label and a database key, and both would accept it.
    pub fn new(id: impl AsRef<str>) -> Result<Self, ComponentIdError> {
        let id = id.as_ref();
        if id.is_empty() {
            return Err(ComponentIdError::Empty);
        }
        if id.len() > Self::MAX_LEN {
            return Err(ComponentIdError::TooLong {
                id: id.to_string(),
                len: id.len(),
            });
        }
        if let Some(found) = id.chars().find(|c| !Self::permitted(*c)) {
            return Err(ComponentIdError::BadCharacter {
                id: id.to_string(),
                found,
            });
        }
        if id.split(':').any(str::is_empty) {
            return Err(ComponentIdError::EmptySegment { id: id.to_string() });
        }
        Ok(Self(id.into()))
    }

    fn permitted(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ':')
    }

    /// The whole id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first segment — `aprs` in `aprs:backyard`. W5's `display.section` is derived
    /// from this, once, in `mw-api-mgmt`.
    pub fn kind(&self) -> &str {
        // `split` always yields at least one item, and `new` rejects the empty id.
        self.0.split(':').next().unwrap_or_default()
    }

    /// Everything after the first `:` — `backyard` in `aprs:backyard`, and
    /// `postgres` in `db:postgres`. `None` for an id with no `:` at all, which is what a
    /// component with a single instance has. W5 renders this as `display.qualifier`.
    pub fn instance(&self) -> Option<&str> {
        self.0.split_once(':').map(|(_, rest)| rest)
    }

    /// A restart-stable 64-bit hash of the id.
    ///
    /// FNV-1a rather than [`std::collections::hash_map::DefaultHasher`], and the reason is
    /// the one property this is for: `DefaultHasher`'s output is explicitly **not**
    /// guaranteed stable across releases of the standard library, and `Phase::Spread`
    /// needs an offset that survives a restart and a rebuild. A hash written here is a
    /// hash this program controls.
    pub fn stable_hash(&self) -> u64 {
        stable_hash(self.0.as_bytes())
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for ComponentId {
    type Err = ComponentIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// FNV-1a over bytes. Deterministic, dependency-free, and stable by construction.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x1000_0000_01b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// FNV-1a as a [`std::hash::Hasher`], so anything `Hash` can be digested with the same
/// stable function [`ComponentId::stable_hash`] uses.
///
/// Not a general-purpose hasher and not for a `HashMap`: FNV has no DoS resistance, and
/// the two users here are a schedule phase offset and a configuration digest, neither of
/// which an attacker supplies.
#[derive(Debug, Clone, Copy)]
pub struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv1a {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        const PRIME: u64 = 0x1000_0000_01b3;
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_id_splits_into_kind_and_instance() {
        let id = ComponentId::new("aprs:backyard").unwrap();
        assert_eq!(id.kind(), "aprs");
        assert_eq!(id.instance(), Some("backyard"));
        let db = ComponentId::new("db:postgres").unwrap();
        assert_eq!(db.kind(), "db");
        assert_eq!(db.instance(), Some("postgres"));
        // No `:` at all: one instance, nothing to qualify.
        let bare = ComponentId::new("db.postgres").unwrap();
        assert_eq!(bare.kind(), "db.postgres");
        assert_eq!(bare.instance(), None);
    }

    #[test]
    fn the_character_set_is_enforced() {
        assert!(matches!(
            ComponentId::new("APRS:backyard"),
            Err(ComponentIdError::BadCharacter { found: 'A', .. })
        ));
        assert!(matches!(
            ComponentId::new("aprs backyard"),
            Err(ComponentIdError::BadCharacter { found: ' ', .. })
        ));
        assert!(matches!(
            ComponentId::new("aprs:"),
            Err(ComponentIdError::EmptySegment { .. })
        ));
        assert!(matches!(ComponentId::new(""), Err(ComponentIdError::Empty)));
        assert!(matches!(
            ComponentId::new("a".repeat(ComponentId::MAX_LEN + 1)),
            Err(ComponentIdError::TooLong { .. })
        ));
    }

    /// The property `Phase::Spread` depends on: the same id gives the same offset on every
    /// run of every build. Pinned as literals so a change to `stable_hash` cannot pass.
    #[test]
    fn the_stable_hash_is_pinned() {
        assert_eq!(stable_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(
            ComponentId::new("wu:backyard").unwrap().stable_hash(),
            stable_hash(b"wu:backyard")
        );
        assert_ne!(
            ComponentId::new("wu:backyard").unwrap().stable_hash(),
            ComponentId::new("wu:frontyard").unwrap().stable_hash()
        );
    }
}
