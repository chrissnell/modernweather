//! The listener over real sockets: `/metrics`, the mounted tables, the one 404, the gRPC
//! socket and the bounded drain.
//!
//! A raw HTTP/1.1 exchange rather than a client crate: the assertions are about the status
//! line, the `Content-Type` a scraper reads and the exact bytes of a 404 body, and a client
//! that normalised any of them would be testing itself. A-4 permits test infrastructure its
//! own crates; not needing one is better than being permitted one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use mw_component::{ComponentId, Methods, RouteEntry, RouteManifest, Scope};
use mw_http::{bind, metrics_table, HostTable, Listen, MetricsRoute, Mount, RouteTable};
use mw_types::WebsiteId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

async fn request(addr: SocketAddr, host: &str, path: &str) -> String {
    let mut socket = TcpStream::connect(addr)
        .await
        .expect("the listener accepts");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the request is written");
    let mut response = String::new();
    socket
        .read_to_string(&mut response)
        .await
        .expect("the response is read to the close");
    response
}

fn any_port() -> Listen {
    Listen {
        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        http_port: 0,
        https_port: None,
        grpc_port: 0,
    }
}

fn id(name: &str) -> ComponentId {
    ComponentId::new(name).expect("a legal component id")
}

fn metrics(payload: impl Fn() -> String + Send + Sync + 'static) -> RouteTable {
    metrics_table(MetricsRoute::new(Arc::new(payload), CONTENT_TYPE))
}

/// A table shaped like the frozen surface's: `Scope::Any`, one route, no fallback of its
/// own.
fn frozen_like() -> RouteTable {
    RouteTable::new(
        id("api:legacy"),
        RouteManifest::new(Scope::Any, vec![RouteEntry::exact("/latest", Methods::ANY)]),
        Router::new().route(
            "/latest",
            axum::routing::any(|| async { "the frozen surface\n" }),
        ),
    )
}

fn mount(tables: Vec<RouteTable>) -> Mount {
    Mount::new(
        tables,
        HostTable::new([("localhost".to_string(), WebsiteId::new(1))]),
    )
    .expect("the tables mount")
    .0
}

