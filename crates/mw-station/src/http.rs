//! The outbound HTTP request one `Trigger::Every` occurrence makes.
//!
//! **Why this is written out rather than taken from a client crate.** The obvious answer is
//! `hyper::client::conn::http1`, and it was tried; the reason it is not here is
//! `clippy.toml`'s disallowed-methods rule, which forbids `tokio::spawn` with "spawn
//! through `mw_supervisor::task`, so the task table owns the handle and a leak is
//! unexpressible (§3.2)". `hyper`'s client splits a connection into a sender and a
//! connection future that **something else has to drive**, which on this path means exactly
//! the detached task that rule exists to prevent — and `mw-station` may not name
//! `mw_supervisor` at all, because both crates are in the `service` layer and the
//! supervisor must not know that a station exists. So the sanctioned mechanism is
//! unreachable from here and the forbidden one is the only way to use that API. The tension
//! is real and is recorded in this item's PR; the first component that genuinely needs an
//! internal task is what should resolve it.
//!
//! What is left to write is small, and smaller than the gap it closes: one `GET` with no
//! body, against an appliance on the LAN. The request builder and the response-head parse
//! below are **pure functions over byte strings**, which is the same trade `crate::mdns`
//! and `crate::endpoint` make — no crate in **D-32**'s table does the job, and what is
//! needed is a subset a golden test can pin.
//!
//! All three response framings RFC 9112 §6 permits are implemented, because a client that
//! only understood `Content-Length` would work until a firmware update: `Content-Length`,
//! chunked transfer, and read-to-close as the fallback §6.3 leaves for a response with
//! neither.
//!
//! **This crate may open an outbound connection and may not accept one.**
//! `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule names the two accepting socket
//! types and the three server constructors; a `TcpStream::connect` is none of them, and
//! §1.4's monopoly is over the socket a request *arrives* on. That asymmetry is the
//! decision, which is also why `UdpSocket::bind` is deliberately absent from the same rule.
//!
//! (The rule is a whitespace-squeezed substring match over whole files and does not know
//! what a comment is, which is why the types it forbids are described here rather than
//! named -- `ci/layers.toml` records the same hazard about its own stdout rule.)

use std::time::Duration;

use mw_codec::Frame;
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::endpoint::Endpoint;
use crate::resolve::{self, ResolveError};
use crate::transport::HttpMethod;

/// The largest response body this will read.
///
/// A `current_conditions` body with a leaf/soil station is under 8 KiB. The cap exists
/// because the peer is an appliance on a LAN and a bounded read is the difference between a
/// misbehaving device and an out-of-memory daemon.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The largest response head this will read. Generous for a device that sends six headers,
/// and small enough that a peer sending headers forever is a failure rather than a leak.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Why one request did not produce a body.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The host did not become an address.
    #[error(transparent)]
    Resolve(#[from] ResolveError),

    /// The connection could not be made, or failed mid-request.
    #[error("{what} {url}")]
    Transport {
        /// What was being attempted, in words that name the action (§5.9).
        what: &'static str,
        /// Where.
        url: String,
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// The device answered with something that is not an HTTP response.
    #[error("{url} did not answer with HTTP: {why}")]
    Malformed {
        /// Where.
        url: String,
        /// Which rule was broken.
        why: &'static str,
    },

    /// The device answered with a status that is not success.
    #[error("{url} answered {status}")]
    Status {
        /// Where.
        url: String,
        /// What it answered.
        status: u16,
    },

    /// The device answered with more than [`MAX_BODY_BYTES`].
    #[error("{url} answered with more than {} bytes", MAX_BODY_BYTES)]
    TooLarge {
        /// Where.
        url: String,
    },

    /// The request did not finish inside its window.
    #[error("{url} did not answer within {}ms", timeout.as_millis())]
    Timeout {
        /// Where.
        url: String,
        /// The window.
        timeout: Duration,
    },

    /// A TLS station transport, which nothing builds yet.
    ///
    /// Unreachable from the one driver that exists: a WeatherLink Live's local API is plain
    /// HTTP and `crate::plan` refuses an `https` address for one at plan time, with that
    /// sentence. It is a variant rather than a `todo!()` because the first TLS station
    /// transport — §4a.7's vendor cloud poll (M6-1) — needs a place to add the handshake,
    /// and a panic in its place would be a daemon that exits on a configuration value.
    #[error("{url} is HTTPS, and no station transport speaks TLS yet")]
    Tls {
        /// Where.
        url: String,
    },
}

impl mw_types::Classify for HttpError {
    /// Every variant is retryable, including [`HttpError::Status`].
    ///
    /// A `404` from a device looks permanent and is not: a WeatherLink Live answers `404`
    /// on `/v1/real_time` while its firmware is updating, and `409` when another client
    /// holds the broadcast. The next occurrence is the retry (W2a Decision 29), and a
    /// `Fatal` here would be a station that stays dead until the daemon is restarted.
    /// [`HttpError::Tls`] is the one that cannot clear on its own, and it is still
    /// retryable rather than fatal because the thing that clears it is a configuration
    /// change, which restarts the component anyway.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::RETRYABLE
    }
}

