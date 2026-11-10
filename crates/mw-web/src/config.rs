//! What this crate plans and renders from: one resolved website, and the nine-value
//! configuration payload.
//!
//! [`SiteView`] is the website row **joined to its primary station**, because that is what
//! the Go handler renders from: `ServeWeatherWebsiteTemplate` resolves
//! `getPrimaryDeviceConfigForWebsite` and reads the station's id, name, coordinates and
//! Aeris flag off it (`handlers.go:869`). Joining in the binary rather than here is the
//! same move `mw_api_legacy::config` makes: this crate may not name `mw-store`, so the row
//! arrives already flattened.
//!
//! # `/config.json`, and why the inline block is still primary
//!
//! **D-79** delivers the payload two ways. The inline `<script id="mw-config">` block is
//! primary because a `fetch` at module initialisation would add a round trip and an
//! asynchronous boundary to a module that is currently synchronous -- the kind of change
//! that produces a flash of unstyled content in a page that is supposed to be unchanged.
//! `GET /config.json` is additive, per-website by `Host`, `Cache-Control: no-cache`, for
//! anything else that wants the same values.
//!
//! [`config_json`] is what the endpoint serves and the **template** interpolates the same
//! nine values itself, one field at a time. That is deliberate rather than duplication for
//! its own sake: a whole JSON document interpolated as one value would be escaped by the
//! page's escaper -- every `"` would become `&#34;` -- and the only way out is an escaping
//! bypass, which the layout gate forbids outside the one file that needs it for a number and
//! which would here turn escaping off for values an operator controls. So the template carries the JSON *punctuation* as template text and the values
//! as fields, exactly as the page's own `radar-config` block already did, and
//! [`tests::the_endpoint_and_the_inline_block_carry_the_same_nine_values`] asserts the two
//! agree.

use mw_types::{StationKey, WebsiteId};

use crate::gojs::js_string;

/// One website, resolved against its primary station.
#[derive(Debug, Clone, PartialEq)]
pub struct SiteView {
    /// `website.id`.
    pub id: WebsiteId,
    /// `website.hostname`, **as the column holds it**: the schema's three `CHECK`s make it
    /// lower-case, port-free and without a trailing dot, and this field is a verbatim copy
    /// rather than a normalised one. `mw_http::HostTable::new` normalises on ingest, so
    /// routing does not depend on the column being clean; what reads this field is the
    /// "a website is mounted" log line. `""` is the default website.
    pub hostname: String,
    /// `website.name`, which is the page's `StationName` -- the *website's* name, not the
    /// station's. Faithful to `handlers.go:896`.
    pub name: String,
    /// `website.page_title`. Reaches no rendered byte: `weather-station.html.tmpl` has no
    /// `{{ .PageTitle }}` action and `portal.html.tmpl` has none at all, so the Go handler
    /// passes it and both templates ignore it. Kept because the field is what makes that
    /// checkable rather than remembered.
    pub page_title: String,
    /// `website.apple_app_id`. Empty removes the `<meta name="apple-itunes-app">` tag.
    pub apple_app_id: String,
    /// `website.is_portal`: `/` serves the portal page and `/portal` exists.
    pub is_portal: bool,
    /// `website.radar_enabled`.
    pub radar_enabled: bool,
    /// `website.radar_token`. **Public** in the rendered markup, which is why it is a plain
    /// `String` -- the schema's "write-only" note is about the management API.
    pub radar_token: String,
    /// `website.snow_enabled`.
    pub snow_enabled: bool,
    /// The snow station's **name**, which is what the JavaScript queries by. Empty when
    /// snow is disabled.
    pub snow_device: String,
    /// `website.air_quality_enabled`.
    pub air_quality_enabled: bool,
    /// The air-quality station's name. Empty when air quality is disabled.
    pub air_quality_device: String,
    /// The air-quality station's surrogate id, or `0` when there is none -- which is what
    /// `getAirQualityDeviceID` returns.
    pub air_quality_device_id: i64,
    /// The primary station's key, for the log line and the digest.
    pub primary_station: Option<StationKey>,
    /// The primary station's **name**, which is `pullFromDevice`.
    pub primary_station_name: String,
    /// The primary station's surrogate id, which is `stationID`.
    pub primary_station_id: i64,
    /// The primary station's latitude, or `None` for a station with no location.
    pub latitude: Option<f64>,
    /// The primary station's longitude.
    pub longitude: Option<f64>,
    /// Whether the primary station has Aeris enabled, which is what gates the forecast
    /// band. **The station's flag, not the website's** -- `handlers.go:908` reads
    /// `primaryDevice.AerisEnabled`.
    pub aeris_enabled: bool,
}

