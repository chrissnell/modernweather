#!/usr/bin/env python3
"""Capture golden fixtures for the frozen legacy REST contract from a running Go server.

A re-capture preserves each fixture's `class` and `pending` because both are declared on the
`spec(...)` line below rather than read back from the file. It does NOT preserve a
`deviation` block: those record a response the Go server does not produce, so there is
nothing to capture and the two that exist were written by the work item that introduced the
deviation (implementation-plan.md M4-2, DEV-10 on `/stationinfo`). A re-capture therefore
drops them, and `cargo xtask gate conformance` fails naming the deviation it could not find
-- which is the right failure: a deviation whose fixture has been recaptured needs its new
half re-derived from the new recording, not silently carried.
"""
import base64, hashlib, http.client, json, os, re, sys, datetime

BASE_HOST = os.environ.get("RW_CAPTURE_HOST", "127.0.0.1")
BASE_PORT = int(os.environ.get("RW_CAPTURE_PORT", "18080"))
OUT = sys.argv[1] if len(sys.argv) > 1 else "fixtures"
os.makedirs(OUT, exist_ok=True)

RECORDED_HEADERS = ["Content-Type", "Cache-Control", "Access-Control-Allow-Origin", "Vary"]


def fetch(method, path, host):
    c = http.client.HTTPConnection(BASE_HOST, BASE_PORT, timeout=120)
    # Identity encoding on purpose: CompressHandler wraps every route, and the fixtures are
    # meant to record the bodies the handlers produce, not their gzip envelopes.
    c.request(method, path, headers={"Host": host, "User-Agent": "rw-contract-capture/1",
                                     "Accept-Encoding": "identity"})
    r = c.getresponse()
    body = r.read()
    hdrs = {k: v for k, v in ((h, r.getheader(h)) for h in RECORDED_HEADERS) if v is not None}
    c.close()
    return r.status, hdrs, body


SPECS = []


# docs/reconciliation.md §6.4's three classes, as work item N-18 asks for.
#
#   frozen   the five endpoints the iOS client calls. A diff is a CONTRACT BREAK: the client
#            is deployed independently of the server and cannot be updated with it.
#   coupled  everything the website calls. A diff is a LOCKSTEP CHANGE -- allowed, but only
#            in the same PR as the server and the embedded JavaScript, because they ship as
#            one binary (D-69).
#   retired  `/alerts`, `/api/remote-stations` and `format=msgpack`. Not carried forward
#            (D-69); the fixtures are KEPT so that the runner still attempts all 70 and a
#            classification cannot arrive as a silent filter.
#
# `pending` names the work item that will implement a fixture's subject, for the ones no
# landed item has. It is absent once that item lands, and `xtask/src/conformance.rs` asserts
# the pending set by name -- so adding one to dodge a failure fails the gate instead.
FROZEN, COUPLED, RETIRED = "frozen", "coupled", "retired"


def spec(name, path, host="localhost", method="GET", kind="json", provenance="", desc="",
         key_order=True, substrings=None, store_body=True, numeric_convention="go_encoder",
         cls=FROZEN, pending=None):
    SPECS.append(dict(name=name, path=path, host=host, method=method, kind=kind,
                      provenance=provenance, desc=desc, key_order=key_order,
                      substrings=substrings or [], store_body=store_body,
                      numeric_convention=numeric_convention, cls=cls, pending=pending))


EMU_DAVIS = "davis-emulator (cmd/davis-emulator), LOOP packets over TCP"
EMU_CAMP = "campbell-emulator (cmd/campbell-emulator), JSON records over TCP"
SEEDED = "seeded directly into the weather hypertable (no Go driver exists that can be emulated here)"
SEEDED_AERIS = "seeded directly into aeris_weather_forecasts / aeris_weather_alerts (Aeris API key not available)"
CONFIG = "derived from the SQLite configuration database only"

# ---- /latest -------------------------------------------------------------
spec("latest-full-complement", "/latest?station=davis-full", provenance=EMU_DAVIS,
     desc="Station reporting a full sensor complement (Davis: soil/leaf temps, UV, solar, storm/day/month/year rain, battery).", cls=FROZEN)
