//! `http:listener` -- the component §6.1 describes, and the end of the boot sequence's
//! step 6a.
//!
//! # What changes by this being a component rather than a `join!` arm in `main`
//!
//! M2-4 bound one socket directly from `crates/modernweather/src/boot.rs` because
//! `/metrics` had to be served before any component existed, and recorded two consequences
//! of that: `main` owned a fourth root resource, and **the listener's teardown ran
//! concurrently with the supervisor's reverse-topological component stops rather than being
//! ranked by them**, so §3.6's "a consumer must never observe a revoked capability" did not
//! cover it. The second one is what this closes: the listener is now stopped in rank order
//! like everything else, so the socket closes *after* the components whose tables it serves
//! have stopped.
//!
//! **That holds up to the drain deadline and not past it**, which M4-3's code review asked
//! to have bounded rather than implied. `axum::serve` spawns each connection on its own
//! task; when [`crate::DRAIN`] expires, the select drops the serve futures -- closing the
//! listeners, which is what the `warn!` says -- and does not abort those tasks. So a
//! connection that was still in flight at the deadline can run its request against providers
//! the supervisor has since stopped. What makes that defensible rather than a hole is
//! **D-48**: a frozen handler with its capability gone answers its contract response, which
//! `crates/mw-api-legacy/tests/degraded.rs` asserts byte for byte. Tracking the connection
//! tasks instead would mean not using `axum::serve`, and the thing it would buy is already
//! guaranteed one layer up.
//!
//! # One behaviour change, stated rather than discovered: a held port
//!
//! The old shape made a port that another process holds a **startup failure** -- `bind`
//! returned an error, `main` propagated it and the process exited non-zero. A component's
//! build failure is a lifecycle transition instead (W2a Decision 3): `Failed` → `Backoff`,
//! retried with exponential backoff capped at 60 s and **no permanent give-up** (W2a
//! Decision 30), with the address in `last_error` and the failure in `STATUS=`. So the
//! daemon now stays up and keeps trying rather than exiting.
//!
//! That is the behaviour §6.1, §3.4 and Decision 30 jointly specify, and it is better than
//! what it replaces rather than a regression: `Restart=always` plus `StartLimitBurst` turned
//! the old exit into a unit that eventually gave up and stayed down, while this one
//! self-heals the moment the port frees and is visible as state the whole time. What it
//! costs is that `READY=1` can now be sent by a daemon whose listener is in `Backoff` --
//! readiness is "the supervisor has converged" by §3.5 and deliberately not "every
//! component is `Running`".
//!
//! # Why the `Mount` is built by the binary
//!
//! For `RouteState`'s reason, one level up: the route tables come from `mw-api-legacy` and
//! `mw-web`, and this crate may name neither (`ci/layers.toml`). So the binary -- the one
//! place a consumer and a provider may meet (§1.3 rule 1) -- builds the [`Mount`] as a root
//! resource and hands it over, exactly as it does `SnapshotState`, `RollupState` and the
//! frozen surface's state slot. What travels on the capability bus is the **graph edge**:
//! `Capability::HttpRoutes` is additive and this component subscribes to it, so "a provider
//! is published" and "its component is `Running`" stay one fact.
//!
//! The subscription is `Soft`, which is **W2a Decision 19** and is load-bearing: the
//! listener comes up with the routes it has and never waits for all providers, because a
//! management UI that is unreachable exactly when something is broken is the worst possible
//! failure.

use std::sync::Arc;

use mw_component::{
    Additive, Builder, Capability, ComponentError, ComponentId, ComponentIdError, ComponentSpec,
    ConfigDigest, Cx, Need, RouteProvider, Serve, Trigger,
};
use tracing::info;

use crate::mount::Mount;
use crate::sni::SniResolver;
use crate::socket::{self, Bound, Listen};

/// The component's id. A constant because the spec, the log lines and a plan test name it.
pub const ID: &str = "http:listener";

/// What this crate needs from the configuration to plan `http:listener`.
///
/// Three things, and two of them are root resources the binary built: where to bind, the
/// mounted router, and the certificate resolver when a TLS port is configured.
pub trait ListenerConfig {
    /// The sockets the configuration names (§6.1).
    fn listen(&self) -> Listen;

