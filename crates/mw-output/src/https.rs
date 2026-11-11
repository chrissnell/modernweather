//! The outbound HTTP/1.1 request the two HTTP uploaders make, over TLS.
//!
//! # Why this is written out rather than taken from a client crate
//!
//! `mw-output`'s own module documentation has carried the note since the crate was created:
//! *"The outbound HTTP client is not named by any design document; it arrives with the work
//! item that writes `Uploader`."* This is that item, and the answer is the same one
//! `mw_station::http` reached for the same reason, with one addition.
//!
//! - **`hyper`'s client splits a connection into a sender and a connection future that
//!   something else has to drive**, which on this path means the `tokio::spawn`
//!   `clippy.toml` forbids — and the sanctioned mechanism it points at,
//!   `mw_supervisor::task`, is a crate in a *lower* layer that this one may name but whose
//!   task table belongs to the supervisor rather than to a component's own tick.
//! - **`reqwest` would answer that**, and it is 132 crates, a second HTTP stack beside
//!   `axum`'s, and `rustls-platform-verifier` — on a build whose release targets include an
//!   ARM1176 measured once, by `ci/spike/probe`, which is the evidence behind
//!   `docs/release.md` Appendix A.
//! - **What is actually needed is one `GET` with no body against two endpoints whose
//!   response this program reads three things out of**: the status, `Retry-After`, and a
//!   short reason string. The request builder and the response-head parse below are pure
//!   functions over byte strings, which a golden test pins.
//!
//! The addition over `mw_station::http` is TLS, which that module refuses outright
//! (`HttpError::Tls`) because a station is on the LAN. A credential crosses the public
//! internet here, so plaintext is not an option the default endpoints offer: `rustls` and
//! `tokio-rustls` are already pinned for the listener, and the one new dependency is
//! **`webpki-roots`** — a pure-data crate with no build step, recorded as an amendment in
//! `docs/reconciliation.md` (**A-M6-2-1**) because **D-32** governs the daemon's
//! dependencies and a client was not in its table.
//!
//! Why `webpki-roots` rather than the platform store: the daemon ships as a static musl
//! binary to four targets and into a container, where `/etc/ssl/certs` is whatever the base
//! image has — and a trust store that depends on the image is a trust store that fails on
//! one target and not another. A compiled-in root set fails or succeeds identically
//! everywhere, which is the property worth having for two fixed public endpoints.
//!
//! # What is tested here, and what is not
//!
//! The request bytes, the response-head parse, the `Retry-After` parse and the status
//! classification are unit-tested; the socket and the handshake are not. That boundary is
//! deliberate and it is this item's instruction: *"a test that actually posts to CWOP or
//! Weather Underground is a test that pollutes a public network with fabricated
//! observations from the owner's callsign."* `crate::uploader`'s tests stub
//! `crate::uploader::Transmit`, so every scheduling, retry and rate-limiting property is
//! asserted without a socket at all.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::BoxFuture;
use mw_station::session::ByteStream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

use crate::error::{Target, UploadError, MAX_BODY_EXCERPT};
use crate::form::Form;
use crate::uploader::Transmit;

/// How long to wait for the TCP connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a response this reads.
///
/// Both services answer with a line or two. The cap exists because a captive portal or a
/// misrouted request can answer with a page: a bounded read is the difference between a
/// surprising response and an out-of-memory daemon.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// What the `User-Agent` and `softwaretype` both derive from.
///
/// One constant so that the two cannot disagree about which program sent the upload, which
/// is the first thing a service's support asks.
pub const AGENT: &str = concat!("modernweather/", env!("CARGO_PKG_VERSION"));

