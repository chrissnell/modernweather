//! `mw_station::register` — the pure half, and every structural error it can report.
//!
//! The checks asserted here are the ones `plan()` makes *through* a spec — the identity
//! shape against the trigger (W2a Decisions 41 and 42) — plus the ones only this crate can
//! make, because only this crate knows what a WeatherLink Live is.
//!
//! `mw_supervisor::plan::validate` is deliberately **not** called: it is in the same layer
//! as this crate and `ci/layers.toml` counts a dev-dependency, so the assertions below use
//! `IdentityShape::problem` directly — which is the function `plan()` itself calls. The
//! binary's `tests/boot.rs` is where the whole plan is validated end to end.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{Cardinality, Continuity, IdentitySource, Ingress, Initiation, Trigger};
use mw_station::config::{StationConfig, StationRow};
use mw_station::plan::{self, StationPlanError};
use mw_station::testing::RecordingIngress;
use mw_station::TransportColumns;

/// The binary's configuration type, stood in for.
struct Cfg {
    rows: Vec<StationRow>,
    ingress: Option<Arc<dyn Ingress>>,
    upload: Option<Arc<mw_station::ambient::UploadState>>,
}

impl Cfg {
    fn new(rows: Vec<StationRow>) -> Self {
        Self {
            rows,
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
            upload: Some(Arc::new(mw_station::ambient::UploadState::new())),
        }
    }

    /// A node with stations and no HTTP listener, which is what refuses an Ambient row.
    fn without_listener(rows: Vec<StationRow>) -> Self {
        Self {
            rows,
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
            upload: None,
        }
    }

    /// A node that serves and does not ingest.
    fn without_ingress(rows: Vec<StationRow>) -> Self {
        Self {
            rows,
            ingress: None,
            upload: Some(Arc::new(mw_station::ambient::UploadState::new())),
        }
    }
}

impl StationConfig for Cfg {
    fn stations(&self) -> &[StationRow] {
        &self.rows
    }

    fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
        self.ingress.as_ref()
    }

    fn ambient_upload_state(&self) -> Option<&Arc<mw_station::ambient::UploadState>> {
        self.upload.as_ref()
    }
}

/// A Davis console, on whichever transport the caller wants.
fn davis(key: &str, transport: TransportColumns) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "davis".into(),
        enabled: true,
        identity_kind: "fixed".into(),
        transport: Some(transport),
        ..StationRow::default()
    }
}

/// A Campbell datalogger.
fn campbell(key: &str, transport: TransportColumns) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "campbellscientific".into(),
        enabled: true,
        identity_kind: "fixed".into(),
        transport: Some(transport),
        ..StationRow::default()
    }
}

fn serial(dev: &str, baud: i64) -> TransportColumns {
    TransportColumns {
        kind: "serial".into(),
        dev: Some(dev.into()),
        baud: Some(baud),
        ..TransportColumns::default()
    }
}

fn tcp(host: &str, port: i64) -> TransportColumns {
    TransportColumns {
        kind: "tcp_out".into(),
        host: Some(host.into()),
        port: Some(port),
        ..TransportColumns::default()
    }
}

/// An AirGradient monitor.
fn airgradient(key: &str, url: &str) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "airgradient".into(),
        enabled: true,
        identity_kind: "fixed".into(),
        transport: Some(TransportColumns {
            kind: "http_out".into(),
            url: Some(url.into()),
            ..TransportColumns::default()
        }),
        ..StationRow::default()
    }
}

/// An Ambient console, which is a row and not a listener.
fn ambient(key: &str, claim: &str, mount: &str) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "ambient-customized".into(),
        enabled: true,
        identity_kind: "from_query".into(),
        presented_key: Some(claim.into()),
        has_ingest_secret: true,
        transport: Some(TransportColumns {
            kind: "http_in".into(),
            mount: Some(mount.into()),
            ..TransportColumns::default()
        }),
        ..StationRow::default()
    }
}

/// A snow gauge, which this crate validates and does not plan.
fn snowgauge(key: &str, url: &str) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "snowgauge".into(),
        enabled: true,
        identity_kind: "fixed".into(),
        snow_base_distance_mm: Some(1500.0),
        transport: Some(TransportColumns {
            kind: "grpc_out".into(),
            url: Some(url.into()),
            streaming: Some(true),
            ..TransportColumns::default()
        }),
        ..StationRow::default()
    }
}

/// A broadcasting WeatherLink Live this node also arms.
fn broadcast(key: &str, serial: &str, port: i64, host: Option<&str>) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "weatherlink-live".into(),
        enabled: true,
        identity_kind: "from_payload".into(),
        presented_key: Some(serial.into()),
        transport: Some(TransportColumns {
            kind: "udp_in".into(),
            port: Some(port),
            host: host.map(str::to_string),
            ..TransportColumns::default()
        }),
        wll_sensor_mapping: Some("th:1, wind:1, baro".into()),
        wll_poll_interval_seconds: None,
        ..StationRow::default()
    }
}

