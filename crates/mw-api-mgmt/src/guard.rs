//! Everything that happens to a management request **before** a handler sees it, and the
//! headers that go on the way out.
//!
//! W5 §3.3's CSRF pair, §3.5's CORS allowlist and security headers, and the session gate the
//! whole of §3 exists to put in front of the surface implementation-plan.md **M7-1**
//! shipped without one. Five steps, in this order, in one function:
//!
//! | | | |
//! |---|---|---|
//! | 1 | CORS preflight | an `OPTIONS` from an allowlisted origin is answered here and never reaches a route |
//! | 2 | CSRF | a state-changing method without `X-MW-Request` is refused |
//! | 3 | resolve | the cookie becomes an `Option<`[`Current`]`>` in the request's extensions |
//! | 4 | require | everything but the three unauthenticated routes needs a session |
//! | 5 | headers | the security headers of §3.5, and the CORS echo, on every response |
//!
//! # Why one function rather than five layers
//!
//! Because the order is the security property and a `tower` stack states it backwards. Three
//! of these steps are only correct in a particular position -- the CSRF check has to precede
//! the handler *and* be independent of whether the session resolved, the preflight has to
//! short-circuit before the session gate can `401` it, and the headers have to be added even
//! to the responses the earlier steps generate themselves. Written as layers, each of those
//! is a `.layer()` call whose position a later reader cannot tell is load-bearing.
//!
//! # What is deliberately not here
//!
//! **No `WWW-Authenticate`.** The `401` is for an SPA with a login screen, and that header
//! makes a browser open its own dialog over it.
//!
//! **No origin reflection, ever.** [`Guard::cors`] is an exact-match list and the value this
//! writes into `Access-Control-Allow-Origin` is **the entry from the list**, not the header
//! that was presented. The Go version's `Access-Control-Allow-Origin: *` on the admin API is
//! what §3.1 lists as a defect; a reflected `Origin` is the same defect with more steps.
//!
//! **No rate limit on anything but login.** §3.5 scopes the progressive delay to
//! authentication, and a request that already carries a valid session is the operator.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::{self, AuthStore, Session};
use crate::error::ApiError;

/// The three routes a caller with no session may reach.
///
/// **An exact-match list, checked against `uri().path()`**, so the default is closed: a route
/// added later is authenticated until somebody puts it here on purpose, which is the opposite
/// of a prefix like `/api/v1/auth/` (under which `DELETE /api/v1/auth/sessions` -- "sign out
/// everywhere" -- would have been reachable by anyone who could reach the port).
///
/// Each one has to be unauthenticated to do its job:
///
/// - `GET /api/v1/auth/session` is how a client learns whether to show a login box, a setup
///   screen or the application (§1.1), and the answer on a cold browser is "you have no
///   session" -- which cannot be a `401`, because that is the question.
/// - `POST /api/v1/auth/login` is what produces a session.
/// - `POST /api/v1/auth/claim` is the first-run claim, on an instance where no credential
///   exists to authenticate against (§3.4).
///
/// Spelled as `(method, path)` strings rather than as `http::Method` constants because
/// `Method` has drop glue and so cannot be promoted into a `const` array. The comparison is
/// against `Method::as_str()`, which is exact and allocates nothing.
pub(crate) const UNAUTHENTICATED: &[(&str, &str)] = &[
    ("GET", "/api/v1/auth/session"),
    ("POST", "/api/v1/auth/login"),
    ("POST", "/api/v1/auth/claim"),
];

/// Where the request came from, as a handler reads it off the request.
///
/// `None` when the serving path did not supply one, which is every `oneshot` test and would
/// be any future transport that is not a socket. Inserted **unconditionally** by [`enforce`],
/// so a handler takes `Extension<Peer>` rather than `Option<Extension<Peer>>` and the
/// "we did not learn the address" case is a `None` inside the value rather than a missing
/// extractor -- `admin_session.created_ip` is nullable for exactly that reason.
///
/// Rendered as the address without its port: §3.4 wants the claim's **source IP** in the
/// audit line and §3.3 wants `created_ip` for the session list, and an ephemeral port in
/// either is noise that changes on every request.
#[derive(Debug, Clone, Default)]
pub struct Peer(pub Option<String>);

/// The signed-in administrator, as a handler reads it off the request.
///
/// Inserted by [`enforce`] into the request's extensions, so a handler that needs the
/// current session takes `Extension<Current>` and one that does not is unchanged. The gate
/// has already run by then: a handler on an authenticated route can treat its presence as
/// given, which is why the extractor below is for [`Current`] and not for `Option<Current>`.
#[derive(Debug, Clone)]
pub struct Current {
    /// The administrator's username.
    pub username: String,
    /// The session this request arrived on.
    pub session: Session,
}

