//! `plan()`'s structural errors, one test each.
//!
//! This file is the done-criterion of implementation-plan.md M2-3: *every structural error
//! in W2a Decision 2's list is a unit test* — unknown capability, duplicate exclusive
//! provider, unprovided requirement, cycle, bad deadline, sub-floor interval, impossible
//! identity, duplicate route. `plan()`'s purity is what makes that possible without a
//! runtime, which is the whole point of the decision: each of these is a test rather than
//! a runtime surprise, and **none of them can reach a running process**.
//!
//! It runs under `cargo test` with no runtime, no socket and no database — there is not an
//! `async fn` in the file.

#![cfg(not(loom))]

use std::time::Duration;

use mw_component::{
    Arity, Builder, Capability, ComponentId, ComponentSpec, Continuity, CronSpec, Generation,
    IdentityProblem, IdentityShape, IdentitySource, Initiation, LocalTime, Methods, Misfire, Need,
    PlanError, Provision, ReadingFilter, ReadingPolicy, RouteEntry, RouteManifest, Scope, Trigger,
    TzSource, Zone,
};
use mw_supervisor::plan::{
    diff, plan, validate, Plan, PlanContext, RegisterError, Registrar, RootSet,
};
use mw_supervisor::testing::{id, CountingTick};

// --- helpers ------------------------------------------------------------------------

fn tick_builder() -> Builder {
    Builder::tick(|_| async { Ok(CountingTick::default()) })
}

fn every(period_s: u64, deadline_s: u64) -> Trigger {
    Trigger::every(
        Duration::from_secs(period_s),
        Duration::from_secs(deadline_s),
    )
}

fn spec(name: &str, trigger: Trigger) -> ComponentSpec {
    let build = match trigger.body_kind() {
        mw_component::BodyKind::Tick => tick_builder(),
        mw_component::BodyKind::OnReading => Builder::on_reading(|_| async {
            Ok(mw_supervisor::testing::CountingOnReading::default())
        }),
        mw_component::BodyKind::Serve => {
            Builder::serve(|_| async { Ok(mw_supervisor::testing::CooperativeServe) })
        }
    };
    ComponentSpec::new(id(name), trigger, build)
}

fn ctx() -> PlanContext {
    PlanContext::new(Generation::new(7))
}

fn planned(specs: Vec<ComponentSpec>) -> Plan {
    validate(specs, &ctx()).expect("this plan is valid")
}

// --- the eight structural errors --------------------------------------------------

/// **Unknown capability.** A requirement nothing in this *build* can provide, as opposed to
/// one no component in this *configuration* provides. `Trigger::OnReading` implies a hard
/// requirement on the reading stream — the supervisor delivers, so the component never
/// declares it — and a process assembled without a stream can never satisfy it.
///
/// This is the one error no configuration change can fix, which is why it is separate from
/// `Unprovided` below.
#[test]
fn an_on_reading_component_without_a_stream_is_an_unknown_capability() {
    let writer = spec(
        "writer:timescale",
        Trigger::OnReading {
            filter: ReadingFilter::All,
            policy: ReadingPolicy::Durable,
            deadline: Duration::from_secs(5),
        },
    );
    let err = validate(
        vec![writer],
        &ctx().with_roots(RootSet::new([Capability::ComponentControl])),
    )
    .expect_err("a plan with no stream cannot carry an OnReading component");
    assert!(matches!(
        err,
        PlanError::UnknownCapability {
            capability: Capability::ReadingStream,
            ..
        }
    ));
    assert!(err.to_string().contains("writer:timescale"));

    // With the stream root present, the same spec plans.
    let writer = spec(
        "writer:timescale",
        Trigger::OnReading {
            filter: ReadingFilter::All,
            policy: ReadingPolicy::Durable,
            deadline: Duration::from_secs(5),
        },
    );
    assert_eq!(validate(vec![writer], &ctx()).unwrap().len(), 1);
}