/// A polled WeatherLink Live.
fn polled(key: &str, url: &str) -> StationRow {
    StationRow {
        key: key.into(),
        name: key.into(),
        kind: "weatherlink-live".into(),
        enabled: true,
        identity_kind: "fixed".into(),
        presented_key: None,
        transport: Some(TransportColumns {
            kind: "http_out".into(),
            url: Some(url.into()),
            ..TransportColumns::default()
        }),
        wll_sensor_mapping: Some("th:1, wind:1, baro".into()),
        wll_poll_interval_seconds: None,
        ..StationRow::default()
    }
}

fn register(cfg: &Cfg) -> Result<Vec<mw_component::ComponentSpec>, StationPlanError> {
    let mut out = Vec::new();
    plan::register(cfg, &mut out)?;
    Ok(out)
}

fn ids(specs: &[mw_component::ComponentSpec]) -> Vec<String> {
    let mut v: Vec<String> = specs.iter().map(|s| s.id.as_str().to_string()).collect();
    v.sort();
    v
}

/// Every spec a registrar emits has to survive the same check `plan()` makes.
fn every_spec_is_plannable(specs: &[mw_component::ComponentSpec]) {
    for spec in specs {
        if let Some(identity) = &spec.identity {
            assert_eq!(
                identity.problem(&spec.trigger),
                None,
                "{} declares an impossible identity",
                spec.id
            );
        }
        if let Trigger::Every {
            period,
            deadline,
            floor,
            ..
        } = &spec.trigger
        {
            assert!(deadline <= period, "{}: deadline exceeds period", spec.id);
            assert!(period >= floor, "{}: period below floor", spec.id);
            assert!(!deadline.is_zero(), "{}: a zero deadline", spec.id);
        }
        assert_eq!(
            spec.trigger.body_kind(),
            spec.build.kind(),
            "{}: the trigger and the body disagree",
            spec.id
        );
    }
}

#[tokio::test]
async fn a_broadcasting_station_this_node_arms_becomes_a_listener_and_a_re_arm() {
    let cfg = Cfg::new(vec![broadcast(
        "suncrest:roof",
        "001D0A700002",
        22222,
        Some("192.168.1.50"),
    )]);
    let specs = register(&cfg).expect("a valid row");
    assert_eq!(
        ids(&specs),
        vec![
            "station:wll:suncrest:roof:rearm".to_string(),
            "station:wll:udp:22222".to_string(),
        ]
    );
    every_spec_is_plannable(&specs);

    let listener = specs
        .iter()
        .find(|s| s.id.as_str() == "station:wll:udp:22222")
        .expect("the listener");
    assert_eq!(listener.trigger, Trigger::Service, "a listener is a stream");
    let shape = listener.identity.expect("a station component has a shape");
    assert_eq!(shape.initiation, Initiation::Inbound);
    assert_eq!(shape.source, IdentitySource::FromPayload);
    assert_eq!(
        shape.cardinality,
        Cardinality::Many,
        "one endpoint, N units (W2a Decision 42)"
    );
    assert_eq!(shape.continuity, Continuity::Stream);

    let rearm = specs
        .iter()
        .find(|s| s.id.as_str().ends_with(":rearm"))
        .expect("the re-arm");
    assert!(
        matches!(rearm.trigger, Trigger::Every { period, .. } if period == Duration::from_secs(2880)),
        "0.8 x 3600 (W2a §4a.5), not the Go driver's 0.9: {:?}",
        rearm.trigger
    );
    assert_eq!(
        rearm.identity, None,
        "the re-arm emits no readings, so it identifies no station"
    );
    assert!(
        rearm.requires.is_empty(),
        "the device row is closed over at declaration time, not required at run time"
    );
}

/// `transport_host` absent is a unit something else arms. A configuration, not a mistake.
#[tokio::test]
async fn a_listen_only_station_produces_no_re_arm() {
    let cfg = Cfg::new(vec![broadcast("roof", "001D0A700002", 22222, None)]);
    let specs = register(&cfg).expect("a valid row");
    assert_eq!(ids(&specs), vec!["station:wll:udp:22222".to_string()]);
    every_spec_is_plannable(&specs);
}

