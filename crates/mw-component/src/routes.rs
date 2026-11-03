//! The pure half of a route table: [`RouteManifest`], declared on a [`crate::Provision`]
//! so that a duplicate route is a plan error naming both owners (**D-51**).
//!
//! `mw-http` owns the impure half — the `MethodRouter`s, the mounting and the `Router`
//! rebuild (docs/service-architecture.md §6.2). What is here is only what `plan()` needs
//! to answer "do two components claim the same path", which it can answer without a socket
//! existing. Conflict detection is **not** available at compile time and the reason is
//! worth stating so nobody tries: the set of websites is configuration, not source.
//!
//! The method set is a small bitset rather than `http::Method`, because this crate sits
//! below the transport layer and a route *pattern* is not an HTTP concept the way a
//! request is. `mw-http` converts.

use std::fmt;

use mw_types::WebsiteId;

/// Which HTTP methods a declared route answers.
///
/// A bitset rather than a `Vec<Method>`: it is `Copy`, comparing two is one instruction,
/// and `Methods::ANY` is the frozen surface's actual behaviour — the Go handlers are
/// registered without a method filter, so `/latest` answers `PUT`, and W1 pinned that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Methods(u16);

impl Methods {
    /// `GET`.
    pub const GET: Self = Self(1 << 0);
    /// `HEAD`.
    pub const HEAD: Self = Self(1 << 1);
    /// `POST`.
    pub const POST: Self = Self(1 << 2);
    /// `PUT`.
    pub const PUT: Self = Self(1 << 3);
    /// `PATCH`.
    pub const PATCH: Self = Self(1 << 4);
    /// `DELETE`.
    pub const DELETE: Self = Self(1 << 5);
    /// `OPTIONS`.
    pub const OPTIONS: Self = Self(1 << 6);

    /// Every method. What the frozen legacy surface declares, per W1.
    pub const ANY: Self = Self(0b111_1111);

    /// `GET` and `HEAD` together, which is what a static or read-only route wants.
    pub const READ: Self = Self(Self::GET.0 | Self::HEAD.0);

    /// The union of two sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether the two sets share a method — the test a route conflict is made of.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether the set is empty, which a declared route may not be.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Methods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::ANY {
            return f.write_str("ANY");
        }
        let names = [
            (Self::GET, "GET"),
            (Self::HEAD, "HEAD"),
            (Self::POST, "POST"),
            (Self::PUT, "PUT"),
            (Self::PATCH, "PATCH"),
            (Self::DELETE, "DELETE"),
            (Self::OPTIONS, "OPTIONS"),
        ];
        let mut first = true;
        for (m, name) in names {
            if self.intersects(m) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

/// Which router a route table belongs to.
///
/// Two tables may declare the same pattern as long as they are in different scopes: a
/// per-website route and a management route are not in conflict, and three websites each
/// declaring `/` are not either (docs/service-architecture.md §6.3 routes by host first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// One configured website, selected by hostname.
    Site(WebsiteId),
    /// Every host: the frozen REST surface, ingest endpoints, `/metrics`.
    Any,
    /// The management API.
    Management,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Site(id) => write!(f, "site:{id}"),
            Scope::Any => f.write_str("any"),
            Scope::Management => f.write_str("management"),
        }
    }
}

/// How a pattern claims paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Match {
    /// One path, possibly with `{param}` segments — `"/span/{span}"`.
    Exact,
    /// Everything under a prefix. This is how a gRPC service is mounted (by its full
    /// method prefix) and how an asset tree is served.
    Prefix,
}

/// One declared route: a pattern, how it matches, and which methods it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    /// The pattern, in the listener's syntax.
    pub pattern: String,
    /// Exact or prefix.
    pub matching: Match,
    /// The methods it answers.
    pub methods: Methods,
}

impl RouteEntry {
    /// An exact route.
    pub fn exact(pattern: impl Into<String>, methods: Methods) -> Self {
        Self {
            pattern: pattern.into(),
            matching: Match::Exact,
            methods,
        }
    }