/// **Duplicate exclusive provider.** Everything except `HttpRoutes` is exclusive, and the
/// error names both owners rather than one of them.
#[test]
fn two_exclusive_providers_of_one_capability_are_rejected() {
    let a =
        spec("db:postgres", Trigger::Service).provides([Provision::of(Capability::ReadingStore)]);
    let b =
        spec("db:replica", Trigger::Service).provides([Provision::of(Capability::ReadingStore)]);
    let err = validate(vec![a, b], &ctx()).expect_err("two providers of an exclusive capability");
    let PlanError::DuplicateExclusive {
        capability,
        first,
        second,
    } = &err
    else {
        panic!("wrong error: {err}");
    };
    assert_eq!(*capability, Capability::ReadingStore);
    assert_eq!(first.as_str(), "db:postgres");
    assert_eq!(second.as_str(), "db:replica");
}

/// Two providers of the **additive** capability are not an error — that is what additive
/// means, and `http:listener` mounting several route tables is the ordinary case.
#[test]
fn two_providers_of_an_additive_capability_are_fine() {
    let legacy = spec("api:legacy", Trigger::Service).provides([Provision::routes(
        RouteManifest::new(Scope::Any, vec![RouteEntry::exact("/latest", Methods::ANY)]),
    )]);
    let mgmt =
        spec("api:management", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Management,
            vec![RouteEntry::exact(
                "/api/v1/runtime/components",
                Methods::GET,
            )],
        ))]);
    let listener =
        spec("http:listener", Trigger::Service).requires([Need::hard(Capability::HttpRoutes)]);
    let plan = planned(vec![legacy, mgmt, listener]);
    assert_eq!(plan.providers_of(Capability::HttpRoutes).len(), 2);
}

/// A provider that declares the wrong arity for a capability.
#[test]
fn an_arity_a_capability_does_not_have_is_rejected() {
    let bad = spec("db:postgres", Trigger::Service).provides([Provision {
        capability: Capability::ReadingStore,
        arity: Arity::Additive,
        detail: mw_component::ProvisionDetail::None,
    }]);
    assert!(matches!(
        validate(vec![bad], &ctx()),
        Err(PlanError::ArityMismatch {
            capability: Capability::ReadingStore,
            declared: Arity::Additive,
            expected: Arity::Exclusive,
            ..
        })
    ));
}

/// A component may not provide a **root** capability: the runtime spine does.
#[test]
fn a_component_cannot_provide_a_root_capability() {
    let impostor = spec("api:management", Trigger::Service)
        .provides([Provision::of(Capability::ComponentControl)]);
    assert!(matches!(
        validate(vec![impostor], &ctx()),
        Err(PlanError::NotComponentProvidable {
            capability: Capability::ComponentControl,
            ..
        })
    ));
}

/// **Unprovided requirement.** A hard need for a component-provided capability that this
/// configuration contains no provider for. The fix is a configuration change, which is
/// what distinguishes it from `UnknownCapability`.
#[test]
fn a_hard_requirement_nobody_provides_is_rejected() {
    let uploader =
        spec("wu:backyard", every(60, 30)).requires([Need::hard(Capability::WindowedView)]);
    let err = validate(vec![uploader], &ctx()).expect_err("nobody provides WindowedView");
    assert!(matches!(
        err,
        PlanError::Unprovided {
            capability: Capability::WindowedView,
            ..
        }
    ));
    assert!(err
        .to_string()
        .contains("no component in this configuration"));
}

/// A **soft** requirement nobody provides is not an error: that is what keeps the website
/// from waiting on Aeris, and the component starts `Running { degraded }`.
#[test]
fn a_soft_requirement_nobody_provides_is_not_an_error() {
    let site = spec("web:site:1", Trigger::Service).requires([
        Need::soft(Capability::ForecastStore),
        Need::soft(Capability::AlertStore),
        Need::soft(Capability::SnowCache),
    ]);
    assert_eq!(planned(vec![site]).len(), 1);
}

/// **A cycle**, reported as a configuration error naming the path rather than experienced
/// as a hang.
#[test]
fn a_cycle_is_reported_with_its_path() {
    // a provides WindowedView and needs LatestSnapshot; b provides LatestSnapshot and
    // needs WindowedView.
    let a = spec("window:rollup", every(60, 10))
        .provides([Provision::of(Capability::WindowedView)])
        .requires([Need::hard(Capability::LatestSnapshot)]);
    let b = spec("snapshot:latest", Trigger::Service)
        .provides([Provision::of(Capability::LatestSnapshot)])
        .requires([Need::hard(Capability::WindowedView)]);
    let err = validate(vec![a, b], &ctx()).expect_err("that is a cycle");
    assert_cycle(&err, &["window:rollup", "snapshot:latest"]);
}