#[tokio::test]
async fn a_polled_station_becomes_one_request_per_occurrence() {
    let cfg = Cfg::new(vec![polled("suncrest:roof", "http://192.168.1.50")]);
    let specs = register(&cfg).expect("a valid row");
    assert_eq!(ids(&specs), vec!["station:wll:suncrest:roof".to_string()]);
    every_spec_is_plannable(&specs);

    let Trigger::Every {
        period,
        floor,
        first,
        ..
    } = &specs[0].trigger
    else {
        panic!("not an Every: {:?}", specs[0].trigger);
    };
    assert_eq!(*period, Duration::from_secs(60), "the driver's default");
    assert_eq!(
        *floor,
        Duration::from_secs(30),
        "W2a 4a.5 declares a 30-second floor; plan() is what enforces it"
    );
    assert_eq!(*first, mw_component::FirstRun::AtPhase);

    let shape = specs[0].identity.expect("a station component has a shape");
    assert_eq!(shape.source, IdentitySource::Fixed);
    assert_eq!(shape.cardinality, Cardinality::One);
    assert_eq!(shape.continuity, Continuity::Request);
    assert_eq!(shape.initiation, Initiation::Outbound);
}

#[tokio::test]
async fn a_configured_poll_interval_is_used_and_a_sub_floor_one_is_plan_visible() {
    let mut row = polled("roof", "http://192.168.1.50");
    row.wll_poll_interval_seconds = Some(45);
    let specs = register(&Cfg::new(vec![row])).expect("valid");
    assert!(
        matches!(specs[0].trigger, Trigger::Every { period, .. } if period == Duration::from_secs(45))
    );

    // The column's CHECK stops a sub-floor value being written; if one is there anyway, the
    // spec carries it and `plan()` rejects it by name rather than this crate clamping it
    // per occurrence as `runPollingMode` does.
    let mut row = polled("roof", "http://192.168.1.50");
    row.wll_poll_interval_seconds = Some(5);
    let specs = register(&Cfg::new(vec![row])).expect("valid rows, an invalid interval");
    let Trigger::Every { period, floor, .. } = &specs[0].trigger else {
        panic!("not an Every");
    };
    assert!(period < floor, "the plan error is plan()'s to report");
}

#[tokio::test]
async fn two_units_on_one_port_share_one_listener() {
    let cfg = Cfg::new(vec![
        broadcast("roof", "001D0A700002", 22222, None),
        broadcast("field", "001D0A700003", 22222, None),
    ]);
    let specs = register(&cfg).expect("two valid rows");
    assert_eq!(
        ids(&specs),
        vec!["station:wll:udp:22222".to_string()],
        "a second station is a row, not a second socket (W2a §4a.4)"
    );
    // ... and two ports are two listeners, because the component is per endpoint.
    let cfg = Cfg::new(vec![
        broadcast("roof", "001D0A700002", 22222, None),
        broadcast("field", "001D0A700003", 22223, None),
    ]);
    assert_eq!(register(&cfg).expect("valid").len(), 2);
}

#[tokio::test]
async fn two_stations_presenting_one_serial_are_refused() {
    let cfg = Cfg::new(vec![
        broadcast("roof", "001D0A700002", 22222, None),
        // The same unit, in the other case, which is the way this mistake actually looks.
        broadcast("field", "001d0a700002", 22222, None),
    ]);
    let e = register(&cfg).expect_err("ambiguous");
    assert!(matches!(e, StationPlanError::DuplicateClaim { .. }), "{e}");
    assert!(e.to_string().contains("resolves to neither"), "{e}");
}

#[tokio::test]
async fn a_station_that_is_not_collected_here_produces_nothing() {
    // Switched off (W5 §1.2).
    let mut off = broadcast("roof", "001D0A700002", 22222, None);
    off.enabled = false;
    assert!(register(&Cfg::new(vec![off])).expect("valid").is_empty());

    // Received from a peer: inventory on this node, not a component.
    let received = StationRow {
        key: "satellite-a:backyard".into(),
        name: "backyard".into(),
        kind: "davis".into(),
        enabled: true,
        identity_kind: "from_registration".into(),
        presented_key: None,
        transport: None,
        wll_sensor_mapping: None,
        wll_poll_interval_seconds: None,
        ..StationRow::default()
    };
    assert!(register(&Cfg::new(vec![received]))
        .expect("valid")
        .is_empty());
}

/// A station type whose driver has not landed is an error that names the work item, not a
/// silence.
#[tokio::test]
async fn a_station_type_with_no_driver_is_refused_by_name() {
    // A WeatherFlow Tempest, which docs/component-model.md §4a.7 lists as a vendor-cloud
    // poll and which no item has written.
    let mut row = polled("backyard", "http://192.168.1.9");
    row.kind = "tempest".into();
    let e = register(&Cfg::new(vec![row])).expect_err("no Tempest driver");
    assert!(matches!(e, StationPlanError::NoDriver { .. }), "{e}");
    let text = e.to_string();
    assert!(text.contains("`backyard`"), "{text}");
    // The message names the types this build DOES serve, which is what an operator needs
    // and is also the list a reader can check against the schema's own `CHECK`.
    for kind in [
        "weatherlink-live",
        "davis",
        "campbellscientific",
        "ambient-customized",
        "airgradient",
        "snowgauge",
    ] {
        assert!(text.contains(kind), "{text}");
    }
}