    /// A prefix mount.
    pub fn prefix(pattern: impl Into<String>, methods: Methods) -> Self {
        Self {
            pattern: pattern.into(),
            matching: Match::Prefix,
            methods,
        }
    }

    /// Whether two entries would both claim some request.
    ///
    /// Three cases, and the prefix ones are the interesting half: two exact patterns
    /// conflict when they are equal; a prefix conflicts with anything it contains,
    /// including another prefix. Parameter *names* are not compared, because
    /// `/span/{span}` and `/span/{window}` claim the same paths — a conflict a
    /// string comparison would miss and an operator would experience as one of their two
    /// handlers never running.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        if !self.methods.intersects(other.methods) {
            return false;
        }
        match (self.matching, other.matching) {
            (Match::Exact, Match::Exact) => shape(&self.pattern) == shape(&other.pattern),
            (Match::Prefix, _) => contains(&self.pattern, &other.pattern),
            (_, Match::Prefix) => contains(&other.pattern, &self.pattern),
        }
    }
}

/// A pattern with every `{param}` segment replaced by a positional marker, so that two
/// patterns claiming the same paths compare equal whatever their parameters are called.
fn shape(pattern: &str) -> Vec<&str> {
    pattern
        .split('/')
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                "{}"
            } else {
                seg
            }
        })
        .collect()
}

/// Whether `prefix` claims every path `other` claims.
fn contains(prefix: &str, other: &str) -> bool {
    let p = shape(prefix);
    let o = shape(other);
    o.len() >= p.len() && o[..p.len()] == p[..]
}

/// Every path one component claims, in one scope.
///
/// Carried on the `Provision` rather than discovered at mount time, which is the whole of
/// **D-51**: the alternative puts the failure in `http:listener`, *after* its siblings
/// have started, and makes the one component everything else's reachability depends on the
/// component that fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteManifest {
    /// Which router these belong to.
    pub scope: Scope,
    /// The declared routes.
    pub entries: Vec<RouteEntry>,
}

impl RouteManifest {
    /// A manifest for one scope.
    pub fn new(scope: Scope, entries: Vec<RouteEntry>) -> Self {
        Self { scope, entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_only_conflict_when_they_overlap() {
        let get = RouteEntry::exact("/latest", Methods::GET);
        let post = RouteEntry::exact("/latest", Methods::POST);
        let any = RouteEntry::exact("/latest", Methods::ANY);
        assert!(!get.conflicts_with(&post));
        assert!(get.conflicts_with(&any));
        assert!(get.conflicts_with(&get));
    }

    #[test]
    fn a_parameter_name_does_not_make_two_patterns_different() {
        let a = RouteEntry::exact("/span/{span}", Methods::GET);
        let b = RouteEntry::exact("/span/{window}", Methods::GET);
        assert!(a.conflicts_with(&b));
        let c = RouteEntry::exact("/span/24h", Methods::GET);
        assert!(!c.conflicts_with(&RouteEntry::exact("/span/240h", Methods::GET)));
    }

    #[test]
    fn a_prefix_claims_what_is_under_it() {
        let mount = RouteEntry::prefix("/weather.v1.WeatherV1", Methods::POST);
        let inner = RouteEntry::exact("/weather.v1.WeatherV1/GetLatest", Methods::POST);
        assert!(mount.conflicts_with(&inner));
        assert!(inner.conflicts_with(&mount));
        let elsewhere = RouteEntry::exact("/weather.v1.Other/GetLatest", Methods::POST);
        assert!(!mount.conflicts_with(&elsewhere));
        // Two prefixes, one inside the other.
        assert!(mount.conflicts_with(&RouteEntry::prefix(
            "/weather.v1.WeatherV1/Sub",
            Methods::POST
        )));
    }

    #[test]
    fn methods_render_for_a_log_line() {
        assert_eq!(Methods::ANY.to_string(), "ANY");
        assert_eq!(Methods::READ.to_string(), "GET|HEAD");
        assert_eq!(
            Methods::GET.union(Methods::DELETE).to_string(),
            "GET|DELETE"
        );
    }
}
