//! The one route that exists, over a real socket.
//!
//! A raw HTTP/1.1 exchange rather than a client crate: the assertions are about the status
//! line and the `Content-Type` a scraper reads, and a client that normalises either would
//! be testing itself. A-4 permits test infrastructure its own crates; not needing one is
//! better than being permitted one.

use std::sync::Arc;
use std::time::Duration;

use mw_http::{bind, MetricsRoute};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

async fn request(addr: std::net::SocketAddr, path: &str) -> String {
    let mut socket = TcpStream::connect(addr)
        .await
        .expect("the listener accepts");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
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

#[tokio::test]
async fn metrics_is_served_on_the_bound_port_and_everything_else_is_a_404() {
    let payload = "mw_readings_ingested_total{station=\"backyard\"} 7\n";
    let route = MetricsRoute::new(Arc::new(move || payload.to_string()), CONTENT_TYPE);
    // Port 0: the kernel chooses, so two tests on one machine cannot collide.
    let bound = bind("127.0.0.1:0".parse().expect("a literal address"), route)
        .await
        .expect("the port is bindable");
    let addr = bound.local_addr();
    assert_ne!(addr.port(), 0, "the bound port is read back, not echoed");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        let metrics = request(addr, "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200 OK"), "{metrics}");
        assert!(
            metrics.contains(&format!("content-type: {CONTENT_TYPE}")),
            "a scraper reads the content type: {metrics}"
        );
        assert!(metrics.ends_with(payload), "{metrics}");

        let missing = request(addr, "/nothing-here").await;
        assert!(
            missing.starts_with("HTTP/1.1 404 Not Found"),
            "one route table, one fallback: {missing}"
        );

        stop.send(()).expect("the server is still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("the listener drains rather than erroring on shutdown");
}

#[tokio::test]
async fn the_payload_is_rendered_per_request_rather_than_once() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let route = MetricsRoute::new(
        Arc::new(move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            format!("mw_render_calls_total {n}\n")
        }),
        CONTENT_TYPE,
    );
    let bound = bind("127.0.0.1:0".parse().expect("a literal address"), route)
        .await
        .expect("bindable");
    let addr = bound.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let client = async move {
        assert!(request(addr, "/metrics").await.ends_with("1\n"));
        assert!(
            request(addr, "/metrics").await.ends_with("2\n"),
            "it costs nothing when nobody scrapes, which means it renders on demand"
        );
        stop.send(()).expect("still listening");
    };
    let (served, ()) = tokio::join!(server, client);
    served.expect("clean shutdown");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_port_already_held_is_reported_with_the_address() {
    let first = bind(
        "127.0.0.1:0".parse().expect("a literal address"),
        MetricsRoute::new(Arc::new(String::new), CONTENT_TYPE),
    )
    .await
    .expect("bindable");
    let addr = first.local_addr();
    let again = bind(addr, MetricsRoute::new(Arc::new(String::new), CONTENT_TYPE)).await;
    let error = again.expect_err("the port is held");
    assert!(
        error.to_string().contains(&addr.to_string()),
        "the message names the address that could not be bound: {error}"
    );
}

#[tokio::test]
async fn a_client_that_never_finishes_its_request_cannot_hold_the_drain_open() {
    // `with_graceful_shutdown` on its own waits for every connection with an unfinished
    // request, with no bound -- so a request line with no blank line after it kept the
    // process alive for as long as the client cared to. `/metrics` is on the
    // unauthenticated main port, which is what made that a remote problem rather than a
    // developer's.
    let route = MetricsRoute::new(Arc::new(String::new), CONTENT_TYPE);
    let bound = bind("127.0.0.1:0".parse().expect("a literal address"), route)
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