/// The path must be **a cycle**, not a path that leads into one.
///
/// A forward walk over consumer edges can leave the cycle down an acyclic tail and stop,
/// and the reported path then neither closes nor consists of real edges — it named a
/// component that was not part of the problem, which is the one thing `PlanError::Cycle`
/// exists not to do.
#[test]
fn a_cycle_with_an_innocent_consumer_hanging_off_it_reports_only_the_cycle() {
    let a = spec("window:rollup", every(60, 10))
        .provides([Provision::of(Capability::WindowedView)])
        .requires([Need::hard(Capability::LatestSnapshot)]);
    let b = spec("snapshot:latest", Trigger::Service)
        .provides([Provision::of(Capability::LatestSnapshot)])
        .requires([Need::hard(Capability::WindowedView)]);
    // Consumes the cycle and is consumed by nothing: the acyclic tail.
    let innocent =
        spec("wu:backyard", every(60, 30)).requires([Need::hard(Capability::WindowedView)]);

    // Both declaration orders. A diagnostic that depends on which component happened to
    // be declared first is itself the defect: with the tail declared between the two
    // cycle members, a forward walk over consumer edges leaves the cycle, dead-ends, and
    // falls back to reporting the whole unorderable remainder.
    for (label, specs) in [
        ("cycle first", vec![a.clone(), b.clone(), innocent.clone()]),
        (
            "tail in the middle",
            vec![a.clone(), innocent.clone(), b.clone()],
        ),
        ("tail first", vec![innocent, a, b]),
    ] {
        let err = validate(specs, &ctx()).expect_err("that is still a cycle");
        let path = assert_cycle(&err, &["window:rollup", "snapshot:latest"]);
        assert!(
            !path.iter().any(|id| id.as_str() == "wu:backyard"),
            "`wu:backyard` is not on the cycle ({label}): {path:?}"
        );
    }
}

/// Two interlocking cycles: the reported path still has to be one of them, closed.
#[test]
fn two_interlocking_cycles_still_report_a_closed_path() {
    let one = |n: u8| {
        let (provides, requires) = if n == 1 {
            (Capability::WindowedView, Capability::LatestSnapshot)
        } else {
            (Capability::LatestSnapshot, Capability::WindowedView)
        };
        spec(&format!("c{n}:node"), Trigger::Service)
            .provides([Provision::of(provides)])
            .requires([Need::hard(requires)])
    };
    let two = |n: u8| {
        let (provides, requires) = if n == 1 {
            (Capability::ForecastStore, Capability::AlertStore)
        } else {
            (Capability::AlertStore, Capability::ForecastStore)
        };
        spec(&format!("d{n}:node"), Trigger::Service)
            .provides([Provision::of(provides)])
            .requires([Need::hard(requires)])
    };
    let err = validate(vec![one(1), one(2), two(1), two(2)], &ctx())
        .expect_err("two cycles are two cycles");
    let path = assert_cycle(&err, &[]);
    let names: Vec<&str> = path.iter().map(ComponentId::as_str).collect();
    let in_one_cycle =
        names.iter().all(|n| n.starts_with('c')) || names.iter().all(|n| n.starts_with('d'));
    assert!(
        in_one_cycle,
        "the path mixes two separate cycles: {names:?}"
    );
}

/// Assert that a `Cycle` error's path is a real, closed cycle, and return it.
fn assert_cycle(err: &PlanError, expected_members: &[&str]) -> Vec<ComponentId> {
    let PlanError::Cycle { path } = err else {
        panic!("wrong error: {err}");
    };
    assert!(
        path.len() >= 3,
        "a closed cycle of N components has N+1 entries: {path:?}"
    );
    assert_eq!(
        path.first(),
        path.last(),
        "the path does not close: {path:?}"
    );
    let members: Vec<&str> = path[..path.len() - 1]
        .iter()
        .map(ComponentId::as_str)
        .collect();
    for expected in expected_members {
        assert!(
            members.contains(expected),
            "{expected} is missing from {members:?}"
        );
    }
    if !expected_members.is_empty() {
        assert_eq!(
            members.len(),
            expected_members.len(),
            "the path has members that are not on the cycle: {members:?}"
        );
    }
    let rendered = err.to_string();
    for member in &members {
        assert!(rendered.contains(member), "{rendered}");
    }
    path.clone()
}

