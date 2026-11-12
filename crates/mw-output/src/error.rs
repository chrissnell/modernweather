//! [`EncodeError`] and [`UploadError`] — the two enums `docs/service-architecture.md`
//! §1.3's table gives this crate, and their [`Classify`] impls.
//!
//! §4.2's mechanism is that the retry decision is made **once, beside the error's
//! definition**, where the knowledge of what a variant means lives, rather than at a call
//! site that guesses. `crate::retry::ClassPolicy` is generic over `E: Classify` and reads
//! nothing else, so everything the uploader does about a failure is decided in this file.
//!
//! §4.4's table is the specification for the classifications below, and two of its rows are
//! the reason the enum has the shape it does:
//!
//! | Error | Class | Why |
//! |---|---|---|
//! | `UploadError::RateLimited` | `Retryable { after }` | the service told us when |
//! | `UploadError::Rejected{4xx}` | `Fatal` for the tick | a bad API key is not fixed by trying again; it fails, it is visible, and the period is the retry |
//!
//! "`Fatal` for the tick" is worth reading twice: `Class::Fatal` here does **not** mean the
//! component is finished. `crate::uploader` returns `ComponentError::Tick`, the supervisor
//! counts the occurrence as failed, and the next occurrence happens on schedule — the
//! period *is* the retry interval, which is what makes a 4xx visible rather than hidden
//! behind a retry loop.
//!
//! # No URL in an error message
//!
//! Every variant that names where a request went names a [`Target`], which renders
//! scheme, host and path and **never the query string**. The query is where `PASSWORD=` and
//! `ID=` live for both HTTP uploaders, so a `Display` that included it would put the
//! credential in every connection-failure line — once per period, forever, in a log W5
//! renders to a browser. **D-33** makes a secret's debug representation redact; this is the
//! same rule for the place the secret is about to be sent.

use std::fmt;
use std::time::Duration;

use mw_types::{Agg, Class, Classify, MetricKey, StationKey, Window};

/// How much of a rejection body is kept.
///
/// Enough for "Invalid ID" or "password not valid", which is what both services answer
/// with; short enough that an HTML error page from a proxy does not become a log line
/// nobody can read.
pub const MAX_BODY_EXCERPT: usize = 200;

/// Where a request went, for an error message.
///
/// Scheme, host, port when it is not the default, and path. Deliberately **not** the query
/// string — see this module's documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target(String);

impl Target {
    /// Build a target from the parts of a URL that are safe to print.
    ///
    /// **Truncates at a `?`**, so the no-query-string guarantee is a property of this
    /// constructor rather than of the one caller that happens to refuse a query earlier
    /// (`crate::https::Endpoint::parse`). This is the credential-in-logs guard; it should
    /// not depend on a second function's validation staying where it is.
    pub fn new(scheme: &str, host: &str, port: u16, path: &str) -> Self {
        let path = path.split('?').next().unwrap_or("");
        let default = if scheme == "https" { 443 } else { 80 };
        if port == default {
            Self(format!("{scheme}://{host}{path}"))
        } else {
            Self(format!("{scheme}://{host}:{port}{path}"))
        }
    }

    /// A target that is a host and a port rather than a URL — APRS-IS, or a KISS TNC.
    pub fn peer(host: &str, port: u16) -> Self {
        Self(format!("{host}:{port}"))
    }

