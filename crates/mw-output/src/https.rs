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
///
/// Strictly less than `crate::uploader::ATTEMPT_TIMEOUT`, and that is the whole reason for
/// the number: the attempt timeout starts first and bounds everything, so a connect budget
/// equal to it would make the specific "timed out connecting to" diagnosis unreachable and
/// a hung connect would consume the attempt with the less useful message.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
        let default_port = if tls { 443 } else { 80 };
        // An IPv6 literal is bracketed (RFC 3986 §3.2.2) and is full of colons, so the
        // port cannot be found by looking for the last one: `https://[::1]/submit` has no
        // port at all and `rsplit_once(':')` reads `1]` as one. The literal is stored
        // **unbracketed**, because that is the form `rustls` parses as a `ServerName` and
        // the form an error message should carry; [`Endpoint::authority`] puts the
        // brackets back wherever the wire needs them.
        let (host, port) = match authority.strip_prefix('[') {
            Some(after_bracket) => {
                let (literal, rest) =
                    after_bracket
                        .split_once(']')
                        .ok_or_else(|| EndpointError::HostLiteral {
                            url: url.to_string(),
                        })?;
                let port = match rest {
                    "" => default_port,
                    rest => {
                        let digits = rest.strip_prefix(':').ok_or_else(|| EndpointError::Port {
                            url: url.to_string(),
                            port: rest.to_string(),
                        })?;
                        digits.parse::<u16>().map_err(|_| EndpointError::Port {
                            url: url.to_string(),
                            port: digits.to_string(),
                        })?
                    }
                };
                (literal, port)
            }
            None => match authority.rsplit_once(':') {
                Some((host, port)) => {
                    let port = port.parse::<u16>().map_err(|_| EndpointError::Port {
                        url: url.to_string(),
                        port: port.to_string(),
                    })?;
                    (host, port)
                }
                None => (authority, default_port),
            },
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

    /// The host as it appears on the wire: an IPv6 literal bracketed, anything else as-is.
    ///
    /// [`Self::host`] is the bare host, which is what `rustls` wants as a `ServerName` and
    /// what reads correctly in an error message. The `Host` header, the `connect` address
    /// and the displayed URL all need the RFC 3986 §3.2.2 form instead, and an unbracketed
    /// `::1:443` is neither a host nor a port.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// Where it goes, for an error message. Never the query (`crate::error::Target`).
    pub fn target(&self) -> Target {
        Target::new(
            if self.tls { "https" } else { "http" },
            &self.authority(),
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

    /// A bracketed host literal with no closing bracket.
    #[error(
        "`{url}` opens a bracketed IPv6 literal and never closes it. The form is \
         `https://[2001:db8::1]:8443/submit`."
    )]
    HostLiteral {
        /// What was configured.
        url: String,
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
    let response = read_response(wire.as_mut(), &target, MAX_RESPONSE_BYTES).await?;
    let head = parse_head(&response, &target)?;
    classify(&head, &target, Utc::now())
}

/// One `GET`, read to `max` bytes, with the head parsed and the body decoded.
///
/// The upload path above is this function with the body thrown away and the status
/// classified. It is separate because the two callers want opposite halves of the same
/// exchange: an upload reads the status and the reason, and a fetch reads the payload. Both
/// go through the same [`connect`], the same [`request_bytes`] and the same three framings,
/// which is what stops a second HTTP client appearing in this workspace the first time
/// something needs a response body.
///
/// The caller classifies. [`classify`] is right here for an upload and is **not** right for
/// a fetch of an API that answers `200` with `{"success": false}` -- which is what Aeris
/// does for a bad credential -- so this function returns the exchange and leaves the verdict
/// to whoever knows what a successful response looks like.
pub async fn get(
    endpoint: &Endpoint,
    query: &str,
    max: usize,
) -> Result<(Head, Vec<u8>), UploadError> {
    let target = endpoint.target();
    let mut wire = connect(endpoint, &target).await?;
    let request = request_bytes(endpoint, query);
    wire.write_all(&request)
        .await
        .map_err(|source| UploadError::Connect {
            what: "could not write a request to",
            target: target.clone(),
            source,
        })?;
    let response = read_response(wire.as_mut(), &target, max).await?;
    parse_response(&response, &target)
}

/// Build the request bytes.
///
/// `GET`, `Connection: close` and no body. `Connection: close` asks the peer to end the
/// response by closing, which is RFC 9112 §6.3's read-to-close framing — but **it does not
/// stop the peer sending a `Content-Length` or a chunked body instead**, so
/// [`read_response`] implements all three framings rather than relying on this header.
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
        endpoint.authority()
    } else {
        format!("{}:{}", endpoint.authority(), endpoint.port)
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

/// The longest hold a `Retry-After` can install, in either of its two forms.
///
/// A service answering `Retry-After: 31536000` — or, equivalently, a date a year out — is
/// not a reason to stop uploading for a year. The cap is here rather than at the parse,
/// because [`RetryAfter::delay`] is the one place both forms become a duration: capping
/// only the delta-seconds form left the date form uncapped, and an uncapped hold is the
/// worst of the failure modes available — the component reports healthy, logs nothing
/// further, and never uploads again.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 3600);

impl RetryAfter {
    /// How long to wait, measured from `now`, capped at [`MAX_RETRY_AFTER`].
    ///
    /// A date in the past is zero rather than negative: a service whose clock is ahead of
    /// ours should not make us wait a day, and one whose date has already passed is telling
    /// us to go now.
    pub fn delay(self, now: DateTime<Utc>) -> Duration {
        let delay = match self {
            RetryAfter::Delta(d) => d,
            RetryAfter::At(when) => (when - now).to_std().unwrap_or(Duration::ZERO),
        };
        delay.min(MAX_RETRY_AFTER)
    }
}

/// Parse a response head, discarding the body past [`MAX_BODY_EXCERPT`].
///
/// What an upload needs: the status, `Retry-After` and a short reason string. A caller that
/// wants the body itself -- `crate::aeris`, whose whole purpose is the JSON in it -- calls
/// [`parse_response`] and gets both.
pub fn parse_head(response: &[u8], target: &Target) -> Result<Head, UploadError> {
    parse_response(response, target).map(|(head, _)| head)
}

/// Parse a response head **and** its decoded body.
///
/// The body is decoded, not raw: chunk sizes are framing, and a caller that handed
/// `1f\r\n{"success":true}` to a JSON parser would be parsing the framing. `Content-Length`
/// truncates to exactly its value, a chunked body is de-chunked, and neither leaves this
/// function with a trailer or a chunk header in it.
pub fn parse_response(response: &[u8], target: &Target) -> Result<(Head, Vec<u8>), UploadError> {
    let malformed = |why: &'static str| UploadError::Malformed {
        target: target.clone(),
        expected: "HTTP",
        why,
    };
    // Any 1xx preamble belongs to the exchange and not to the outcome.
    let response = final_response(response);
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
        if name.trim().eq_ignore_ascii_case("retry-after") && retry_after.is_none() {
            // The **first** parseable value, so a repeated header whose second copy is
            // unparseable cannot erase a good one. RFC 9110 permits neither a repeat nor an
            // unparseable value; a proxy produces both.
            retry_after = parse_retry_after(value.trim());
        }
    }

    // The body excerpt is the **decoded** body: the chunk sizes are framing, and a log
    // line carrying `1f\r\nInvalid ID` instead of `Invalid ID` is the reason string a
    // service's support is asked for.
    let chunked = header(&head, "transfer-encoding").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("chunked"))
    });
    let decoded: Vec<u8> = if chunked {
        dechunk(body)
    } else {
        match header(&head, "content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
            // **A body shorter than its declared length is a framing violation, and saying
            // so is the point.** Truncating silently and handing the short body on means a
            // middlebox that drops the connection mid-response is diagnosed by whoever
            // parses the payload -- which for `crate::aeris` is "the Aeris response was not
            // the JSON this endpoint answers with", pointing at a vendor schema change.
            // This is the layer that knows the declared length.
            Some(len) if len > body.len() => {
                return Err(malformed(
                    "the body was shorter than its `Content-Length`, so the transfer was                      cut short",
                ))
            }
            Some(len) => body[..len].to_vec(),
            None => body.to_vec(),
        }
    };
    Ok((
        Head {
            status,
            retry_after,
            // Truncated before the lossy decode rather than after, so a 64 KiB page is not
            // allocated as a `String` to take 200 characters from it.
            body: String::from_utf8_lossy(&decoded[..MAX_BODY_EXCERPT.min(decoded.len())])
                .chars()
                .take(MAX_BODY_EXCERPT)
                .collect(),
        },
        decoded,
    ))
}