/// The same relationship through **soft** edges is not a cycle, which is what makes `Soft`
/// the escape hatch W2a Decision 17 describes.
#[test]
fn the_same_pair_is_acyclic_if_one_edge_is_soft() {
    let a = spec("window:rollup", every(60, 10))
        .provides([Provision::of(Capability::WindowedView)])
        .requires([Need::soft(Capability::LatestSnapshot)]);
    let b = spec("snapshot:latest", Trigger::Service)
        .provides([Provision::of(Capability::LatestSnapshot)])
        .requires([Need::hard(Capability::WindowedView)]);
    let plan = planned(vec![a, b]);
    let order: Vec<&str> = plan.ids().map(ComponentId::as_str).collect();
    assert_eq!(order, vec!["window:rollup", "snapshot:latest"]);
}

/// A component that requires what it provides, hard, is its own cycle.
#[test]
fn a_hard_self_edge_is_a_cycle() {
    let narcissus = spec("window:rollup", every(60, 10))
        .provides([Provision::of(Capability::WindowedView)])
        .requires([Need::hard(Capability::WindowedView)]);
    assert!(matches!(
        validate(vec![narcissus], &ctx()),
        Err(PlanError::Cycle { .. })
    ));
}

/// **A bad deadline.** `deadline <= period` is what makes `Skip` the only overlap policy
/// this system needs, so a deadline longer than the period is rejected rather than
/// tolerated.
#[test]
fn a_deadline_longer_than_its_period_is_rejected() {
    let bad = spec("pws:backyard", every(60, 90));
    assert!(matches!(
        validate(vec![bad], &ctx()),
        Err(PlanError::DeadlineExceedsPeriod { .. })
    ));

    let zero = spec("pws:backyard", every(60, 0));
    assert!(matches!(
        validate(vec![zero], &ctx()),
        Err(PlanError::BadDeadline { .. })
    ));

    // The boundary is permitted: a deadline equal to the period means an overlap can only
    // ever be momentary, which is exactly the property being asserted.
    assert_eq!(planned(vec![spec("pws:backyard", every(60, 60))]).len(), 1);
}

/// **A sub-floor interval.** Enforced once, here, rather than clamped and logged on every
/// call as `ResolveUploadInterval` does today.
#[test]
fn an_interval_below_its_declared_floor_is_rejected() {
    let floor = Duration::from_secs(60);
    let too_fast = spec(
        "wu:backyard",
        Trigger::Every {
            period: Duration::from_secs(30),
            deadline: Duration::from_secs(10),
            first: mw_component::FirstRun::AtPhase,
            phase: mw_component::Phase::Spread,
            floor,
        },
    );
    let err = validate(vec![too_fast], &ctx()).expect_err("30s is below the 60s floor");
    assert!(matches!(err, PlanError::IntervalBelowFloor { .. }));
    assert!(err.to_string().contains("wu:backyard"));
}

/// **An impossible identity.** The Ambient defect, made unrepresentable: an endpoint the
/// station initiates cannot have a fixed identity, because the station would be whichever
/// listener received the request.
#[test]
fn an_inbound_endpoint_with_a_fixed_identity_is_rejected() {
    let ambient_today = spec("station:ambient", Trigger::Service).identity(IdentityShape {
        initiation: Initiation::Inbound,
        source: IdentitySource::Fixed,
        cardinality: mw_component::Cardinality::One,
        continuity: Continuity::Stream,
    });
    let err = validate(vec![ambient_today], &ctx()).expect_err("that is the Ambient bug");
    assert!(matches!(
        err,
        PlanError::ImpossibleIdentity {
            problem: IdentityProblem::InboundCannotBeFixed,
            ..
        }
    ));

    // One endpoint, N stations, identity from `?ID=`: the shape this item replaces it with.
    let ambient_now = spec("station:ambient", Trigger::Service).identity(IdentityShape {
        initiation: Initiation::Inbound,
        source: IdentitySource::FromQuery,
        cardinality: mw_component::Cardinality::Many,
        continuity: Continuity::Stream,
    });
    assert_eq!(planned(vec![ambient_now]).len(), 1);
}