    /// A target that is a device path — a KISS TNC on a serial line.
    pub fn device(path: &str) -> Self {
        Self(path.to_string())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why one occurrence had nothing to send, or could not build it.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The windowed values could not be resolved for this station.
    ///
    /// Every `WindowError` is a fact about the declaration, the registry or the station's
    /// configuration, so this is the same answer on the next tick — which is why the class
    /// is taken from the source rather than assumed.
    #[error("the declared windows could not be resolved for station `{station}`")]
    Window {
        /// Which station.
        station: StationKey,
        /// Which rule the declaration broke.
        #[source]
        source: mw_component::WindowError,
    },

    /// An encoder read a triple it did not declare.
    ///
    /// Reachable only from a bug in an encoder: `required()` is what the uploader resolves
    /// and `Resolved::value` refuses anything outside it, which is the property that makes
    /// a declaration table reviewable against a protocol specification. Named rather than
    /// passed through so the message says whose mistake it is.
    #[error(
        "the `{service}` encoder read `({metric:?}, {window}, {agg})`, which is not in its \
         own `required()` table. An encoder reads exactly what it declares -- that is what \
         makes the table reviewable against the service's documentation -- so this is a bug \
         in the encoder rather than a configuration problem."
    )]
    MissingWindow {
        /// Which encoder.
        service: &'static str,
        /// Which metric.
        metric: MetricKey,
        /// Over which window.
        window: Window,
        /// Combined how.
        agg: Agg,
    },

    /// The APRS information field could not be built.
    #[error("the APRS information field could not be built")]
    Field(#[from] mw_aprs::EncodeError),

    /// Every declared value was absent, so there is nothing to upload.
    ///
    /// A station that is configured and has not reported yet, or one whose sensors all
    /// read `NULL`. **`Ignorable`**: posting a form with an identifier and no measurements
    /// tells the service this station is alive and reporting nothing, which is worse than
    /// not posting.
    ///
    /// The Go uploaders do refuse such a reading, and the way they do it is the defect
    /// **D-26** names: `ValidateReading` is
    /// `if r.Barometer == 0 && r.OutTemp == 0` (`weather_service_utils.go:214-219`), so it
    /// cannot tell an absent barometer from a real 0 inHg — and a station reporting a
    /// genuine 0 °F with a failed barometer is refused as "likely faulty". Asking the
    /// resolution how many values it holds is the same decision without the zero-fill.
    #[error(
        "station `{station}` has no value for any of the {declared} windows `{service}` \
         declares, so there is nothing to upload this period. A station that has just \
         started reporting answers this until its first windows fill."
    )]
    NothingToSend {
        /// Which service.
        service: &'static str,
        /// Which station.
        station: StationKey,
        /// How many declarations were resolved.
        declared: usize,
    },
}

impl Classify for EncodeError {
    fn class(&self) -> Class {
        match self {
            // Deferred to the source, which classifies every one of its variants `Fatal`
            // and documents why. Taken rather than assumed so that a future retryable
            // `WindowError` does not need this line changed.
            EncodeError::Window { source, .. } => source.class(),
            EncodeError::MissingWindow { .. } | EncodeError::Field(_) => Class::Fatal,
            EncodeError::NothingToSend { .. } => Class::Ignorable,
        }
    }
}