spec("latest-minimal-complement", "/latest?station=csi-minimal", provenance=EMU_CAMP,
     desc="Station reporting a minimal sensor complement (Campbell: temp, humidity, wind, barometer, solar, rain only).", cls=FROZEN)
spec("latest-by-station-id", "/latest?station_id=1", provenance=EMU_DAVIS,
     desc="station_id form, the parameter the iOS app and the website both prefer.", cls=FROZEN)
spec("latest-air-quality-device", "/latest?station_id=4", provenance=SEEDED,
     desc="The iOS/website air-quality call: /latest against the AirGradient device, decoded as a WeatherResponse.", cls=FROZEN)
spec("latest-snow-device", "/latest?station=snowgauge", provenance=SEEDED,
     desc="Snow fields as they appear in /latest. snowdepth here is MILLIMETRES (baseDistance - snowdistance), unlike /snow and /span which are inches.", cls=FROZEN)
spec("latest-default-station", "/latest", provenance=EMU_DAVIS,
     desc="No station parameter: falls back to the website's primary device.", cls=FROZEN)
spec("latest-snow-device-offline", "/latest?station=snowgauge-stale", kind="text", provenance=SEEDED,
     desc="Snow device configured but offline: last reading is older than the 10-minute window, so /latest is 404, not an empty reading.", cls=FROZEN)
spec("latest-no-recent-data", "/latest?station=nolocation", kind="text", provenance=CONFIG,
     desc="Configured device that has never reported: 404 with a plain-text body.", cls=FROZEN)
spec("latest-station-not-found", "/latest?station=does-not-exist", kind="text", provenance=CONFIG, cls=FROZEN)
spec("latest-station-id-not-found", "/latest?station_id=99", kind="text", provenance=CONFIG, cls=FROZEN)
spec("latest-invalid-station-id", "/latest?station_id=abc", kind="text", provenance=CONFIG, cls=FROZEN)
spec("latest-msgpack", "/latest?station=davis-full&format=msgpack", kind="binary", provenance=EMU_DAVIS,
     desc="format=msgpack switches the encoder for every endpoint that goes through responseformat.Formatter.", cls=RETIRED)

# ---- /span ---------------------------------------------------------------
spec("span-1h-one-minute-buckets", "/span/1h?station=csi-minimal", provenance=EMU_CAMP,
     desc="span <= 6h reads weather_1m.", cls=FROZEN)
spec("span-24h-five-minute-buckets", "/span/24h?station=davis-full", provenance=EMU_DAVIS,
     desc="6h < span <= 48h reads weather_5m. This is the website's default chart span.", cls=FROZEN)
spec("span-72h-hourly-with-gap", "/span/72h?station=snowgauge", provenance=SEEDED,
     desc="48h < span <= 14d reads weather_1h. This series has a deliberate six-hour hole: the gap is simply absent buckets, not nulls.", cls=FROZEN)
spec("span-720h-daily", "/span/720h?station=snowgauge", provenance=SEEDED,
     desc="span > 14d reads weather_1d.", cls=FROZEN)
spec("span-snow-device-with-station-id", "/span/24h?station=snowgauge&station_id=3", provenance=SEEDED,
     desc="The iOS fetchDeviceSpan form. station_id is accepted and ignored by this handler; only station is used.", cls=FROZEN)
spec("span-empty", "/span/24h?station=nolocation", provenance=CONFIG,
     desc="No data in the window: an empty JSON array, not 404.", cls=FROZEN)
spec("span-missing-station", "/span/24h", kind="text", provenance=CONFIG, cls=FROZEN)
spec("span-invalid-duration", "/span/notaduration?station=davis-full", kind="text", provenance=CONFIG,
     desc="The span segment is parsed by Go's time.ParseDuration, so '30d' is also invalid; only ns/us/ms/s/m/h units exist.", cls=FROZEN)
spec("span-day-unit-rejected", "/span/30d?station=davis-full", kind="text", provenance=CONFIG, cls=FROZEN)
spec("span-exceeds-one-year", "/span/9000h?station=davis-full", kind="text", provenance=CONFIG, cls=FROZEN)
spec("span-station-not-found", "/span/24h?station=does-not-exist", kind="text", provenance=CONFIG, cls=FROZEN)

