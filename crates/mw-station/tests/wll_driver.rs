//! The WeatherLink Live driver against recorded transport output.
//!
//! **Level 2 of W3 D-82's three for the driver this item ships**, which has no `Session`.
//! `tests/transcript.rs` holds the harness and the contract (and a session seen failing);
//! this file is the level applied to WeatherLink Live. For a driver that has no `Session`:
//! transports deliver one frame per read, so what a transcript means here is *a recorded
//! sequence of datagrams and of response bodies*, and the assertion that matters is the
//! same one — **every** frame in the sequence produces an admission, not only the first.
//! That is the Davis "stops polling after one batch" defect stated for an inbound driver,
//! and it is the one property a codec golden test structurally cannot see.
//!
//! The broadcast listener is additionally driven over a **real loopback socket**, because
//! the receive loop, the cancellation path and the bind are the parts of this driver that
//! no amount of calling `on_datagram` exercises.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_codec::{Frame, SensorMapping, WllConditions};
use mw_component::{ComponentId, Cx, Generation, Ingress, Reject, Serve};
use mw_station::testing::RecordingIngress;
use mw_station::wll::{BroadcastListener, BroadcastUnit, Poll};
use mw_types::StationKey;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

const MAPPING: &str = "th:1, rain:1, wind:1, th_indoor, baro";

/// `did` of the unit every fixture below is from.
const SERIAL: &str = "001D0A700002";

fn codec() -> WllConditions {
    WllConditions::new(SensorMapping::parse(MAPPING).expect("a literal mapping"))
}

fn unit(serial: &str, station: &str) -> BroadcastUnit {
    BroadcastUnit {
        serial: serial.to_string(),
        station: StationKey::new(station),
        codec: codec(),
    }
}

fn id(text: &str) -> ComponentId {
    ComponentId::new(text).expect("a literal component id")
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("a literal instant")
}

/// A datagram in the shape a WeatherLink Live broadcasts: one ISS record, wind and rain.
fn datagram(serial: &str, ts: i64, wind: f64) -> Vec<u8> {
    format!(
        r#"{{"did":"{serial}","ts":{ts},"conditions":[{{"lsid":48308,
           "data_structure_type":1,"txid":1,"wind_speed_last":{wind},"wind_dir_last":165,
           "rain_size":2,"rain_rate_last":0,"rainfall_daily":63}}]}}"#
    )
    .into_bytes()
}

/// A `current_conditions` body, which is the same document wrapped in `data`.
fn body(serial: &str, ts: i64, temp: f64) -> Frame {
    Frame::new(
        format!(
            r#"{{"data":{{"did":"{serial}","ts":{ts},"conditions":[{{"lsid":48308,
               "data_structure_type":1,"txid":1,"temp":{temp},"hum":50.0}}]}},"error":null}}"#
        )
        .into_bytes(),
    )
}

fn cx(component: &ComponentId) -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    (
        Cx::new(component.clone(), token.clone(), Generation::ZERO, rx),
        token,
    )
}

/// **The assertion this whole file exists for.** Three datagrams, three admissions.
#[tokio::test]
async fn every_datagram_is_admitted_and_not_only_the_first() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:udp:22222");
    let listener = BroadcastListener::new(
        22222,
        None,
        [unit(SERIAL, "suncrest:roof")],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    for (n, wind) in [(1i64, 2.0), (2, 3.5), (3, 0.0)] {
        listener
            .on_datagram(
                &component,
                &datagram(SERIAL, 1_762_200_000 + n, wind),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
                at(1_762_200_000 + n),
            )
            .await;
    }
    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 3, "a driver that stops after one batch");
    assert_eq!(ingress.readings(), 3, "one observation per datagram");
    for (admission, wind) in admissions.iter().zip([2.0f32, 3.5, 0.0]) {
        assert_eq!(admission.batch.len(), 1);
        assert_eq!(admission.batch[0].values.wind_speed, Some(wind));
        assert_eq!(
            admission.presented_key.as_deref(),
            Some(SERIAL),
            "the claim is the serial, verbatim, for the registry to resolve"
        );
        assert_eq!(admission.receiver, "station:wll:udp:22222");
    }
}

