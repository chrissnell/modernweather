//! The embedded asset tree, the hard-coded mime table and the catch-all file server.
//!
//! # Embedded, with `debug-embed` on
//!
//! `rust-embed`, as the Go version already does with `embed.FS`: one static musl binary per
//! architecture, no asset path to configure, and no partial upgrade where the binary and
//! the JavaScript disagree about the API (§7.2).
//!
//! **`debug-embed` is enabled on purpose.** Without it `rust-embed` reads from the source
//! tree in debug builds, which would make a debug daemon depend on the directory it was
//! compiled in -- and `cargo xtask gate conformance` runs a debug binary with
//! `current_dir` at the repository root, so the gate would pass for the wrong reason and a
//! packaged debug build would 404 every asset. §7.2's `--assets-dir` development override
//! is **not** implemented: it would be a second way for the asset tree to differ from the
//! binary, which is the property embedding exists to buy, and nothing in this programme has
//! asked for it.
//!
//! # The mime table is a `const`, not `mime_guess` (D-76)
//!
//! Two of the frozen content types are quirks a mime crate will get "right" and therefore
//! wrong:
//!
//! | Path | Frozen `Content-Type` | Why a mime crate differs |
//! |---|---|---|
//! | `/js/weather-app.js` | `text/javascript` -- **no charset** | it came from a handler, not the file server |
//! | `/site.webmanifest` | `text/plain; charset=utf-8` | Go's mime table has no entry for the extension, so `DetectContentType` sniffed it |
//!
//! Every entry in [`content_type`] was produced by **running** Go's
//! `http.FileServer(http.FS(...))` over this exact tree; `tests/mime.rs` asserts the table
//! against that capture (`tests/golden/go-content-types.tsv`), so a wrong entry is a test
//! failure rather than a content type nobody checks.
//!
//! # Cache headers: the frozen behaviour, plus conditional GET, and no `max-age`
//!
//! Only `/fonts/` carries `Cache-Control: public, max-age=31536000, immutable`; every other
//! asset is served with no caching headers at all. That is reproduced byte for byte, and
//! `ETag` (the embedded content's SHA-256) plus `Last-Modified` are **added** (**DEV-1**),
//! so a client sending `If-None-Match` gets a `304` and one that does not gets today's
//! response.
//!
//! `max-age` is deliberately not added anywhere else: the asset filenames are not
//! content-hashed, so a long `max-age` means a browser keeps running the previous release's
//! `weather-app.js` against the new server until the TTL expires, and `ETag` already saves
//! the bytes.
//!
//! # DEV-3: a directory is a 404, and so is a miss
//!
//! `http.FileServer` renders an HTML index for `/js/`, `/css/`, `/images/` and `/fonts/`.
//! W1's OQ-W1-4 recommended not reproducing it and W3 **D-78** took that recommendation, so
//! both answer `404 page not found\n` as `text/plain; charset=utf-8` -- the same bytes a
//! missing file gets, which `asset-not-found` asserts.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri};
use rust_embed::Embed;

/// The asset tree, embedded.
///
/// `folder` is relative to the crate root, which is what `rust-embed` expects.
#[derive(Embed)]
#[folder = "assets/"]
struct Assets;

/// Every embedded path, in `rust-embed`'s order.
///
/// **For `tests/mime.rs`, which was golden-driven and therefore blind to a new file.**
/// M4-3's code review found that iterating the recorded capture rather than the tree meant
/// an asset added with an extension [`content_type`] does not list would fall through to
/// `text/plain; charset=utf-8` -- under a comment asserting that every unlisted extension in
/// this tree is text -- with no test disagreeing. The test now pairs the two sets, so adding
/// an asset forces the capture to be regenerated.
pub fn embedded_paths() -> Vec<String> {
    Assets::iter().map(|p| p.to_string()).collect()
}

