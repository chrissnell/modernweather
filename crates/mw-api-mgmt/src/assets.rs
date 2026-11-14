//! The management UI's bundle, embedded (§6.4).
//!
//! `rust-embed`, matching W3 §7.2's decision for the public site's assets and the Go
//! version's `embed.FS`: one static binary, no second artefact to deploy and nothing to
//! configure about where the UI lives.
//!
//! # Two cache policies, because the bundle has two kinds of file
//!
//! Vite emits hashed filenames under `/assets/*`, so those are immutable for a year --
//! their names change when their contents do. `index.html` is `no-cache`, because it is
//! the one file whose name never changes and whose job is to name the others. The current
//! UI has neither policy, which is why a browser holds a stale admin screen across an
//! upgrade.
//!
//! # The SPA fallback, and the one thing it must not swallow
//!
//! `adapter-static` with `fallback: 'index.html'` means the client owns the routing, so any
//! path that is not a file is `index.html`. That is correct for `/stations/backyard` and
//! **wrong** for `/api/v1/anything`: a mistyped API path answering 200 with an HTML
//! document is a 404 that looks like a success to every client that checks the status and
//! not the body. So the fallback refuses `/api/` outright and lets the router's own 404
//! stand.
//!
//! # A build with no bundle says so
//!
//! `cargo build` on a tree that has never run `pnpm build` embeds an empty tree, because
//! the Rust build must not require a Node toolchain. That case answers one plain sentence
//! rather than an empty page: the thing that is supposed to prevent it reaching a release
//! is `cargo xtask dist`, which builds the bundle natively before the cross matrix and
//! fails if it is missing or stale (**D-58**).

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri};
use rust_embed::Embed;

/// The built bundle.
///
/// `folder` is relative to the crate root, which is what `rust-embed` expects.
/// `debug-embed` is on for the same reason `mw-web` turns it on: a debug build and a
/// release build must serve the same bytes from the same place, or a bug that only appears
/// in one of them is a bug nobody can reproduce. Serving a bundle from disk is a
/// deliberate, named choice instead -- [`Assets::from_dir`].
#[derive(Embed)]
#[folder = "../../ui/build/"]
struct Bundle;

/// One year, for the hashed files whose names change when their contents do.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// For `index.html`, the one file whose name never changes.
const NO_CACHE: &str = "no-cache";

/// Where the bundle comes from.
#[derive(Debug, Clone, Default)]
pub struct Assets {
    /// `Some` when `--assets-dir` was given: serve from disk instead (§6.4).
    dir: Option<PathBuf>,
}

impl Assets {
    /// The embedded bundle.
    pub fn embedded() -> Self {
        Assets { dir: None }
    }

    /// Serve from a directory on disk, for iterating on the UI without rebuilding.
    ///
    /// §6.4's `--assets-dir`. Not the dev loop -- that is `vite dev` on :5173 proxying
    /// `/api` to a running daemon, which needs nothing from this crate -- but the answer
    /// to "I have a bundle and a binary and I want to try them together".
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        Assets {
            dir: Some(dir.into()),
        }
    }

    /// Whether this build has a bundle at all.
    pub fn is_empty(&self) -> bool {
        match &self.dir {
            Some(dir) => !dir.join("index.html").is_file(),
            None => Bundle::get("index.html").is_none(),
        }
    }

    fn read(&self, path: &str) -> Option<(Vec<u8>, Option<[u8; 32]>)> {
        match &self.dir {
            Some(dir) => {
                // A `..` segment cannot escape, because every segment is checked before
                // the path is joined: `rust-embed` is a map and cannot be traversed, and a
                // directory can, so the two sources need the same guarantee by different
                // means.
                if path
                    .split('/')
                    .any(|s| s == ".." || s == "." || s.is_empty())
                {
                    return None;
                }
                std::fs::read(dir.join(path))
                    .ok()
                    .map(|bytes| (bytes, None))
            }
            None => Bundle::get(path).map(|f| (f.data.to_vec(), Some(f.metadata.sha256_hash()))),
        }
    }
}

/// Every embedded path, for a test and for `cargo xtask gate-ui-bundle`'s sanity check.
pub fn embedded_paths() -> Vec<String> {
    Bundle::iter().map(|p| p.to_string()).collect()
}

/// The `Content-Type` for one bundle path.
///
/// Narrow on purpose: this tree is `vite build`'s output, so the extension set is known
/// and short. An unlisted extension is `application/octet-stream` rather than
/// `text/plain`, because the alternative -- guessing text for something the bundler
/// emitted -- is how a `.wasm` gets served as a document.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/vnd.microsoft.icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// What the management root answers on a build with no bundle.
const NO_BUNDLE: &str = "The management interface was not built into this copy of \
                         RemoteWeather. The API is working; the pages are not here.\n";

