//! [`RouteTable`] -- the impure half of what a route provider offers, and the type the one
//! listener mounts.
//!
//! The *pure* half is the [`RouteManifest`] on the provider's `Provision`, which is what
//! `plan()` needs to answer "do two components claim the same path" before any socket
//! exists (**D-51**). This is the other half: the `axum::Router` that answers them.
//!
//! **A table carries its own manifest, and [`RouteTable::new`] does not check them against
//! each other.** It cannot: a `Router`'s path set is not readable back out of it. What is
//! available -- and what every provider in this workspace has a unit test for -- is that
//! the manifest and the router are built from the same constants, so a typo exists in one
//! place (§6.2).

use axum::Router;
use mw_component::{ComponentId, RouteManifest, Scope};

/// One component's routes, ready to mount.
///
/// `scope` decides how it is mounted rather than being advisory:
///
/// | Scope | Mounted as |
/// |---|---|
/// | [`Scope::Any`] | merged into the shared router every host reaches |
/// | [`Scope::Site`] | the router for one website, selected by `Host` before anything else |
/// | [`Scope::Management`] | **refused** -- the management API has its own socket (A-W5-3), and nothing provides one yet |
#[derive(Debug)]
pub struct RouteTable {
    owner: ComponentId,
    scope: Scope,
    router: Router,
    manifest: RouteManifest,
    catch_all: bool,
}

impl RouteTable {
    /// A table for one owner, in the scope its manifest declares.
    ///
    /// The scope is taken from the manifest rather than passed again: two values that have
    /// to agree are one value that cannot disagree.
    pub fn new(owner: ComponentId, manifest: RouteManifest, router: Router) -> Self {
        Self {
            owner,
            scope: manifest.scope,
            router,
            manifest,
            catch_all: false,
        }
    }

    /// Mark this table's router as the listener's **catch-all**: what answers a path no
    /// route claims.
    ///
    /// Exactly one table may be the catch-all and the listener refuses a second
    /// ([`crate::MountError::TwoCatchAlls`]), because `axum` panics when two routers with
    /// fallbacks are merged and because "which 404 did I get" is not a question an
    /// operator should have to ask. It is `mw-web`'s asset tree: the Go router's last entry
    /// is `router.PathPrefix("/").Handler(http.FileServer(...))`, so a missing asset and an
    /// unrouted path are the same response -- `404 page not found\n` as
    /// `text/plain; charset=utf-8`, which `asset-not-found` and `html-portal-on-non-portal`
    /// both assert.
    ///
    /// With no catch-all table mounted the listener's own [`crate::not_found`] answers,
    /// which is the same bytes. The distinction is which component owns them.
    #[must_use]
    pub fn catch_all(mut self) -> Self {
        self.catch_all = true;
        self
    }

    /// Which component owns these routes.
    pub fn owner(&self) -> &ComponentId {
        &self.owner
    }

    /// Which router this table belongs to.
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// The paths this table claims, as declared.
    pub fn manifest(&self) -> &RouteManifest {
        &self.manifest
    }

    /// Whether this table is the catch-all.
    pub fn is_catch_all(&self) -> bool {
        self.catch_all
    }

    /// Take the router, consuming the table. The mount is the only caller.
    pub(crate) fn into_router(self) -> Router {
        self.router
    }

    /// [`RouteTable::into_router`], for a test in a route-providing crate that wants to
    /// drive its own handlers without mounting a whole listener.
    ///
    /// Named so that it reads as test-only at every call site, which is
    /// `mw_api_legacy::RouteState::install_for_test`'s reason. `crates/mw-web/tests/routes.rs`
    /// is the caller: `Mount` would give it the host dispatch and the catch-all as well,
    /// and what those tests are about is one website's five responses.
    #[doc(hidden)]
    pub fn into_router_for_test(table: Self) -> Router {
        table.into_router()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::{Methods, RouteEntry};
    use mw_types::WebsiteId;

    fn table(scope: Scope) -> RouteTable {
        RouteTable::new(
            ComponentId::new("x:y").expect("a legal id"),
            RouteManifest::new(scope, vec![RouteEntry::exact("/a", Methods::READ)]),
            Router::new(),
        )
    }

    #[test]
    fn the_scope_comes_from_the_manifest() {
        let t = table(Scope::Site(WebsiteId::new(3)));
        assert_eq!(t.scope(), Scope::Site(WebsiteId::new(3)));
        assert_eq!(t.manifest().entries.len(), 1);
        assert!(!t.is_catch_all());
        assert_eq!(t.owner().as_str(), "x:y");
    }

    #[test]
    fn a_catch_all_is_opt_in() {
        assert!(table(Scope::Any).catch_all().is_catch_all());
    }
}