/// One embedded file's bytes, or a panic naming it.
///
/// For the three files this crate serves by name rather than by request path --
/// `portal.html`, `privacy.html`, `support.html`. A missing one is a packaging mistake that
/// cannot be configured away, and the alternative is a handler that answers `404` for a file
/// that is supposed to be compiled in. [`tests::every_named_file_is_embedded`] is what makes
/// the panic unreachable.
pub(crate) fn must_get(path: &'static str) -> &'static [u8] {
    match Assets::get(path) {
        Some(file) => match file.data {
            std::borrow::Cow::Borrowed(bytes) => bytes,
            // `debug-embed` is on, so every file is a `'static` borrow. An owned variant
            // would mean the feature had been turned off.
            std::borrow::Cow::Owned(_) => unreachable!(
                "`{path}` was loaded from disk rather than embedded; \
                 `rust-embed`'s `debug-embed` feature must stay on"
            ),
        },
        None => unreachable!("`{path}` is not in crates/mw-web/assets/"),
    }
}

/// `/fonts/`'s one-year immutable cache, which no other asset tree carries.
const FONT_CACHE: &str = "public, max-age=31536000, immutable";

/// The frozen `Content-Type` for one asset path.
///
/// `path` is the request path **without its leading slash**, which is how `rust-embed` keys
/// the tree and how [`serve`] passes it -- `content_type("/js/weather-app.js")` would miss
/// the one path-specific entry. The distinction matters because two of the entries are
/// **path**-specific rather than extension-specific, and M4-3's code review found this doc
/// saying "the full request path".
pub fn content_type(path: &str) -> &'static str {
    // The one path whose content type came from a handler rather than from the file
    // server, and therefore carries no charset (§7.2).
    if path == "js/weather-app.js" {
        return "text/javascript";
    }
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/vnd.microsoft.icon",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "eot" => "application/vnd.ms-fontobject",
        "json" => "application/json",
        // `site.webmanifest`, and anything else Go's mime table has no entry for:
        // `DetectContentType` sniffs the leading bytes and answers `text/plain` for text.
        // Every file in this tree with an unlisted extension is text.
        _ => "text/plain; charset=utf-8",
    }
}

/// Serve one asset by request path, or Go's 404.
///
/// `path` is the request path **without** its leading slash, which is how `rust-embed` keys
/// the tree.
pub fn serve(uri: &Uri, request_headers: &HeaderMap) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    // DEV-3: a directory, with or without its trailing slash, is a 404 rather than an HTML
    // index. `rust-embed` has no directory entries, so an empty path and a trailing slash
    // are the two shapes that reach here.
    if path.is_empty() || path.ends_with('/') {
        return not_found();
    }
    // A path with a `..` segment cannot escape the tree -- `rust-embed` is a map, not a
    // filesystem -- but it also cannot *match*, so it falls through to the 404 below. Said
    // out loud because "is this traversable" is the first question a reader will have.
    let Some(file) = Assets::get(path) else {
        return not_found();
    };

    let etag = etag(&file.metadata.sha256_hash());
    let mut builder = Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(path)),
        )
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        // **DEV-1**, additive: the conformance runner asserts a per-fixture allowlist of
        // header names and never enumerates a response's headers, so neither of these is
        // visible to it.
        .header(
            header::ETAG,
            HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"\"")),
        );
    if let Some(modified) = file.metadata.last_modified().and_then(http_date) {
        builder = builder.header(
            header::LAST_MODIFIED,
            HeaderValue::from_str(&modified).unwrap_or(HeaderValue::from_static("")),
        );
    }
    if path.starts_with("fonts/") {
        builder = builder.header(header::CACHE_CONTROL, HeaderValue::from_static(FONT_CACHE));
    }

    // Conditional GET (**DEV-1**). A strong comparison, which is what `If-None-Match`
    // wants for a byte-exact validator, and a list so that a proxy sending several
    // candidates is handled.
    if if_none_match_matches(request_headers, &etag) {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap_or_else(|_| not_found());
    }

    builder
        .status(StatusCode::OK)
        .body(Body::from(file.data.into_owned()))
        .unwrap_or_else(|_| not_found())
}

/// Go's `http.NotFound` body. The same bytes `mw-http`'s own fallback gives, which is the
/// point: a missing asset and an unrouted path are one response.
fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .body(Body::from("404 page not found\n"))
        .unwrap_or_else(|_| {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        })
}

/// A strong `ETag` over the embedded content's SHA-256.
///
/// The first sixteen hex digits rather than all sixty-four: the validator only has to
/// distinguish the finitely many byte sequences this binary can serve, and a 64-bit prefix
/// of SHA-256 does that for any tree a human will ever build.
fn etag(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(20);
    out.push('"');
    for byte in &hash[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push('"');
    out
}

/// Whether `If-None-Match` names this entity.
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|candidate| candidate == etag || candidate == "*")
}

/// An IMF-fixdate, which is the only format `Last-Modified` may use.
///
/// Hand-rolled because the alternative is a `chrono` dependency in this crate for one
/// header, and `chrono`'s own formatter would need the same civil-date arithmetic anyway.
/// Days-from-epoch to a civil date is Howard Hinnant's algorithm, which is the one
/// `chrono` and `time` both use.
fn http_date(unix_seconds: u64) -> Option<String> {
    const DAY: u64 = 86_400;
    let days = (unix_seconds / DAY) as i64;
    let secs_of_day = unix_seconds % DAY;
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let weekday = ((days + 4).rem_euclid(7)) as usize;
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month_name = MONTHS.get((month as usize).checked_sub(1)?)?;
    Some(format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        day,
        month_name,
        year,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    ))
}

