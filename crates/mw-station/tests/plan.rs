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
}

impl Cfg {
    fn new(rows: Vec<StationRow>) -> Self {
        Self {
            rows,
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        }
    }

    /// A node that serves and does not ingest.
    fn without_ingress(rows: Vec<StationRow>) -> Self {
        Self {
            rows,
            ingress: None,
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
    };
    assert!(register(&Cfg::new(vec![received]))
        .expect("valid")
        .is_empty());
}

/// A station type whose driver has not landed is an error that names the work item, not a
/// silence.
#[tokio::test]
async fn a_station_type_with_no_driver_is_refused_by_name() {
    let mut row = polled("backyard", "http://192.168.1.9");
    row.kind = "davis".into();
    let e = register(&Cfg::new(vec![row])).expect_err("no Davis driver yet");
    assert!(matches!(e, StationPlanError::NoDriver { .. }), "{e}");
    let text = e.to_string();
    assert!(text.contains("`backyard`"), "{text}");
    assert!(text.contains("M6-1"), "{text}");
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
    row.kind = "davis".into();
    let cfg = Cfg::new(vec![row]);
    // `register` refuses the row outright...
    assert!(matches!(
        register(&cfg).expect_err("no Davis driver yet"),
        StationPlanError::NoDriver { .. }
    ));
    // ... so nothing may claim a `station:wll:backyard` receiver for it. Without the guard
    // this returned one, and was safe only because `boot.rs` happens to call `register`
    // first.
    assert!(
        plan::fixed_receivers(&cfg).expect("no error").is_empty(),
        "a station with no driver has no receiver"
    );
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