/// Parse a `Retry-After` value.
///
/// Both forms, because a service that answers `429` with a date rather than a delta would
/// otherwise be retried immediately — the header would parse to `None` and the limiter
/// would have nothing to honour, which is the quiet half of the pushback defect.
pub fn parse_retry_after(value: &str) -> Option<RetryAfter> {
    if let Ok(seconds) = value.parse::<u64>() {
        // Capped here as well as in `delay`, so the parsed value does not claim a hold
        // longer than the one that will be honoured. `delay` is the enforcement point.
        return Some(RetryAfter::Delta(
            Duration::from_secs(seconds).min(MAX_RETRY_AFTER),
        ));
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

/// Read a whole response, in whichever of RFC 9112 §6's framings the peer used.
///
/// Three, not one, and for the reason `mw_station::http`'s module documentation gives for
/// implementing all three there: *"a client that only understood `Content-Length` would
/// work until a firmware update."* The mirror of that argument applies to a public service
/// behind a load balancer nobody here controls.
///
/// - **`Content-Length`**: stop at exactly that many body bytes. Without this a server that
///   ignores `Connection: close` and keeps the connection open blocks until the attempt
///   timeout, and the occurrence is reported as `Timeout` rather than as the status the
///   service actually returned.
/// - **`Transfer-Encoding: chunked`**: stop at the terminating zero-length chunk. Without
///   this the hex chunk sizes end up inside `Head::body`, which is the reason string
///   `UploadError::Rejected` surfaces and a log line carries.
/// - **Neither**: read to end of stream, which is §6.3's fallback and what
///   `Connection: close` asks the peer for.
///
/// **A truncated close is tolerated once the head is complete.** `rustls` answers
/// `ErrorKind::UnexpectedEof` from a read when the peer closed TCP without `close_notify`,
/// which a load balancer that resets after the body does routinely — and treating that as a
/// failure turned a successful `200` into three retried attempts and a failed occurrence.
async fn read_response(
    wire: &mut dyn ByteStream,
    target: &Target,
    max: usize,
) -> Result<Vec<u8>, UploadError> {
    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if complete(&response) {
            break;
        }
        let read = match wire.read(&mut chunk).await {
            Ok(read) => read,
            // A close without `close_notify`, after a complete head, is this response
            // ending. Before the head it is a truncated response and still a failure.
            //
            // All three abrupt-close kinds, not `UnexpectedEof` alone: a load balancer
            // that answers and then RSTs the socket surfaces as `ConnectionReset` (or
            // `ConnectionAborted` on some stacks), and the response already in the buffer
            // is just as complete as it is after a missing `close_notify`. Tolerating one
            // kind and retrying the other two turned the same successful `200` into a
            // `Connect` failure depending on which middlebox was in front of it.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) && head_end(final_response(&response)).is_some() =>
            {
                break;
            }
            Err(source) => {
                return Err(UploadError::Connect {
                    what: "could not read a response from",
                    target: target.clone(),
                    source,
                })
            }
        };
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() >= max {
            break;
        }
    }
    Ok(response)
}

/// The response this uploader reports, with any 1xx interim responses stepped over.
///
/// RFC 9110 §15.2: a `100 Continue` or a `103 Early Hints` preamble is a complete head of
/// its own, with no body, and the final response follows it. Reading the first status line
/// in the buffer therefore reports `103` as the outcome — which classifies `Fatal`, so a
/// perfectly successful `200` behind an early-hints preamble becomes a permanently stopped
/// uploader. A load balancer is exactly the kind of peer that emits them.
fn final_response(response: &[u8]) -> &[u8] {
    let mut rest = response;
    loop {
        let Some(end) = head_end(rest) else {
            return rest;
        };
        if !matches!(status_of(rest), Some(100..=199)) {
            return rest;
        }
        rest = &rest[end..];
    }
}

/// The status code of the head at the front of `response`, if it has one.
fn status_of(response: &[u8]) -> Option<u16> {
    let head_end = head_end(response)?;
    let head = String::from_utf8_lossy(&response[..head_end]);
    let status_line = head.split("\r\n").next()?;
    let mut parts = status_line.split(' ');
    parts.next().filter(|v| v.starts_with("HTTP/1."))?;
    parts.next()?.parse().ok()
}

/// Where the head ends, as an offset past the blank line.
fn head_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

/// Whether enough has been read that the peer has nothing more to say about this response.
///
/// `false` until the head is complete, then whichever framing the head declared. A `false`
/// answer for the read-to-close case is what makes the loop continue to end of stream.
fn complete(response: &[u8]) -> bool {
    let response = final_response(response);
    let Some(body_at) = head_end(response) else {
        return false;
    };
    let head = String::from_utf8_lossy(&response[..body_at]);
    let body = &response[body_at..];
    // RFC 9112 §6.1's *status-defined* framing comes first: a `204` or a `304` has no body
    // whatever its headers say, and `304` carries the cached response's `Content-Length`
    // routinely. Reading that header as a promise of bytes that never arrive means the
    // loop runs to the attempt timeout and a successful conditional request is reported as
    // `Timeout`.
    if matches!(status_of(response), Some(204 | 304)) {
        return true;
    }
    if header(&head, "transfer-encoding").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("chunked"))
    }) {
        return chunked_end(body).is_some();
    }
    match header(&head, "content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(len) => body.len() >= len,
        None => false,
    }
}

