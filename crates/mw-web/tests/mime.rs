//! The hard-coded mime table, asserted against a capture of Go's own file server.
//!
//! **D-76** makes the table a `const` rather than `mime_guess`, and the argument is that two
//! of the frozen content types are quirks a mime crate would "correct". A hand-written table
//! defended that way has to be *checked* against the thing it claims to reproduce, or it is
//! a list of guesses with a citation.
//!
//! `tests/golden/go-content-types.tsv` is `http.FileServer(http.FS(os.DirFS(...)))` run over
//! `crates/mw-web/assets` -- the tree this crate embeds -- recording the status and
//! `Content-Type` for every file plus the four directory paths **DEV-3** turns into 404s.
//! Regenerate it with `scratch/gorender/mime`.
//!
//! # The one path where this crate deliberately differs, and why it is one rather than two
//!
//! `/js/weather-app.js` is `text/javascript; charset=utf-8` in the golden and
//! `text/javascript` here. That is not a disagreement with Go: in the Go tree the file is
//! `js/weather-app.js.tmpl` and the URL is served by `ServeWeatherAppJS`, which sets
//! `text/javascript` with no charset (`handlers.go:985`). **D-38** makes the module a static
//! asset, so the path moves into the file server -- and `contract/fixtures/asset-weather-app-js`
//! asserts the handler's content type, so the path keeps it. The golden cannot show that,
//! because the golden is a file server over a tree where the file is now static; the
//! exception is therefore declared below by name, which is the only form in which it can be
//! reviewed.
//!
//! `site.webmanifest` needs no exception: Go's mime table has no entry for the extension in
//! either tree, so the golden already records `text/plain; charset=utf-8`.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};

/// The one path this crate answers differently from a file server over the same tree, and
/// the reason.
const DECLARED_EXCEPTIONS: [(&str, &str, &str); 1] = [(
    "/js/weather-app.js",
    "text/javascript",
    "D-38 makes the module static; the frozen content type is the Go handler's, which \
     carries no charset (contract/fixtures/asset-weather-app-js)",
)];

struct Row {
    path: String,
    status: u16,
    content_type: String,
}

fn golden() -> Vec<Row> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("go-content-types.tsv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let rows: Vec<Row> = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|line| {
            let mut f = line.split('\t');
            Row {
                path: f.next().expect("a path").to_string(),
                status: f.next().expect("a status").parse().expect("a number"),
                content_type: f.next().unwrap_or("").to_string(),
            }
        })
        .collect();
    assert!(
        rows.len() > 50,
        "the golden table is {} rows; the asset tree has more files than that",
        rows.len()
    );
    rows
}

fn served(path: &str) -> (StatusCode, String) {
    let resp = mw_web::assets::serve(&path.parse().expect("a legal uri"), &HeaderMap::new());
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (status, content_type)
}

/// Every file in the tree, against the content type Go gives it.
#[test]
fn every_content_type_is_gos_content_type() {
    let mut files = 0;
    for row in golden() {
        // The directory paths are DEV-3's, asserted separately below.
        if row.path.ends_with('/') || row.status != 200 {
            continue;
        }
        let (status, content_type) = served(&row.path);
        assert_eq!(status, StatusCode::OK, "{}", row.path);
        let expected = DECLARED_EXCEPTIONS
            .iter()
            .find(|(path, _, _)| *path == row.path)
            .map(|(_, declared, _)| (*declared).to_string())
            .unwrap_or(row.content_type);
        assert_eq!(content_type, expected, "{}", row.path);
        files += 1;
    }
    assert!(files >= 50, "only {files} files were checked");
}

/// Both halves of **DEV-3**: Go answers a directory with a `200` and an HTML index, and this
/// crate answers `404 page not found\n`.
#[tokio::test]
async fn a_directory_listing_is_a_404_here_and_a_200_in_go() {
    let directories: Vec<Row> = golden()
        .into_iter()
        .filter(|r| r.path.ends_with('/'))
        .collect();
    assert_eq!(
        directories.len(),
        4,
        "`/js/`, `/css/`, `/images/` and `/fonts/` are the four W1's OQ-W1-4 names"
    );
    for row in directories {
        assert_eq!(row.status, 200, "{}: Go renders an index", row.path);
        assert_eq!(row.content_type, "text/html; charset=utf-8", "{}", row.path);
        let resp =
            mw_web::assets::serve(&row.path.parse().expect("a legal uri"), &HeaderMap::new());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{}", row.path);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .expect("a readable body");
        assert_eq!(&body[..], b"404 page not found\n", "{}", row.path);
    }
}

/// A declared exception must actually be one, or the list is a way to silence a real diff.
#[test]
fn every_declared_exception_really_differs_from_go() {
    let golden = golden();
    for (path, declared, reason) in DECLARED_EXCEPTIONS {
        let row = golden
            .iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("{path} is not in the golden table"));
        assert_ne!(
            row.content_type, declared,
            "{path} is declared an exception and does not differ from Go: {reason}"
        );
        assert_eq!(served(path).1, declared, "{path}");
    }
}

/// And the body is served, not just its header -- which is what makes `asset-static-js`'s
/// `required_substrings` pass.
#[tokio::test]
async fn an_asset_body_is_the_embedded_file() {
    let resp = mw_web::assets::serve(
        &"/js/weather-charts.js".parse().expect("uri"),
        &HeaderMap::new(),
    );
    let body = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("a readable body");
    let on_disk = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("js")
            .join("weather-charts.js"),
    )
    .expect("the file is in the tree");
    assert_eq!(body.to_vec(), on_disk);
    let _ = Body::empty();
}