    /// The mounted router set, built by `main` as a root resource.
    fn mount(&self) -> Option<&Arc<Mount>>;

    /// The SNI resolver, when `listen().https_port` is set.
    ///
    /// `None` with no TLS port is the ordinary case. `None` *with* one is refused at build
    /// time by [`crate::HttpError::NoCertificate`] rather than here, because "a TLS port
    /// with no certificate" is a configuration an operator can fix and a `Backoff` is how
    /// they see it.
    fn sni_resolver(&self) -> Option<&Arc<SniResolver>>;
}

/// Why `http:listener` cannot be planned.
#[derive(Debug, thiserror::Error)]
pub enum ListenerPlanError {
    /// The component id is not a legal one. Unreachable -- [`ID`] is a literal -- and
    /// carried rather than `expect`ed so that the registrar has one error type.
    #[error("`{ID}` is not a usable component id")]
    Id(#[from] ComponentIdError),

    /// `main` did not build the router. A wiring mistake in the boot sequence, not a
    /// configuration: the mount has to exist before `plan()` runs, because the tables come
    /// from components `plan()` is about to validate.
    #[error(
        "the boot sequence did not build the mounted router, so `{ID}` has nothing to \
         serve; `crates/modernweather/src/boot.rs` builds it before `plan()`"
    )]
    NoMount,
}

/// Plan the one listener.
///
/// **Unconditional on the configuration.** There is no node that should not be listening:
/// `/metrics` is on the one listener (§5.10) and the schema's `listener` row is seeded with
/// defaults, so every node has an address. A node with no website and no station still
/// serves `/metrics` and still answers `404 page not found` -- which is what
/// `mw_pipeline::snapshot`'s note about a node with no frozen route table says it does.
pub fn register<C: ListenerConfig>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), ListenerPlanError> {
    let id = ComponentId::new(ID)?;
    let listen = cfg.listen();
    let mount = Arc::clone(cfg.mount().ok_or(ListenerPlanError::NoMount)?);
    let resolver = cfg.sni_resolver().map(Arc::clone);
    // Every configuration value the builder closes over, folded in. The router and the
    // resolver are root resources whose *contents* follow the configuration the components
    // that built them digested, so what is this component's own is the socket set.
    let digest = ConfigDigest::of(&(
        listen.addr.to_string(),
        listen.http_port,
        listen.https_port,
        listen.grpc_port,
    ));

    out.push(
        ComponentSpec::new(
            id,
            Trigger::Service,
            Builder::serve(move |cx| {
                let listen = listen.clone();
                let mount = Arc::clone(&mount);
                let resolver = resolver.clone();
                async move {
                    // Additive and soft: the set of providers is observed, never waited
                    // for (W2a Decision 19).
                    let routes = cx.additive::<Arc<dyn RouteProvider>>()?;
                    let bound =
                        socket::bind_with_hosts(&listen, mount.router(), resolver, mount.hosts())
                            .await
                            .map_err(|source| mw_component::BuildError::Resource {
                                what: format!("the listener on {}", listen.main()),
                                source: Box::new(source),
                            })?;
                    info!(
                        main = %bound.local_addr(),
                        tls = listen.tls(),
                        grpc = %bound.grpc_addr(),
                        redirect = ?bound.redirect_addr(),
                        sites = mount.sites(),
                        providers = routes.load().len(),
                        "the one listener is serving"
                    );
                    Ok(Listener { bound, routes })
                }
            }),
        )
        // **Soft, and additive.** `plan()` does not require a provider for a soft need, so
        // a node whose only route table is the listener's own `/metrics` plans and serves.
        .requires([Need::soft(Capability::HttpRoutes)])
        .config_digest(digest),
    );
    Ok(())
}