impl SiteView {
    /// Whether this website can render the weather page.
    ///
    /// A non-portal website with no primary station cannot: the Go handler answers
    /// `500 No device configured for this website` (`handlers.go:869`). A portal does not
    /// need one.
    pub fn renders_weather_page(&self) -> bool {
        !self.is_portal && self.primary_station.is_some()
    }
}

/// The nine values **D-80** fixes, as the JSON `/config.json` serves.
///
/// Keys **in the order the JavaScript object literal had them**, because a key order that
/// drifts makes a diff against the old module unreadable for no gain. Written by hand
/// rather than through `serde` for one reason: `encoding/json` escapes `<`, `>` and `&` as
/// `\u003c`, `\u003e` and `\u0026` and `serde_json` does not,
/// `</script>` inside a `<script type="application/json">` block would otherwise terminate
/// it. The inline block gets the same escaping from [`crate::gojs::js_string`], which is
/// stricter still.
pub fn config_json(site: &SiteView) -> String {
    let mut out = String::with_capacity(256);
    out.push('{');
    push_str_field(&mut out, "pullFromDevice", &site.primary_station_name, true);
    out.push_str(&format!(r#""stationID": {}, "#, site.primary_station_id));
    push_str_field(&mut out, "stationName", &site.name, true);
    out.push_str(&format!(r#""snowEnabled": {}, "#, site.snow_enabled));
    push_str_field(&mut out, "snowDevice", &site.snow_device, true);
    out.push_str(&format!(
        r#""airQualityEnabled": {}, "#,
        site.air_quality_enabled
    ));
    push_str_field(&mut out, "airQualityDevice", &site.air_quality_device, true);
    out.push_str(&format!(
        r#""airQualityDeviceID": {}, "#,
        site.air_quality_device_id
    ));
    out.push_str(&format!(r#""aerisWeatherEnabled": {}"#, site.aeris_enabled));
    out.push('}');
    out
}

/// One `"key": "value"` pair, with the value escaped the way the inline block escapes it.
///
/// The **same** escaping as the inline block rather than `encoding/json`'s, so that the two
/// deliveries of D-79 are byte-identical and a client cannot tell which one it read.
/// `js_string` is a superset of what JSON requires -- it also escapes `/`, `` ` `` and `+`,
/// all of which are legal unescaped in JSON -- so the output is valid JSON either way.
fn push_str_field(out: &mut String, key: &str, value: &str, trailing_comma: bool) {
    out.push('"');
    out.push_str(key);
    out.push_str(r#"": ""#);
    out.push_str(&js_string(value));
    out.push('"');
    if trailing_comma {
        out.push_str(", ");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The `localhost` website of `ci/conformance/rust-seed/01-config.sql`, which is what
    /// W1 captured `html-weather-station` and `asset-weather-app-js` against.
    pub(crate) fn suncrest() -> SiteView {
        SiteView {
            id: WebsiteId::new(1),
            hostname: "localhost".into(),
            name: "Suncrest Live Weather".into(),
            page_title: "Suncrest Live Weather".into(),
            apple_app_id: "6755874087".into(),
            is_portal: false,
            radar_enabled: true,
            radar_token: "contract-radar-token".into(),
            snow_enabled: true,
            snow_device: "snowgauge".into(),
            air_quality_enabled: true,
            air_quality_device: "aqmon".into(),
            air_quality_device_id: 4,
            primary_station: Some(StationKey::new("davis-full")),
            primary_station_name: "davis-full".into(),
            primary_station_id: 1,
            latitude: Some(39.900),
            longitude: Some(-105.500),
            aeris_enabled: true,
        }
    }

    /// The `portal.test` website: a portal, with no primary station.
    pub(crate) fn portal_site() -> SiteView {
        SiteView {
            id: WebsiteId::new(3),
            hostname: "portal.test".into(),
            name: "Weather Portal".into(),
            page_title: "Weather Management Portal".into(),
            apple_app_id: String::new(),
            is_portal: true,
            radar_enabled: false,
            radar_token: String::new(),
            snow_enabled: false,
            snow_device: String::new(),
            air_quality_enabled: false,
            air_quality_device: String::new(),
            air_quality_device_id: 0,
            primary_station: None,
            primary_station_name: String::new(),
            primary_station_id: 0,
            latitude: None,
            longitude: None,
            aeris_enabled: false,
        }
    }

    #[test]
    fn the_payload_is_the_nine_values_in_the_javascript_objects_order() {
        let json = config_json(&suncrest());
        assert_eq!(
            json,
            r#"{"pullFromDevice": "davis-full", "stationID": 1, "stationName": "Suncrest Live Weather", "snowEnabled": true, "snowDevice": "snowgauge", "airQualityEnabled": true, "airQualityDevice": "aqmon", "airQualityDeviceID": 4, "aerisWeatherEnabled": true}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed.as_object().expect("an object").len(), 9);
    }

    /// **D-79**'s two deliveries must not disagree, and the only way to be sure is to
    /// compare their **bytes**.
    ///
    /// M4-3's code review found this comparing two `serde_json::Value`s, which cannot see
    /// key order, whitespace or escaping -- the three properties
    /// [`push_str_field`]'s own documentation says the shared escaping exists to guarantee.
    /// The two constructions are genuinely independent (the template interpolates
    /// `page.rs`'s `*_js` fields; this function builds the document by hand), so a
    /// divergence in any of the three is exactly what can happen, and a parsed comparison
    /// would have passed through it.
    #[test]
    fn the_endpoint_and_the_inline_block_are_byte_identical() {
        for site in [suncrest(), hostile(), portal_site()] {
            let page = crate::page::weather_station(&site, "0.1.0").expect("renders");
            assert_eq!(
                inline_block(&page),
                config_json(&site),
                "website {}: the inline block and `/config.json` differ",
                site.id
            );
        }
    }

    /// The `mw-config` payload, lifted out of a rendered page.
    fn inline_block(page: &str) -> String {
        let start = page
            .find(r#"<script id="mw-config" type="application/json">"#)
            .expect("the inline block is in the page");
        let body = &page[start..];
        let open = body.find('\n').expect("the payload is on its own line") + 1;
        let close = body.find("</script>").expect("the block closes");
        body[open..close].trim().to_string()
    }

    /// A website whose every string is hostile, so the byte comparison above has something
    /// to disagree about.
    fn hostile() -> SiteView {
        SiteView {
            name: "Bob's \"</script><script>alert(1)</script>\" & Co +1".into(),
            primary_station_name: "a/b\\c`d".into(),
            snow_device: "snow\u{2028}gauge".into(),
            air_quality_device: "aq<mon>".into(),
            ..suncrest()
        }
    }

    /// A `</script>` in a station name must not terminate the block, which is the whole
    /// reason the escaping is Go's rather than `serde_json`'s.
    #[test]
    fn a_closing_script_tag_in_a_name_cannot_terminate_the_block() {
        let mut hostile = suncrest();
        hostile.name = "</script><script>alert(1)</script>".into();
        let json = config_json(&hostile);
        assert!(!json.contains("</script>"), "{json}");
        assert!(json.contains(r"\u003c"), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("still valid JSON");
        assert_eq!(parsed["stationName"], "</script><script>alert(1)</script>");

        let page = crate::page::weather_station(&hostile, "0.1.0").expect("renders");
        let start = page
            .find(r#"<script id="mw-config" type="application/json">"#)
            .expect("the inline block");
        let close = page[start..]
            .find("</script>")
            .expect("the block still closes");
        let payload = &page[start..start + close];
        assert!(
            !payload.contains("<script>"),
            "nothing in the payload opens a tag: {payload}"
        );
    }

    #[test]
    fn a_portal_does_not_need_a_primary_station_and_a_site_does() {
        assert!(suncrest().renders_weather_page());
        assert!(!portal_site().renders_weather_page());
        let mut orphan = suncrest();
        orphan.primary_station = None;
        assert!(!orphan.renders_weather_page());
    }
}