/// What the gate needs in order to answer.
pub struct Guard {
    /// The credential and session store.
    auth: Arc<dyn AuthStore>,
    /// The exact-match CORS allowlist (§3.5). **Empty by default**, which means same-origin
    /// only and is correct for an embedded SPA -- `management_cors_origin` has no seeded row,
    /// so the default is the absence of data rather than a sentinel.
    cors: Vec<String>,
    /// Whether this socket is TLS, which decides the cookie's name and its `Secure`
    /// attribute (§3.3).
    secure: bool,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("allowed_origins", &self.cors.len())
            .field("secure", &self.secure)
            .finish_non_exhaustive()
    }
}

impl Guard {
    /// Assemble it. The binary is the only caller outside tests.
    ///
    /// `cors` is taken as the operator configured it and **sorted here**, because the lookup
    /// is a binary search and a caller that passed an unsorted list would get an allowlist
    /// that silently refused half its own entries.
    pub fn new(auth: Arc<dyn AuthStore>, mut cors: Vec<String>, secure: bool) -> Self {
        cors.sort();
        cors.dedup();
        Self { auth, cors, secure }
    }

    /// Whether this socket is TLS, so the routes can name the cookie correctly.
    pub fn secure(&self) -> bool {
        self.secure
    }

    /// The store, for the handlers.
    pub fn store(&self) -> &Arc<dyn AuthStore> {
        &self.auth
    }

    /// The allowlist entry matching a presented `Origin`, if any.
    ///
    /// Returns the **stored** entry rather than the presented header, which is what keeps
    /// this from being a reflection: the bytes that reach
    /// `Access-Control-Allow-Origin` came out of the configuration.
    fn allowed(&self, origin: &str) -> Option<&str> {
        self.cors
            .binary_search_by(|entry| entry.as_str().cmp(origin))
            .ok()
            .map(|at| self.cors[at].as_str())
    }
}

/// Whether a method changes state, and therefore needs the CSRF header (§3.3).
///
/// `GET`, `HEAD` and `OPTIONS` are the safe methods; everything else on this surface is a
/// write. **`TRACE` and `CONNECT` count as unsafe** rather than being enumerated as safe,
/// which is the fail-closed direction: this list is what a future method is measured
/// against, and a method nobody has thought about should need the header.
fn changes_state(method: &Method) -> bool {
    !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
}

/// The management API's gate — steps 1 to 5 of this module's table.
///
/// Installed by [`crate::routes::router`] with `axum::middleware::from_fn_with_state`, which
/// hands the state over as the first argument directly rather than through a `State`
/// extractor -- hence the bare `Arc<Guard>`.
pub async fn enforce(
    axum::extract::State(guard): axum::extract::State<Arc<Guard>>,
    mut request: Request,
    next: Next,
) -> Response {
    let origin = header_string(&request, header::ORIGIN);
    let allowed = origin
        .as_deref()
        .and_then(|o| guard.allowed(o))
        .map(str::to_string);
    let allowed = allowed.as_deref();

    // The peer address, for `created_ip` and the audit line. Taken from the extension
    // `mw_http`'s serving path installs; see [`Peer`] for why it is always inserted.
    let peer = Peer(
        request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|info| info.0.ip().to_string()),
    );
    request.extensions_mut().insert(peer);

    // ---- 1. The preflight, answered here.
    //
    // Before everything else, because a browser sends it with no cookie and no custom
    // header: a preflight that went through the CSRF check would be refused for lacking the
    // very header it is asking permission to send.
    if *request.method() == Method::OPTIONS {
        return preflight(allowed);
    }

    // ---- 2. CSRF: `SameSite=Strict` **plus** a required header, both (§3.3).
    //
    // Independent of the session, so that a cross-site form post is refused before anything
    // reads a cookie. A form post cannot set this header without a preflight, and a
    // preflight from an origin that is not on the allowlist gets no permission above.
    if changes_state(request.method()) && request.headers().get(auth::CSRF_HEADER).is_none() {
        return decorate(
            ApiError::Invalid {
                summary: "That request did not come from the management interface.".to_string(),
                errors: Vec::new(),
            }
            .into_response(),
            allowed,
        );
    }

    // ---- 3. Resolve the cookie, if there is one.
    //
    // Attempted on every request including the unauthenticated ones, because
    // `GET /api/v1/auth/session` has to be able to say "yes, you are signed in" without
    // being behind the gate that would have refused it.
    // The cookie is read out of the request **before** the await rather than inside
    // `resolve`, and that is a borrow-checking fact rather than a style choice: holding a
    // `&Request` across an `.await` would require `axum::body::Body` to be `Sync`, which it
    // is not, and the resulting future would not be `Send`.
    let cookies = header_string(&request, header::COOKIE);
    let current = match resolve(&guard, cookies.as_deref()).await {
        Ok(current) => current,
        Err(e) => return decorate(e.into_response(), allowed),
    };

    // ---- 4. Require one, unless this is one of the three.
    let exempt = UNAUTHENTICATED.iter().any(|(method, path)| {
        *method == request.method().as_str() && *path == request.uri().path()
    });
    if current.is_none() && !exempt {
        return decorate(
            ApiError::Unauthenticated {
                summary: "Sign in to change this daemon's settings.".to_string(),
            }
            .into_response(),
            allowed,
        );
    }
    if let Some(current) = current {
        request.extensions_mut().insert(current);
    }

    // ---- 5. The response, with the headers of §3.5.
    decorate(next.run(request).await, allowed)
}