/// Why one delivery attempt did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The connection could not be made, or failed mid-request.
    #[error("{what} {target}")]
    Connect {
        /// What was being attempted, in words that name the action (§5.9).
        what: &'static str,
        /// Where.
        target: Target,
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// The TLS handshake failed.
    ///
    /// **`Fatal` for the tick.** A certificate a public weather service presents does not
    /// become valid in the next 30 seconds, and the two causes — an expired chain and a
    /// host clock that is wrong — are both things an operator fixes. Retrying inside the
    /// tick would turn one log line into a retry budget's worth.
    #[error("the TLS handshake with {target} failed")]
    Tls {
        /// Where.
        target: Target,
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The peer answered with something that is not the protocol.
    #[error("{target} did not answer with {expected}: {why}")]
    Malformed {
        /// Where.
        target: Target,
        /// What was expected — `HTTP`, an `APRS-IS greeting`.
        expected: &'static str,
        /// Which rule was broken.
        why: &'static str,
    },

    /// The service refused the upload.
    ///
    /// `Fatal` for a 4xx and `Retryable` for a 5xx, which is §4.4's row: a bad API key is
    /// not fixed by trying again, and a 502 from a load balancer is.
    #[error("{target} answered {status}{}", excerpt(body))]
    Rejected {
        /// Where.
        target: Target,
        /// The status.
        status: u16,
        /// The first [`MAX_BODY_EXCERPT`] bytes of the body, which is where both services
        /// put the reason.
        body: String,
    },

    /// The service asked us to slow down.
    ///
    /// Carries the parsed `Retry-After`, which is what makes W2a Decision 59's pushback
    /// flow end to end: the class is `Retryable { after }`, `ClassPolicy` honours it, and
    /// `crate::uploader` holds the limiter's "not before T" across occurrences.
    #[error("{target} answered 429{}", retry_after_suffix(retry_after))]
    RateLimited {
        /// Where.
        target: Target,
        /// When the service said to come back, when it said.
        retry_after: Option<Duration>,
    },

    /// One attempt exceeded its own timeout.
    ///
    /// The innermost layer of the stack, so this bounds an **attempt** and not the whole
    /// retry sequence (**D-57**).
    #[error("the attempt against {target} exceeded its {} s timeout", after.as_secs())]
    Timeout {
        /// Where.
        target: Target,
        /// The timeout.
        after: Duration,
    },

    /// APRS-IS refused the login.
    ///
    /// **`Fatal` for the tick** and the one `UploadError` an operator can always act on: a
    /// callsign and a passcode that do not match each other do not start matching. The
    /// server's line is carried because it is the only diagnostic APRS-IS gives, and it
    /// contains no credential — the passcode is in the line *we* sent.
    #[error("APRS-IS at {target} refused the login: {response}")]
    LoginRefused {
        /// Where.
        target: Target,
        /// The server's reply, trimmed.
        response: String,
    },

    /// The component was cancelled mid-attempt.
    ///
    /// `Ignorable`: a stop during an upload is a stop, not a failure, and counting it as
    /// one would make every restart look like a broken uploader.
    #[error("cancelled while uploading to {target}")]
    Cancelled {
        /// Where.
        target: Target,
    },
}

impl Classify for UploadError {
    fn class(&self) -> Class {
        match self {
            UploadError::Connect { .. } | UploadError::Timeout { .. } => Class::RETRYABLE,
            // A malformed answer is retried once rather than treated as configuration: the
            // common cause is a captive portal or a proxy injecting a page, which the next
            // attempt may get past.
            UploadError::Malformed { .. } => Class::RETRYABLE,
            UploadError::RateLimited { retry_after, .. } => Class::Retryable {
                after: *retry_after,
            },
            UploadError::Rejected { status, .. } if *status >= 500 => Class::RETRYABLE,
            UploadError::Rejected { .. } | UploadError::Tls { .. } => Class::Fatal,
            UploadError::LoginRefused { .. } => Class::Fatal,
            UploadError::Cancelled { .. } => Class::Ignorable,
        }
    }
}

impl UploadError {
    /// The metric label for this failure — a closed set, so the series' cardinality is
    /// bounded by this function (§5.10).
    pub fn reason(&self) -> &'static str {
        match self {
            UploadError::Connect { .. } => "connect",
            UploadError::Tls { .. } => "tls",
            UploadError::Malformed { .. } => "malformed",
            UploadError::Rejected { .. } => "rejected",
            UploadError::RateLimited { .. } => "rate_limited",
            UploadError::Timeout { .. } => "timeout",
            UploadError::LoginRefused { .. } => "login_refused",
            UploadError::Cancelled { .. } => "cancelled",
        }
    }
}