#[tokio::test]
async fn a_weatherlink_live_with_a_transport_it_has_no_mode_for_is_refused() {
    let mut row = polled("roof", "http://192.168.1.50");
    row.transport = Some(TransportColumns {
        kind: "serial".into(),
        dev: Some("/dev/ttyUSB0".into()),
        baud: Some(19_200),
        ..TransportColumns::default()
    });
    let e = register(&Cfg::new(vec![row])).expect_err("no serial mode");
    assert!(matches!(e, StationPlanError::WrongTransport { .. }), "{e}");
    assert!(e.to_string().contains("`serial`"), "{e}");
}

#[tokio::test]
async fn the_mapping_is_required_and_is_parsed_at_plan_time() {
    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.wll_sensor_mapping = None;
    let e = register(&Cfg::new(vec![row])).expect_err("no mapping");
    assert!(
        matches!(
            e,
            StationPlanError::Missing {
                column: "wll_sensor_mapping",
                ..
            }
        ),
        "{e}"
    );
    assert!(
        e.to_string().contains("decodes to an empty reading"),
        "the message says why the column matters: {e}"
    );

    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.wll_sensor_mapping = Some("wnid:1".into());
    let e = register(&Cfg::new(vec![row])).expect_err("a typo in the mapping");
    assert!(matches!(e, StationPlanError::Mapping { .. }), "{e}");
    assert!(e.to_string().contains("not a sensor role"), "{e}");
}

/// The allow-list a `UdpIn` endpoint resolves against has to have contents.
#[tokio::test]
async fn a_broadcasting_station_without_its_serial_is_refused() {
    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.presented_key = None;
    let e = register(&Cfg::new(vec![row])).expect_err("nothing to resolve against");
    assert!(
        matches!(
            e,
            StationPlanError::Missing {
                column: "presented_key",
                ..
            }
        ),
        "{e}"
    );
    assert!(e.to_string().contains("allow-list"), "{e}");
}

#[tokio::test]
async fn an_identity_strategy_this_driver_cannot_use_is_refused() {
    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.identity_kind = "from_path".into();
    let e = register(&Cfg::new(vec![row])).expect_err("a UDP datagram has no path");
    assert!(matches!(e, StationPlanError::WrongIdentity { .. }), "{e}");
    assert!(e.to_string().contains("from_payload"), "{e}");
}

/// `transport_host` is a free-text column, and the one thing a misused host column holds
/// is a URL — which is where a credential hides.
#[tokio::test]
async fn a_host_column_holding_more_than_a_host_is_refused_without_quoting_it() {
    for (what, host) in [
        ("a scheme", "http://192.168.1.50"),
        ("a path", "192.168.1.50/v1/real_time"),
        ("a port", "192.168.1.50:8080"),
    ] {
        let mut row = broadcast("roof", "001D0A700002", 22222, None);
        row.transport.as_mut().expect("a transport").host = Some(host.to_string());
        let e = register(&Cfg::new(vec![row])).expect_err(what);
        assert!(matches!(e, StationPlanError::Host { .. }), "{what}: {e}");
        assert!(e.to_string().contains("transport_host"), "{what}: {e}");
    }
    // The one that matters: a credential is refused and never quoted back.
    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.transport.as_mut().expect("a transport").host = Some("admin:hunter2@1.2.3.4".into());
    let e = register(&Cfg::new(vec![row])).expect_err("a credential in a host column");
    let text = mw_types::DisplayChain(&e).to_string();
    assert!(!text.contains("hunter2"), "{text}");
}

/// `fixed_receivers` must not name a component `register` would refuse to emit.
#[tokio::test]
async fn fixed_receivers_applies_the_same_station_type_guard_as_register() {
    let mut row = polled("backyard", "http://192.168.1.9");
    row.kind = "tempest".into();
    let cfg = Cfg::new(vec![row]);
    // `register` refuses the row outright...
    assert!(matches!(
        register(&cfg).expect_err("no Tempest driver"),
        StationPlanError::NoDriver { .. }
    ));
    // ... so nothing may claim a `station:wll:backyard` receiver for it. Without the guard
    // this returned one, and was safe only because `boot.rs` happens to call `register`
    // first.
    assert!(
        plan::fixed_receivers(&cfg).expect("no error").is_empty(),
        "a station with no driver has no receiver"
    );

    // And the pairing guard, which is the other half: a `davis` row with an `http_out`
    // transport is a row `register` refuses, so it has no receiver either -- where without
    // the pairing it would claim `station:davis:backyard`, an id nothing emits.
    let mut row = polled("backyard", "http://192.168.1.9");
    row.kind = "davis".into();
    let cfg = Cfg::new(vec![row]);
    assert!(matches!(
        register(&cfg).expect_err("Davis has no HTTP mode"),
        StationPlanError::WrongTransport { .. }
    ));
    assert!(plan::fixed_receivers(&cfg).expect("no error").is_empty());
}