/// Where an upload goes: an absolute `http` or `https` URL with a path and no query.
///
/// Its own type rather than `mw_station::Endpoint`, which deliberately holds **no path** —
/// a station transport's paths are the driver's. `api_endpoint` here is a full URL an
/// operator may override, so the path is part of the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl Endpoint {
    /// Parse an absolute URL.
    ///
    /// A query string in the configured endpoint is **refused** rather than merged: the
    /// uploader appends its own query, and two sources of parameters is how a `PASSWORD`
    /// ends up in a configuration column that no endpoint is supposed to return
    /// (**D-67**).
    pub fn parse(url: &str) -> Result<Self, EndpointError> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| EndpointError::NotAbsolute {
                url: url.to_string(),
            })?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            _ => {
                return Err(EndpointError::Scheme {
                    url: url.to_string(),
                    scheme: scheme.to_string(),
                })
            }
        };
        if rest.contains('?') || rest.contains('#') {
            return Err(EndpointError::HasQuery {
                url: url.to_string(),
            });
        }
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, "/".to_string()),
        };
        if let Some((_credential, host)) = authority.rsplit_once('@') {
            return Err(EndpointError::Userinfo {
                // Only the part after the `@`: everything before it is a credential.
                host: host.to_string(),
            });
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse::<u16>().map_err(|_| EndpointError::Port {
                    url: url.to_string(),
                    port: port.to_string(),
                })?;
                (host, port)
            }
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(EndpointError::NoHost {
                url: url.to_string(),
            });
        }
        Ok(Self {
            tls,
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    /// Whether this is TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Where it goes, for an error message. Never the query (`crate::error::Target`).
    pub fn target(&self) -> Target {
        Target::new(
            if self.tls { "https" } else { "http" },
            &self.host,
            self.port,
            &self.path,
        )
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.target())
    }
}

/// Why a configured endpoint is not a URL this can request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// No scheme.
    #[error(
        "`{url}` is not an absolute URL. `uploader.api_endpoint` is a full endpoint such \
         as `https://pwsupdate.pwsweather.com/api/v1/submitwx`; leave it NULL for the \
         service's default."
    )]
    NotAbsolute {
        /// What was configured.
        url: String,
    },

    /// A scheme this does not speak.
    #[error(
        "`{url}` has the scheme `{scheme}`, and an upload endpoint is `https` or `http`. \
         Both services publish `https` endpoints and a credential crosses the public \
         internet on this path, so `http` is for a local test receiver only."
    )]
    Scheme {
        /// What was configured.
        url: String,
        /// The scheme.
        scheme: String,
    },

    /// A query or a fragment in the configured endpoint.
    #[error(
        "`{url}` carries a query string or a fragment. The uploader appends the \
         measurements and the credential as the query, so an endpoint that has one already \
         would mean two sources of parameters -- set the path only."
    )]
    HasQuery {
        /// What was configured.
        url: String,
    },

    /// Credentials in the URL.
    ///
    /// **The configured value is not quoted**, which is the one refusal here that cannot
    /// name what it refused: the whole content of the mistake is a credential, and an
    /// error message that echoed it would put the credential in the log of the process
    /// that refused to use it.
    #[error(
        "the configured `api_endpoint` for the `{host}` host carries userinfo before the \
         host, and its value is not quoted here on purpose. The credential belongs in \
         `uploader.api_key`, where it is a `SecretString` that no endpoint returns \
         (D-33, D-67)."
    )]
    Userinfo {
        /// The host, which is the part after the `@` and is not a credential.
        host: String,
    },

    /// A port that is not a number.
    #[error("`{url}` has the port `{port}`, which is not a number between 1 and 65535")]
    Port {
        /// What was configured.
        url: String,
        /// The text after the colon.
        port: String,
    },

    /// No host.
    #[error("`{url}` has no host")]
    NoHost {
        /// What was configured.
        url: String,
    },
}

/// One occurrence's HTTP upload: an endpoint, and nothing else.
///
/// The query comes from the encoder as a [`Form`]; the credential is inside it, which is
/// why that type's `Debug` redacts.
#[derive(Debug, Clone)]
pub struct HttpUpload {
    endpoint: Arc<Endpoint>,
}

impl HttpUpload {
    /// Build an uploader for one endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
        }
    }

    /// Where it sends.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl Transmit for HttpUpload {
    type Payload = Form;

    fn transmit(self, form: Form) -> BoxFuture<'static, Result<(), UploadError>> {
        Box::pin(async move { upload(self.endpoint, form).await })
    }
}

/// Connect, write the request, read the response, classify it.
async fn upload(endpoint: Arc<Endpoint>, form: Form) -> Result<(), UploadError> {
    let target = endpoint.target();
    let mut wire = connect(&endpoint, &target).await?;
    let request = request_bytes(&endpoint, form.as_str());
    // `form` is never logged: its `Debug` redacts and its `as_str` is called exactly here.
    debug!(
        endpoint = %target,
        measurements = form.measurements(),
        "uploading a windowed summary"
    );
    wire.write_all(&request)
        .await
        .map_err(|source| UploadError::Connect {
            what: "could not write a request to",
            target: target.clone(),
            source,
        })?;
    let response = read_response(wire.as_mut(), &target).await?;
    let head = parse_head(&response, &target)?;
    classify(&head, &target, Utc::now())
}