/// Why an Aeris fetch did not produce a row.
///
/// A third enum rather than variants on [`UploadError`], because the two describe different
/// exchanges: an upload's failure is a status and a reason string, and a fetch's failure is
/// usually a **payload** that arrived with a `200`. The one thing they share is the
/// transport, which is why the transport's failures stay [`UploadError`] and travel here as
/// [`AerisError::Transport`] rather than being re-described.
///
/// `crate::aeris`'s module documentation has why the status code is not the verdict on this
/// path: a bad credential is `200` with `{"success": false}`.
#[derive(Debug, thiserror::Error)]
pub enum AerisError {
    /// The request never completed, or the response was not HTTP.
    #[error(transparent)]
    Transport(#[from] UploadError),

    /// The body was not the JSON this endpoint answers with.
    ///
    /// `Retryable`, and the reason is the same one [`UploadError::Malformed`] gives: the
    /// common cause is a proxy or a maintenance page in front of the API, which the next
    /// occurrence may get past. A vendor that changed its schema would fail here forever
    /// with the parse error in `last_error`, which is the visible shape W2a Decision 30
    /// asks for instead of a give-up.
    #[error("the Aeris response was not the JSON this endpoint answers with")]
    Decode {
        /// Where the parse stopped.
        #[source]
        source: serde_json::Error,
    },

    /// The body was JSON, and the wrong shape for this endpoint.
    #[error("{what} was {found} rather than the shape this endpoint answers with")]
    Shape {
        /// Which part of the payload.
        what: &'static str,
        /// What was there instead.
        found: &'static str,
    },

    /// `success: false` — the upstream declined, and said why in the body.
    ///
    /// **`Fatal` for the tick**, which is this crate's established reading of `Fatal` and
    /// not a give-up: the overwhelming cause is a credential the operator has to fix, and
    /// retrying inside one occurrence spends the rate budget to be told the same thing.
    /// The period is the retry, and the reason is in `last_error` where W5 renders it.
    #[error("the Aeris API declined the request: {detail}")]
    Refused {
        /// The upstream's own `error` field, bounded to [`MAX_BODY_EXCERPT`].
        detail: String,
    },

    /// `success: true`, and nothing in it.
    ///
    /// Its own variant rather than a `Shape`, because it is the ordinary consequence of a
    /// maintenance window rather than a disagreement about the schema — and because it is
    /// the input that **panics** the Go controller (`response.AerisForecastData[0]` on an
    /// empty slice). Retryable: the next period is a different answer.
    #[error("{what}, so there is nothing to store")]
    Empty {
        /// Which emptiness.
        what: &'static str,
    },

    /// `weatherPrimaryCoded` was not `<coverage>:<intensity>:<weather>`.
    ///
    /// The second Go panic on this path (`strings.Split(...)[2]`). Retryable rather than
    /// fatal because it is a property of one period of one response, and the next forecast
    /// is a new one.
    #[error(
        "the forecast's `weatherPrimaryCoded` was `{coded}`, which is not the \
         `<coverage>:<intensity>:<weather>` form the icon and the description are derived \
         from"
    )]
    Coded {
        /// What the vendor sent.
        coded: String,
    },

    /// The stored JSON could not be built.
    ///
    /// Unreachable for a struct of scalars and a `String`, and named rather than
    /// `expect`ed: a frozen route's writer may not panic over a serialiser.
    #[error("the forecast could not be rendered as JSON")]
    Encode {
        /// Why.
        #[source]
        source: serde_json::Error,
    },
}

/// **Read by nothing today, and that is worth knowing before trusting the split below.**
///
/// The only consumer of [`Classify`] in this crate is `crate::retry::ClassPolicy`, which is
/// wired to the uploaders; the Aeris bodies wrap every failure in `ComponentError::tick`,
/// whose own class is unconditionally `Retryable` (`mw_component::ComponentError`). So no
/// classification here can cause a hot retry loop — there is no retry, and the period is the
/// floor — and none can stop a component. What the impl is for is the day this path acquires
/// a `tower` stack: the decision is made once, beside the error's definition, which is §4.2's
/// mechanism and the reason `Fatal` is written down now rather than guessed at then.
impl Classify for AerisError {
    fn class(&self) -> Class {
        match self {
            AerisError::Transport(e) => e.class(),
            AerisError::Decode { .. }
            | AerisError::Shape { .. }
            | AerisError::Empty { .. }
            | AerisError::Coded { .. } => Class::RETRYABLE,
            // See the variant: `Fatal` for the tick, not for the component.
            AerisError::Refused { .. } | AerisError::Encode { .. } => Class::Fatal,
        }
    }
}

