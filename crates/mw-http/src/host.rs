//! Host routing: the three normalisation steps of §6.3, the host table, and the
//! single-label wildcard match **DEV-6** also uses for certificates.
//!
//! This module is the one answer to "which website is this request for". `mw-api-legacy`
//! resolves the same question for the frozen surface (its routes are `Scope::Any` and the
//! website is selected inside the handler), and it asks this function rather than carrying
//! a second copy -- §6.3 puts host normalisation here, and two copies of a normaliser is
//! two answers to "does `EXAMPLE.COM.` match `example.com`".

use std::collections::HashMap;

use mw_types::WebsiteId;

/// `Host`, normalised the three ways §6.3 specifies: port stripped, one trailing dot
/// stripped, lower-cased.
///
/// The lower-casing is **DEV-2**, and it closes a defect rather than preserving one: the Go
/// lookup is a case-sensitive map, so a request for `EXAMPLE.COM` falls through to the
/// default website and silently serves someone else's station under the wrong hostname. No
/// fixture covers mixed case.
///
/// **One trailing dot, not every trailing dot.** `example.com.` is the fully-qualified
/// spelling of `example.com` and is the one §6.3 names; `example.com..` is not a hostname
/// at all, and normalising it into one would be inventing a match the DNS root does not
/// have.
///
/// An IPv6 literal keeps its brackets and loses its port -- `[::1]:8080` is `[::1]`, which
/// is what a stored hostname would have to be to match it. Splitting on the **last** colon
/// is what makes that work; splitting on the first would truncate the address.
pub fn normalise(host: &str) -> String {
    let mut h = host.trim();
    if let Some(close) = h.rfind(']') {
        h = &h[..=close];
    } else if let Some(colon) = h.rfind(':') {
        // Only when what precedes the colon is a name rather than a bare IPv6 literal --
        // `::1` ends in a digit too, and splitting it would leave `:`. An unbracketed
        // literal is not legal in a `Host` header, so this only has to not mangle it.
        let (head, port) = (&h[..colon], &h[colon + 1..]);
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !head.contains(':') {
            h = head;
        }
    }
    h.strip_suffix('.').unwrap_or(h).to_ascii_lowercase()
}

/// Whether `name` is matched by a wildcard whose suffix (the part after `*.`) is `suffix`,
/// matching **exactly one label** (**DEV-6**).
///
/// `a.example.com` yes, `a.b.example.com` no, `evilexample.com` no. The Go implementation
/// is `strings.HasSuffix(hostname, domain)` after stripping `*.`, which matches any depth
/// *and* matches `evilexample.com` for `*.example.com` -- a certificate handed out for a
/// name it does not cover, and a website served under a hostname its operator does not own.
///
/// Both callers pass an already-lower-cased pair: [`normalise`] for a request, and the
/// configuration reader for the stored value.
pub fn matches_one_label(name: &str, suffix: &str) -> bool {
    let Some(label) = name.strip_suffix(suffix) else {
        return false;
    };
    // `label` is everything before the suffix and must be exactly `<one-label>.`: a
    // non-empty label with no dot inside it.
    let Some(label) = label.strip_suffix('.') else {
        // Either an exact match (`example.com` against `example.com`, which a wildcard does
        // not cover) or a suffix that does not start at a label boundary (`evilexample.com`).
        return false;
    };
    !label.is_empty() && !label.contains('.')
}

/// Which website a `Host` resolves to.
///
/// Published as `ArcSwap<HostTable>` by the listener, so a reader does one atomic load and
/// holds an `Arc` for the duration of the request -- it sees one generation or the next and
/// never a mixture. **This is the direct replacement for the measured race**: the Go
/// controller holds the website map, the device map, the device-name map, the
/// devices-by-website map and the TLS configuration as plain fields with no mutex and
/// replaces them wholesale from the management API while in-flight requests read them
/// (`controller.go:708`), and a concurrent map read and write in Go is an unrecoverable
/// fatal error (**D-28**).
#[derive(Debug, Default)]
pub struct HostTable {
    /// Exact matches, lower-cased and port-free.
    exact: HashMap<Box<str>, WebsiteId>,
    /// `*.example.com` as `("example.com", id)`, matched one label deep.
    wildcard: Vec<(Box<str>, WebsiteId)>,
    /// What an unmatched `Host` resolves to (Q-18), or `None` on a node with no website.
    default: Option<WebsiteId>,
    /// Which rebuild this is, for a log line and for a test that a swap happened.
    generation: u64,
}