/// Perform one request and return its body as a [`Frame`].
///
/// `path` is the driver's — `/v1/current_conditions` — because the request paths of a
/// device's API are the driver's knowledge and `endpoint` is the device
/// (`crate::endpoint`).
pub async fn request(
    endpoint: &Endpoint,
    method: HttpMethod,
    path: &str,
    auth: Option<&SecretString>,
    timeout: Duration,
) -> Result<Frame, HttpError> {
    let url = format!("{endpoint}{path}");
    if endpoint.is_tls() {
        return Err(HttpError::Tls { url });
    }
    match tokio::time::timeout(timeout, exchange(endpoint, method, path, auth, &url)).await {
        Ok(result) => result,
        Err(_) => Err(HttpError::Timeout { url, timeout }),
    }
}

async fn exchange(
    endpoint: &Endpoint,
    method: HttpMethod,
    path: &str,
    auth: Option<&SecretString>,
    url: &str,
) -> Result<Frame, HttpError> {
    let addr = resolve::socket_addr(
        endpoint.host(),
        endpoint.port(),
        crate::mdns::DEFAULT_TIMEOUT,
    )
    .await?;
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| transport("could not connect to", url, e))?;
    // Nagle off: the request is one small write and the response is one small read, so
    // waiting for a second segment to coalesce with adds latency to every occurrence.
    let _ = stream.set_nodelay(true);

    stream
        .write_all(&head(method, path, &endpoint.authority(), auth))
        .await
        .map_err(|e| transport("could not send a request to", url, e))?;
    stream
        .flush()
        .await
        .map_err(|e| transport("could not send a request to", url, e))?;

    read_response(&mut stream, url).await
}

/// The request bytes. Pure, so the request line and the header block are golden-testable.
fn head(method: HttpMethod, path: &str, authority: &str, auth: Option<&SecretString>) -> Vec<u8> {
    let mut out = String::with_capacity(128);
    out.push_str(method.as_str());
    out.push(' ');
    out.push_str(path);
    out.push_str(" HTTP/1.1\r\nHost: ");
    // HTTP/1.1 requires it, and a WeatherLink Live's embedded server is one of the servers
    // that answers 400 without it.
    out.push_str(authority);
    // One request per occurrence, so the connection closes afterwards either way. Saying so
    // lets the device release it immediately instead of holding a keep-alive slot, which
    // matters on an appliance with a handful of them.
    out.push_str("\r\nConnection: close\r\nAccept: application/json\r\n");
    if let Some(secret) = auth {
        // §4a.7's vendor cloud poll. The value comes from `transport_auth`, which is a
        // `SecretString` from the moment it is read (**D-33**) -- so this is the one place
        // it is exposed, and it is exposed into a header rather than into a log line or a
        // URL.
        out.push_str("Authorization: ");
        out.push_str(secret.expose_secret());
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// The parts of a response head this client acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    status: u16,
    /// How the body is framed.
    framing: Framing,
    /// Where the body starts in the buffer the head was parsed from.
    body_at: usize,
}

/// How a response says where its body ends (RFC 9112 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// `Content-Length`.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Neither, so the body ends when the connection does (§6.3, the last case).
    UntilClose,
}