/// Read one header as a `String`, or `None` when it is absent or not text.
fn header_string(request: &Request, name: header::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Turn the presented cookie into the signed-in administrator, if it is one.
async fn resolve(guard: &Guard, cookies: Option<&str>) -> Result<Option<Current>, ApiError> {
    let Some(cookies) = cookies else {
        return Ok(None);
    };
    let Some(value) = auth::cookie_in(cookies) else {
        return Ok(None);
    };
    let Some(presented) = auth::Presented::parse(value) else {
        return Ok(None);
    };
    let Some(session) = guard.auth.resolve(presented).await? else {
        return Ok(None);
    };
    // The username comes off the credential rather than out of the session row, because
    // there is one administrator (**D-35**) and a username copied onto every session would be
    // a second answer to who this is -- one that a rename through
    // `modernweather reset-password --username` would leave stale.
    let Some(credential) = guard.auth.credential().await? else {
        return Ok(None);
    };
    Ok(Some(Current {
        username: credential.username,
        session,
    }))
}

/// The preflight response.
///
/// `204` with permission when the origin is allowlisted, `403` with **no CORS headers at
/// all** when it is not -- which is what makes the refusal visible in the browser console as
/// a CORS failure rather than as a mysterious network error.
fn preflight(allowed: Option<&str>) -> Response {
    let Some(origin) = allowed else {
        return decorate(StatusCode::FORBIDDEN.into_response(), None);
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    // Every method this surface has. Spelled out rather than echoing
    // `Access-Control-Request-Method`, for `allowed`'s reason: a reflected value is not an
    // allowlist.
    insert(
        headers,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, PUT, DELETE, OPTIONS",
    );
    // `content-type` because every write is JSON, and the CSRF header because a cross-origin
    // client that is allowed to be here still has to be able to send it.
    insert(
        headers,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "content-type, x-mw-request",
    );
    // Ten minutes. Long enough that a page's writes do not each cost a preflight, short
    // enough that removing an origin from the allowlist takes effect while the operator is
    // still watching.
    insert(headers, header::ACCESS_CONTROL_MAX_AGE, "600");
    decorate(response, Some(origin))
}

/// Add the security headers of §3.5, and the CORS echo when there is one.
fn decorate(mut response: Response, allowed: Option<&str>) -> Response {
    let headers = response.headers_mut();
    // §3.5, verbatim. `'self'` throughout and no `unsafe-inline`, which the SPA does not
    // need: W5 §6.4 embeds the bundle with `rust-embed` and there is no CDN and no inline
    // script. `frame-ancestors 'none'` is the clickjacking answer and replaces
    // `X-Frame-Options`, which it supersedes.
    insert(
        headers,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    );
    insert(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert(headers, header::REFERRER_POLICY, "no-referrer");
    // Not in §3.5's list and added for one specific reason: every response on this surface
    // is derived from the operator's own configuration, and a shared cache between the
    // browser and a session cookie is how one gets served to the next request.
    insert(headers, header::CACHE_CONTROL, "no-store");
    if let Some(origin) = allowed {
        insert(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        // Credentials, because the session is a cookie: without this an allowlisted
        // cross-origin client would be permitted to make the request and would then send it
        // without the cookie, which is a `401` the operator cannot diagnose.
        insert(headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
        // The response varies by `Origin`, so a cache that holds one must not serve it to
        // another.
        insert(headers, header::VARY, "Origin");
    }
    response
}

/// Insert a header, skipping a value that is not a legal one.
///
/// Every call site above passes a literal except the CORS origin, which came out of the
/// configuration -- and `management_cors_origin`'s `CHECK` constraints already forbid a
/// space, a comma and a control character. This is the belt: a header value this could not
/// build is dropped rather than panicking a request.
fn insert(headers: &mut header::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(origins: &[&str]) -> Guard {
        Guard::new(
            Arc::new(crate::auth::fake::Store::unclaimed()) as Arc<dyn AuthStore>,
            origins.iter().map(|o| (*o).to_string()).collect(),
            false,
        )
    }

    /// The allowlist is exact and the value that reaches the header is the **stored** one.
    #[test]
    fn the_cors_allowlist_is_exact_and_never_reflects() {
        let g = guard(&["https://weather.example.com", "http://localhost:5173"]);
        assert_eq!(
            g.allowed("https://weather.example.com"),
            Some("https://weather.example.com")
        );
        assert_eq!(
            g.allowed("http://localhost:5173"),
            Some("http://localhost:5173")
        );
        for refused in [
            "https://weather.example.com.evil.test",
            "https://weather.example.com/",
            "HTTPS://WEATHER.EXAMPLE.COM",
            "https://evil.test",
            "*",
            "null",
            "",
        ] {
            assert_eq!(g.allowed(refused), None, "`{refused}` was allowed");
        }
    }

    /// Empty is the default and means same-origin only: no origin is ever allowed, which is
    /// the state a fresh install is in because `management_cors_origin` has no seeded row.
    #[test]
    fn an_empty_allowlist_allows_nothing() {
        let g = guard(&[]);
        assert_eq!(g.allowed("https://weather.example.com"), None);
        // And the preflight from any origin is a `403` with no CORS headers on it, so the
        // browser reports a CORS failure rather than an opaque one.
        let response = preflight(None);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        // The security headers are still on it, because step 5 runs on every response this
        // module generates and not only on a handler's.
        assert!(response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY));
    }

    /// An unsorted list from the caller still works, because `new` sorts it -- the lookup is
    /// a binary search and this is the one way it could have silently refused its own
    /// entries.
    #[test]
    fn the_allowlist_is_sorted_on_the_way_in() {
        let g = guard(&["https://z.test", "https://a.test", "https://m.test"]);
        for origin in ["https://a.test", "https://m.test", "https://z.test"] {
            assert_eq!(g.allowed(origin), Some(origin));
        }
    }

    /// Which methods need the CSRF header. The fail-closed direction is the assertion:
    /// anything not on the safe list needs it.
    #[test]
    fn every_unsafe_method_needs_the_csrf_header() {
        for safe in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!changes_state(&safe), "{safe}");
        }
        for unsafe_method in [
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::TRACE,
            Method::CONNECT,
        ] {
            assert!(changes_state(&unsafe_method), "{unsafe_method}");
        }
    }

    /// The unauthenticated list is **three method-and-path pairs**, and the thing worth
    /// asserting is what is *not* on it: `DELETE /api/v1/auth/sessions` is "sign out
    /// everywhere", and a prefix rule over `/api/v1/auth/` would have left it open to
    /// anyone who could reach the port.
    #[test]
    fn only_three_exact_routes_are_unauthenticated() {
        let open = |method: &Method, path: &str| {
            UNAUTHENTICATED
                .iter()
                .any(|(m, p)| *m == method && *p == path)
        };
        assert!(open(&Method::GET, "/api/v1/auth/session"));
        assert!(open(&Method::POST, "/api/v1/auth/login"));
        assert!(open(&Method::POST, "/api/v1/auth/claim"));
        assert_eq!(UNAUTHENTICATED.len(), 3);

        for (method, path) in [
            (Method::DELETE, "/api/v1/auth/session"),
            (Method::DELETE, "/api/v1/auth/sessions"),
            (Method::GET, "/api/v1/auth/sessions"),
            (Method::POST, "/api/v1/auth/password"),
            (Method::GET, "/api/v1/config"),
            (Method::GET, "/api/v1/auth/session/"),
            (Method::GET, "/api/v1/logs"),
        ] {
            assert!(!open(&method, path), "{method} {path} is unauthenticated");
        }
    }

    /// Every header §3.5 names is on an ordinary response, and none of the CORS ones are
    /// when there is no allowlisted origin.
    #[test]
    fn the_security_headers_are_on_every_response() {
        let response = decorate(StatusCode::OK.into_response(), None);
        let headers = response.headers();
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));

        let allowed = decorate(StatusCode::OK.into_response(), Some("https://a.test"));
        assert_eq!(
            allowed.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://a.test"
        );
        assert_eq!(
            allowed.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
            "true"
        );
        assert_eq!(allowed.headers()[header::VARY], "Origin");
    }
}