/// One header's value, if the head carries it.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field.trim().eq_ignore_ascii_case(name).then_some(value)
    })
}

/// The offset just past a chunked body's terminating zero-length chunk, if it has arrived.
fn chunked_end(body: &[u8]) -> Option<usize> {
    let mut at = 0usize;
    loop {
        let line_end = body[at..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| at + p)?;
        let size_text = String::from_utf8_lossy(&body[at..line_end]);
        // A chunk size may carry extensions after a `;`, which this does not read.
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        let data_at = line_end + 2;
        if size == 0 {
            // The trailer section, terminated by its own blank line.
            return body[data_at..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .map(|p| data_at + p + 2);
        }
        at = data_at.checked_add(size)?.checked_add(2)?;
        if at > body.len() {
            return None;
        }
    }
}

/// A chunked body's data, concatenated — what goes into [`Head::body`].
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut at = 0usize;
    while let Some(p) = body[at..].windows(2).position(|w| w == b"\r\n") {
        let line_end = at + p;
        let size_text = String::from_utf8_lossy(&body[at..line_end]);
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let data_at = line_end + 2;
        // `checked_add`, for the same reason `chunked_end` uses it twenty lines above: a
        // peer that answers `ffffffffffffffff` as a chunk size overflows the addition,
        // which panics in a debug build and wraps to a smaller offset in a release one --
        // and `min` on the wrapped value is no protection at all.
        let Some(end) = data_at.checked_add(size) else {
            break;
        };
        let end = end.min(body.len());
        out.extend_from_slice(&body[data_at..end]);
        at = end + 2;
        if at >= body.len() {
            break;
        }
    }
    out
}