impl HostTable {
    /// Build a table from `(hostname, id)` pairs in **configuration order**, which is id
    /// order.
    ///
    /// The default is `controller.go:186` reproduced precisely: the first website the
    /// configuration returns, **unless some website has an empty hostname, in which case
    /// that one wins**. Q-18 is preserved exactly -- nothing in the response distinguishes
    /// "you asked for the right host" from "you asked for an unknown host and got someone
    /// else's station".
    ///
    /// A stored hostname is normalised on the way in rather than trusted. The schema's
    /// three `CHECK`s make a value that can never match unrepresentable going forward, but
    /// this table is also built in tests and from rows an older schema wrote.
    pub fn new(sites: impl IntoIterator<Item = (String, WebsiteId)>) -> Self {
        let mut exact: HashMap<Box<str>, WebsiteId> = HashMap::new();
        let mut wildcard: Vec<(Box<str>, WebsiteId)> = Vec::new();
        let mut first = None;
        let mut empty = None;
        for (hostname, id) in sites {
            let hostname = normalise(&hostname);
            if first.is_none() {
                first = Some(id);
            }
            if hostname.is_empty() {
                // The default-website marker, not a hostname. The first such row wins, for
                // the same reason the first row does.
                if empty.is_none() {
                    empty = Some(id);
                }
                continue;
            }
            match hostname.strip_prefix("*.") {
                // `entry` rather than `insert`: the first row wins, so a duplicate hostname
                // cannot make routing depend on iteration order. The schema's UNIQUE makes
                // it unreachable from a migrated database.
                Some(suffix) => wildcard.push((suffix.into(), id)),
                None => {
                    exact.entry(hostname.into()).or_insert(id);
                }
            }
        }
        Self {
            exact,
            wildcard,
            default: empty.or(first),
            generation: 0,
        }
    }

    /// Record which rebuild this is.
    #[must_use]
    pub fn generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Which rebuild this is.
    pub fn generation_of(&self) -> u64 {
        self.generation
    }

    /// The website a raw `Host` header resolves to, or `None` on a node with no website at
    /// all.
    ///
    /// Exact first, then wildcards in configuration order, then the default.
    pub fn resolve(&self, host: &str) -> Option<WebsiteId> {
        self.resolve_normalised(&normalise(host))
    }

    /// [`HostTable::resolve`] for a caller that has already normalised.
    pub fn resolve_normalised(&self, host: &str) -> Option<WebsiteId> {
        if let Some(id) = self.exact.get(host) {
            return Some(*id);
        }
        if let Some((_, id)) = self
            .wildcard
            .iter()
            .find(|(suffix, _)| matches_one_label(host, suffix))
        {
            return Some(*id);
        }
        self.default
    }

    /// Whether the table resolves this host **exactly** -- without the Q-18 fallback.
    ///
    /// For a log line and for a test: "the default website answered" and "the configured
    /// website answered" are the same bytes by design, so the distinction has to be
    /// available somewhere other than the response.
    pub fn resolves_exactly(&self, host: &str) -> bool {
        let host = normalise(host);
        self.exact.contains_key(host.as_str())
            || self
                .wildcard
                .iter()
                .any(|(suffix, _)| matches_one_label(&host, suffix))
    }

    /// How many hostnames are mapped, wildcards included.
    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    /// Whether no hostname is mapped.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What an unmatched `Host` resolves to.
    pub fn default_website(&self) -> Option<WebsiteId> {
        self.default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: i64) -> WebsiteId {
        WebsiteId::new(n)
    }