/// Serve one path out of the bundle, with the SPA fallback (§6.4).
pub fn serve(assets: &Assets, uri: &Uri, request_headers: &HeaderMap) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');

    // The API's own 404 must stay the API's. See the module note.
    if path.starts_with("api/") {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"summary":"There is no such API path."}"#))
            .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response_fallback());
    }

    if assets.is_empty() {
        return Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(NO_BUNDLE))
            .unwrap_or_else(|_| StatusCode::NOT_IMPLEMENTED.into_response_fallback());
    }

    // A file if there is one, `index.html` otherwise -- which is what makes a deep link
    // like `/stations/backyard` reach the client router rather than a 404.
    let (requested, bytes, hash) = match assets.read(path) {
        Some((bytes, hash)) => (path.to_string(), bytes, hash),
        None => match assets.read("index.html") {
            Some((bytes, hash)) => ("index.html".to_string(), bytes, hash),
            None => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(Body::from("Not found\n"))
                    .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response_fallback())
            }
        },
    };

    let immutable = requested.starts_with("assets/") && requested != "index.html";
    let etag = hash.map(|h| format!("\"{}\"", hex32(&h)));

    // A conditional request on a hashed asset is the common case after the first load, and
    // answering it with the body is the one thing an immutable cache policy makes
    // pointless.
    if let (Some(etag), Some(inm)) = (&etag, request_headers.get(header::IF_NONE_MATCH)) {
        if inm
            .to_str()
            .is_ok_and(|v| v.split(',').any(|c| c.trim() == etag))
        {
            let mut builder = Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag.clone());
            builder = builder.header(
                header::CACHE_CONTROL,
                if immutable { IMMUTABLE } else { NO_CACHE },
            );
            return builder
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::NOT_MODIFIED.into_response_fallback());
        }
    }

    let mut builder = Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&requested)),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if immutable { IMMUTABLE } else { NO_CACHE }),
        )
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(etag) = etag {
        builder = builder.header(header::ETAG, etag);
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response_fallback())
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A response builder cannot fail for the headers above, and `unwrap` in a serving path is
/// the one thing worth not writing. This is the fallback that makes it unnecessary.
trait Fallback {
    fn into_response_fallback(self) -> Response<Body>;
}

impl Fallback for StatusCode {
    fn into_response_fallback(self) -> Response<Body> {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = self;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback must not swallow the API's 404. A mistyped API path answering an HTML
    /// document with a 200 is a failure that looks like a success to every client that
    /// checks the status rather than the body.
    #[test]
    fn an_unknown_api_path_is_not_the_single_page_app() {
        let assets = Assets::embedded();
        let response = serve(
            &assets,
            &"/api/v1/nonsense".parse().expect("a uri"),
            &HeaderMap::new(),
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    /// A build with no bundle says one sentence rather than serving an empty page, and
    /// says it with a status a client can act on.
    #[test]
    fn a_build_with_no_bundle_says_so() {
        if !Assets::embedded().is_empty() {
            // The bundle is built in this tree, which is the other half of the contract
            // and is asserted by `the_bundle_serves_its_own_index` below.
            return;
        }
        let response = serve(
            &Assets::embedded(),
            &"/".parse().expect("a uri"),
            &HeaderMap::new(),
        );
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    /// With a bundle, the root is the document and a hashed asset is immutable.
    #[test]
    fn the_bundle_serves_its_own_index() {
        let assets = Assets::embedded();
        if assets.is_empty() {
            return;
        }
        let response = serve(&assets, &"/".parse().expect("a uri"), &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(NO_CACHE),
            "index.html is the one file whose name never changes"
        );

        let hashed = embedded_paths()
            .into_iter()
            .find(|p| p.starts_with("assets/") && p.ends_with(".js"));
        if let Some(path) = hashed {
            let uri: Uri = format!("/{path}").parse().expect("a uri");
            let response = serve(&assets, &uri, &HeaderMap::new());
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some(IMMUTABLE)
            );
        }
    }

    /// A deep link is the single-page application, which is the whole point of the
    /// fallback: `/stations/backyard` is a client route and the server has never heard of
    /// it.
    #[test]
    fn a_deep_link_is_the_single_page_app() {
        let assets = Assets::embedded();
        if assets.is_empty() {
            return;
        }
        let response = serve(
            &assets,
            &"/stations/backyard".parse().expect("a uri"),
            &HeaderMap::new(),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
    }
}