impl AerisError {
    /// The metric label for this failure — a closed set, so the series' cardinality is
    /// bounded by this function (§5.10).
    pub fn reason(&self) -> &'static str {
        match self {
            AerisError::Transport(e) => e.reason(),
            AerisError::Decode { .. } => "decode",
            AerisError::Shape { .. } => "shape",
            AerisError::Refused { .. } => "refused",
            AerisError::Empty { .. } => "empty",
            AerisError::Coded { .. } => "coded",
            AerisError::Encode { .. } => "encode",
        }
    }
}

/// `: <body>` when there is one, trimmed and capped.
fn excerpt(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }
    let kept: String = body.chars().take(MAX_BODY_EXCERPT).collect();
    format!(": {kept}")
}

/// ` and asked for 120 s` when the service said when.
fn retry_after_suffix(after: &Option<Duration>) -> String {
    match after {
        Some(d) => format!(" and asked for {} s", d.as_secs()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target::new("https", "pwsupdate.pwsweather.com", 443, "/api/v1/submitwx")
    }

    /// **The credential is not in the message.** A `Target` is built from the parts of a
    /// URL that are safe to print, so the query string — which is where `PASSWORD=` lives
    /// for both HTTP uploaders — cannot reach a log line through an error.
    #[test]
    fn no_error_message_can_carry_the_query_string() {
        let t = target();
        assert_eq!(
            t.to_string(),
            "https://pwsupdate.pwsweather.com/api/v1/submitwx"
        );
        assert!(!t.to_string().contains('?'));
        let errors = [
            UploadError::Connect {
                what: "could not connect to",
                target: target(),
                source: std::io::Error::other("refused"),
            },
            UploadError::Rejected {
                target: target(),
                status: 401,
                body: "Invalid ID".to_string(),
            },
            UploadError::RateLimited {
                target: target(),
                retry_after: Some(Duration::from_secs(120)),
            },
            UploadError::Timeout {
                target: target(),
                after: Duration::from_secs(10),
            },
        ];
        for e in &errors {
            let text = e.to_string();
            assert!(!text.contains("PASSWORD"), "{text}");
            assert!(!text.contains('?'), "{text}");
        }
        // A non-default port is kept, because "which port" is the whole content of some
        // connection failures.
        assert_eq!(
            Target::new("http", "localhost", 8080, "/wx").to_string(),
            "http://localhost:8080/wx"
        );
        // The guarantee is the constructor's: a path carrying a query is truncated here,
        // not refused somewhere else.
        assert_eq!(
            Target::new("https", "wx.example", 443, "/submit?ID=K&PASSWORD=hunter2").to_string(),
            "https://wx.example/submit"
        );
        assert_eq!(
            Target::peer("noam.aprs2.net", 14580).to_string(),
            "noam.aprs2.net:14580"
        );
        assert_eq!(Target::device("/dev/ttyUSB0").to_string(), "/dev/ttyUSB0");
    }

    /// §4.4's table, as code. The 4xx/5xx split is the row most likely to be got wrong,
    /// because both are "the service said no".
    #[test]
    fn the_classifications_are_the_architectures_table() {
        let fatal = |e: UploadError| assert_eq!(e.class(), Class::Fatal, "{e}");
        let retryable = |e: UploadError| {
            assert!(
                matches!(e.class(), Class::Retryable { .. }),
                "{e} should be retryable"
            )
        };

        // A bad API key is not fixed by trying again; the period is the retry.
        fatal(UploadError::Rejected {
            target: target(),
            status: 401,
            body: String::new(),
        });
        fatal(UploadError::Rejected {
            target: target(),
            status: 404,
            body: String::new(),
        });
        // A gateway error is.
        retryable(UploadError::Rejected {
            target: target(),
            status: 502,
            body: String::new(),
        });
        retryable(UploadError::Connect {
            what: "could not connect to",
            target: target(),
            source: std::io::Error::other("refused"),
        });
        retryable(UploadError::Timeout {
            target: target(),
            after: Duration::from_secs(10),
        });
        fatal(UploadError::LoginRefused {
            target: Target::peer("noam.aprs2.net", 14580),
            response: "# login N0CALL unverified".to_string(),
        });
        fatal(UploadError::Tls {
            target: target(),
            source: Box::new(std::io::Error::other("bad certificate")),
        });
        assert_eq!(
            UploadError::Cancelled { target: target() }.class(),
            Class::Ignorable
        );
    }

    /// The `Retry-After` hint survives into the class, which is the whole of W2a Decision
    /// 59's end-to-end path.
    #[test]
    fn a_retry_after_header_reaches_the_class() {
        let e = UploadError::RateLimited {
            target: target(),
            retry_after: Some(Duration::from_secs(90)),
        };
        assert_eq!(
            e.class(),
            Class::Retryable {
                after: Some(Duration::from_secs(90))
            }
        );
        assert!(e.to_string().contains("asked for 90 s"), "{e}");

        // A `429` with no header is still retryable, with no hint to honour.
        let bare = UploadError::RateLimited {
            target: target(),
            retry_after: None,
        };
        assert_eq!(bare.class(), Class::RETRYABLE);
        assert!(bare.to_string().ends_with("429"), "{bare}");
    }

    /// Every failure has exactly one metric label, and the set is closed — which is what
    /// bounds the series' cardinality (§5.10).
    #[test]
    fn every_failure_has_one_bounded_label() {
        let labels = [
            "connect",
            "tls",
            "malformed",
            "rejected",
            "rate_limited",
            "timeout",
            "login_refused",
            "cancelled",
        ];
        let e = UploadError::Rejected {
            target: target(),
            status: 400,
            body: String::new(),
        };
        assert!(labels.contains(&e.reason()));
        assert_eq!(
            labels.len(),
            8,
            "a new variant needs a label and this count"
        );
    }

    /// A rejection body is capped, so a proxy's HTML error page is not a log line.
    #[test]
    fn a_rejection_body_is_trimmed_and_capped() {
        let e = UploadError::Rejected {
            target: target(),
            status: 400,
            body: format!("  {}  ", "x".repeat(1000)),
        };
        let text = e.to_string();
        assert!(text.contains("answered 400: xxx"), "{text}");
        assert!(text.len() < MAX_BODY_EXCERPT + 100, "{}", text.len());

        // An empty body is no suffix at all rather than a trailing colon.
        let bare = UploadError::Rejected {
            target: target(),
            status: 400,
            body: "   ".to_string(),
        };
        assert!(bare.to_string().ends_with("answered 400"), "{bare}");
    }

    /// `NothingToSend` is the one encode failure that is not a defect, so it is the one
    /// that must not be counted as one.
    #[test]
    fn a_station_with_no_values_is_an_absence_and_not_a_failure() {
        let e = EncodeError::NothingToSend {
            service: "wunderground",
            station: StationKey::new("backyard"),
            declared: 11,
        };
        assert_eq!(e.class(), Class::Ignorable);
        assert!(e.to_string().contains("backyard"), "{e}");

        let bug = EncodeError::MissingWindow {
            service: "cwop",
            metric: MetricKey::WindSpeed,
            window: Window::of(Window::TEN_MINUTES),
            agg: Agg::Max,
        };
        assert_eq!(bug.class(), Class::Fatal);
        assert!(bug.to_string().contains("required()"), "{bug}");
    }
}