/// The other half of the identity rule, and the one the MQTT flexibility test corrected:
/// cardinality follows the identity **source**, not the connection direction.
#[test]
fn cardinality_must_agree_with_the_identity_source() {
    let wrong = spec("station:davis:backyard", Trigger::Service).identity(IdentityShape {
        cardinality: mw_component::Cardinality::Many,
        ..IdentityShape::FIXED_STREAM
    });
    assert!(matches!(
        validate(vec![wrong], &ctx()),
        Err(PlanError::ImpossibleIdentity {
            problem: IdentityProblem::CardinalityDisagreesWithSource { .. },
            ..
        })
    ));
}

/// W2a Decision 41: a stream is a `Service` and a request/response is an occurrence, so a
/// transport cannot carry its own cadence.
#[test]
fn a_polled_endpoint_cannot_be_a_service() {
    let polled =
        spec("station:airgradient:1", Trigger::Service).identity(IdentityShape::FIXED_REQUEST);
    assert!(matches!(
        validate(vec![polled], &ctx()),
        Err(PlanError::ImpossibleIdentity {
            problem: IdentityProblem::TriggerDisagreesWithContinuity { .. },
            ..
        })
    ));
}

/// **A duplicate route** (**D-51**), named with both owners at plan time rather than
/// resolved by last-registration-wins at mount time.
#[test]
fn two_components_claiming_one_path_are_rejected() {
    let legacy =
        spec("api:legacy", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![
                RouteEntry::exact("/latest", Methods::ANY),
                RouteEntry::exact("/span/{span}", Methods::ANY),
            ],
        ))]);
    let impostor =
        spec("web:site:1", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::exact("/span/{window}", Methods::GET)],
        ))]);
    let err = validate(vec![legacy, impostor], &ctx()).expect_err("both claim /span/*");
    let PlanError::RouteConflict {
        scope,
        pattern,
        methods,
        a,
        b,
    } = &err
    else {
        panic!("wrong error: {err}");
    };
    assert_eq!(*scope, Scope::Any);
    // Both patterns, because they differ as strings and reporting one loses half the
    // problem; and the methods they actually **share**, not one owner's whole set.
    assert_eq!(pattern, "/span/{span} / /span/{window}");
    assert_eq!(*methods, Methods::GET);
    assert_eq!(a.as_str(), "api:legacy");
    assert_eq!(b.as_str(), "web:site:1");
}

/// A frozen path and a per-site path are **not** in disjoint scopes: one `Router` is
/// served on every socket and hostname routing selects the site, so a website
/// configuration that claimed `/latest` would make the frozen REST path unreachable on
/// that host. `Scope::Any` means every host, and that is what makes it overlap.
#[test]
fn an_any_scope_route_conflicts_with_a_per_site_one() {
    let legacy = spec("api:legacy", Trigger::Service).provides([Provision::routes(
        RouteManifest::new(Scope::Any, vec![RouteEntry::exact("/latest", Methods::ANY)]),
    )]);
    let site =
        spec("web:site:1", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Site(mw_types::WebsiteId::new(1)),
            vec![RouteEntry::exact("/latest", Methods::GET)],
        ))]);
    assert!(matches!(
        validate(vec![legacy, site], &ctx()),
        Err(PlanError::RouteConflict { .. })
    ));
}

/// A route manifest on a capability no listener consumes would be conflict-checked and
/// then never mounted, with nothing saying so.
#[test]
fn routes_declared_on_another_capability_are_rejected() {
    let confused = spec("web:site:1", Trigger::Service).provides([mw_component::Provision {
        capability: Capability::LatestSnapshot,
        arity: Arity::Exclusive,
        detail: mw_component::ProvisionDetail::Routes(RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::exact("/latest", Methods::GET)],
        )),
    }]);
    assert!(matches!(
        validate(vec![confused], &ctx()),
        Err(PlanError::RoutesOnNonRouteCapability {
            capability: Capability::LatestSnapshot,
            ..
        })
    ));
}