# ---- /snow ---------------------------------------------------------------
spec("snow-reporting", "/snow", provenance=SEEDED,
     desc="Snow gauge reporting: depth in inches from (base_snow_distance - snowdistance) / 25.4, thresholded at 0.1in.", cls=FROZEN)
spec("snow-by-station-id", "/snow?station_id=3", provenance=SEEDED, cls=FROZEN)
spec("snow-station-param-overrides-depth-only", "/snow?station=davis-full", provenance=SEEDED,
     desc="station/station_id only redirect the depth query; stationname and every total still come from the website's snow_device_name.", cls=FROZEN)
spec("snow-device-stale", "/snow", host="stalesnow.test", provenance=SEEDED,
     desc="Snow device offline for three days: /snow has NO recency filter, so it keeps returning the last known depth indefinitely.", cls=FROZEN)
spec("snow-device-never-reported", "/snow", host="deadsnow.test", provenance=CONFIG,
     desc="Snow device configured but physically absent: no row with snowdistance > 0 exists, so every field is zero and the response is still 200.", cls=FROZEN)
spec("snow-disabled-for-website", "/snow", host="nosnow.test", kind="text", provenance=CONFIG, cls=FROZEN)
spec("snow-msgpack", "/snow?format=msgpack", kind="binary", provenance=SEEDED, cls=RETIRED)

# ---- /stationinfo --------------------------------------------------------
spec("stationinfo-full", "/stationinfo", provenance=CONFIG,
     desc="Every optional field populated, including radar_enabled/radar_token and the device location.", cls=FROZEN)
spec("stationinfo-about-text-omitted", "/stationinfo", host="minimal.test", provenance=CONFIG,
     desc="HAZARD: about_station_html is empty, so about_text is omitted. iOS StationInfo.aboutText is a non-optional String, so the whole decode fails.", cls=FROZEN)
spec("stationinfo-no-weather-device", "/stationinfo", host="portal.test", provenance=CONFIG,
     desc="HAZARD: a portal website has no device_id, so weather_device AND about_text are both omitted. iOS StationInfo.weatherDevice is a non-optional Int.", cls=FROZEN)

# ---- /forecast -----------------------------------------------------------
spec("forecast-240-ten-daily-periods", "/forecast/240", provenance=SEEDED_AERIS, key_order=False, numeric_convention="as_stored",
     desc="Ten daily periods. The data value is Postgres jsonb passed through verbatim, so key order is jsonb-normalised (length then bytewise) and numeric text is whatever was stored -- note snowIN: 0.0, not 0.", cls=FROZEN)
spec("forecast-24-hourly-periods", "/forecast/24", provenance=SEEDED_AERIS, key_order=False, numeric_convention="as_stored", cls=FROZEN)
spec("forecast-single-period", "/forecast/240?period=3", provenance=SEEDED_AERIS, key_order=False, numeric_convention="as_stored",
     desc="period=N replaces the data array with a single OBJECT. The iOS client decodes data as [ForecastPeriod], so this form would fail there; no shipped client sends it.", cls=FROZEN)
spec("forecast-empty-data", "/forecast/240?station_id=2", provenance=SEEDED_AERIS, key_order=False, numeric_convention="as_stored",
     desc="A stored-but-empty forecast: data is [].", cls=FROZEN)
spec("forecast-station-not-found", "/forecast/240?station_id=99", kind="text", provenance=CONFIG,
     desc="404 with a completely empty body and no Content-Type.", cls=FROZEN)
spec("forecast-by-station-name", "/forecast/240?station=davis-full", provenance=SEEDED_AERIS,
     key_order=False, numeric_convention="as_stored",
     desc="The legacy `station` form: the name is resolved to a device id before the lookup.", cls=FROZEN)
spec("forecast-station-name-not-found", "/forecast/240?station=does-not-exist", kind="text",
     provenance=CONFIG,
     desc="A fourth error-body convention on this one endpoint: a JSON {\"error\"} object written by "
          "json.NewEncoder after WriteHeader, so the Content-Type is the sniffed text/plain.", cls=FROZEN)
