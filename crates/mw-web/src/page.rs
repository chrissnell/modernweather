//! The rendered pages: the `askama` port of `weather-station.html.tmpl`, and the three that
//! became static files.
//!
//! # What is a template and what is not
//!
//! | Go | Here | Why |
//! |---|---|---|
//! | `weather-station.html.tmpl` | `templates/weather-station.html` | 17 actions over per-website values |
//! | `portal.html.tmpl` | `assets/portal.html` | **it consumes nothing** -- the handler passes `PageTitle` and the template has zero actions (**D-75**) |
//! | `privacy.html` | `assets/privacy.html` | already static |
//! | `support.html` | `assets/support.html` | already static |
//!
//! `portal.html` being a static asset is recorded here because it looks like an omission
//! otherwise: a reader expecting three templates finds one.
//!
//! # The acceptance test is a byte diff, and it reads the fixtures
//!
//! `tests/byte_diff.rs` loads `contract/fixtures/html-weather-station.json` and
//! `html-portal.json`, applies the **one** expected delta, and compares every byte. So the
//! test is against W1's recording rather than against a copy of it, and the delta is a
//! patch a reviewer can read rather than a tolerance.
//!
//! **The one delta** is the `<script id="mw-config" type="application/json">` block
//! (**D-79**, **D-80**), inserted immediately before the module that reads it. §7.4
//! predicted two deltas -- that block "and a changed `src` attribute on the weather-app
//! script tag" -- and the second one does not happen: `/js/weather-app.js` keeps its path,
//! because §7.2's mime table freezes that exact path's `Content-Type` at `text/javascript`
//! with no charset and `contract/fixtures/asset-weather-app-js` requests it. Changing the
//! `src` would have moved a frozen content type to a new URL for no gain.
//!
//! # `Version` is the one value the recording pins to the Go build
//!
//! The footer renders the daemon's version, which was `5.10.22` when W1 captured the page
//! and is this crate's `CARGO_PKG_VERSION` now. It is therefore a **parameter** rather than
//! a constant: the registrar passes the binary's version, and the byte-diff test passes
//! `5.10.22`. Nothing else in the page depends on the build.

use askama::Template;

/// What a station with no coordinates renders as, in the `radar-config` block.
///
/// **A named constant rather than a literal, and the reason is `ci/layers.toml`'s
/// `one-zero-fill` rule**: the zero-fill that matters is `mw_api_legacy::zf`, which turns an
/// absent *measurement* into the contract's `0`, and that function is the only one in the
/// workspace allowed to do it. This is not that. The Go column is `REAL NOT NULL` and the
/// handler reads `primaryDevice.Latitude` unconditionally; the new schema makes it nullable,
/// and the rig's `nolocation` station stores `0.0` precisely so that the same predicate --
/// `latitude = 0 AND longitude = 0` -- is the one being exercised
/// (`ci/conformance/rust-seed/01-config.sql`). So `0` is what the recorded page carries for
/// a station with no location, and naming it is the rule's own standard: "the rule is not
/// 'do not default', it is 'say what the default is'".
///
/// What it costs is visible rather than hidden: a radar panel centred on the Gulf of Guinea
/// for a website whose primary station has no coordinates. The Go page has the same bug and
/// `website.radar_enabled` is what keeps it off the page.
const NO_LOCATION: f64 = 0.0;

use crate::config::SiteView;
use crate::gojs::{js_number, js_string, JsNumber};

/// `weather-station.html`, with every value the page interpolates.
///
/// **Five of the fields exist twice in different escapings** -- `station_name` and
/// `station_name_js`, and the three device names -- and that is the contextual escaping
/// `html/template` does for free and askama does not: the page uses the station name in
/// HTML text and in a JSON block, and the two need different bytes. Naming the second one
/// `_js` is what makes a template author's mistake visible at the call site rather than at
/// render time.
#[derive(Template, Debug)]
#[template(path = "weather-station.html")]
pub struct WeatherStationPage {
    /// `{{ .StationName }}` in HTML text, four times.
    station_name: String,
    /// `{{ .Version }}` -- the daemon's version, in the footer.
    version: String,
    /// `{{ .AppleAppID }}` in a quoted attribute, and the condition on the `<meta>` tag.
    apple_app_id: String,
    /// `{{ .Latitude }}` in the `radar-config` JSON block, padded as `jsValEscaper` pads.
    latitude_js: JsNumber,
    /// `{{ .Longitude }}`, likewise.
    longitude_js: JsNumber,
    /// `{{ .RadarToken }}` as a JavaScript string literal.
    radar_token_js: String,