/// A pattern the router cannot mount is a plan error, not an `axum` panic inside
/// `http:listener` after its siblings have started.
#[test]
fn an_unmountable_route_pattern_is_rejected() {
    let bad = |pattern: &str| {
        spec("api:legacy", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::exact(pattern, Methods::GET)],
        ))])
    };
    for pattern in ["", "latest", "/span/{span"] {
        assert!(
            matches!(
                validate(vec![bad(pattern)], &ctx()),
                Err(PlanError::BadRoute { .. })
            ),
            "`{pattern}` should not be mountable"
        );
    }
    assert_eq!(planned(vec![bad("/latest")]).len(), 1);
}

/// The id the runtime spine publishes the root capabilities under. A component with that
/// name would, on its first stop, revoke `ComponentControl` and `ReadingStream` for the
/// whole process.
#[test]
fn a_component_may_not_be_called_spine() {
    assert!(matches!(
        validate(vec![spec("spine", Trigger::Service)], &ctx()),
        Err(PlanError::ReservedComponentId { .. })
    ));
}

/// **Unknown capability, through an explicitly declared need.** The implied
/// `OnReading → ReadingStream` path above is one route to this error; the canonical one is
/// W2a §2.5's `api:management` requiring `ComponentControl` in a process that was not
/// given it.
#[test]
fn a_declared_need_for_an_absent_root_is_an_unknown_capability() {
    let mgmt = spec("api:management", Trigger::Service)
        .requires([Need::hard(Capability::ComponentControl)]);
    let err = validate(vec![mgmt], &ctx().with_roots(RootSet::empty()))
        .expect_err("a process with no component control cannot satisfy it");
    assert!(
        matches!(
            err,
            PlanError::UnknownCapability {
                capability: Capability::ComponentControl,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("does not provide at all"), "{err}");

    // The ordinary root set satisfies it, which is what makes the error about the build
    // rather than about the configuration.
    let mgmt = spec("api:management", Trigger::Service)
        .requires([Need::hard(Capability::ComponentControl)]);
    assert_eq!(planned(vec![mgmt]).len(), 1);
}

/// `RootSet` only admits root-class capabilities. Claiming a component-provided one as a
/// root would make `Unprovided` unreachable for it.
#[test]
fn a_root_set_ignores_a_capability_no_root_can_provide() {
    let pretend = RootSet::new([Capability::ReadingStore, Capability::ComponentControl]);
    assert!(pretend.contains(Capability::ComponentControl));
    assert!(!pretend.contains(Capability::ReadingStore));

    let consumer =
        spec("wu:backyard", every(60, 30)).requires([Need::hard(Capability::ReadingStore)]);
    assert!(matches!(
        validate(vec![consumer], &ctx().with_roots(pretend)),
        Err(PlanError::Unprovided { .. })
    ));
}

/// A gRPC mount is a prefix, and a prefix claims what is under it.
#[test]
fn a_prefix_mount_conflicts_with_a_route_inside_it() {
    let grpc =
        spec("grpc:weatherv1", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::prefix("/weather.v1.WeatherV1", Methods::POST)],
        ))]);
    let intruder =
        spec("api:legacy", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::exact(
                "/weather.v1.WeatherV1/GetLatest",
                Methods::POST,
            )],
        ))]);
    assert!(matches!(
        validate(vec![grpc, intruder], &ctx()),
        Err(PlanError::RouteConflict { .. })
    ));
}

/// The same pattern in two **scopes** is not a conflict: three websites each declaring `/`
/// is the ordinary configuration, and the listener routes by host first.
#[test]
fn the_same_pattern_in_two_scopes_is_not_a_conflict() {
    let site = |n: i64| {
        spec(&format!("web:site:{n}"), Trigger::Service).provides([Provision::routes(
            RouteManifest::new(
                Scope::Site(mw_types::WebsiteId::new(n)),
                vec![RouteEntry::exact("/", Methods::READ)],
            ),
        )])
    };
    assert_eq!(planned(vec![site(1), site(2), site(3)]).len(), 3);
}