/// Parse a response head. `None` if the terminating blank line has not arrived yet.
///
/// Pure, and deliberately narrow: it reads a status and a body framing. The one thing it is
/// strict about is that `Transfer-Encoding: chunked` wins over `Content-Length`, which is
/// RFC 9112 §6.1's rule and the one a lenient parser gets wrong in the direction that
/// desynchronises a connection.
fn parse_head(buf: &[u8], url: &str) -> Result<Option<Head>, HttpError> {
    let malformed = |why: &'static str| HttpError::Malformed {
        url: url.to_string(),
        why,
    };
    let Some(end) = find(buf, b"\r\n\r\n") else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| malformed("the head is not text"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split(' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(malformed("the status line names no HTTP/1.x version"));
    }
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| malformed("the status line carries no status code"))?;

    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(malformed("a header line has no colon"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            // A `Content-Length` that is not a number is a framing this client will not
            // guess at, because guessing desynchronises the stream.
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| malformed("Content-Length is not a length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "chunked")
        {
            chunked = true;
        }
    }
    let framing = if chunked {
        // §6.1: when both are present, `Transfer-Encoding` wins and `Content-Length` is
        // ignored.
        Framing::Chunked
    } else {
        match length {
            Some(n) => Framing::Length(n),
            None => Framing::UntilClose,
        }
    };
    Ok(Some(Head {
        status,
        framing,
        body_at: end + 4,
    }))
}

/// Read one response off the stream.
async fn read_response<S>(stream: &mut S, url: &str) -> Result<Frame, HttpError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head = loop {
        if let Some(head) = parse_head(&buf, url)? {
            break head;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HttpError::Malformed {
                url: url.to_string(),
                why: "the response head is longer than this client will read",
            });
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| transport("could not read a response from", url, e))?;
        if n == 0 {
            return Err(HttpError::Malformed {
                url: url.to_string(),
                why: "the connection closed before the response head was complete",
            });
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    // Checked before the body is read, so a device answering 500 with a megabyte of HTML
    // costs one read rather than a megabyte of them.
    if !(200..300).contains(&head.status) {
        return Err(HttpError::Status {
            url: url.to_string(),
            status: head.status,
        });
    }

    let mut body = buf.split_off(head.body_at);
    loop {
        if body.len() > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                url: url.to_string(),
            });
        }
        if let Some(decoded) = complete(head.framing, &body, url)? {
            return Ok(Frame::new(decoded));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| transport("could not read a response body from", url, e))?;
        if n == 0 {
            return match head.framing {
                // The only framing for which end-of-stream *is* the end of the body.
                Framing::UntilClose => Ok(Frame::new(body)),
                _ => Err(HttpError::Malformed {
                    url: url.to_string(),
                    why: "the connection closed in the middle of the response body",
                }),
            };
        }
        body.extend_from_slice(&chunk[..n]);
    }
}

/// Whether the body is complete, and what it decodes to. Pure.
fn complete(framing: Framing, body: &[u8], url: &str) -> Result<Option<Vec<u8>>, HttpError> {
    match framing {
        Framing::Length(n) if body.len() >= n => Ok(Some(body[..n].to_vec())),
        Framing::Length(_) => Ok(None),
        Framing::Chunked => dechunk(body).map_err(|why| HttpError::Malformed {
            url: url.to_string(),
            why,
        }),
        // Only the close ends it, so there is nothing to decide here.
        Framing::UntilClose => Ok(None),
    }
}