    /// `{{ if .SnowEnabled }}` -- seven blocks.
    snow_enabled: bool,
    /// `{{ if .AirQualityEnabled }}` -- seven blocks, plus one `or`.
    air_quality_enabled: bool,
    /// `{{ if .RadarEnabled }}` -- two blocks, plus the same `or`.
    radar_enabled: bool,
    /// `{{ if .AerisWeatherEnabled }}` -- the forecast band.
    aeris_weather_enabled: bool,

    // The `mw-config` block: exactly W1 §9's nine consumed values (**D-80**), with keys
    // equal to the JavaScript property names so that `weather-app.js`'s body does not
    // change at all.
    /// `pullFromDevice`.
    pull_from_device_js: String,
    /// `stationID`.
    station_id: i64,
    /// `stationName`.
    station_name_js: String,
    /// `snowDevice`.
    snow_device_js: String,
    /// `airQualityDevice`.
    air_quality_device_js: String,
    /// `airQualityDeviceID`.
    air_quality_device_id: i64,
}

impl WeatherStationPage {
    /// The page for one website, at one build.
    pub fn new(site: &SiteView, version: &str) -> Self {
        Self {
            station_name: site.name.clone(),
            version: version.to_string(),
            apple_app_id: site.apple_app_id.clone(),
            latitude_js: js_number(site.latitude.unwrap_or(NO_LOCATION)),
            longitude_js: js_number(site.longitude.unwrap_or(NO_LOCATION)),
            radar_token_js: js_string(&site.radar_token),
            snow_enabled: site.snow_enabled,
            air_quality_enabled: site.air_quality_enabled,
            radar_enabled: site.radar_enabled,
            aeris_weather_enabled: site.aeris_enabled,
            pull_from_device_js: js_string(&site.primary_station_name),
            station_id: site.primary_station_id,
            station_name_js: js_string(&site.name),
            snow_device_js: js_string(&site.snow_device),
            air_quality_device_js: js_string(&site.air_quality_device),
            air_quality_device_id: site.air_quality_device_id,
        }
    }

    /// The `latitude` token, for the test that composes the escapers.
    #[cfg(test)]
    pub(crate) fn latitude_token(&self) -> &str {
        self.latitude_js.as_str()
    }
}

/// Why a page could not be rendered.
///
/// One variant, and it is the one askama's compile-time checking cannot remove: a `Display`
/// impl in the context can fail, and `Template::render` surfaces that. A missing field is a
/// **build** error, which is the whole reason askama was chosen over a runtime engine
/// (§7.1), and the per-request `htmltemplate.Must(...ParseFS(...))` that could panic inside
/// a Go handler (`handlers.go:801`, `:873`, `:947`) has no counterpart here.
#[derive(Debug, thiserror::Error)]
#[error("the website page could not be rendered")]
pub struct RenderError {
    #[from]
    #[source]
    source: askama::Error,
}

impl mw_types::Classify for RenderError {
    /// Fatal: the only way here is a `Display` impl in the context struct failing, which is
    /// a build defect and not an operational condition.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

/// Render one website's page.
pub fn weather_station(site: &SiteView, version: &str) -> Result<String, RenderError> {
    Ok(WeatherStationPage::new(site, version).render()?)
}

/// The portal page, which consumes nothing (**D-75**).
///
/// A `&'static str` rather than a render: the file is embedded, it has no actions, and
/// `/` and `/portal` on a portal website serve the same bytes.
pub fn portal() -> &'static [u8] {
    crate::assets::must_get("portal.html")
}

/// `/privacy`, which the Go handler reads straight from the asset filesystem.
pub fn privacy() -> &'static [u8] {
    crate::assets::must_get("privacy.html")
}