/// Days since 1970-01-01 to `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Response<Body> {
        serve(&path.parse().expect("a legal uri"), &HeaderMap::new())
    }

    async fn body(resp: Response<Body>) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), 8 << 20)
            .await
            .expect("a readable body")
            .to_vec()
    }

    #[test]
    fn every_named_file_is_embedded() {
        for path in ["portal.html", "privacy.html", "support.html"] {
            assert!(!must_get(path).is_empty(), "{path}");
        }
    }

    /// **D-76**'s two quirks, which are the whole reason the table is hand-written.
    #[test]
    fn the_two_quirks_a_mime_crate_would_correct() {
        assert_eq!(content_type("js/weather-app.js"), "text/javascript");
        assert_eq!(
            content_type("js/weather-charts.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("site.webmanifest"),
            "text/plain; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn an_asset_is_served_with_its_frozen_content_type() {
        let resp = get("/js/weather-charts.js");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
        assert!(resp.headers().get(header::CACHE_CONTROL).is_none());
        let bytes = body(resp).await;
        assert!(String::from_utf8_lossy(&bytes).contains("WeatherCharts"));
    }

    #[tokio::test]
    async fn only_fonts_carry_the_immutable_cache() {
        let font = get("/fonts/Cinzel-VariableFont_wght.ttf");
        assert_eq!(
            font.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(FONT_CACHE)
        );
        assert_eq!(
            font.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("font/ttf")
        );
        for other in ["/favicon.ico", "/css/weather.css", "/site.webmanifest"] {
            assert!(
                get(other).headers().get(header::CACHE_CONTROL).is_none(),
                "{other} must carry no caching headers at all"
            );
        }
    }

    /// **DEV-3**: a directory listing becomes the same 404 a missing file gets.
    #[tokio::test]
    async fn a_directory_and_a_miss_are_the_same_404() {
        for path in ["/js/", "/css/", "/images/", "/fonts/", "/nonexistent-asset"] {
            let resp = get(path);
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("text/plain; charset=utf-8"),
                "{path}"
            );
            assert_eq!(body(resp).await, b"404 page not found\n", "{path}");
        }
    }

    #[tokio::test]
    async fn a_traversal_does_not_leave_the_tree() {
        for path in ["/../Cargo.toml", "/../../../etc/passwd", "/..%2fCargo.toml"] {
            assert_eq!(get(path).status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// **DEV-1**: the two additive headers, and the `304` they buy.
    #[tokio::test]
    async fn a_conditional_get_gets_a_304() {
        let first = get("/css/weather.css");
        let etag = first
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("an ETag")
            .to_string();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
        assert!(first.headers().get(header::LAST_MODIFIED).is_some());

        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&etag).expect("a legal value"),
        );
        let again = serve(&"/css/weather.css".parse().expect("uri"), &headers);
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert!(body(again).await.is_empty());

        // A validator that does not match still gets the bytes.
        let mut stale = HeaderMap::new();
        stale.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"nope\""));
        assert_eq!(
            serve(&"/css/weather.css".parse().expect("uri"), &stale).status(),
            StatusCode::OK
        );
    }

    #[test]
    fn the_http_date_is_an_imf_fixdate() {
        // 2026-11-10T07:21:24Z was a Tuesday, which is the capture the mime golden carries.
        assert_eq!(
            http_date(1_794_295_284).as_deref(),
            Some("Tue, 10 Nov 2026 07:21:24 GMT")
        );
        assert_eq!(
            http_date(0).as_deref(),
            Some("Thu, 01 Jan 1970 00:00:00 GMT")
        );
        // A leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(
            http_date(1_709_164_800).as_deref(),
            Some("Thu, 29 Feb 2024 00:00:00 GMT")
        );
    }

    /// `/js/weather-app.js` is a **static asset** now (**D-38**), and its first statement is
    /// the one that reads the inline block.
    #[tokio::test]
    async fn weather_app_js_reads_the_inline_block_and_keeps_its_content_type() {
        let resp = get("/js/weather-app.js");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript"),
            "no charset: §7.2's frozen quirk"
        );
        let text = String::from_utf8(body(resp).await).expect("utf-8");
        assert!(text.contains("document.getElementById('mw-config')"));
        assert!(
            text.contains("new URLSearchParams(window.location.search).get('debug') === '1'"),
            "the tenth value is still computed client-side"
        );
        assert!(
            !text.contains("{{"),
            "no template action may survive the port"
        );
    }
}