#[tokio::test]
async fn a_tls_address_for_a_unit_with_no_tls_listener_is_refused_at_plan_time() {
    let e = register(&Cfg::new(vec![polled("roof", "https://192.168.1.50")]))
        .expect_err("the unit has no TLS listener");
    assert!(matches!(e, StationPlanError::NotTls { .. }), "{e}");
}

#[tokio::test]
async fn a_node_with_stations_and_no_reading_stream_is_refused() {
    let cfg = Cfg::without_ingress(vec![broadcast("roof", "001D0A700002", 22222, None)]);
    let e = register(&cfg).expect_err("nowhere to admit to");
    assert!(matches!(e, StationPlanError::NoIngress { .. }), "{e}");
    // ... and a node with no stations at all is fine without one.
    assert!(register(&Cfg::without_ingress(Vec::new()))
        .expect("valid")
        .is_empty());
}

/// The two functions that mint component ids must agree, because the registry resolves a
/// `Fixed` frame by its receiver and a mismatch would make every polled reading unknown.
#[tokio::test]
async fn fixed_receivers_names_exactly_the_components_register_emits() {
    let cfg = Cfg::new(vec![
        polled("suncrest:roof", "http://192.168.1.50"),
        polled("suncrest:field", "http://192.168.1.51"),
        // A broadcasting unit is `FromPayload`, so it is resolved by its claim and must
        // **not** appear here -- nor must its re-arm, which receives no frames at all.
        broadcast("suncrest:barn", "001D0A700002", 22222, Some("192.168.1.52")),
    ]);
    let specs = register(&cfg).expect("valid");
    every_spec_is_plannable(&specs);

    let receivers = plan::fixed_receivers(&cfg).expect("valid");
    let mut named: Vec<String> = receivers
        .iter()
        .map(|r| format!("{} -> {}", r.component, r.station))
        .collect();
    named.sort();
    assert_eq!(
        named,
        vec![
            "station:wll:suncrest:field -> suncrest:field".to_string(),
            "station:wll:suncrest:roof -> suncrest:roof".to_string(),
        ]
    );
    // Every receiver is a component the plan actually contains.
    let planned = ids(&specs);
    for r in &receivers {
        assert!(
            planned.contains(&r.component.as_str().to_string()),
            "{} is not in the plan: {planned:?}",
            r.component
        );
    }
}

/// A key that cannot be a metric label cannot be a component id either, and that is a plan
/// error naming the key rather than a label nothing can scrape.
#[tokio::test]
async fn a_station_key_that_cannot_name_a_component_is_refused() {
    let e = register(&Cfg::new(vec![polled("Roof Station", "http://h")]))
        .expect_err("a space and two capitals");
    assert!(matches!(e, StationPlanError::Key { .. }), "{e}");
    assert!(e.to_string().contains("Roof Station"), "{e}");
}

/// The digest is what makes the plan diff accurate (W2a Decision 25).
#[tokio::test]
async fn changing_one_units_mapping_changes_the_listeners_fingerprint() {
    let before = register(&Cfg::new(vec![broadcast(
        "roof",
        "001D0A700002",
        22222,
        None,
    )]))
    .expect("valid");
    let mut row = broadcast("roof", "001D0A700002", 22222, None);
    row.wll_sensor_mapping = Some("th:1, wind:2, baro".into());
    let after = register(&Cfg::new(vec![row])).expect("valid");
    assert_ne!(
        before[0].fingerprint(),
        after[0].fingerprint(),
        "the listener closes over the mapping, so it has to restart when it changes"
    );
}

// ---------------------------------------------------------------------------------------
// M6-1's six drivers.