/// The same property for the poll, which is the transport the Davis defect was actually on.
#[tokio::test]
async fn every_poll_response_is_admitted_and_not_only_the_first() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:suncrest:roof");
    let poll = Poll::new(
        mw_station::Endpoint::http("192.168.1.50"),
        mw_station::HttpMethod::Get,
        None,
        StationKey::new("suncrest:roof"),
        codec(),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    for (n, temp) in [(1i64, 60.0), (2, 61.5), (3, 62.0)] {
        poll.on_body(
            &component,
            &body(SERIAL, 1_762_200_000 + n, temp),
            at(1_762_200_000 + n),
        )
        .await
        .expect("each occurrence succeeds");
    }
    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 3);
    for (admission, temp) in admissions.iter().zip([60.0f32, 61.5, 62.0]) {
        assert_eq!(admission.batch[0].values.out_temp, Some(temp));
        assert_eq!(
            admission.presented_key, None,
            "a Fixed identity presents nothing; the registry resolves by receiver"
        );
    }
}

/// W2a §4a.6 for `UdpIn`: an unknown serial is dropped, counted, and never registered.
#[tokio::test]
async fn a_datagram_from_an_unconfigured_unit_is_dropped() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:udp:22222");
    let listener = BroadcastListener::new(
        22222,
        None,
        [unit(SERIAL, "suncrest:roof")],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    // A neighbour's unit on the same LAN, broadcasting to the same port.
    listener
        .on_datagram(
            &component,
            &datagram("001D0A7FFFFF", 1, 1.0),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99)),
            at(1),
        )
        .await;
    // A datagram that is not a WeatherLink Live document at all.
    listener
        .on_datagram(
            &component,
            b"<html>not a weather station</html>",
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99)),
            at(2),
        )
        .await;
    // A document with no serial in it, which cannot be attributed to any unit.
    listener
        .on_datagram(
            &component,
            br#"{"ts":3,"conditions":[{"data_structure_type":1,"txid":1,"temp":1.0}]}"#,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99)),
            at(3),
        )
        .await;
    assert!(
        ingress.admissions().is_empty(),
        "nothing reached admission, so nothing could be stored"
    );
}

/// The serial comparison is case-insensitive, which is the difference between a station
/// that works and one that silently never resolves.
#[tokio::test]
async fn a_serial_resolves_whatever_case_the_operator_typed() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:udp:22222");
    let listener = BroadcastListener::new(
        22222,
        None,
        // The column holds lower case; the unit broadcasts upper.
        [unit("001d0a700002", "suncrest:roof")],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    listener
        .on_datagram(
            &component,
            &datagram("001D0A700002", 1, 4.0),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            at(1),
        )
        .await;
    assert_eq!(ingress.admissions().len(), 1);
}

/// Cardinality `Many`: one endpoint, two units, each decoded with its own sensor mapping.
#[tokio::test]
async fn two_units_share_one_endpoint_and_keep_their_own_mappings() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:udp:22222");
    let roof = BroadcastUnit {
        serial: "001D0A700002".into(),
        station: StationKey::new("suncrest:roof"),
        // Wind on transmitter 1.
        codec: WllConditions::new(SensorMapping::parse("wind:1").expect("parses")),
    };
    let field = BroadcastUnit {
        serial: "001D0A700003".into(),
        station: StationKey::new("suncrest:field"),
        // Wind on transmitter 2 -- the same payload shape, a different deployment.
        codec: WllConditions::new(SensorMapping::parse("wind:2").expect("parses")),
    };
    let listener = BroadcastListener::new(
        22222,
        None,
        [roof, field],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    assert_eq!(listener.units(), 2);
    for serial in ["001D0A700002", "001D0A700003"] {
        listener
            .on_datagram(
                &component,
                &datagram(serial, 1, 7.0),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                at(1),
            )
            .await;
    }
    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 2);
    // The roof's mapping reads transmitter 1 and finds the wind; the field's reads
    // transmitter 2 and finds nothing, so its reading is empty and admission is what
    // refuses it. The two mappings were applied to the same bytes and disagreed, which is
    // the property "one component, N stations, N mappings" has to have.
    assert_eq!(admissions[0].batch[0].values.wind_speed, Some(7.0));
    assert_eq!(admissions[1].batch[0].values.wind_speed, None);
}

