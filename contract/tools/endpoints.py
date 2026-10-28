"""Endpoint inventory for the frozen legacy REST surface.

Every claim here is backed by a captured fixture (`evidence`) or by a cited source line.
Order matches internal/controllers/restserver/controller.go setupRouter().
"""

COMMON = {
    "methods": "Any. No route restricts the HTTP method; GET and POST return identical bodies "
               "and HEAD returns the headers with an empty body.",
    "host_routing": "The Host header selects the website via websiteMiddleware. An unmatched Host "
                    "falls back to DefaultWebsite, which is the first website returned by the config "
                    "provider unless some website has an empty hostname. There is no 404-on-unknown-Host.",
    "compression": "gorilla/handlers.CompressHandler wraps every route, so every response carries "
                   "Vary: Accept-Encoding and is gzipped when the client asks.",
    "debug_header": "RW-Debug: 1 switches the GORM log level to Info for /span, /snow and /forecast. "
                    "It does not change the response.",
}

ENDPOINTS = [
    {
        "path": "/span/{span}",
        "grammar": "/span/<Go time.ParseDuration string>  e.g. /span/24h, /span/90m, /span/1h30m",
        "handler": "handlers.go GetWeatherSpan",
        "query": [
            {"name": "station", "required": True, "type": "device name",
             "note": "Required. 400 if absent, 404 if it is not a configured device name."},
            {"name": "station_id", "required": False, "type": "int",
             "note": "Accepted by both clients but IGNORED by this handler: only `station` selects data."},
            {"name": "format", "required": False, "values": ["msgpack"],
             "note": "NOT honoured here -- /span encodes with json.NewEncoder directly, not through the formatter."},
        ],
        "response": {"type": "array of WeatherReading", "wrapper": None},
        "statuses": {
            "200": "JSON array, possibly empty.",
            "400": "`station parameter is required`, `error: invalid span duration`, or "
                   "`time span exceeds maximum allowed duration of 1 year` -- plain text, trailing newline.",
            "404": "`station not found` -- plain text.",
            "500": "`error fetching weather data` or `database not enabled`.",
            "503": "JSON {error, message} when no website resolves.",
        },
        "headers": {"Content-Type": "application/json",
                    "Access-Control-Allow-Origin": "*",
                    "Cache-Control": "max-age=<data interval in seconds minus 1, default 60>"},
        "aggregate_source": "span <= 6h -> weather_1m; <= 48h -> weather_5m; <= 14d -> weather_1h; else weather_1d.",
        "clients": ["ios", "website"],
        "evidence": ["span-1h-one-minute-buckets", "span-24h-five-minute-buckets",
                     "span-72h-hourly-with-gap", "span-720h-daily", "span-empty",
                     "span-missing-station", "span-invalid-duration", "span-day-unit-rejected",
                     "span-exceeds-one-year", "span-station-not-found",
                     "span-snow-device-with-station-id"],
    },
    {
        "path": "/latest",
        "grammar": "/latest",
        "handler": "handlers.go GetWeatherLatest",
        "query": [
            {"name": "station_id", "required": False, "type": "int", "note": "Preferred form. 400 on a non-integer, 404 on an unknown id."},
            {"name": "station", "required": False, "type": "device name", "note": "Legacy form. 404 on an unknown name."},
            {"name": "format", "required": False, "values": ["msgpack"], "note": "Honoured."},
        ],
        "response": {"type": "WeatherReading", "wrapper": None},
        "statuses": {
            "200": "A single JSON object.",
            "400": "`invalid station_id`.",
            "404": "`station not found`, or `no recent weather data available for this station` when the "
                   "newest reading for the station is older than 10 minutes.",
            "500": "`error fetching weather data` or `database not enabled`.",
            "503": "JSON {error, message} when no website resolves.",
        },
        "headers": {"Content-Type": "application/json",
                    "Access-Control-Allow-Origin": "*",
                    "Cache-Control": "no-cache, no-store, must-revalidate"},
        "notes": ["Neither parameter given: the website's primary device is used.",
                  "The 10-minute recency window is the only place in the contract where staleness becomes a 404."],
        "clients": ["ios", "website"],
        "evidence": ["latest-full-complement", "latest-minimal-complement", "latest-by-station-id",
                     "latest-air-quality-device", "latest-snow-device", "latest-default-station",
                     "latest-snow-device-offline", "latest-no-recent-data", "latest-station-not-found",
                     "latest-station-id-not-found", "latest-invalid-station-id", "latest-msgpack"],
    },
    {
        "path": "/snow",
        "grammar": "/snow",
        "handler": "handlers.go GetSnowLatest",
        "query": [
            {"name": "station_id", "required": False, "type": "int"},
            {"name": "station", "required": False, "type": "device name"},
            {"name": "format", "required": False, "values": ["msgpack"], "note": "Honoured."},
        ],
        "response": {"type": "SnowReading", "wrapper": None},
        "statuses": {
            "200": "A single JSON object. All zeros when the gauge has never reported.",
            "400": "`invalid station_id`.",
            "404": "`snow data not enabled for this website` (website snow_enabled false), or `station not found`.",
            "500": "`error fetching readings from DB`, `error encoding snow data`, `database not enabled`.",
            "503": "JSON {error, message} when no website resolves.",
        },
        "headers": {"Content-Type": "application/json",
                    "Access-Control-Allow-Origin": "*",
                    "Cache-Control": "no-cache, no-store, must-revalidate"},
        "notes": [
            "The iOS client sends NO query parameters at all; the website sends station_id.",
            "station/station_id only redirect the depth lookup. stationname and all six totals come "
            "from the website's configured snow_device_name regardless.",
            "There is NO recency filter on the depth query, so an offline gauge keeps returning its "
            "last known depth indefinitely.",
        ],
        "clients": ["ios", "website"],
        "evidence": ["snow-reporting", "snow-by-station-id", "snow-station-param-overrides-depth-only",
                     "snow-device-stale", "snow-device-never-reported", "snow-disabled-for-website",
                     "snow-msgpack"],
    },
    {
        "path": "/almanac",
        "grammar": "/almanac",
        "handler": "handlers.go GetAlmanac",
        "query": [{"name": "station", "required": True, "type": "device name"}],
        "response": {"type": "AlmanacData", "wrapper": None},
        "statuses": {"200": "JSON object; `{}` when the station has no cached extremes.",
                     "400": "`station parameter is required`.",
                     "404": "`station not found`.",
                     "500": "`error querying almanac data` or `database not enabled`.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "application/json",
                    "Access-Control-Allow-Origin": "*", "Cache-Control": "max-age=3600"},
        "notes": ["Every field is omitempty, so absent metrics are absent keys, never nulls.",
                  "Air-quality extremes are read from the website's air_quality_device, not from `station`.",
                  "Snow extremes (deepest_snow, max_snow_hour, max_snow_day) are never populated: "
                  "handlers.go:1416 disables them."],
        "clients": ["website"],
        "evidence": ["almanac-populated", "almanac-empty", "almanac-missing-station",
                     "almanac-station-not-found"],
    },
    {
        "path": "/forecast/{span}",
        "grammar": "/forecast/<1 to 4 digits>  (regex ^\\d{1,4}$). The value is forecast_span_hours, "
                   "not a duration: 24 is the hourly forecast, 240 the ten-day one.",
        "handler": "handlers.go GetForecast",
        "registered_when": "at least one device has aeris_enabled; otherwise the route does not exist (404 from the file server)",
        "query": [
            {"name": "station_id", "required": False, "type": "int", "note": "Preferred."},
            {"name": "station", "required": False, "type": "device name", "note": "Legacy."},
            {"name": "period", "required": False, "type": "int >= 0",
             "note": "Replaces the data array with the single element at that index -- an OBJECT, not an array."},
            {"name": "format", "required": False, "values": ["msgpack"], "note": "Honoured."},
        ],
        "response": {"type": "raw jsonb passthrough", "wrapper": '{"lastUpdated": <string>, "data": <stored jsonb>}'},
        "statuses": {
            "200": "Wrapped object.",
            "400": "Empty body when the span fails the regex; a JSON error body served as text/plain for a bad `period`.",
            "404": "EMPTY body, no Content-Type, when no forecast record matches.",
            "500": "`error encoding forecast data` or `database not enabled`.",
            "503": "JSON {error, message}.",
        },
        "headers": {"Content-Type": "application/json", "Access-Control-Allow-Origin": "*"},
        "notes": [
            "lastUpdated is Go's time.Time.String() -- `2006-01-02 15:04:05.999999999 -0700 MST`, "
            "NOT RFC3339. weather-utils.js parses it with moment format `YYYY-MM-DD HH:mm:ss.SSSSSS-ZZ`.",
            "The response is NOT newline-terminated: WriteRawJSON writes bytes rather than using json.Encoder.",
            "`data` is Postgres jsonb written back verbatim, so key order is jsonb-normalised (key length, "
            "then bytewise) and the numeric text is whatever was stored -- `0.0`, not `0`.",
            "The 18 period fields come from json.Marshal of []AerisWeatherForecastPeriod in "
            "internal/controllers/aerisweather/controller.go, so the shape is server-controlled, not Aeris-controlled.",
        ],
        "clients": ["ios", "website"],
        "evidence": ["forecast-240-ten-daily-periods", "forecast-24-hourly-periods",
                     "forecast-single-period", "forecast-empty-data", "forecast-station-not-found",
                     "forecast-span-not-found", "forecast-span-too-many-digits",
                     "forecast-period-out-of-range", "forecast-period-not-a-number", "forecast-msgpack"],
    },
    {
        "path": "/alerts",
        "grammar": "/alerts",
        "handler": "handlers.go GetAlerts",
        "registered_when": "at least one device has aeris_enabled",
        "query": [{"name": "station_id", "required": True, "type": "int"}],
        "response": {"type": "array of Alert", "wrapper": None},
        "statuses": {"200": "JSON array, possibly empty.",
                     "400": "`station_id parameter is required` or `invalid station_id parameter`.",
                     "500": "`error querying alerts` or `database not enabled`.",
                     "503": "JSON {error, message} through the formatter."},
        "headers": {"Content-Type": "application/json",
                    "Access-Control-Allow-Origin": "*", "Cache-Control": "max-age=300"},
        "notes": ["Active alerts only: begins_at <= now AND expires_at > now, ordered begins_at DESC.",
                  "No shipped client reads this endpoint. The website JS never references it."],
        "clients": [],
        "evidence": ["alerts-active", "alerts-empty", "alerts-missing-station-id",
                     "alerts-invalid-station-id"],
    },
    {
        "path": "/",
        "grammar": "/ exactly (gorilla/mux HandleFunc, so /latest/ and other trailing-slash paths fall "
                   "through to the file server and 404)",
        "handler": "handlers.go ServeWeatherWebsiteTemplate",
        "response": {"type": "text/html", "template": "weather-station.html.tmpl"},
        "statuses": {"200": "Rendered HTML.",
                     "500": "`No device configured for this website`.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "text/html"},
        "notes": ["A portal website serves portal.html.tmpl here instead.",
                  "No Cache-Control header is set."],
        "clients": ["website"],
        "evidence": ["html-weather-station", "html-portal-root"],
    },
    {
        "path": "/js/weather-app.js",
        "grammar": "/js/weather-app.js exactly",
        "handler": "handlers.go ServeWeatherAppJS",
        "response": {"type": "text/javascript", "template": "js/weather-app.js.tmpl"},
        "statuses": {"200": "Rendered JavaScript.", "500": "`No device configured for this website`.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "text/javascript"},
        "notes": ["Content-Type has NO charset, unlike the static JS the file server returns "
                  "(text/javascript; charset=utf-8). A client that string-matches the content type "
                  "would see two different values for two .js URLs on the same site.",
                  "This is the one deliberate exception to carrying the website forward as-is (DESIGN D-38)."],
        "clients": ["website"],
        "evidence": ["asset-weather-app-js", "asset-static-js"],
    },
    {
        "path": "/portal",
        "grammar": "/portal exactly",
        "handler": "handlers.go ServePortal",
        "response": {"type": "text/html", "template": "portal.html.tmpl"},
        "statuses": {"200": "Rendered HTML (portal websites only).",
                     "404": "`404 page not found` on a non-portal website.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "text/html"},
        "notes": ["portal.html.tmpl consumes NO template variables at all; PageTitle is passed and ignored."],
        "clients": ["website"],
        "evidence": ["html-portal", "html-portal-on-non-portal"],
    },
    {
        "path": "/privacy",
        "grammar": "/privacy exactly",
        "handler": "handlers.go ServePrivacy (static file, not a template)",
        "response": {"type": "text/html; charset=utf-8"},
        "statuses": {"200": "privacy.html verbatim.", "404": "`Privacy policy not found`."},
        "headers": {"Content-Type": "text/html; charset=utf-8"},
        "notes": ["Served for EVERY website, including portals, and before any website check -- "
                  "it is the only route that works with no website configured."],
        "clients": ["website"],
        "evidence": ["html-privacy"],
    },
    {
        "path": "/support",
        "grammar": "/support exactly",
        "handler": "handlers.go ServeSupport (static file)",
        "response": {"type": "text/html; charset=utf-8"},
        "statuses": {"200": "support.html verbatim.", "404": "`Support page not found`."},
        "headers": {"Content-Type": "text/html; charset=utf-8"},
        "clients": ["website"],
        "evidence": ["html-support"],
    },
    {
        "path": "/api/stations",
        "grammar": "/api/stations exactly",
        "handler": "handlers.go GetStations",
        "response": {"type": "array of StationData"},
        "statuses": {"200": "JSON array.",
                     "403": "`stations API not available for this website` on a non-portal website.",
                     "500": "`error loading station data` / `error loading website data` / `error encoding stations`.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "application/json", "Access-Control-Allow-Origin": "*"},
        "notes": ["Devices with latitude == 0 AND longitude == 0 are excluded.",
                  "Disabled devices ARE included, with enabled: false.",
                  "The nested `website` object is present only for devices bound to a non-portal website "
                  "that has a hostname."],
        "clients": ["website"],
        "evidence": ["api-stations-portal", "api-stations-forbidden"],
    },
    {
        "path": "/api/remote-stations",
        "grammar": "/api/remote-stations exactly",
        "handler": "handlers.go GetRemoteStations",
        "response": {"type": "array of RemoteStationResponse (declared inside the handler)"},
        "statuses": {"200": "JSON array.",
                     "403": "`remote stations API not available for this website`.",
                     "500": "`error loading remote station data`, including when any text column of a "
                            "remote_stations row is NULL -- the scan into a Go string fails.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "application/json", "Access-Control-Allow-Origin": "*"},
        "notes": ["status is derived from last_seen: < 5 min online, < 30 min stale, else offline.",
                  "No shipped client reads this. The only in-repo consumer is the management UI, which "
                  "fetches `/api/remote-stations` relative to the management server, where no such route exists."],
        "clients": [],
        "evidence": ["api-remote-stations-populated", "api-remote-stations-forbidden"],
    },
    {
        "path": "/stationinfo",
        "grammar": "/stationinfo exactly",
        "handler": "handlers.go GetStationInfo",
        "response": {"type": "StationInfoResponse"},
        "statuses": {"200": "JSON object.",
                     "500": "`error loading station data` or `error encoding station info`.",
                     "503": "JSON {error, message}."},
        "headers": {"Content-Type": "application/json", "Access-Control-Allow-Origin": "*"},
        "notes": ["Takes no parameters: the website is selected entirely by the Host header.",
                  "`stations` lists every device with a location, enabled or not, including the snow "
                  "gauge and the air-quality monitor. The iOS client filters those out client-side.",
                  "This is the only endpoint carrying a live compatibility hazard -- see `hazards`."],
        "clients": ["ios", "website"],
        "evidence": ["stationinfo-full", "stationinfo-about-text-omitted", "stationinfo-no-weather-device"],
    },
    {
        "path": "/fonts/",
        "grammar": "PathPrefix /fonts/",
        "handler": "cachingMiddleware(365d) + http.FileServer over the embedded assets FS",
        "response": {"type": "per-file, from Go's mime sniffing (font/ttf, font/woff2, image/svg+xml, ...)"},
        "statuses": {"200": "File bytes, or a directory listing for /fonts/ itself.",
                     "404": "`404 page not found`."},
        "headers": {"Cache-Control": "public, max-age=31536000, immutable", "Vary": "Accept-Encoding"},
        "notes": ["This is the ONLY asset tree with a Cache-Control header."],
        "clients": ["website"],
        "evidence": ["asset-font"],
    },
    {
        "path": "/",
        "grammar": "PathPrefix / -- the catch-all file server over the embedded assets FS",
        "handler": "http.FileServer",
        "response": {"type": "per-file, from Go's mime sniffing"},
        "statuses": {"200": "File bytes, or a directory listing for a directory path.",
                     "404": "`404 page not found`."},
        "headers": {"Vary": "Accept-Encoding"},
        "notes": ["Serves /js/, /css/, /images/, favicons, site.webmanifest, privacy.html, support.html.",
                  "No Cache-Control. Directory listings ARE exposed for /js/, /css/, /images/, /fonts/.",
                  "site.webmanifest is served as text/plain; charset=utf-8, not application/manifest+json."],
        "clients": ["website"],
        "evidence": ["asset-static-js", "asset-static-css", "asset-webmanifest", "asset-favicon",
                     "asset-not-found"],
    },
]

EXTRA_TYPES = {
    "RemoteStationResponse": {
        "declared_in": "handlers.go, inside GetRemoteStations -- a function-local type, so it is "
                       "invisible to any tool that scans package-level declarations",
        "fields": [
            {"json": "station_id", "go_type": "string", "json_type": "string", "omitempty": False},
            {"json": "station_name", "go_type": "string", "json_type": "string", "omitempty": False},
            {"json": "station_type", "go_type": "string", "json_type": "string", "omitempty": False},
            {"json": "last_seen", "go_type": "time.Time", "json_type": "string", "omitempty": False},
            {"json": "services", "go_type": "anonymous struct", "json_type": "object", "omitempty": False,
             "fields": [
                 {"json": "aprs_enabled", "go_type": "bool", "json_type": "boolean", "omitempty": False},
                 {"json": "wu_enabled", "go_type": "bool", "json_type": "boolean", "omitempty": False},
                 {"json": "aeris_enabled", "go_type": "bool", "json_type": "boolean", "omitempty": False},
                 {"json": "pws_enabled", "go_type": "bool", "json_type": "boolean", "omitempty": False},
             ]},
            {"json": "status", "go_type": "string", "json_type": "string", "omitempty": False,
             "values": ["online", "stale", "offline"]},
        ],
    },
    "AerisWeatherForecastPeriod": {
        "declared_in": "internal/controllers/aerisweather/controller.go; marshalled into the jsonb "
                       "column that /forecast returns verbatim",
        "fields": [
            {"json": "dateTimeISO", "go_type": "time.Time", "json_type": "string"},
            {"json": "maxTempF", "go_type": "int16", "json_type": "number"},
            {"json": "minTempF", "go_type": "int16", "json_type": "number"},
            {"json": "avgTempF", "go_type": "int16", "json_type": "number"},
            {"json": "pop", "go_type": "int16", "json_type": "number"},
            {"json": "precipIN", "go_type": "float32", "json_type": "number"},
            {"json": "iceaccumIN", "go_type": "float32", "json_type": "number"},
            {"json": "snowIN", "go_type": "float32", "json_type": "number"},
            {"json": "maxFeelslikeF", "go_type": "int16", "json_type": "number"},
            {"json": "minFeelslikeF", "go_type": "int16", "json_type": "number"},
            {"json": "windSpeedMPH", "go_type": "int16", "json_type": "number"},
            {"json": "windSpeedMaxMPH", "go_type": "int16", "json_type": "number"},
            {"json": "windDir", "go_type": "string", "json_type": "string"},
            {"json": "windDirDEG", "go_type": "uint16", "json_type": "number"},
            {"json": "weather", "go_type": "string", "json_type": "string"},
            {"json": "weatherPrimaryCoded", "go_type": "string", "json_type": "string"},
            {"json": "weatherIcon", "go_type": "string", "json_type": "string"},
            {"json": "compactWeather", "go_type": "string", "json_type": "string"},
        ],
    },
}

QUIRKS = [
    {"id": "Q-1", "summary": "`ohum` and `outhumidity` are the same measurement, emitted twice.",
     "detail": "transform.go sets both OutHumidity and OutsideHumidity from r.OutHumidity. The website "
               "reads `ohum`; the iOS client decodes both. Neither may be dropped.",
     "evidence": "latest-full-complement"},
    {"id": "Q-2", "summary": "Field naming is inconsistent and must stay that way.",
     "detail": "`otemp`, `bar`, `winds`, `windd`, `itemp`, `ihum`, `windch`, `heatidx` are run together; "
               "`period_rain` is snake_case; `moonPhaseName`, `moonIllumination`, `moonAge`, "
               "`moonCrescentAngle` are camelCase; everything else is lower-concatenated.",
     "evidence": "latest-full-complement"},
    {"id": "Q-3", "summary": "`ts` is milliseconds since the Unix epoch as a JSON integer.",
     "detail": "transform.go uses UnixMilli() on Timestamp for /latest and on Bucket for /span. "
               "Go emits it as a bare integer (Int64), 13 digits at present. Swift decodes it as Int64.",
     "evidence": "latest-full-complement"},
    {"id": "Q-4", "summary": "`snowdepth` is MILLIMETRES on /latest and INCHES on /snow and /span.",
     "detail": "fetchLatestReading selects (base_distance - snowdistance) in mm and transformLatestReadings "
               "passes it through unconverted; transformSpanReadings and GetSnowLatest both apply "
               "mmToInchesWithThreshold. Same key, same endpoint family, two units.",
     "evidence": ["latest-snow-device", "snow-reporting", "span-72h-hourly-with-gap"]},
    {"id": "Q-5", "summary": "`windcard` carries omitempty but is never absent.",
     "detail": "headingToCardinalDirection always returns one of sixteen non-empty strings, including for "
               "a wind direction of 0 (`N`), so the key is always present in practice.",
     "evidence": "latest-full-complement"},
    {"id": "Q-6", "summary": "/span never populates windgust, rainfall24h/48h/72h or rainfallstorm.",
     "detail": "transformSpanReadings does not set them, and they have no omitempty, so every /span element "
               "carries them as 0. Only /latest computes them.",
     "evidence": "span-24h-five-minute-buckets"},
    {"id": "Q-7", "summary": "/span overwrites extrafloat1 with a smoothed snow-depth estimate for snow stations.",
     "detail": "fetchWeatherSpan joins snow_depth_est_5m and writes snow_depth_est_in into ExtraFloat1 when "
               "the station has a base snow distance. The same key means PM1.0 on an AirGradient device. "
               "SnowResponse.swift documents this ('Optional because /snow doesn't include them, but /span does').",
     "evidence": "span-72h-hourly-with-gap"},
    {"id": "Q-8", "summary": "/snow reports the website's snow device name even when another station was queried.",
     "detail": "stationname is always website.SnowDeviceName; `station`/`station_id` only redirect the depth "
               "lookup. Querying /snow?station=<a weather station> yields that station's (absent) depth under "
               "the snow gauge's name, with the snow gauge's totals.",
     "evidence": "snow-station-param-overrides-depth-only"},
    {"id": "Q-9", "summary": "A gap in /span data is simply missing buckets.",
     "detail": "The continuous aggregates produce no row for a bucket with no readings, and the handler does "
               "not gap-fill, so the ts sequence jumps. Clients must not assume a constant interval.",
     "evidence": "span-72h-hourly-with-gap"},
    {"id": "Q-10", "summary": "Bad-`period` errors on /forecast are JSON bodies labelled text/plain.",
     "detail": "w.WriteHeader(400) runs before formatter.WriteResponse, so Go has already committed the "
               "sniffed Content-Type. The body is valid JSON; the header says otherwise.",
     "evidence": ["forecast-period-out-of-range", "forecast-period-not-a-number"]},
    {"id": "Q-11", "summary": "Three /forecast failure modes return a completely empty body and no Content-Type.",
     "detail": "A span failing the 1-to-4-digit regex, a span with no record, and a station name with no device "
               "all call w.WriteHeader(...) and return. Content-Length is 0 and there is no Content-Type header.",
     "evidence": ["forecast-span-too-many-digits", "forecast-span-not-found", "forecast-station-not-found"]},
    {"id": "Q-12", "summary": "Hand-encoded endpoints end with a newline; /forecast does not.",
     "detail": "json.NewEncoder(w).Encode appends '\\n'. /forecast goes through WriteRawJSON, which writes "
               "the jsonb bytes with no terminator. This is a real byte-level difference.",
     "evidence": ["latest-full-complement", "forecast-240-ten-daily-periods"]},
    {"id": "Q-13", "summary": "Go emits `0`, not `0.0`, for a zero float32; jsonb passthrough emits whatever was stored.",
     "detail": "encoding/json uses the shortest round-tripping representation, so 0 is `0`, 57.0 is `57`, "
               "23.730469 is `23.730469`. /forecast's `data` is Postgres jsonb text, which preserves the "
               "numeric literal as written -- the same contract therefore contains both `0` and `0.0`.",
     "evidence": ["latest-full-complement", "forecast-240-ten-daily-periods"]},
    {"id": "Q-14", "summary": "`format=msgpack` is a parallel encoding of the same contract, and loses number types.",
     "detail": "responseformat.Formatter switches to vmihailenco/msgpack with SetCustomStructTag(\"json\"), so "
               "the key names match. For /forecast it decodes the jsonb into interface{} first, so every number "
               "becomes a float64. /span does NOT honour the parameter at all. No shipped client sends it.",
     "evidence": ["latest-msgpack", "snow-msgpack", "forecast-msgpack"]},
    {"id": "Q-15", "summary": "`/span` is 500, not 200, for a station whose winddir is NULL in the window.",
     "detail": "circular_avg_final divides by the accumulator, which stays 0 when the aggregate saw no non-NULL "
               "winddir, so the aggregate query fails with SQLSTATE 22012. The shipping Go daemon never writes "
               "NULL -- it inserts its whole reading struct, zeros included -- so the bug is latent today. It "
               "becomes live for any writer that leaves unpopulated metrics NULL.",
     "evidence": "reproduced during capture; see docs/legacy-api-contract.md section 8"},
    {"id": "Q-16", "summary": "`/api/remote-stations` is 500 for a remote_stations row with any NULL text column.",
     "detail": "The row is scanned into Go strings, and `converting NULL to string is unsupported` surfaces as "
               "`error loading remote station data`.",
     "evidence": "reproduced during capture; see docs/legacy-api-contract.md section 8"},
    {"id": "Q-17", "summary": "Every endpoint accepts every HTTP method.",
     "detail": "No route calls .Methods(...). POST /latest returns the same body as GET. HEAD returns headers "
               "with an empty body.",
     "evidence": "probed during capture; see docs/legacy-api-contract.md section 1"},
    {"id": "Q-18", "summary": "An unknown Host silently serves the first configured website.",
     "detail": "websiteMiddleware falls back to DefaultWebsite, set to the first website the config provider "
               "returns unless one has an empty hostname. Nothing distinguishes 'wrong host' from 'right host'.",
     "evidence": "probed during capture; see docs/legacy-api-contract.md section 1"},
    {"id": "Q-19", "summary": "Almanac snow metrics are permanently absent.",
     "detail": "deepest_snow, max_snow_hour and max_snow_day are declared in AlmanacData but never assigned; "
               "handlers.go:1416 carries `Snow metrics temporarily disabled due to performance issues`. The "
               "keys are omitempty, so they simply never appear.",
     "evidence": "almanac-populated"},
    {"id": "Q-20", "summary": "Directory listings are served for the asset trees.",
     "detail": "http.FileServer renders an HTML index for /js/, /css/, /images/ and /fonts/. Whether the Rust "
               "implementation reproduces that is a decision, not an accident -- record it either way.",
     "evidence": "probed during capture; see docs/legacy-api-contract.md section 6"},
]