/// Decode a chunked body. `Ok(None)` while the terminating zero-length chunk has not
/// arrived. Pure.
fn dechunk(body: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    let mut out = Vec::with_capacity(body.len());
    let mut at = 0usize;
    loop {
        let Some(eol) = find(&body[at..], b"\r\n") else {
            return Ok(None);
        };
        let line = &body[at..at + eol];
        // A chunk extension is permitted and ignored: `1f;name=value`.
        let size_text = line.split(|b| *b == b';').next().unwrap_or_default();
        let size_text = std::str::from_utf8(size_text).map_err(|_| "a chunk size is not text")?;
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|_| "a chunk size is not hexadecimal")?;
        at += eol + 2;
        if size == 0 {
            // The trailer section, then the final blank line. Trailers are not read: no
            // device's API puts anything in one, and the body is already complete.
            return Ok(Some(out));
        }
        if body.len() < at + size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&body[at..at + size]);
        if &body[at + size..at + size + 2] != b"\r\n" {
            return Err("a chunk is not followed by a line ending");
        }
        at += size + 2;
    }
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A transport failure, naming the action and keeping the cause.
fn transport(what: &'static str, url: &str, source: std::io::Error) -> HttpError {
    HttpError::Transport {
        what,
        url: url.to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    const URL: &str = "http://wll.lan/v1/current_conditions";

    #[test]
    fn the_request_is_the_bytes_a_device_expects() {
        let bytes = head(
            HttpMethod::Get,
            "/v1/current_conditions",
            "192.168.1.50",
            None,
        );
        assert_eq!(
            String::from_utf8(bytes).expect("ASCII"),
            "GET /v1/current_conditions HTTP/1.1\r\n\
             Host: 192.168.1.50\r\n\
             Connection: close\r\n\
             Accept: application/json\r\n\r\n"
        );
    }

    #[test]
    fn a_credential_goes_into_a_header_and_into_no_other_field() {
        let secret = SecretString::from("Bearer hunter2".to_string());
        let bytes = head(
            HttpMethod::Post,
            "/v1/x",
            "vendor.example:8443",
            Some(&secret),
        );
        let text = String::from_utf8(bytes).expect("ASCII");
        assert!(text.starts_with("POST /v1/x HTTP/1.1\r\n"), "{text}");
        assert!(
            text.contains("\r\nAuthorization: Bearer hunter2\r\n"),
            "{text}"
        );
        assert!(
            !text.contains("/v1/x?"),
            "the credential is a header value and never a query parameter"
        );
    }

    #[test]
    fn a_head_is_parsed_once_its_blank_line_has_arrived() {
        // Incomplete: no answer yet, and no error either.
        assert_eq!(
            parse_head(b"HTTP/1.1 200 OK\r\n", URL).expect("no error"),
            None
        );
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let head = parse_head(raw, URL).expect("parses").expect("complete");
        assert_eq!(head.status, 200);
        assert_eq!(head.framing, Framing::Length(2));
        assert_eq!(&raw[head.body_at..], b"{}");
    }

    /// RFC 9112 §6.1: chunked wins, and a client that preferred the length would read two
    /// bytes of a chunk header as a body.
    #[test]
    fn chunked_wins_over_content_length() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nTransfer-Encoding: chunked\r\n\r\n",
            URL,
        )
        .expect("parses")
        .expect("complete");
        assert_eq!(head.framing, Framing::Chunked);
    }

    #[test]
    fn a_response_with_neither_framing_header_ends_at_the_close() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nServer: wll\r\n\r\n", URL)
            .expect("parses")
            .expect("complete");
        assert_eq!(head.framing, Framing::UntilClose);
    }

    #[test]
    fn a_head_that_is_not_http_is_refused_with_the_rule_it_broke() {
        for (bytes, fragment) in [
            (&b"<html>hello</html>\r\n\r\n"[..], "names no HTTP/1.x"),
            (&b"HTTP/1.1 twohundred OK\r\n\r\n"[..], "no status code"),
            (&b"HTTP/1.1 200 OK\r\nnot-a-header\r\n\r\n"[..], "no colon"),
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: lots\r\n\r\n"[..],
                "not a length",
            ),
        ] {
            let e = parse_head(bytes, URL).expect_err("refused");
            assert!(e.to_string().contains(fragment), "{e}");
        }
    }

    #[test]
    fn a_chunked_body_decodes_and_an_incomplete_one_waits() {
        let full = b"4\r\n{\"a\"\r\n4\r\n:1}!\r\n0\r\n\r\n";
        let decoded = dechunk(full).expect("valid").expect("complete");
        assert_eq!(decoded, b"{\"a\":1}!".to_vec());
        // Every prefix is either incomplete or complete, and never a decode failure: a
        // short read is the ordinary case on a socket.
        for cut in 0..full.len() {
            assert!(
                dechunk(&full[..cut]).is_ok(),
                "a {cut}-byte prefix was a failure rather than an incomplete body"
            );
        }
        // A chunk extension is ignored rather than refused.
        assert_eq!(
            dechunk(b"2;name=value\r\nhi\r\n0\r\n\r\n")
                .expect("valid")
                .expect("complete"),
            b"hi".to_vec()
        );
        assert!(dechunk(b"zz\r\n").is_err(), "a non-hexadecimal chunk size");
    }

    #[tokio::test]
    async fn a_content_length_response_is_read_off_a_stream() {
        let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"data\":null}trailing";
        let frame = read_response(&mut &bytes[..], URL).await.expect("reads");
        assert_eq!(frame.bytes(), b"{\"data\":null}", "exactly Content-Length");
    }

    #[tokio::test]
    async fn a_chunked_response_is_read_off_a_stream() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"a\":1\r\n1\r\n}\r\n0\r\n\r\n";
        let frame = read_response(&mut &bytes[..], URL).await.expect("reads");
        assert_eq!(frame.bytes(), b"{\"a\":1}");
    }

    #[tokio::test]
    async fn a_close_delimited_response_ends_at_the_close() {
        let bytes = b"HTTP/1.1 200 OK\r\nServer: wll\r\n\r\n{\"a\":1}";
        let frame = read_response(&mut &bytes[..], URL).await.expect("reads");
        assert_eq!(frame.bytes(), b"{\"a\":1}");
    }

    #[tokio::test]
    async fn a_status_that_is_not_success_is_reported_before_the_body_is_read() {
        // The body is a lie about its length and the status still comes out right, which is
        // the property "status first" buys.
        let bytes = b"HTTP/1.1 409 Conflict\r\nContent-Length: 500\r\n\r\nshort";
        let e = read_response(&mut &bytes[..], URL)
            .await
            .expect_err("a refusal");
        assert!(matches!(e, HttpError::Status { status: 409, .. }), "{e}");
    }

    #[tokio::test]
    async fn a_truncated_body_is_a_failure_and_not_a_short_frame() {
        let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nshort";
        let e = read_response(&mut &bytes[..], URL)
            .await
            .expect_err("truncated");
        assert!(
            e.to_string().contains("closed in the middle"),
            "a short body must not decode as a frame: {e}"
        );
    }

    #[test]
    fn a_tls_address_is_refused_before_a_socket_is_opened() {
        let endpoint = Endpoint::parse("https://wll.lan").expect("parses");
        let e = block(request(
            &endpoint,
            HttpMethod::Get,
            "/v1/current_conditions",
            None,
            Duration::from_secs(1),
        ))
        .expect_err("TLS is not spoken");
        assert!(matches!(e, HttpError::Tls { .. }), "{e}");
        assert!(e.to_string().contains("https://wll.lan"), "{e}");
    }

    #[test]
    fn every_request_failure_is_retryable() {
        for e in [
            HttpError::Status {
                url: URL.into(),
                status: 404,
            },
            HttpError::Timeout {
                url: URL.into(),
                timeout: Duration::from_secs(10),
            },
            HttpError::TooLarge { url: URL.into() },
            HttpError::Malformed {
                url: URL.into(),
                why: "x",
            },
        ] {
            assert_eq!(e.class(), Class::RETRYABLE, "{e}");
        }
        assert_eq!(
            HttpError::Status {
                url: "http://h/v1/real_time".into(),
                status: 409,
            }
            .to_string(),
            "http://h/v1/real_time answered 409"
        );
    }

    /// A one-shot block, so the synchronous assertions above need no runtime of their own.
    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
            .block_on(f)
    }
}