/// Davis and Campbell on both of their transports, with the component ids and the shapes
/// W2a §4a.5's table declares.
#[tokio::test]
async fn davis_and_campbell_are_planned_on_serial_and_on_tcp() {
    let specs = register(&Cfg::new(vec![
        davis("suncrest:backyard", serial("/dev/ttyUSB0", 19_200)),
        // **N-20**: the owner's station is on USB serial and the daemon is moving into
        // Kubernetes, so the Campbell has to work over a `ser2net` bridge as well.
        campbell("suncrest:meadow", tcp("ser2net.lan", 3001)),
    ]))
    .expect("a valid plan");
    assert_eq!(
        ids(&specs),
        vec![
            "station:campbell:suncrest:meadow",
            "station:davis:suncrest:backyard",
        ]
    );
    for spec in &specs {
        // A conversation owns its own loop, so the trigger is `Service` and the shape is
        // the outbound, 1:1, streaming one -- which is what `IdentityShape::problem`
        // checks the trigger against.
        assert!(matches!(spec.trigger, Trigger::Service), "{}", spec.id);
        let shape = spec.identity.as_ref().expect("a declared shape");
        assert_eq!(shape.initiation, Initiation::Outbound, "{}", spec.id);
        assert_eq!(shape.source, IdentitySource::Fixed, "{}", spec.id);
        assert_eq!(shape.cardinality, Cardinality::One, "{}", spec.id);
        assert_eq!(shape.continuity, Continuity::Stream, "{}", spec.id);
        assert_eq!(shape.problem(&spec.trigger), None, "{}", spec.id);
    }

    // And the other transport for each, so that neither is planned on one alone.
    let specs = register(&Cfg::new(vec![
        davis("suncrest:backyard", tcp("davis.lan", 22_222)),
        campbell("suncrest:meadow", serial("/dev/ttyUSB1", 19_200)),
    ]))
    .expect("a valid plan");
    assert_eq!(specs.len(), 2);
}

#[tokio::test]
async fn a_davis_or_campbell_row_on_a_transport_it_has_no_mode_for_is_refused() {
    for row in [
        davis(
            "suncrest:backyard",
            TransportColumns {
                kind: "udp_in".into(),
                port: Some(22_222),
                ..TransportColumns::default()
            },
        ),
        campbell(
            "suncrest:meadow",
            TransportColumns {
                kind: "http_out".into(),
                url: Some("http://meadow.lan".into()),
                ..TransportColumns::default()
            },
        ),
    ] {
        let kind = row.kind.clone();
        let e = register(&Cfg::new(vec![row])).expect_err("no such mode");
        assert!(
            matches!(e, StationPlanError::WrongTransport { .. }),
            "{kind}: {e}"
        );
    }
}

/// The identity a driver resolves is checked, which is the Ambient defect from the other
/// side.
#[tokio::test]
async fn a_station_whose_identity_its_driver_does_not_resolve_is_refused() {
    let mut row = davis("suncrest:backyard", serial("/dev/ttyUSB0", 19_200));
    row.identity_kind = "from_payload".into();
    let e = register(&Cfg::new(vec![row])).expect_err("Davis is Fixed");
    let StationPlanError::IdentityMismatch {
        expected, found, ..
    } = &e
    else {
        panic!("expected an identity mismatch, got {e}");
    };
    assert_eq!(*expected, "fixed");
    assert_eq!(*found, "from_payload");
}

/// AirGradient: one request per occurrence, at the device's own cadence.
#[tokio::test]
async fn an_airgradient_monitor_is_planned_as_a_poll_at_three_seconds() {
    let specs = register(&Cfg::new(vec![airgradient(
        "suncrest:aqmon",
        "http://aqmon.lan",
    )]))
    .expect("a valid plan");
    assert_eq!(ids(&specs), vec!["station:airgradient:suncrest:aqmon"]);
    let Trigger::Every {
        period,
        deadline,
        floor,
        ..
    } = specs[0].trigger
    else {
        panic!("not an Every trigger: {:?}", specs[0].trigger);
    };
    assert_eq!(period, Duration::from_secs(3), "§4a.5's measured default");
    assert!(deadline <= period, "`plan()` enforces this");
    assert_eq!(floor, Duration::from_secs(1));
    let shape = specs[0].identity.as_ref().expect("a declared shape");
    assert_eq!(shape.continuity, Continuity::Request);
    assert_eq!(shape.problem(&specs[0].trigger), None);
}

#[tokio::test]
async fn an_airgradient_over_tls_is_refused_and_the_message_names_the_type() {
    let e = register(&Cfg::new(vec![airgradient(
        "suncrest:aqmon",
        "https://aqmon.lan",
    )]))
    .expect_err("the monitor has no TLS listener");
    let StationPlanError::NotTls { kind, .. } = &e else {
        panic!("expected a TLS refusal, got {e}");
    };
    assert_eq!(kind, "airgradient");
}