/// Build the request bytes.
///
/// `GET`, `Connection: close` and no body. `Connection: close` is what makes reading to end
/// of stream a correct framing for the response, which is the branch RFC 9112 §6.3 leaves
/// for a response with neither a `Content-Length` nor a chunked encoding — and the one both
/// services' answers have historically used.
pub fn request_bytes(endpoint: &Endpoint, query: &str) -> Vec<u8> {
    let path = if query.is_empty() {
        endpoint.path.clone()
    } else {
        format!("{}?{query}", endpoint.path)
    };
    // The `Host` header carries the port only when it is not the scheme's default, which is
    // what RFC 9110 §7.2 requires and what a virtual host matches on.
    let default_port = if endpoint.tls { 443 } else { 80 };
    let host = if endpoint.port == default_port {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {AGENT}\r\n\
         Accept: */*\r\n\
         Connection: close\r\n\
         \r\n"
    )
    .into_bytes()
}

/// What this reads out of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The status code.
    pub status: u16,
    /// `Retry-After`, parsed — either delta-seconds or an HTTP date.
    pub retry_after: Option<RetryAfter>,
    /// The first [`MAX_BODY_EXCERPT`] characters of the body, which is where both services
    /// put the reason.
    pub body: String,
}

/// A `Retry-After` value, in the two forms RFC 9110 §10.2.3 permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAfter {
    /// `Retry-After: 120`.
    Delta(Duration),
    /// `Retry-After: Fri, 31 Dec 2027 23:59:59 GMT`.
    At(DateTime<Utc>),
}

impl RetryAfter {
    /// How long to wait, measured from `now`.
    ///
    /// A date in the past is zero rather than negative: a service whose clock is ahead of
    /// ours should not make us wait a day, and one whose date has already passed is telling
    /// us to go now.
    pub fn delay(self, now: DateTime<Utc>) -> Duration {
        match self {
            RetryAfter::Delta(d) => d,
            RetryAfter::At(when) => (when - now).to_std().unwrap_or(Duration::ZERO),
        }
    }
}

/// Parse a response head and body.
pub fn parse_head(response: &[u8], target: &Target) -> Result<Head, UploadError> {
    let malformed = |why: &'static str| UploadError::Malformed {
        target: target.clone(),
        expected: "HTTP",
        why,
    };
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| (&response[..at], &response[at + 4..]));
    // A head with no blank line after it is a truncated response, not a response with no
    // body: the blank line is what says the head ended.
    let (head, body) = split.ok_or_else(|| malformed("the head was not terminated"))?;
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| malformed("there was no status line"))?;
    let mut parts = status_line.split(' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(malformed("the status line did not begin with `HTTP/1.`"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| malformed("the status line carried no status code"))?;

    let mut retry_after = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("retry-after") {
            retry_after = parse_retry_after(value.trim());
        }
    }

    Ok(Head {
        status,
        retry_after,
        body: String::from_utf8_lossy(body)
            .chars()
            .take(MAX_BODY_EXCERPT)
            .collect(),
    })
}

/// Parse a `Retry-After` value.
///
/// Both forms, because a service that answers `429` with a date rather than a delta would
/// otherwise be retried immediately — the header would parse to `None` and the limiter
/// would have nothing to honour, which is the quiet half of the pushback defect.
pub fn parse_retry_after(value: &str) -> Option<RetryAfter> {
    if let Ok(seconds) = value.parse::<u64>() {
        // Capped at a day: a service answering `Retry-After: 31536000` is not a reason to
        // hold a component's limiter for a year.
        return Some(RetryAfter::Delta(Duration::from_secs(
            seconds.min(24 * 3600),
        )));
    }
    // RFC 9110 names three date formats and requires IMF-fixdate be sent; the other two are
    // obsolete and are parsed because a service that sends one is still telling us when.
    for format in [
        "%a, %d %b %Y %H:%M:%S GMT",
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(RetryAfter::At(naive.and_utc()));
        }
    }
    None
}

/// Turn a response into success or the right [`UploadError`].
pub fn classify(head: &Head, target: &Target, now: DateTime<Utc>) -> Result<(), UploadError> {
    match head.status {
        200..=299 => Ok(()),
        429 => Err(UploadError::RateLimited {
            target: target.clone(),
            retry_after: head.retry_after.map(|r| r.delay(now)),
        }),
        // A `503` with a `Retry-After` is the same instruction as a `429` with one, and both
        // services use it under load. Classified as rate limiting so the limiter honours the
        // hint rather than retrying inside the attempt budget.
        503 if head.retry_after.is_some() => Err(UploadError::RateLimited {
            target: target.clone(),
            retry_after: head.retry_after.map(|r| r.delay(now)),
        }),
        status => Err(UploadError::Rejected {
            target: target.clone(),
            status,
            body: head.body.clone(),
        }),
    }
}

/// Read the whole response, capped.
async fn read_response(wire: &mut dyn ByteStream, target: &Target) -> Result<Vec<u8>, UploadError> {
    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let read = wire
            .read(&mut chunk)
            .await
            .map_err(|source| UploadError::Connect {
                what: "could not read a response from",
                target: target.clone(),
                source,
            })?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() >= MAX_RESPONSE_BYTES {
            break;
        }
    }
    Ok(response)
}

/// Connect, and wrap in TLS when the endpoint says so.
async fn connect(endpoint: &Endpoint, target: &Target) -> Result<Box<dyn ByteStream>, UploadError> {
    let address = format!("{}:{}", endpoint.host, endpoint.port);
    let connecting = tokio::net::TcpStream::connect(&address);
    let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(source)) => {
            return Err(UploadError::Connect {
                what: "could not connect to",
                target: target.clone(),
                source,
            })
        }
        Err(_) => {
            return Err(UploadError::Connect {
                what: "timed out connecting to",
                target: target.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no connection within {} s", CONNECT_TIMEOUT.as_secs()),
                ),
            })
        }
    };
    if !endpoint.tls {
        return Ok(Box::new(Wire(tcp)));
    }
    let name = rustls::pki_types::ServerName::try_from(endpoint.host.clone()).map_err(|e| {
        UploadError::Tls {
            target: target.clone(),
            source: Box::new(e),
        }
    })?;
    let connector = tokio_rustls::TlsConnector::from(tls_config());
    let tls = connector
        .connect(name, tcp)
        .await
        .map_err(|source| UploadError::Tls {
            target: target.clone(),
            source: Box::new(source),
        })?;
    Ok(Box::new(Wire(tls)))
}

/// The one TLS client configuration this process builds.
///
/// `OnceLock` because assembling a root store is the expensive part and an uploader runs
/// once a period: building it per attempt would parse the compiled-in anchors every time.
/// `ring` explicitly rather than by default, which is **A-W8-3**'s choice and the same call
/// `mw_http::socket` makes — `aws-lc-rs` needs `cmake` and is not on the ARMv6 allowlist.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        // The provider is `ring` and the versions are TLS 1.2 and 1.3, which it supports:
        // this cannot fail, and an `expect` with the reason beats an error type whose only
        // inhabitant is a build mistake.
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(config)
    }))
}

/// A [`ByteStream`] over anything `tokio` can read and write.
///
/// `mw_station::ByteStream` is narrower than `AsyncRead + AsyncWrite` on purpose — "give me
/// more bytes" and "send these bytes" — and that is exactly the surface this module needs,
/// so the TLS stream and the plain socket become one type and `read_response` is written
/// once. The same trait is what lets `crate::frame`'s login conversation be played against
/// `mw_station::testing::Transcript`.
struct Wire<S>(S);

impl<S> fmt::Debug for Wire<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Wire")
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for Wire<S> {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move { self.0.read(buf).await })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            self.0.write_all(bytes).await?;
            // Flushed here rather than left to `Drop`: a TLS stream buffers, and a record
            // that is still in the buffer when the connection closes is a request the
            // service never saw.
            self.0.flush().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    fn endpoint() -> Endpoint {
        Endpoint::parse("https://pwsupdate.pwsweather.com/api/v1/submitwx").expect("parses")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    #[test]
    fn an_endpoint_is_a_scheme_a_host_a_port_and_a_path() {
        let e = endpoint();
        assert!(e.is_tls());
        assert_eq!(e.host(), "pwsupdate.pwsweather.com");
        assert_eq!(e.port(), 443);
        assert_eq!(
            e.to_string(),
            "https://pwsupdate.pwsweather.com/api/v1/submitwx"
        );

        // The default port follows the scheme, and an explicit one is kept.
        let plain = Endpoint::parse("http://wx.lan:8080/submit").expect("parses");
        assert_eq!((plain.is_tls(), plain.port()), (false, 8080));
        assert_eq!(
            Endpoint::parse("http://wx.lan/submit")
                .expect("parses")
                .port(),
            80
        );
        // A host with no path is the root.
        assert_eq!(
            Endpoint::parse("https://WX.LAN")
                .expect("parses")
                .to_string(),
            "https://wx.lan/"
        );
    }

    /// Each refusal, and the two that are about a credential rather than a syntax.
    #[test]
    fn an_endpoint_that_would_carry_a_credential_is_refused() {
        assert!(matches!(
            Endpoint::parse("pwsupdate.pwsweather.com/api"),
            Err(EndpointError::NotAbsolute { .. })
        ));
        assert!(matches!(
            Endpoint::parse("ftp://wx.lan/submit"),
            Err(EndpointError::Scheme { .. })
        ));
        // Two sources of query parameters is how a credential ends up in a column.
        let e = Endpoint::parse("https://wx.lan/submit?ID=KWA").expect_err("refused");
        assert!(matches!(e, EndpointError::HasQuery { .. }), "{e}");
        assert!(e.to_string().contains("two sources of parameters"), "{e}");
        // Userinfo is a credential in a URL, which an error message could print.
        let e = Endpoint::parse("https://user:pass@wx.lan/submit").expect_err("refused");
        assert!(matches!(e, EndpointError::Userinfo { .. }), "{e}");
        // The message names the host and **not** the credential, which is the whole of
        // what was configured wrongly.
        assert!(e.to_string().contains("wx.lan"), "{e}");
        // The configured text does not appear. (The word "userinfo" does, which is what
        // the mistake is called -- `user:pass@` is what must not.)
        assert!(!e.to_string().contains("user:pass"), "{e}");
        assert!(!e.to_string().contains('@'), "{e}");

        assert!(matches!(
            Endpoint::parse("https://wx.lan:nope/submit"),
            Err(EndpointError::Port { .. })
        ));
        assert!(matches!(
            Endpoint::parse("https:///submit"),
            Err(EndpointError::NoHost { .. })
        ));
    }

    /// The request bytes, which is the golden half of this module.
    #[test]
    fn the_request_is_one_get_with_connection_close() {
        let mut form = Form::new();
        form.set("ID", "KWABACKYARD");
        form.measurement("tempf", "72.3");
        let bytes = request_bytes(&endpoint(), form.as_str());
        let text = String::from_utf8(bytes).expect("ASCII");
        assert_eq!(
            text,
            "GET /api/v1/submitwx?ID=KWABACKYARD&tempf=72.3 HTTP/1.1\r\n\
             Host: pwsupdate.pwsweather.com\r\n\
             User-Agent: modernweather/0.1.0\r\n\
             Accept: */*\r\n\
             Connection: close\r\n\
             \r\n"
        );

        // A non-default port reaches the `Host` header, which is what a virtual host
        // matches on.
        let local = Endpoint::parse("http://wx.lan:8080/submit").expect("parses");
        let text = String::from_utf8(request_bytes(&local, "a=1")).expect("ASCII");
        assert!(text.starts_with("GET /submit?a=1 HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: wx.lan:8080\r\n"), "{text}");

        // An empty query produces no stray `?`.
        let text = String::from_utf8(request_bytes(&local, "")).expect("ASCII");
        assert!(text.starts_with("GET /submit HTTP/1.1"), "{text}");
    }

    #[test]
    fn a_response_head_parses_to_a_status_and_a_body() {
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nsuccess\n",
            &target,
        )
        .expect("parses");
        assert_eq!(head.status, 200);
        assert_eq!(head.body, "success\n");
        assert_eq!(head.retry_after, None);
        assert!(classify(&head, &target, Utc::now()).is_ok());

        // A head with no body at all.
        let head = parse_head(b"HTTP/1.0 204 No Content\r\n\r\n", &target).expect("parses");
        assert_eq!((head.status, head.body.as_str()), (204, ""));
    }

    /// Every malformed shape is named, because "the service answered something odd" is
    /// otherwise indistinguishable from a bug here.
    #[test]
    fn a_response_that_is_not_http_is_named() {
        let target = endpoint().target();
        for (bytes, why) in [
            (
                &b"HTTP/1.1 200 OK\r\nContent-Type: text"[..],
                "the head was not terminated",
            ),
            (
                &b"<html>hello</html>\r\n\r\n"[..],
                "the status line did not begin with `HTTP/1.`",
            ),
            (
                &b"HTTP/1.1 nope OK\r\n\r\n"[..],
                "the status line carried no status code",
            ),
        ] {
            let e = parse_head(bytes, &target).expect_err("refused");
            assert!(
                matches!(e, UploadError::Malformed { why: w, .. } if w == why),
                "{e}"
            );
        }
    }

    /// **W2a Decision 59, end to end.** A `429` with a `Retry-After` becomes a class the
    /// retry policy and the limiter both read.
    #[test]
    fn a_rate_limit_carries_its_retry_after_into_the_class() {
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\n\r\nslow down",
            &target,
        )
        .expect("parses");
        assert_eq!(
            head.retry_after,
            Some(RetryAfter::Delta(Duration::from_secs(120)))
        );
        let e = classify(&head, &target, Utc::now()).expect_err("rate limited");
        assert_eq!(
            e.class(),
            Class::Retryable {
                after: Some(Duration::from_secs(120))
            }
        );

        // A `503` with the header is the same instruction.
        let head = parse_head(
            b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 30\r\n\r\n",
            &target,
        )
        .expect("parses");
        let e = classify(&head, &target, Utc::now()).expect_err("rate limited");
        assert!(matches!(e, UploadError::RateLimited { .. }), "{e}");

        // A `503` without it is an ordinary retryable rejection.
        let head =
            parse_head(b"HTTP/1.1 503 Service Unavailable\r\n\r\n", &target).expect("parses");
        let e = classify(&head, &target, Utc::now()).expect_err("rejected");
        assert!(
            matches!(e, UploadError::Rejected { status: 503, .. }),
            "{e}"
        );
        assert!(matches!(e.class(), Class::Retryable { .. }));

        // A bad key is fatal for the tick, and the service's reason survives.
        let head = parse_head(
            b"HTTP/1.1 401 Unauthorized\r\n\r\ninvalid ID or PASSWORD",
            &target,
        )
        .expect("parses");
        let e = classify(&head, &target, Utc::now()).expect_err("rejected");
        assert_eq!(e.class(), Class::Fatal);
        assert!(e.to_string().contains("invalid ID or PASSWORD"), "{e}");
    }

    /// Both `Retry-After` forms, and the capping that stops one from holding a component
    /// for a year.
    #[test]
    fn retry_after_reads_a_delta_and_a_date() {
        assert_eq!(
            parse_retry_after("120"),
            Some(RetryAfter::Delta(Duration::from_secs(120)))
        );
        assert_eq!(
            parse_retry_after("31536000"),
            Some(RetryAfter::Delta(Duration::from_secs(24 * 3600))),
            "a year is capped at a day"
        );
        let imf = parse_retry_after("Wed, 11 Nov 2026 06:00:00 GMT").expect("a date");
        assert_eq!(imf, RetryAfter::At(at("2026-11-11T06:00:00Z")));
        assert_eq!(
            imf.delay(at("2026-11-11T05:30:00Z")),
            Duration::from_secs(1800)
        );
        // A date already past is "go now", not a negative wait.
        assert_eq!(imf.delay(at("2026-11-12T00:00:00Z")), Duration::ZERO);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    /// The header name is matched case-insensitively, because HTTP field names are.
    #[test]
    fn the_retry_after_header_is_matched_case_insensitively() {
        let target = endpoint().target();
        let head =
            parse_head(b"HTTP/1.1 429 x\r\nRETRY-AFTER:  45 \r\n\r\n", &target).expect("parses");
        assert_eq!(
            head.retry_after,
            Some(RetryAfter::Delta(Duration::from_secs(45)))
        );
    }

    /// A long body is capped at the excerpt length, so a proxy's error page is not a log
    /// line.
    #[test]
    fn a_long_body_is_capped() {
        let target = endpoint().target();
        let mut response = b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec();
        response.extend(std::iter::repeat_n(b'x', 10_000));
        let head = parse_head(&response, &target).expect("parses");
        assert_eq!(head.body.chars().count(), MAX_BODY_EXCERPT);
    }

    /// The agent names the program and its version, which is what a service's support asks
    /// for first.
    #[test]
    fn the_agent_identifies_this_program() {
        assert!(AGENT.starts_with("modernweather/"), "{AGENT}");
        assert!(AGENT.len() > "modernweather/".len(), "{AGENT}");
    }
}