#[tokio::test]
async fn metrics_and_a_mounted_table_share_the_one_listener() {
    let payload = "mw_readings_ingested_total{station=\"backyard\"} 7\n";
    let mount = mount(vec![metrics(move || payload.to_string()), frozen_like()]);
    // Port 0: the kernel chooses, so two tests on one machine cannot collide.
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("the ports are bindable");
    let addr = bound.local_addr();
    assert_ne!(addr.port(), 0, "the bound port is read back, not echoed");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        let m = request(addr, "localhost", "/metrics").await;
        assert!(m.starts_with("HTTP/1.1 200 OK"), "{m}");
        assert!(
            m.contains(&format!("content-type: {CONTENT_TYPE}")),
            "a scraper reads the content type: {m}"
        );
        assert!(m.ends_with(payload), "{m}");

        let latest = request(addr, "localhost", "/latest").await;
        assert!(latest.starts_with("HTTP/1.1 200 OK"), "{latest}");
        assert!(latest.ends_with("the frozen surface\n"), "{latest}");

        stop.send(()).expect("the server is still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("the listener drains rather than erroring on shutdown");
}

/// The exact bytes `asset-not-found` and `html-portal-on-non-portal` assert, over a real
/// socket. Both are **DEV-3**'s answer as well: a directory under an asset tree gets this
/// rather than an HTML index.
#[tokio::test]
async fn an_unrouted_path_is_gos_404_byte_for_byte() {
    let mount = mount(vec![metrics(String::new)]);
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("bindable");
    let addr = bound.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        for path in ["/nonexistent-asset", "/js/", "/portal"] {
            let r = request(addr, "localhost", path).await;
            assert!(r.starts_with("HTTP/1.1 404 Not Found"), "{path}: {r}");
            assert!(
                r.contains("content-type: text/plain; charset=utf-8"),
                "{path}: {r}"
            );
            assert!(r.ends_with("404 page not found\n"), "{path}: {r}");
            assert!(
                r.contains("vary: Accept-Encoding"),
                "every one of the 70 fixtures asserts it: {path}: {r}"
            );
        }
        stop.send(()).expect("still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("clean shutdown");
}

/// **D-92**: the legacy gRPC port is its own socket and serves the same router, so no
/// peer's endpoint changes at upgrade and there is still exactly one route table.
#[tokio::test]
async fn the_grpc_socket_serves_the_same_router() {
    let mount = mount(vec![metrics(|| "mw_up 1\n".to_string()), frozen_like()]);
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("bindable");
    let main = bound.local_addr();
    let grpc = bound.grpc_addr();
    assert_ne!(main.port(), grpc.port(), "two sockets, not one");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        let on_grpc = request(grpc, "localhost", "/latest").await;
        assert!(on_grpc.starts_with("HTTP/1.1 200 OK"), "{on_grpc}");
        assert!(on_grpc.ends_with("the frozen surface\n"), "{on_grpc}");
        stop.send(()).expect("still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("clean shutdown");
}

/// One signal, N sockets: cancelling once closes every one of them (§6.1's one shutdown
/// path).
#[tokio::test]
async fn one_signal_stops_every_socket() {
    let mount = mount(vec![metrics(String::new)]);
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("bindable");
    let main = bound.local_addr();
    let grpc = bound.grpc_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        stop.send(()).expect("still listening");
    };
    let (served, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, client)
    })
    .await
    .expect("one cancellation closes both sockets well inside the deadline");
    served.expect("clean shutdown");
    // Both ports are free again, which is what "closed" means from outside the process.
    for addr in [main, grpc] {
        assert!(
            tokio::net::TcpListener::bind(addr).await.is_ok(),
            "{addr} is still held after shutdown"
        );
    }
}

#[tokio::test]
async fn the_payload_is_rendered_per_request_rather_than_once() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let mount = mount(vec![metrics(move || {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        format!("mw_render_calls_total {n}\n")
    })]);
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("bindable");
    let addr = bound.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        assert!(request(addr, "localhost", "/metrics")
            .await
            .ends_with("1\n"));
        assert!(
            request(addr, "localhost", "/metrics")
                .await
                .ends_with("2\n"),
            "it costs nothing when nobody scrapes, which means it renders on demand"
        );
        stop.send(()).expect("still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("clean shutdown");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_client_that_never_finishes_its_request_cannot_hold_the_drain_open() {
    // `with_graceful_shutdown` on its own waits for every connection with an unfinished
    // request, with no bound -- so a request line with no blank line after it kept the
    // process alive for as long as the client cared to. `/metrics` is on the
    // unauthenticated main port, which is what made that a remote problem rather than a
    // developer's.
    let mount = mount(vec![metrics(String::new)]);
    let bound = bind(&any_port(), mount.router(), None)
        .await
        .expect("bindable");
    let addr = bound.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    // A short deadline so the test asserts the deadline exists rather than its value.
    let drain = Duration::from_millis(200);
    let server = bound.serve_with_drain(
        async move {
            let _ = stopped.await;
        },
        drain,
    );
    let client = async move {
        let mut half_open = TcpStream::connect(addr)
            .await
            .expect("the listener accepts");
        half_open
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .expect("the partial request is written");
        // No blank line: the request is never complete.
        stop.send(()).expect("still listening");
        // Held open well past the drain deadline, which is the whole point: the deadline
        // has to be ours, not the client's.
        tokio::time::sleep(drain * 5).await;
        half_open
    };
    let (served, _held_open) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, client)
    })
    .await
    .expect("the listener abandons the drain instead of waiting for the client");
    served.expect("an abandoned drain is a warning, not an error");
}