/// **The structural property.** Three Ambient consoles, one component, one route.
#[tokio::test]
async fn three_ambient_consoles_are_one_component_on_one_route() {
    let specs = register(&Cfg::new(vec![
        ambient("suncrest:roof", "roof", "/ingest/ambient"),
        ambient("suncrest:meadow", "meadow", "/ingest/ambient"),
        ambient("suncrest:orchard", "orchard", "/ingest/ambient"),
    ]))
    .expect("a valid plan");
    assert_eq!(
        ids(&specs),
        vec!["station:ambient"],
        "one component, not three"
    );

    let spec = &specs[0];
    assert!(matches!(spec.trigger, Trigger::Service));
    // The pure half of the provision, which is what makes a duplicate path a plan error
    // naming both owners (D-51).
    let routes: Vec<&mw_component::Provision> = spec
        .provides
        .iter()
        .filter(|p| p.capability == mw_component::Capability::HttpRoutes)
        .collect();
    assert_eq!(routes.len(), 1);
    let mw_component::ProvisionDetail::Routes(manifest) = &routes[0].detail else {
        panic!("a route provision with no manifest");
    };
    assert_eq!(
        manifest
            .entries
            .iter()
            .map(|e| e.pattern.as_str())
            .collect::<Vec<_>>(),
        vec!["/ingest/ambient"]
    );
    // §4a.4's invariant: `Many` because the identity is not `Fixed`.
    let shape = spec.identity.as_ref().expect("a declared shape");
    assert_eq!(shape.initiation, Initiation::Inbound);
    assert_eq!(shape.source, IdentitySource::FromQuery);
    assert_eq!(shape.cardinality, Cardinality::Many);
    assert_eq!(shape.problem(&spec.trigger), None);
}

/// One endpoint means one path.
#[tokio::test]
async fn two_ambient_mounts_are_refused_and_name_both_stations() {
    let e = register(&Cfg::new(vec![
        ambient("suncrest:roof", "roof", "/ingest/ambient"),
        ambient(
            "suncrest:meadow",
            "meadow",
            "/weatherstation/updateweatherstation.php",
        ),
    ]))
    .expect_err("two endpoints");
    let StationPlanError::TwoMounts { first, second, .. } = &e else {
        panic!("expected two mounts, got {e}");
    };
    assert_eq!(first, "suncrest:roof");
    assert_eq!(second, "suncrest:meadow");
    assert!(e.to_string().contains("ONE endpoint"), "{e}");
}

/// The credential the transport has is the only one it has.
#[tokio::test]
async fn an_ambient_station_with_no_stored_secret_is_refused() {
    let mut row = ambient("suncrest:roof", "roof", "/ingest/ambient");
    row.has_ingest_secret = false;
    let e = register(&Cfg::new(vec![row])).expect_err("nothing to verify against");
    assert!(matches!(e, StationPlanError::NoSecret { .. }), "{e}");
    assert!(e.to_string().contains("ingest_secret_hash"), "{e}");
}

#[tokio::test]
async fn an_ambient_station_needs_the_identifier_its_console_presents() {
    let mut row = ambient("suncrest:roof", "roof", "/ingest/ambient");
    row.presented_key = None;
    let e = register(&Cfg::new(vec![row])).expect_err("nothing to resolve");
    assert!(
        matches!(
            e,
            StationPlanError::Missing {
                column: "presented_key",
                ..
            }
        ),
        "{e}"
    );
    assert!(e.to_string().contains("`?ID=`"), "{e}");
}

/// Two consoles presenting one identifier would make resolution ambiguous.
#[tokio::test]
async fn two_ambient_consoles_presenting_one_identifier_are_refused() {
    let e = register(&Cfg::new(vec![
        ambient("suncrest:roof", "ROOF", "/ingest/ambient"),
        ambient("suncrest:meadow", "roof", "/ingest/ambient"),
    ]))
    .expect_err("one claim, two stations");
    assert!(matches!(e, StationPlanError::DuplicateClaim { .. }), "{e}");
}

#[tokio::test]
async fn an_ambient_station_on_a_node_with_no_listener_is_refused() {
    let e = register(&Cfg::without_listener(vec![ambient(
        "suncrest:roof",
        "roof",
        "/ingest/ambient",
    )]))
    .expect_err("no route to arrive on");
    assert!(matches!(e, StationPlanError::NoUploadState { .. }), "{e}");
}

/// The snow gauge: validated here, planned by `mw-grpc`, and **present in the receiver
/// list** -- which is what makes its `Fixed` frames resolve.
#[tokio::test]
async fn a_snow_gauge_is_validated_here_and_has_a_receiver_that_nothing_here_emits() {
    let cfg = Cfg::new(vec![snowgauge("suncrest:gauge", "http://gauge.lan:50051")]);
    assert!(
        register(&cfg).expect("a valid gauge row").is_empty(),
        "the component is `mw_grpc::snowgauge`'s"
    );
    let receivers = plan::fixed_receivers(&cfg).expect("a valid gauge row");
    assert_eq!(receivers.len(), 1);
    assert_eq!(
        receivers[0].component.as_str(),
        "station:snowgauge:suncrest:gauge"
    );
    assert_eq!(receivers[0].station.as_str(), "suncrest:gauge");
}