/// `/support`, likewise.
pub fn support() -> &'static [u8] {
    crate::assets::must_get("support.html")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SiteView;

    fn site() -> SiteView {
        crate::config::tests::suncrest()
    }

    #[test]
    fn the_page_renders_and_carries_the_recorded_substrings() {
        let html = weather_station(&site(), "5.10.22").expect("the page renders");
        for needle in [
            "<!DOCTYPE html>",
            "<title>Suncrest Live Weather - Weather Station</title>",
            "radar-config",
            "contract-radar-token",
            "6755874087",
            "RemoteWeather 5.10.22",
            "mw-config",
        ] {
            assert!(html.contains(needle), "{needle:?} is missing");
        }
    }

    /// The `radar-config` block, which is the one place `html/template` escapes as
    /// JavaScript rather than as HTML -- padding spaces included.
    #[test]
    fn the_radar_config_block_is_gos_bytes() {
        let html = weather_station(&site(), "5.10.22").expect("renders");
        assert!(
            html.contains(
                r#"{"lat":  39.9 , "lon":  -105.5 , "token": "contract-radar-token", "widthMiles": 200}"#
            ),
            "the JS-context padding is what `jsValEscaper` adds"
        );
    }

    /// **D-80**: exactly the nine values, with the JavaScript property names as keys.
    ///
    /// The **order** is asserted on the raw text rather than on a parsed object, because
    /// `serde_json::Value` holds a `BTreeMap` and sorts the keys -- so a parsed comparison
    /// would pass whatever order the page emitted.
    #[test]
    fn the_inline_config_block_carries_exactly_nine_values() {
        let html = weather_station(&site(), "5.10.22").expect("renders");
        let payload = inline_config(&html);
        let keys: Vec<&str> = payload
            .split('"')
            .skip(1)
            .step_by(2)
            .filter(|token| payload.contains(&format!("{token}{quote}: ", quote = '"')))
            .collect();
        assert_eq!(
            keys,
            vec![
                "pullFromDevice",
                "stationID",
                "stationName",
                "snowEnabled",
                "snowDevice",
                "airQualityEnabled",
                "airQualityDevice",
                "airQualityDeviceID",
                "aerisWeatherEnabled",
            ],
            "W1 §9's consumed set, in the JavaScript object's own order"
        );
        let json: serde_json::Value =
            serde_json::from_str(&payload).expect("the payload is valid JSON");
        let object = json.as_object().expect("an object");
        assert_eq!(object.len(), 9);
        assert_eq!(object["pullFromDevice"], "davis-full");
        assert_eq!(object["stationID"], 1);
        assert_eq!(object["airQualityDeviceID"], 4);
        assert_eq!(object["snowEnabled"], true);
    }

    /// The `mw-config` payload, lifted out of a rendered page.
    fn inline_config(html: &str) -> String {
        let start = html
            .find(r#"<script id="mw-config" type="application/json">"#)
            .expect("the block is in the page");
        let body = &html[start..];
        let open = body.find('\n').expect("the payload is on its own line") + 1;
        let close = body.find("</script>").expect("the block closes");
        body[open..close].trim().to_string()
    }

    /// The whole point of **D-38**: a value that broke the page as generated JavaScript is
    /// now data. `';alert(1);//` in a station name used to **execute**.
    #[test]
    fn a_station_name_that_used_to_execute_is_now_data() {
        let mut hostile = site();
        hostile.name = "';alert(1);//".into();
        let html = weather_station(&hostile, "0.1.0").expect("renders");
        // The HTML contexts: the apostrophe becomes Go's `&#39;` and the slashes are
        // left alone, because `htmlReplacementTable` has no entry for `/`.
        assert!(html.contains("&#39;;alert(1);//"), "the title and the <h1>");
        // The JSON block: the apostrophe and both slashes are JavaScript escapes.
        assert!(
            html.contains(r"\u0027;alert(1);\/\/"),
            "the inline configuration block"
        );
        // And nowhere does the raw sequence survive, which is what used to execute.
        assert!(
            !html.contains("';alert(1);//"),
            "a value that was generated into JavaScript is now data"
        );
    }

    /// An empty `apple_app_id` removes the `<meta>` tag entirely, which is what Go's
    /// `{{- if .AppleAppID }}` does -- and the whitespace control has to match too, or the
    /// byte diff fails on a blank line.
    #[test]
    fn an_empty_apple_app_id_removes_the_meta_tag_and_its_line() {
        let mut bare = site();
        bare.apple_app_id = String::new();
        let html = weather_station(&bare, "0.1.0").expect("renders");
        assert!(!html.contains("apple-itunes-app"));
        assert!(
            html.contains("initial-scale=1\" name=\"viewport\">\n  <link rel=\"icon\""),
            "the trimmed form leaves no blank line: {:?}",
            &html[..600]
        );
    }

    #[test]
    fn the_latitude_token_is_padded() {
        assert_eq!(
            WeatherStationPage::new(&site(), "0.1.0").latitude_token(),
            " 39.9 "
        );
    }

    #[test]
    fn the_three_static_pages_are_embedded() {
        assert!(portal().starts_with(b"<!DOCTYPE html>"));
        assert!(!privacy().is_empty());
        assert!(!support().is_empty());
    }
}