    #[test]
    fn the_three_normalisation_steps() {
        assert_eq!(normalise("localhost:18080"), "localhost");
        assert_eq!(normalise("example.com."), "example.com");
        assert_eq!(normalise("EXAMPLE.COM"), "example.com");
        assert_eq!(normalise(" Example.COM.:443 "), "example.com");
    }

    /// §6.3 says "a single trailing dot". Two is not a hostname, and normalising it into
    /// one would invent a match.
    #[test]
    fn only_one_trailing_dot_is_stripped() {
        assert_eq!(normalise("example.com.."), "example.com.");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_and_loses_its_port() {
        assert_eq!(normalise("[::1]:8080"), "[::1]");
        assert_eq!(normalise("[::1]"), "[::1]");
        // A bare literal is not legal in a `Host` header; it must not be truncated anyway.
        assert_eq!(normalise("::1"), "::1");
    }

    /// **DEV-6**, in all three of the shapes the Go check gets wrong.
    #[test]
    fn a_wildcard_matches_exactly_one_label() {
        assert!(matches_one_label("a.example.com", "example.com"));
        assert!(
            !matches_one_label("a.b.example.com", "example.com"),
            "`strings.HasSuffix` matches any depth"
        );
        assert!(
            !matches_one_label("evilexample.com", "example.com"),
            "the Go check hands out a certificate for a name it does not cover"
        );
        assert!(
            !matches_one_label("example.com", "example.com"),
            "a wildcard does not cover the bare domain"
        );
        assert!(!matches_one_label(".example.com", "example.com"));
    }

    #[test]
    fn exact_beats_wildcard_beats_default() {
        let t = HostTable::new([
            ("first.test".to_string(), id(1)),
            ("*.wild.test".to_string(), id(2)),
            ("a.wild.test".to_string(), id(3)),
        ]);
        assert_eq!(t.resolve("a.wild.test"), Some(id(3)));
        assert_eq!(t.resolve("b.wild.test"), Some(id(2)));
        assert_eq!(t.resolve("nobody.test"), Some(id(1)), "Q-18: the first row");
        assert_eq!(t.len(), 3);
    }

    /// `controller.go:186`, reproduced: an empty hostname is the default marker and beats
    /// the first row.
    #[test]
    fn an_empty_hostname_wins_the_default() {
        let t = HostTable::new([
            ("first.test".to_string(), id(1)),
            (String::new(), id(2)),
            ("third.test".to_string(), id(3)),
        ]);
        assert_eq!(t.resolve("nobody.test"), Some(id(2)));
        assert_eq!(t.resolve("third.test"), Some(id(3)));
        assert_eq!(t.len(), 2, "the empty hostname is not a mapped name");
    }

    #[test]
    fn a_node_with_no_website_resolves_to_nothing() {
        let t = HostTable::new(Vec::<(String, WebsiteId)>::new());
        assert_eq!(t.resolve("anything"), None);
        assert!(t.is_empty());
        assert_eq!(t.default_website(), None);
    }

    #[test]
    fn the_default_answering_is_distinguishable_from_a_match() {
        let t = HostTable::new([("first.test".to_string(), id(1))]);
        assert!(t.resolves_exactly("FIRST.TEST:443"));
        assert!(!t.resolves_exactly("nobody.test"));
        assert_eq!(t.resolve("nobody.test"), Some(id(1)));
    }

    #[test]
    fn a_duplicate_hostname_does_not_make_routing_order_dependent() {
        let t = HostTable::new([
            ("dup.test".to_string(), id(1)),
            ("dup.test".to_string(), id(2)),
        ]);
        assert_eq!(t.resolve("dup.test"), Some(id(1)));
    }

    #[test]
    fn the_generation_travels_with_the_table() {
        let t = HostTable::new([("a.test".to_string(), id(1))]).generation(7);
        assert_eq!(t.generation_of(), 7);
    }
}