#[tokio::test]
async fn a_snow_gauge_that_is_not_streaming_is_refused_here() {
    let mut row = snowgauge("suncrest:gauge", "http://gauge.lan:50051");
    row.transport.as_mut().expect("a transport").streaming = Some(false);
    let e = register(&Cfg::new(vec![row])).expect_err("no gauge answers one request");
    assert!(
        matches!(
            e,
            StationPlanError::Missing {
                column: "transport_streaming",
                ..
            }
        ),
        "{e}"
    );
}

/// Every driver whose identity is `Fixed` has a receiver, and the Ambient endpoint does
/// not -- because a `FromQuery` frame resolves by its claim and not by where it arrived.
#[tokio::test]
async fn the_receiver_list_covers_every_fixed_driver_and_no_other() {
    let cfg = Cfg::new(vec![
        polled("suncrest:roof", "http://192.168.1.50"),
        broadcast("suncrest:shed", "001D0A700002", 22222, Some("192.168.1.51")),
        davis("suncrest:backyard", serial("/dev/ttyUSB0", 19_200)),
        campbell("suncrest:meadow", tcp("ser2net.lan", 3001)),
        airgradient("suncrest:aqmon", "http://aqmon.lan"),
        ambient("suncrest:orchard", "orchard", "/ingest/ambient"),
        snowgauge("suncrest:gauge", "http://gauge.lan:50051"),
    ]);
    register(&cfg).expect("a valid plan");
    let mut receivers: Vec<String> = plan::fixed_receivers(&cfg)
        .expect("a valid plan")
        .into_iter()
        .map(|r| r.component.as_str().to_string())
        .collect();
    receivers.sort();
    assert_eq!(
        receivers,
        vec![
            "station:airgradient:suncrest:aqmon",
            "station:campbell:suncrest:meadow",
            "station:davis:suncrest:backyard",
            "station:snowgauge:suncrest:gauge",
            "station:wll:suncrest:roof",
        ],
        "the broadcast listener is `from_payload`, the re-arm admits nothing, and the \
         Ambient endpoint is `from_query`"
    );
}

/// Every component `register` emits for a `Fixed` station is in the receiver list, and
/// every receiver names a component it emits -- **except** the snow gauge's, whose spec is
/// another crate's. Two functions that mint the same ids independently are two functions
/// that can disagree.
#[tokio::test]
async fn the_receiver_list_and_the_plan_agree_about_every_component_they_share() {
    let cfg = Cfg::new(vec![
        davis("suncrest:backyard", serial("/dev/ttyUSB0", 19_200)),
        campbell("suncrest:meadow", tcp("ser2net.lan", 3001)),
        airgradient("suncrest:aqmon", "http://aqmon.lan"),
        polled("suncrest:roof", "http://192.168.1.50"),
        snowgauge("suncrest:gauge", "http://gauge.lan:50051"),
    ]);
    let planned = ids(&register(&cfg).expect("a valid plan"));
    for receiver in plan::fixed_receivers(&cfg).expect("a valid plan") {
        let id = receiver.component.as_str().to_string();
        if id.starts_with("station:snowgauge:") {
            assert!(
                !planned.contains(&id),
                "`{id}` is `mw-grpc`'s to emit (crate::snowgauge)"
            );
            continue;
        }
        assert!(
            planned.contains(&id),
            "`{id}` is a receiver no planned component answers"
        );
    }
}

/// A station of a type this build serves on a transport it does not have, for each of the
/// remaining pairings -- so that "every type is dispatched" is asserted rather than
/// assumed.
#[tokio::test]
async fn every_configured_type_the_schema_permits_reaches_a_driver() {
    // The schema's own `CHECK` list for a local station.
    for kind in [
        "weatherlink-live",
        "davis",
        "campbellscientific",
        "ambient-customized",
        "airgradient",
        "snowgauge",
    ] {
        let mut row = davis("suncrest:x", serial("/dev/ttyUSB0", 19_200));
        row.kind = kind.to_string();
        let e = register(&Cfg::new(vec![row])).err();
        // Whatever each driver says about a serial transport, it is never `NoDriver`:
        // that error means this build does not recognise the type at all.
        assert!(
            !matches!(e, Some(StationPlanError::NoDriver { .. })),
            "`{kind}` reached no driver"
        );
    }
}