/// Connect, and wrap in TLS when the endpoint says so.
async fn connect(endpoint: &Endpoint, target: &Target) -> Result<Box<dyn ByteStream>, UploadError> {
    let address = format!("{}:{}", endpoint.authority(), endpoint.port);
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
            //
            // `shutdown` is deliberately **not** called, so this client never sends
            // `close_notify` either -- the request is one flushed `GET` and the connection
            // is closed by dropping it after the response. That is the write-side
            // counterpart of `read_response`'s tolerance of a peer that does the same, and
            // it is symmetric rather than an oversight.
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

    /// **The cap covers both forms, because an uncapped hold is silent.**
    ///
    /// The delta-seconds cap was applied at the parse and the date form was not, so
    /// `Retry-After: Thu, 01 Jan 2032 00:00:00 GMT` installed a hold of years: the
    /// component reports healthy, logs nothing further, and never uploads again.
    #[test]
    fn a_retry_after_date_is_capped_like_a_delta() {
        let now = at("2026-11-11T06:00:00Z");
        let far = parse_retry_after("Thu, 01 Jan 2032 00:00:00 GMT").expect("a date");
        assert_eq!(
            far.delay(now),
            MAX_RETRY_AFTER,
            "a date five years out is a day, as `Retry-After: 157680000` is"
        );
        assert_eq!(
            parse_retry_after("157680000").expect("a delta").delay(now),
            MAX_RETRY_AFTER
        );
        // And the cap is a ceiling, not a floor: anything inside it is itself.
        assert_eq!(
            parse_retry_after("Wed, 11 Nov 2026 12:00:00 GMT")
                .expect("a date")
                .delay(now),
            Duration::from_secs(6 * 3600)
        );

        // It reaches the class, which is what the uploader's hold is built from.
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 503 Busy\r\nRetry-After: Thu, 01 Jan 2032 00:00:00 GMT\r\n\r\n",
            &target,
        )
        .expect("parses");
        let e = classify(&head, &target, now).expect_err("503 with a hint is rate limiting");
        assert!(
            matches!(e.class(), Class::Retryable { after: Some(after) } if after == MAX_RETRY_AFTER),
            "{e} classified as {:?}",
            e.class()
        );
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

    /// **All three of RFC 9112 §6's framings**, which is what stops a server that ignores
    /// `Connection: close` from being reported as a timeout.
    #[test]
    fn a_response_is_complete_under_each_framing() {
        // Content-Length: complete at exactly that many body bytes, and not before.
        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nsucc";
        assert!(!complete(partial), "four of seven bytes is not complete");
        let whole = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nsuccess";
        assert!(complete(whole));
        // A body longer than the declared length is complete at the length.
        assert!(complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nsuccess"
        ));

        // Chunked: complete at the terminating zero-length chunk and its blank line.
        let chunked_partial =
            b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\na\r\nInvalid ID\r\n";
        assert!(!complete(chunked_partial));
        let chunked =
            b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\na\r\nInvalid ID\r\n0\r\n\r\n";
        assert!(complete(chunked));

        // Neither: never "complete", so the loop reads to end of stream.
        assert!(!complete(b"HTTP/1.1 200 OK\r\n\r\nsuccess"));
        // And an incomplete head is never complete whatever follows.
        assert!(!complete(b"HTTP/1.1 200 OK\r\nContent-Length: 0"));
    }

    /// The chunk sizes are framing and must not reach the reason string a log line carries.
    #[test]
    fn a_chunked_body_is_decoded_before_it_becomes_an_excerpt() {
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\na\r\nInvalid ID\r\n7\r\n, sorry\r\n0\r\n\r\n",
            &target,
        )
        .expect("parses");
        assert_eq!(head.status, 400);
        assert_eq!(head.body, "Invalid ID, sorry");
        let e = classify(&head, &target, Utc::now()).expect_err("rejected");
        assert!(e.to_string().contains("Invalid ID, sorry"), "{e}");
        assert!(!e.to_string().contains("\r\n"), "{e}");

        // `Content-Length` trims trailing bytes a pipelined peer may have sent.
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nsuccessGARBAGE",
            &target,
        )
        .expect("parses");
        assert_eq!(head.body, "success");
    }

    /// A repeated `Retry-After` keeps the first parseable value, because a proxy produces
    /// both a repeat and an unparseable one.
    #[test]
    fn a_repeated_retry_after_keeps_the_first_usable_value() {
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 429 x\r\nRetry-After: 60\r\nRetry-After: soon\r\n\r\n",
            &target,
        )
        .expect("parses");
        assert_eq!(
            head.retry_after,
            Some(RetryAfter::Delta(Duration::from_secs(60)))
        );
    }

    /// **An IPv6 literal is bracketed, and the brackets are not a port separator.**
    ///
    /// `https://[::1]/submit` has no port; reading the last colon as one made it
    /// `EndpointError::Port { port: "1]" }` and a local test receiver unreachable.
    #[test]
    fn an_ipv6_literal_is_a_host_and_not_a_port() {
        let e = Endpoint::parse("https://[::1]/submit").expect("parses");
        // Stored bare, because that is what `rustls` parses as a `ServerName`...
        assert_eq!(e.host(), "::1");
        assert_eq!(e.port(), 443);
        // ...and bracketed wherever it meets the wire.
        assert_eq!(e.authority(), "[::1]");
        assert_eq!(e.to_string(), "https://[::1]/submit");
        let request = String::from_utf8(request_bytes(&e, "")).expect("utf-8");
        assert!(
            request.contains("Host: [::1]\r\n"),
            "the Host header carries the RFC 3986 form: {request}"
        );

        // An explicit port is read from after the closing bracket, and reaches the header
        // because it is not the scheme's default.
        let explicit = Endpoint::parse("https://[2001:DB8::1]:8443/submit").expect("parses");
        assert_eq!((explicit.host(), explicit.port()), ("2001:db8::1", 8443));
        let request = String::from_utf8(request_bytes(&explicit, "")).expect("utf-8");
        assert!(
            request.contains("Host: [2001:db8::1]:8443\r\n"),
            "{request}"
        );

        // A name is unaffected: the last colon is still the port separator.
        assert_eq!(
            Endpoint::parse("http://wx.lan:8080/submit")
                .expect("parses")
                .authority(),
            "wx.lan"
        );
        // And the two malformed literals are refused rather than mangled.
        assert!(matches!(
            Endpoint::parse("https://[::1/submit"),
            Err(EndpointError::HostLiteral { .. })
        ));
        assert!(matches!(
            Endpoint::parse("https://[::1]x/submit"),
            Err(EndpointError::Port { .. })
        ));
    }

    /// **A 1xx preamble is part of the exchange and not the outcome.**
    ///
    /// `103 Early Hints` in front of a `200` was read as the status, and `103` classifies
    /// `Fatal` — so a load balancer emitting hints stopped the uploader permanently on a
    /// successful upload.
    #[test]
    fn an_interim_response_is_not_the_outcome() {
        let target = endpoint().target();
        let response = b"HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\n\
                         HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nsuccess";
        let head = parse_head(response, &target).expect("parses");
        assert_eq!((head.status, head.body.as_str()), (200, "success"));
        classify(&head, &target, Utc::now()).expect("a 200 behind a 103 is a success");
        // And the read loop stops at the final response's framing rather than at the
        // preamble's absence of one.
        assert!(complete(response));

        // Two preambles, which `100 Continue` plus `103` is, and a `Retry-After` that
        // belongs to the final response.
        let head = parse_head(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\n\r\n\
              HTTP/1.1 429 Slow down\r\nRetry-After: 60\r\nContent-Length: 4\r\n\r\nwait",
            &target,
        )
        .expect("parses");
        assert_eq!(head.status, 429);
        assert_eq!(
            head.retry_after,
            Some(RetryAfter::Delta(Duration::from_secs(60)))
        );

        // A preamble that has arrived without its final response is not complete.
        assert!(!complete(b"HTTP/1.1 103 Early Hints\r\n\r\n"));
    }

    /// **RFC 9112 §6.1's status-defined framing comes before the headers.**
    ///
    /// A `304` carries the cached response's `Content-Length` and no body. Waiting for
    /// those bytes on a keep-alive connection burned the whole attempt budget and reported
    /// a successful conditional request as a timeout.
    #[test]
    fn a_status_that_defines_no_body_is_complete_at_its_head() {
        assert!(complete(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 42\r\n\r\n"
        ));
        assert!(complete(
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 1024\r\nETag: \"x\"\r\n\r\n"
        ));
        // The status is read, not guessed: a 200 with the same header still waits.
        assert!(!complete(b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\n\r\n"));
    }

    /// **A chunk size that overflows an offset must not panic.**
    ///
    /// `chunked_end` used `checked_add` and `dechunk` did not, so the same response was
    /// safe to frame and fatal to decode: `data_at + size` panics in a debug build and
    /// wraps in a release one, where `min` on the wrapped value reads out of the body.
    #[test]
    fn a_chunk_size_that_overflows_an_offset_is_refused() {
        let target = endpoint().target();
        let head = parse_head(
            b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\n\
              ffffffffffffffff\r\nInvalid ID\r\n0\r\n\r\n",
            &target,
        )
        .expect("a malicious chunk size is a short body, not a panic");
        assert_eq!(head.status, 400);
        assert!(
            head.body.is_empty(),
            "nothing is decoded from an unframeable body, got {:?}",
            head.body
        );
        // The honest prefix is still decoded when the overflow comes second.
        let head = parse_head(
            b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\n\
              a\r\nInvalid ID\r\nfffffffffffffffe\r\nx\r\n0\r\n\r\n",
            &target,
        )
        .expect("parses");
        assert_eq!(head.body, "Invalid ID");
    }

    /// **All three abrupt-close kinds end a response whose head is complete.**
    ///
    /// A middlebox that answers and then RSTs surfaces as `ConnectionReset`, which was
    /// retried as a `Connect` failure while the identical `UnexpectedEof` was tolerated.
    #[tokio::test]
    async fn an_abrupt_close_after_a_complete_head_is_the_response_ending() {
        /// Yields its scripted bytes once, then fails with `kind`.
        struct Scripted {
            bytes: Option<Vec<u8>>,
            kind: std::io::ErrorKind,
        }
        impl fmt::Debug for Scripted {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "Scripted({:?})", self.kind)
            }
        }
        impl ByteStream for Scripted {
            fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
                Box::pin(async move {
                    match self.bytes.take() {
                        Some(bytes) => {
                            buf[..bytes.len()].copy_from_slice(&bytes);
                            Ok(bytes.len())
                        }
                        None => Err(std::io::Error::new(self.kind, "scripted")),
                    }
                })
            }
            fn write_all<'a>(&'a mut self, _: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
                Box::pin(async move { Ok(()) })
            }
        }

        let target = endpoint().target();
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            let mut wire = Scripted {
                bytes: Some(b"HTTP/1.1 200 OK\r\n\r\nsuccess".to_vec()),
                kind,
            };
            let response = read_response(&mut wire, &target, MAX_RESPONSE_BYTES)
                .await
                .unwrap_or_else(|e| panic!("{kind:?} after a complete head is the end: {e}"));
            let head = parse_head(&response, &target).expect("parses");
            assert_eq!(
                (head.status, head.body.as_str()),
                (200, "success"),
                "{kind:?}"
            );
        }

        // Before the head is complete every one of them is still a failure: a truncated
        // response is not a response.
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            let mut wire = Scripted {
                bytes: Some(b"HTTP/1.1 200 OK\r\nContent-".to_vec()),
                kind,
            };
            let e = read_response(&mut wire, &target, MAX_RESPONSE_BYTES)
                .await
                .expect_err("a truncated head is a failure");
            assert!(matches!(e, UploadError::Connect { .. }), "{kind:?}: {e}");
        }
    }

    /// The connect budget is strictly inside the attempt's, or its diagnosis is
    /// unreachable.
    #[test]
    fn the_connect_timeout_is_inside_the_attempt_timeout() {
        assert!(
            CONNECT_TIMEOUT < crate::uploader::ATTEMPT_TIMEOUT,
            "{CONNECT_TIMEOUT:?} vs {:?}",
            crate::uploader::ATTEMPT_TIMEOUT
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