spec("forecast-span-not-found", "/forecast/0", kind="text", provenance=CONFIG, cls=FROZEN)
spec("forecast-span-too-many-digits", "/forecast/99999", kind="text", provenance=CONFIG,
     desc="The span segment must match ^\\\\d{1,4}$; five digits is 400 with an empty body.", cls=FROZEN)
spec("forecast-period-out-of-range", "/forecast/240?period=99", kind="text", provenance=SEEDED_AERIS,
     desc="QUIRK: a JSON error body served as Content-Type text/plain, because WriteHeader runs before the formatter sets the type.", cls=FROZEN)
spec("forecast-period-not-a-number", "/forecast/240?period=abc", kind="text", provenance=SEEDED_AERIS, cls=FROZEN)
spec("forecast-msgpack", "/forecast/24?format=msgpack", kind="binary", provenance=SEEDED_AERIS,
     desc="msgpack re-encodes the jsonb through interface{}, so every number becomes a float64.", cls=RETIRED)

# ---- /alerts -------------------------------------------------------------
spec("alerts-active", "/alerts?station_id=1", provenance=SEEDED_AERIS,
     desc="Active alerts only (begins_at <= now < expires_at), newest first. Timestamps are RFC3339 from Go's time.Time.", cls=RETIRED)
spec("alerts-empty", "/alerts?station_id=2", provenance=SEEDED_AERIS, cls=RETIRED)
spec("alerts-missing-station-id", "/alerts", kind="text", provenance=CONFIG, cls=RETIRED)
spec("alerts-invalid-station-id", "/alerts?station_id=abc", kind="text", provenance=CONFIG, cls=RETIRED)

# ---- /almanac ------------------------------------------------------------
spec("almanac-populated", "/almanac?station=davis-full", provenance="almanac_cache, refreshed by migration 005/028-030 functions over emulator data",
     desc="Only the metrics present in almanac_cache appear; every field is omitempty, so a station with no cached extremes returns {}.", cls=COUPLED)
spec("almanac-empty", "/almanac?station=nolocation", provenance=CONFIG, cls=COUPLED)
spec("almanac-missing-station", "/almanac", kind="text", provenance=CONFIG, cls=COUPLED)
spec("almanac-station-not-found", "/almanac?station=does-not-exist", kind="text", provenance=CONFIG, cls=COUPLED)

# ---- /api/stations, /api/remote-stations ---------------------------------
spec("api-stations-portal", "/api/stations", host="portal.test", provenance=CONFIG,
     desc="Portal-only. Devices without a location are excluded; disabled devices are included with enabled: false.", cls=COUPLED)
spec("api-stations-forbidden", "/api/stations", kind="text", provenance=CONFIG,
     desc="403 on a non-portal website.", cls=COUPLED)
spec("api-remote-stations-populated", "/api/remote-stations", host="portal.test", provenance="remote_stations rows in the SQLite config database",
     desc="No shipped client reads this: the only in-repo consumer is the management UI, which fetches it relative to the management server, where the route does not exist.", cls=RETIRED)
spec("api-remote-stations-forbidden", "/api/remote-stations", kind="text", provenance=CONFIG, cls=RETIRED)