/// A component that declares the same path twice would register two handlers and keep
/// whichever the router resolved last.
#[test]
fn one_component_claiming_a_path_twice_is_rejected() {
    let twice =
        spec("api:legacy", Trigger::Service).provides([Provision::routes(RouteManifest::new(
            Scope::Any,
            vec![
                RouteEntry::exact("/latest", Methods::GET),
                RouteEntry::exact("/latest", Methods::ANY),
            ],
        ))]);
    assert!(matches!(
        validate(vec![twice], &ctx()),
        Err(PlanError::RouteConflict { .. })
    ));
}

// --- the rest of the per-component declaration checks ------------------------------

#[test]
fn two_components_with_one_id_are_rejected() {
    let a = spec("wu:backyard", every(60, 30));
    let b = spec("wu:backyard", every(120, 30));
    assert!(matches!(
        validate(vec![a, b], &ctx()),
        Err(PlanError::DuplicateComponentId { .. })
    ));
}

/// A `Calendar` component whose station has no usable zone is a plan error, not a silent
/// fallback to UTC — which is what every rollover in the Go tree does today.
#[test]
fn a_calendar_without_a_time_zone_is_rejected() {
    let rollover = spec(
        "snow:season",
        Trigger::Calendar {
            spec: CronSpec::daily_at(LocalTime::new(0, 10)),
            tz: TzSource::Station {
                station: mw_types::StationId::new(4),
                zone: None,
            },
            misfire: Misfire::DEFAULT,
            deadline: Duration::from_secs(60),
        },
    );
    let err = validate(vec![rollover], &ctx()).expect_err("no zone");
    assert!(matches!(err, PlanError::MissingTimeZone { .. }));
    assert!(err.to_string().contains("station 4"), "{err}");

    // The deployment default reads as a sentence rather than as a substitution.
    let deployment = spec(
        "aeris:alerts:gc",
        Trigger::Calendar {
            spec: CronSpec::daily_at(LocalTime::new(3, 0)),
            tz: TzSource::Deployment { zone: None },
            misfire: Misfire::DEFAULT,
            deadline: Duration::from_secs(60),
        },
    );
    let err = validate(vec![deployment], &ctx()).expect_err("no zone");
    assert!(
        err.to_string().contains("deployment's default time zone"),
        "{err}"
    );

    let with_zone = spec(
        "snow:season",
        Trigger::Calendar {
            spec: CronSpec::daily_at(LocalTime::new(0, 10)),
            tz: TzSource::Station {
                station: mw_types::StationId::new(4),
                zone: Some(Zone::new("America/Denver").unwrap()),
            },
            misfire: Misfire::DEFAULT,
            deadline: Duration::from_secs(60),
        },
    );
    assert_eq!(planned(vec![with_zone]).len(), 1);
}

#[test]
fn an_unschedulable_calendar_is_rejected() {
    let bad = spec(
        "aeris:alerts:gc",
        Trigger::Calendar {
            spec: CronSpec::Daily { slots: vec![] },
            tz: TzSource::Deployment {
                zone: Some(Zone::new("UTC").unwrap()),
            },
            misfire: Misfire::DEFAULT,
            deadline: Duration::from_secs(60),
        },
    );
    assert!(matches!(
        validate(vec![bad], &ctx()),
        Err(PlanError::BadCalendar { .. })
    ));
}

/// The trigger-to-body mapping of W2a §1.2 is total, so a mismatch is only reachable from
/// a registrar that assembled a spec by hand — and it is caught before anything is built.
#[test]
fn a_trigger_and_a_body_that_disagree_are_rejected() {
    let wrong = ComponentSpec::new(id("http:listener"), Trigger::Service, tick_builder());
    assert!(matches!(
        validate(vec![wrong], &ctx()),
        Err(PlanError::TriggerBodyMismatch { .. })
    ));
}

// --- ordering, registrars and the diff ----------------------------------------------