/// A refused reading is a counted signal, not a failed occurrence (§4.4).
#[tokio::test]
async fn an_unknown_station_does_not_fail_the_poll_occurrence() {
    let ingress = RecordingIngress::refusing(Reject::UnknownStation);
    let poll = Poll::new(
        mw_station::Endpoint::http("192.168.1.50"),
        mw_station::HttpMethod::Get,
        None,
        StationKey::new("suncrest:roof"),
        codec(),
        ingress as Arc<dyn Ingress>,
    );
    poll.on_body(
        &id("station:wll:suncrest:roof"),
        &body(SERIAL, 1, 60.0),
        at(1),
    )
    .await
    .expect("an Ignorable refusal is not a failed occurrence");
}

/// A body that is not a WeatherLink Live document **is** a failed occurrence, so that
/// `mw_component_ticks_total{outcome="failed"}` shows it.
#[tokio::test]
async fn an_undecodable_body_fails_the_poll_occurrence() {
    let ingress = RecordingIngress::accepting();
    let poll = Poll::new(
        mw_station::Endpoint::http("192.168.1.50"),
        mw_station::HttpMethod::Get,
        None,
        StationKey::new("suncrest:roof"),
        codec(),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    let e = poll
        .on_body(
            &id("station:wll:suncrest:roof"),
            &Frame::new(b"<html>503</html>".to_vec()),
            at(1),
        )
        .await
        .expect_err("a body that is not the API's");
    assert!(
        e.to_string()
            .contains("decoding a WeatherLink Live response"),
        "{e}"
    );
    assert!(ingress.admissions().is_empty());
}

/// The receive loop, the bind and the cancellation path, over a real socket.
#[tokio::test]
async fn the_listener_receives_from_a_real_socket_and_stops_when_cancelled() {
    let ingress = RecordingIngress::accepting();
    let component = id("station:wll:udp:22222");
    let (context, token) = cx(&component);

    // Port 0, so the test does not need a fixed port to be free. `bind` is separate from
    // the loop exactly so that this is possible; no configuration reaches `serve_on` with
    // port 0, because no device broadcasts to one.
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("an ephemeral port is available");
    let port = socket.local_addr().expect("a bound socket").port();

    let listener = Box::new(BroadcastListener::new(
        port,
        None,
        [unit(SERIAL, "suncrest:roof")],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    // `spawn` is forbidden in the daemon (`clippy.toml`: everything goes through
    // `mw_supervisor::task`, §3.2) and this test is the supervisor: a `Serve` body runs
    // until it is cancelled, so driving it and sending to it at the same time is the only
    // way to exercise the receive loop at all. The same exemption `mw-pipeline`'s fan-out
    // test takes, for the same reason.
    #[allow(clippy::disallowed_methods)]
    let serving = tokio::task::spawn(listener.serve_on(socket, context));

    let sender = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("a sending socket");
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    for n in 1..=3i64 {
        sender
            .send_to(&datagram(SERIAL, 1_762_200_000 + n, n as f64), target)
            .await
            .expect("the datagram is sent");
    }

    // Wait for all three rather than sleeping a fixed amount: a timing-dependent assertion
    // in CI is a flake, and the thing being asserted is "all three arrived" and not "they
    // arrived within N milliseconds".
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        while ingress.admissions().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "only {} arrived",
        ingress.admissions().len()
    );

    token.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("the listener returns on cancellation")
        .expect("the task did not panic");
    assert!(
        outcome.is_ok(),
        "cancellation is not a failure: {outcome:?}"
    );
}

/// A `Serve` body that cannot bind reports it as a resource failure, which is what puts the
/// component in `Backoff` rather than killing the process.
#[tokio::test]
async fn a_port_already_held_is_a_resource_failure() {
    let held = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .await
        .expect("an ephemeral port");
    let port = held.local_addr().expect("a bound socket").port();
    let component = id("station:wll:udp:1");
    let (context, _token) = cx(&component);
    let listener = Box::new(BroadcastListener::new(
        port,
        None,
        [unit(SERIAL, "suncrest:roof")],
        RecordingIngress::accepting() as Arc<dyn Ingress>,
    ));
    let e = listener
        .serve(context)
        .await
        .expect_err("the port is already held");
    // The chain, not the top line: §4.3's convention is that `Display` on an error prints
    // one level only, and the level that names the port is the `BuildError` underneath.
    let chain = mw_types::DisplayChain(&e).to_string();
    assert!(
        chain.contains("could not be acquired") && chain.contains("broadcast port"),
        "a resource failure, not a panic: {chain}"
    );
}