/// The component's body: serve until cancelled, then drain.
struct Listener {
    bound: Bound,
    routes: Additive<Arc<dyn RouteProvider>>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

impl Serve for Listener {
    fn serve(
        self: Box<Self>,
        cx: Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), ComponentError>> {
        let Listener { bound, routes } = *self;
        Box::pin(async move {
            // **The subscription is held for the life of the component and deliberately not
            // awaited.** Its purpose is the capability *graph*: a provider appears and
            // disappears with its component, which is what makes "the table is mounted" and
            // "the component is `Running`" one fact. Rebuilding the router when the set
            // changes is a plan diff (§3.7) and has no writer until M7-1 -- so holding it
            // and dropping it with the body, rather than selecting on `changed()` and
            // rebuilding nothing, is the honest shape.
            let _routes = routes;
            let outcome = bound.serve(async move { cx.cancelled().await }).await;
            match outcome {
                Ok(()) => {
                    info!("the one listener has stopped");
                    Ok(())
                }
                Err(e) => Err(ComponentError::tick("serve the one listener", e)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::host::HostTable;

    #[derive(Debug)]
    struct Cfg {
        mount: Option<Arc<Mount>>,
    }

    impl ListenerConfig for Cfg {
        fn listen(&self) -> Listen {
            Listen {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_port: 8080,
                https_port: None,
                grpc_port: 50051,
            }
        }

        fn mount(&self) -> Option<&Arc<Mount>> {
            self.mount.as_ref()
        }

        fn sni_resolver(&self) -> Option<&Arc<SniResolver>> {
            None
        }
    }

    fn cfg() -> Cfg {
        let (mount, _) = Mount::new(Vec::new(), HostTable::default()).expect("mounts");
        Cfg {
            mount: Some(Arc::new(mount)),
        }
    }

    #[test]
    fn the_listener_is_planned_unconditionally() {
        let mut out = Vec::new();
        register(&cfg(), &mut out).expect("the listener plans");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id.as_str(), ID);
        assert!(matches!(out[0].trigger, Trigger::Service));
        assert!(
            out[0].provides.is_empty(),
            "it consumes tables, it does not provide them"
        );
    }

    /// W2a Decision 19: the listener never waits for a route provider, so the need is soft
    /// and `plan()` cannot refuse a node whose only table is `/metrics`.
    #[test]
    fn the_route_subscription_is_soft() {
        let mut out = Vec::new();
        register(&cfg(), &mut out).expect("plans");
        assert_eq!(out[0].hard_needs().count(), 0);
        assert_eq!(
            out[0].requires,
            vec![Need::soft(Capability::HttpRoutes)],
            "a hard HttpRoutes need would make the listener wait for every provider"
        );
    }

    #[test]
    fn a_missing_mount_is_a_named_wiring_error() {
        let mut out = Vec::new();
        let err = register(&Cfg { mount: None }, &mut out)
            .expect_err("the boot sequence must build the router first");
        assert!(matches!(err, ListenerPlanError::NoMount), "{err}");
    }

    /// The digest is what restarts the listener when a port moves. A registrar that read
    /// the row and did not fold it in is a listener that keeps the old socket.
    #[test]
    fn the_digest_follows_every_port() {
        #[derive(Debug)]
        struct Ports(Listen, Arc<Mount>);
        impl ListenerConfig for Ports {
            fn listen(&self) -> Listen {
                self.0.clone()
            }
            fn mount(&self) -> Option<&Arc<Mount>> {
                Some(&self.1)
            }
            fn sni_resolver(&self) -> Option<&Arc<SniResolver>> {
                None
            }
        }
        let (mount, _) = Mount::new(Vec::new(), HostTable::default()).expect("mounts");
        let mount = Arc::new(mount);
        let base = Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 8080,
            https_port: None,
            grpc_port: 50051,
        };
        let digest = |listen: Listen| {
            let mut out = Vec::new();
            register(&Ports(listen, Arc::clone(&mount)), &mut out).expect("plans");
            out[0].config_digest
        };
        let d = digest(base.clone());
        assert_eq!(d, digest(base.clone()), "the same row digests the same");
        for changed in [
            Listen {
                http_port: 8081,
                ..base.clone()
            },
            Listen {
                https_port: Some(8443),
                ..base.clone()
            },
            Listen {
                grpc_port: 50052,
                ..base.clone()
            },
            Listen {
                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                ..base.clone()
            },
        ] {
            assert_ne!(
                d,
                digest(changed),
                "a port change must restart the listener"
            );
        }
    }
}