/// The topological order puts every provider before its consumers, and the ranks are what
/// shutdown walks in reverse.
#[test]
fn the_plan_is_ordered_providers_first_and_ranked() {
    let db = spec("db:postgres", Trigger::Service).provides([
        Provision::of(Capability::ReadingStore),
        Provision::of(Capability::SnowCache),
    ]);
    let rollup = spec("window:rollup", every(60, 10))
        .requires([Need::hard(Capability::ReadingStore)])
        .provides([Provision::of(Capability::WindowedView)]);
    let wu = spec("wu:backyard", every(60, 30)).requires([Need::hard(Capability::WindowedView)]);
    let snow = spec("snow:depth", every(30, 20)).requires([Need::hard(Capability::SnowCache)]);

    // Deliberately in the wrong order on the way in.
    let plan = planned(vec![wu, snow, rollup, db]);
    let order: Vec<&str> = plan.ids().map(ComponentId::as_str).collect();
    assert_eq!(
        order,
        vec!["db:postgres", "snow:depth", "window:rollup", "wu:backyard"]
    );

    let ranks: Vec<Vec<&str>> = plan
        .ranks()
        .iter()
        .map(|rank| rank.iter().map(ComponentId::as_str).collect())
        .collect();
    assert_eq!(
        ranks,
        vec![
            vec!["db:postgres"],
            vec!["snow:depth", "window:rollup"],
            vec!["wu:backyard"],
        ],
        "siblings share a rank and start concurrently"
    );
    assert_eq!(plan.generation(), Generation::new(7));
}

struct Stations(usize);

impl Registrar<usize> for Stations {
    fn name(&self) -> &'static str {
        "stations"
    }

    fn register(&self, cfg: &usize, out: &mut Vec<ComponentSpec>) -> Result<(), RegisterError> {
        if *cfg > self.0 {
            return Err(RegisterError::new(format!(
                "{cfg} stations is more than this test supports"
            )));
        }
        for n in 0..*cfg {
            out.push(
                spec(&format!("station:davis:{n}"), Trigger::Service)
                    .identity(IdentityShape::FIXED_STREAM),
            );
        }
        Ok(())
    }
}

/// `plan()` over registrars, which is the shape the binary calls: the supervisor never
/// names a component kind.
#[test]
fn registrars_contribute_specs_and_a_refusal_names_the_registrar() {
    let stations = Stations(4);
    let registrars: Vec<&dyn Registrar<usize>> = vec![&stations];
    let three = plan(&3usize, &registrars, &ctx()).expect("three stations plan");
    assert_eq!(three.len(), 3);

    let err = plan(&9usize, &registrars, &ctx()).expect_err("nine do not");
    let PlanError::Registrar { registrar, reason } = &err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(registrar, &"stations");
    assert!(reason.contains("9 stations"));
}

/// The plan diff: unchanged components are **not touched**, which is the whole of "saving a
/// website configuration no longer disturbs the APRS schedule".
#[test]
fn the_diff_distinguishes_added_removed_changed_and_unchanged() {
    let before = planned(vec![
        spec("wu:backyard", every(60, 30)).config_digest(mw_component::ConfigDigest::of("KE7XYZ")),
        spec("pws:backyard", every(60, 30)),
        spec("station:ambient", Trigger::Service).identity(IdentityShape {
            initiation: Initiation::Inbound,
            source: IdentitySource::FromQuery,
            cardinality: mw_component::Cardinality::Many,
            continuity: Continuity::Stream,
        }),
    ]);
    let after = planned(vec![
        // Same declaration, different configuration behind the builder: changed, and only
        // the digest can see it.
        spec("wu:backyard", every(60, 30)).config_digest(mw_component::ConfigDigest::of("N0CALL")),
        // Untouched.
        spec("pws:backyard", every(60, 30)),
        // Added.
        spec("aprs:backyard", every(600, 60)),
        // `station:ambient` is gone.
    ]);

    let d = diff(&before, &after);
    let names = |v: &Vec<ComponentId>| -> Vec<String> {
        v.iter().map(|id| id.as_str().to_string()).collect()
    };
    assert_eq!(names(&d.changed), vec!["wu:backyard"]);
    assert_eq!(names(&d.unchanged), vec!["pws:backyard"]);
    assert_eq!(names(&d.added), vec!["aprs:backyard"]);
    assert_eq!(names(&d.removed), vec!["station:ambient"]);
    assert!(!d.is_empty());
    assert!(diff(&before, &before).is_empty());
}

#[test]
fn an_empty_configuration_is_a_valid_plan() {
    let plan = planned(vec![]);
    assert!(plan.is_empty());
    assert!(diff(&plan, &plan).is_empty());
}