# ---- HTML and assets ----------------------------------------------------
spec("html-weather-station", "/", kind="html", provenance=CONFIG,
     desc="weather-station.html.tmpl rendered. Consumes StationName, Version, AppleAppID, Latitude, Longitude, RadarToken and five booleans.",
     substrings=["<!DOCTYPE html>", "Suncrest Live Weather", "radar-config", "contract-radar-token", "6755874087"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("html-portal-root", "/", host="portal.test", kind="html", provenance=CONFIG,
     desc="A portal website serves portal.html.tmpl from / as well as /portal. The template consumes NO variables at all.",
     substrings=["Weather Management Portal", "portal-app.js"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("html-portal", "/portal", host="portal.test", kind="html", provenance=CONFIG,
     substrings=["Weather Management Portal"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("html-portal-on-non-portal", "/portal", kind="text", provenance=CONFIG, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("html-privacy", "/privacy", kind="html", provenance=CONFIG, substrings=["Privacy"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("html-support", "/support", kind="html", provenance=CONFIG, substrings=["Support"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-weather-app-js", "/js/weather-app.js", kind="html", provenance=CONFIG,
     desc="js/weather-app.js.tmpl rendered through text/template. Content-Type is text/javascript with NO charset, unlike the static JS the file server returns.",
     substrings=["pullFromDevice: 'davis-full'", "stationID: 1", "snowEnabled: true", "airQualityDeviceID: 4"], cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-static-js", "/js/weather-charts.js", kind="html", provenance=CONFIG,
     desc="Static asset from the embedded FS: text/javascript; charset=utf-8.", substrings=["WeatherCharts"], store_body=False, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-static-css", "/css/weather.css", kind="html", provenance=CONFIG, substrings=[], store_body=False, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-font", "/fonts/Cinzel-VariableFont_wght.ttf", kind="binary", provenance=CONFIG,
     desc="Fonts carry Cache-Control: public, max-age=31536000, immutable. No other asset tree does.", store_body=False, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-webmanifest", "/site.webmanifest", kind="html", provenance=CONFIG,
     desc="Served as text/plain; charset=utf-8, not application/manifest+json.",
     substrings=["short_name", "icons"], store_body=True, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-favicon", "/favicon.ico", kind="binary", provenance=CONFIG, store_body=False, cls=COUPLED, pending='implementation-plan.md M4-3')
spec("asset-not-found", "/nonexistent-asset", kind="text", provenance=CONFIG, cls=COUPLED, pending='implementation-plan.md M4-3')


def classify_numbers(text):
    """Return the ordered list of numeric literals exactly as the server wrote them."""
    return re.findall(rb'(?<![\w."])-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?', text)


now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
index = []
for s in SPECS:
    status, hdrs, body = fetch(s["method"], s["path"], s["host"])
    path, _, query = s["path"].partition("?")
    fx = {
        "name": s["name"],
        "description": s["desc"],
        "class": s["cls"],
        **({"pending": s["pending"]} if s["pending"] else {}),
        "provenance": s["provenance"],
        "captured_at": now,
        "request": {"method": s["method"], "path": path, "query": query,
                    "headers": {"Host": s["host"]}},
        "response": {
            "status": status,
            "headers": hdrs,
            "body_kind": s["kind"],
            "body_bytes": len(body),
            "body_sha256": hashlib.sha256(body).hexdigest(),
            "trailing_newline": body.endswith(b"\n"),
        },
        "assert": {
            "status": True,
            "headers": sorted(hdrs.keys()),
            "json_shape": s["kind"] == "json",
            "json_key_order": s["kind"] == "json" and s["key_order"],
            "numeric_format": s["kind"] == "json" and s["numeric_convention"] == "go_encoder",
            "numeric_convention": s["numeric_convention"],
            "trailing_newline": s["kind"] in ("json", "text"),
            "body_equals": s["kind"] == "text",
            "required_substrings": s["substrings"],
        },
    }
    if s["kind"] == "binary":
        if s["store_body"]:
            fx["response"]["body_base64"] = base64.b64encode(body).decode()
    elif s["store_body"]:
        fx["response"]["body"] = body.decode("utf-8")
    with open(os.path.join(OUT, s["name"] + ".json"), "w") as f:
        json.dump(fx, f, indent=2, sort_keys=False)
        f.write("\n")
    index.append({"name": s["name"], "file": "fixtures/" + s["name"] + ".json",
                  "method": s["method"], "path": path, "query": query, "host": s["host"],
                  "status": status, "body_kind": s["kind"], "class": s["cls"]})
    print(f"{status} {s['host']:15s} {s['path']:48s} {len(body):7d}B  {s['name']}")

with open(os.path.join(OUT, "..", "fixture-index.json"), "w") as f:
    json.dump({"captured_at": now, "fixtures": index}, f, indent=2)
    f.write("\n")
print(f"\n{len(index)} fixtures written to {OUT}")
