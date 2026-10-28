# The frozen legacy REST contract

> **W1 output.** This document records the external HTTP surface of the Go `remoteweather`
> daemon **exactly as it behaves today**, so that the Rust rewrite can reproduce it. It contains
> no proposed improvements. Where the current behaviour is wrong, it says so and leaves it alone
> (`DESIGN.md` D-37).
>
> Machine-readable companion: [`contract/legacy-v1.json`](../contract/legacy-v1.json).
> Golden fixtures: [`contract/fixtures/`](../contract/fixtures/).
> Conformance runner: [`contract/conformance/`](../contract/conformance/).
>
> **Nothing in this document contradicts `DESIGN.md`.** The closing *Proposed amendments* section
> proposes four additions, none of which replaces a decision.

---

## 0. What this is, in one page

Two independent clients must keep working after the rewrite and both are frozen:

| Client | Source | Surface it uses | How it decodes |
|---|---|---|---|
| iOS / watchOS / widgets | `git@github.com:chrissnell/RemoteWeather.app.git` @ `77967ab` | `/latest`, `/span/{n}h`, `/snow`, `/stationinfo`, `/forecast/{n}` | `Foundation.JSONDecoder`, **strict**: a missing key for a non-optional property fails the whole decode |
| Public weather website | `internal/controllers/restserver/assets/js/` in the Go repo | the above plus `/almanac`, `/api/stations`, `/`, `/portal`, `/privacy`, `/support`, `/js/`, `/css/`, `/fonts/`, `/images/` | `Response.json()`, **tolerant**: every read is guarded with `\|\| '--'` or an `!= null` check |

The contract is the **union** of the two. Where a client is stricter than the server, the client
wins: the server may emit fields nobody reads, but it must never omit or null a field any client
requires.

Three facts dominate everything else:

1. **`/stationinfo` carries the only live compatibility hazard.** `about_text` and
   `weather_device` are `omitempty` on the Go side and **non-optional** on the Swift side. Both are
   routinely absent. Section 4.
2. **`snowdepth` is millimetres on `/latest` and inches on `/snow` and `/span`.** Same key, same
   endpoint family, two units. Fixtures `latest-snow-device` (`120`) and `snow-reporting` (`4.7`)
   are the same measurement. Section 5, Q-4.
3. **The contract contains two different JSON encoders.** Eight endpoints go through Go's
   `encoding/json`; `/forecast` writes Postgres `jsonb` text back verbatim. They disagree on key
   order, on inter-token whitespace, on numeric literals (`0` versus `0.0`) and on whether the body
   ends with a newline. Section 6.

### How this was captured

`DESIGN.md` OQ-1 records that production is not reachable from an agent runtime, so "the current Go
server" means one built and run here:

| | |
|---|---|
| Server | `go build ./cmd/remoteweather` from `chrissnell/remoteweather` @ **`c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf`**, reporting `remoteweather 5.10.22` |
| Database | `timescale/timescaledb-ha:pg17` in the homelab Kubernetes cluster — **PostgreSQL 17.11 + TimescaleDB 2.31.0**, the same image W0's spike used |
| Schema | created by the daemon itself (`internal/storage/timescaledb/sql.go` runs on every start), plus weather migrations 005 and 028–030 for `almanac_cache` and 025 for `snow_depth_est_5m` |
| Stations | `cmd/davis-emulator` and `cmd/campbell-emulator` over TCP, driving a real Davis and a real Campbell driver; snow gauge, air-quality monitor, Aeris forecasts and alerts seeded directly (no emulator exists for those paths) |
| Websites | six, so that Host-based routing, portal mode, snow-enabled/disabled and both hazard cases are all reachable |

Every fixture records its own `provenance`. The rig is reproducible from
[`contract/tools/`](../contract/tools/); see that directory's README.

**Byte-level compatibility is a property of the Go encoder and `restserver/types.go`, not of
production's data.** A locally driven server was in fact necessary rather than merely sufficient:
the minimal-sensor, data-gap, empty-response and snow-device-absent cases had to be *produced*.

---

## 1. Transport behaviour common to every route

These are not per-endpoint details; they apply to the whole surface and each one is a way the
rewrite could silently break a client.

**Every route accepts every HTTP method.** No route calls `.Methods(...)`. `POST /latest` returns
the same body as `GET /latest`; `HEAD` returns the headers with an empty body. (Q-17)

**The `Host` header selects the website, and an unknown host is not an error.**
`websiteMiddleware` looks the hostname up in a map and falls back to `DefaultWebsite` on a miss.
`DefaultWebsite` is the first website the config provider returns, unless some website has an empty
hostname, in which case that one wins (`controller.go:186`). Nothing in the response distinguishes
"you asked for the right host" from "you asked for an unknown host and got someone else's station".
(Q-18)

**Port is stripped from `Host` before matching**, so `localhost:18080` and `localhost` are the same
website.

**Everything is behind `gorilla/handlers.CompressHandler`**, so every response carries
`Vary: Accept-Encoding` and is gzipped when the client asks for it. The fixtures deliberately record
**uncompressed** bodies (`Accept-Encoding: identity`); compression is a transport concern.

**`RW-Debug: 1`** switches the GORM log level to `Info` for `/span`, `/snow` and `/forecast`. It
changes the server's logs, not the response. It is a side effect on a shared `*gorm.DB`, so it
affects concurrent requests too.

**CORS** is `Access-Control-Allow-Origin: *` on every JSON endpoint, and absent on the HTML and
asset routes.

**Routes are exact, not prefixes.** `/latest/` with a trailing slash falls through to the catch-all
file server and returns `404 page not found`.

**`/forecast/{span}` and `/alerts` only exist when at least one device has Aeris enabled.** When no
device does, the routes are never registered and the catch-all file server answers them — a
`404 page not found` body rather than any of the handler's own error shapes.

---

## 2. Endpoint inventory

Fourteen handler routes plus two file-server prefixes. The `Clients` column is what the shipped
clients actually call, verified by reading every iOS target and every website JS module.

| Endpoint | Grammar | Clients | 200 body | Cache-Control |
|---|---|---|---|---|
| `GET /latest` | exact | iOS, website | `WeatherReading` object | `no-cache, no-store, must-revalidate` |
| `GET /span/{span}` | `{span}` is a **Go `time.ParseDuration` string** | iOS, website | array of `WeatherReading` | `max-age=<interval−1>`, default `max-age=60` |
| `GET /snow` | exact | iOS, website | `SnowReading` object | `no-cache, no-store, must-revalidate` |
| `GET /stationinfo` | exact | iOS, website | `StationInfoResponse` object | none |
| `GET /forecast/{span}` | `{span}` matches `^\d{1,4}$`, and is **hours of total span**, not a duration | iOS, website | `{"lastUpdated": …, "data": …}` | none |
| `GET /almanac` | exact | website | `AlmanacData` object | `max-age=3600` |
| `GET /alerts` | exact | **none** | array of `Alert` | `max-age=300` |
| `GET /api/stations` | exact | website (portal) | array of `StationData` | none |
| `GET /api/remote-stations` | exact | **none** | array of `RemoteStationResponse` | none |
| `GET /` | exact | website | `text/html` from `weather-station.html.tmpl` (or `portal.html.tmpl`) | none |
| `GET /portal` | exact | website | `text/html` from `portal.html.tmpl` | none |
| `GET /privacy` | exact | website | `text/html; charset=utf-8`, static | none |
| `GET /support` | exact | website | `text/html; charset=utf-8`, static | none |
| `GET /js/weather-app.js` | exact | website | `text/javascript` from `js/weather-app.js.tmpl` | none |
| `GET /fonts/…` | prefix | website | file bytes | `public, max-age=31536000, immutable` |
| `GET /…` | prefix (catch-all) | website | file bytes, or an HTML directory index | none |

`/alerts` and `/api/remote-stations` have **no shipped consumer.** The website JS never names
`/alerts`, and the only in-repo reference to `/api/remote-stations` is
`internal/controllers/management/assets/js/management-remote-stations.js`, which fetches it
*relative to the management server* — where no such route is registered. Both are still part of the
frozen surface and must be reproduced; neither constrains the rewrite's field choices through a
client.

### 2.1 Query parameters, exhaustively

| Endpoint | Parameter | Type | Required | Behaviour |
|---|---|---|---|---|
| `/latest` | `station_id` | int | no | Preferred. Non-integer → `400 invalid station_id`. Unknown id → `404 station not found`. |
| `/latest` | `station` | device name | no | Legacy. Unknown name → `404 station not found`. Ignored when `station_id` is present. |
| `/latest` | `format` | `msgpack` | no | Honoured. Any other value is JSON. |
| `/latest` | *(neither)* | | | The website's primary device is used. |
| `/span/{span}` | `station` | device name | **yes** | Absent → `400 station parameter is required`. Unknown → `404 station not found`. |
| `/span/{span}` | `station_id` | int | no | **Accepted by both clients and ignored by the handler.** Only `station` selects data. |
| `/span/{span}` | `format` | `msgpack` | no | **Not honoured** — this handler encodes with `json.NewEncoder` directly. |
| `/snow` | `station_id` | int | no | Redirects the *depth* query only. |
| `/snow` | `station` | device name | no | Redirects the *depth* query only. |
| `/snow` | `format` | `msgpack` | no | Honoured. |
| `/stationinfo` | — | | | Takes no parameters at all; the website is chosen entirely by `Host`. |
| `/forecast/{span}` | `station_id` | int | no | Preferred. |
| `/forecast/{span}` | `station` | device name | no | Legacy; resolved to a device id. Unknown → `404 {"error":"Station not found"}` served as `text/plain`. Fixture `forecast-by-station-name` / `forecast-station-name-not-found`. |
| `/forecast/{span}` | `period` | int ≥ 0 | no | Replaces `data` with the single element at that index — **an object, not an array**. |
| `/forecast/{span}` | `format` | `msgpack` | no | Honoured. |
| `/almanac` | `station` | device name | **yes** | Absent → `400`. Unknown → `404`. |
| `/alerts` | `station_id` | int | **yes** | Absent → `400 station_id parameter is required`. Non-integer → `400 invalid station_id parameter`. |

The iOS client sends **no parameters at all** on `/snow` and `/stationinfo`; both depend entirely on
the server's per-website configuration. That makes those two endpoints' answers a function of
`Host`, which matters for a rewrite that might otherwise be tempted to make them parameterised.

### 2.2 Status codes and error body shapes

Four distinct error-body conventions coexist. A client that parses error bodies — neither shipped
client does, but the contract is frozen regardless — sees all four.

| Convention | Content-Type | Body | Where |
|---|---|---|---|
| `http.Error` plain text | `text/plain; charset=utf-8` | message plus `\n` | most 400/403/404/500 |
| `json.Encoder` object | `application/json` | `{"error":…,"message":…}` + `\n` | the 503 "no websites configured" paths |
| JSON body, text/plain header | `text/plain; charset=utf-8` | `{"error":…}` + `\n` | `/forecast` bad `period`, and `/forecast` with an unknown `station` name (Q-10) |
| empty | **absent** | zero bytes | three `/forecast` paths (Q-11) |

Exact bodies, verified by fixture:

| Status | Body | Fixture |
|---|---|---|
| 400 | `invalid station_id\n` | `latest-invalid-station-id` |
| 400 | `station parameter is required\n` | `span-missing-station`, `almanac-missing-station` |
| 400 | `error: invalid span duration\n` | `span-invalid-duration`, `span-day-unit-rejected` |
| 400 | `time span exceeds maximum allowed duration of 1 year\n` | `span-exceeds-one-year` |
| 400 | `station_id parameter is required\n` | `alerts-missing-station-id` |
| 400 | `invalid station_id parameter\n` | `alerts-invalid-station-id` |
| 400 | `{"error":"Invalid period parameter - must be a number"}\n` | `forecast-period-not-a-number` |
| 400 | `{"error":"period 99 exceeds available periods (valid range: 0-9)"}\n` | `forecast-period-out-of-range` |
| 400 | *(empty)* | `forecast-span-too-many-digits` |
| 403 | `stations API not available for this website\n` | `api-stations-forbidden` |
| 403 | `remote stations API not available for this website\n` | `api-remote-stations-forbidden` |
| 404 | `station not found\n` | `latest-station-not-found`, `span-station-not-found`, `almanac-station-not-found` |
| 404 | `no recent weather data available for this station\n` | `latest-no-recent-data`, `latest-snow-device-offline` |
| 404 | `snow data not enabled for this website\n` | `snow-disabled-for-website` |
| 404 | `404 page not found\n` | `html-portal-on-non-portal`, `asset-not-found` |
| 404 | `{"error":"Station not found"}\n` | `forecast-station-name-not-found` |
| 404 | *(empty)* | `forecast-station-not-found`, `forecast-span-not-found` |

**`/latest` is the only endpoint where staleness becomes a 404.** `fetchLatestReading` filters
`time >= NOW() - INTERVAL '10 minutes'`, so a station that stopped reporting eleven minutes ago is
indistinguishable from one that does not exist. Both `/snow` and `/span` have no recency filter at
all.

**`/span` returns `200 []` when there is no data in the window**, not 404 (`span-empty`).

### 2.3 `/span`'s aggregate selection

`{span}` is a Go duration, so `/span/30d` is a **400**: `time.ParseDuration` has no day unit.
`/span/1h30m` and `/span/90m` are valid. Both shipped clients only ever build `/span/{integer}h`.

| Span | Table read |
|---|---|
| ≤ 6 h | `weather_1m` |
| > 6 h and ≤ 48 h | `weather_5m` |
| > 48 h and ≤ 14 d | `weather_1h` |
| > 14 d | `weather_1d` |
| > 365 d | `400` |

The bucket timestamp becomes `ts`. A bucket with no readings produces no row, so **a gap in the data
is missing array elements, not nulls** — clients must not assume a constant interval (Q-9, fixture
`span-72h-hourly-with-gap`).

---

## 3. Field-by-field response schema

"Emitted" is what the Go struct tag says. "iOS" is the Swift property type the key decodes into, and
`—` means the iOS client does not decode that key at all. "Website" means some website JS module
reads the key by name.

Counts, derived mechanically from source rather than transcribed:

| | |
|---|---|
| JSON tags in `restserver/types.go` | **133** |
| JSON tags across all `restserver` response structs with tags | **153** (163 including the ten in the function-local `RemoteStationResponse`) |
| `omitempty` tags among those 153 | **48** (30 of them in `types.go`) |
| Swift decoded properties in `WeatherResponse.swift` | **126** across 4 structs |
| Swift decoded properties including `StationInfo.swift` | **139** across 6 structs, **105 non-optional** |

> Two numbers in the W1 prompt are slightly off and are corrected here: `types.go` has **30**
> `omitempty` tags, not 31; and `WeatherResponse.swift` holds **4** structs with 126 properties and
> **no** explicit `CodingKeys` at all. The only `CodingKeys` in the iOS REST layer are the ten in
> `StationInfo.swift` and one in `RadarMapsClient.swift` — and the `schema_version` key belongs to
> the external `maps.nw5w.com` radar manifest, not to this server.


#### `WeatherReading` — `/latest`, `/span/{span}`

| JSON key | Go type | Emitted | iOS | Website |
|---|---|---|---|---|
| `stationname` | `string` | always | `String` | reads |
| `stationtype` | `string` | `omitempty` | `String?` | reads |
| `ts` | `int64` | always | `Int64` | reads |
| `otemp` | `float32` | always | `Double` | reads |
| `extratemp1` | `float32` | always | `Double` | — |
| `extratemp2` | `float32` | always | `Double` | — |
| `extratemp3` | `float32` | always | `Double` | — |
| `extratemp4` | `float32` | always | `Double` | — |
| `extratemp5` | `float32` | always | `Double` | — |
| `extratemp6` | `float32` | always | `Double` | — |
| `extratemp7` | `float32` | always | `Double` | — |
| `soiltemp1` | `float32` | always | `Double` | — |
| `soiltemp2` | `float32` | always | `Double` | — |
| `soiltemp3` | `float32` | always | `Double` | — |
| `soiltemp4` | `float32` | always | `Double` | — |
| `leaftemp1` | `float32` | always | `Double` | — |
| `leaftemp2` | `float32` | always | `Double` | — |
| `leaftemp3` | `float32` | always | `Double` | — |
| `leaftemp4` | `float32` | always | `Double` | — |
| `outhumidity` | `float32` | always | `Double` | — |
| `extrahumidity1` | `float32` | always | `Double` | — |
| `extrahumidity2` | `float32` | always | `Double` | — |
| `extrahumidity3` | `float32` | always | `Double` | — |
| `extrahumidity4` | `float32` | always | `Double` | — |
| `extrahumidity5` | `float32` | always | `Double` | — |
| `extrahumidity6` | `float32` | always | `Double` | — |
| `extrahumidity7` | `float32` | always | `Double` | — |
| `ohum` | `float32` | always | `Double` | reads |
| `rainrate` | `float32` | always | `Double` | reads |
| `rainincremental` | `float32` | always | `Double` | — |
| `period_rain` | `float32` | always | `Double` | — |
| `solarwatts` | `float32` | always | `Double` | reads |
| `potentialsolarwatts` | `float32` | always | `Double` | reads |
| `solarjoules` | `float32` | always | `Double` | — |
| `uv` | `float32` | always | `Double` | — |
| `radiation` | `float32` | always | `Double` | — |
| `stormrain` | `float32` | always | `Double` | — |
| `dayrain` | `float32` | always | `Double` | — |
| `monthrain` | `float32` | always | `Double` | — |
| `yearrain` | `float32` | always | `Double` | — |
| `bar` | `float32` | always | `Double` | reads |
| `rainfall24h` | `float32` | always | `Double` | reads |
| `rainfall48h` | `float32` | always | `Double` | reads |
| `rainfall72h` | `float32` | always | `Double` | reads |
| `rainfallstorm` | `float32` | always | `Double` | reads |
| `winds` | `float32` | always | `Double` | reads |
| `windgust` | `float32` | always | `Double` | reads |
| `windd` | `float32` | always | `Double` | reads |
| `windcard` | `string` | `omitempty` | `String?` | reads |
| `rainday` | `float32` | always | `Double` | reads |
| `windch` | `float32` | always | `Double` | reads |
| `heatidx` | `float32` | always | `Double` | reads |
| `itemp` | `float32` | always | `Double` | — |
| `ihum` | `float32` | always | `Double` | — |
| `consbatteryvoltage` | `float32` | always | `Double` | — |
| `stationbatteryvoltage` | `float32` | always | `Double` | reads |
| `snowdepth` | `float32` | always | `Double` | reads |
| `snowdistance` | `float32` | always | `Double` | — |
| `pm25` | `float32` | always | `Double` | reads |
| `co2` | `float32` | always | `Double` | reads |
| `aqi_pm25_aqin` | `float32` | always | `Double` | — |
| `aqi_pm10_aqin` | `float32` | always | `Double` | — |
| `extrafloat1` | `float32` | always | `Double` | reads |
| `extrafloat2` | `float32` | always | `Double` | reads |
| `extrafloat3` | `float32` | always | `Double` | reads |
| `extrafloat4` | `float32` | always | `Double` | reads |
| `extrafloat5` | `float32` | always | `Double` | — |
| `extrafloat6` | `float32` | always | `Double` | — |
| `extrafloat7` | `float32` | always | `Double` | — |
| `extrafloat8` | `float32` | always | `Double` | — |
| `extrafloat9` | `float32` | always | `Double` | — |
| `extrafloat10` | `float32` | always | `Double` | — |
| `extratext1` | `string` | `omitempty` | `String?` | — |
| `extratext2` | `string` | `omitempty` | `String?` | — |
| `extratext3` | `string` | `omitempty` | `String?` | — |
| `extratext4` | `string` | `omitempty` | `String?` | — |
| `extratext5` | `string` | `omitempty` | `String?` | — |
| `extratext6` | `string` | `omitempty` | `String?` | — |
| `extratext7` | `string` | `omitempty` | `String?` | — |
| `extratext8` | `string` | `omitempty` | `String?` | — |
| `extratext9` | `string` | `omitempty` | `String?` | — |
| `extratext10` | `string` | `omitempty` | `String?` | — |
| `sunrise` | `string` | `omitempty` | `String?` | reads |
| `sunset` | `string` | `omitempty` | `String?` | reads |
| `moonPhaseName` | `string` | `omitempty` | `String?` | reads |
| `moonIllumination` | `float32` | `omitempty` | `Double?` | reads |
| `moonAge` | `float32` | `omitempty` | `Double?` | reads |
| `moonCrescentAngle` | `float32` | `omitempty` | `Double?` | reads |

#### `SnowReading` — `/snow`

| JSON key | Go type | Emitted | iOS | Website |
|---|---|---|---|---|
| `stationname` | `string` | always | `String` | reads |
| `snowdepth` | `float32` | always | `Double` | reads |
| `snowtoday` | `float32` | always | `Double` | reads |
| `snowlast24` | `float32` | always | `Double` | reads |
| `snowlast72` | `float32` | always | `Double` | reads |
| `snowseason` | `float32` | always | `Double` | reads |
| `snowstorm` | `float32` | always | `Double` | reads |
| `snowfallrate` | `float32` | always | `Double` | reads |

#### `StationInfoResponse` — `/stationinfo`

| JSON key | Go type | Emitted | iOS | Website |
|---|---|---|---|---|
| `website_name` | `string` | always | `String` | reads |
| `about_text` | `string` | `omitempty` | `String` | reads |
| `stations` | `[]StationInfoItem` | always | `[Station]` | reads |
| `weather_device` | `*int` | `omitempty` (pointer) | `Int` | reads |
| `snow_device` | `*string` | `omitempty` (pointer) | `String?` | reads |
| `air_quality_device` | `*string` | `omitempty` (pointer) | `String?` | reads |
| `latitude` | `float64` | `omitempty` | `Double?` | reads |
| `longitude` | `float64` | `omitempty` | `Double?` | reads |
| `radar_enabled` | `bool` | `omitempty` | `Bool?` | reads |
| `radar_token` | `string` | `omitempty` | `String?` | reads |

#### `StationInfoItem` — `/stationinfo` → `stations[]`

| JSON key | Go type | Emitted | iOS | Website |
|---|---|---|---|---|
| `id` | `int` | always | `Int` | reads |
| `name` | `string` | always | `String` | reads |
| `type` | `string` | always | `String` | reads |

#### `StationData` — `/api/stations`

| JSON key | Go type | Emitted | Website |
|---|---|---|---|
| `id` | `int` | always | reads |
| `name` | `string` | always | reads |
| `type` | `string` | always | reads |
| `latitude` | `float64` | always | reads |
| `longitude` | `float64` | always | reads |
| `enabled` | `bool` | always | reads |
| `website` | `*StationWebsiteData` | `omitempty` (pointer) | — |

#### `StationWebsiteData` — `/api/stations` → `website`

| JSON key | Go type | Emitted | Website |
|---|---|---|---|
| `name` | `string` | always | reads |
| `hostname` | `string` | always | reads |
| `page_title` | `string` | always | reads |
| `protocol` | `string` | always | reads |
| `port` | `int` | always | reads |

#### `Alert` — `/alerts`

| JSON key | Go type | Emitted | Website |
|---|---|---|---|
| `alert_id` | `string` | always | — |
| `station_id` | `int` | always | — |
| `location` | `string` | always | — |
| `name` | `string` | always | reads |
| `color` | `string` | always | — |
| `body` | `string` | always | — |
| `body_full` | `string` | always | — |
| `issued_at` | `*time.Time` | `omitempty` (pointer) | — |
| `begins_at` | `*time.Time` | `omitempty` (pointer) | — |
| `expires_at` | `*time.Time` | `omitempty` (pointer) | — |

#### `AlmanacData` — `/almanac`

| JSON key | Go type | Emitted | Website |
|---|---|---|---|
| `high_temp` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `low_temp` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_wind_speed` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `max_rain_hour` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `max_rain_day` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `low_barometer` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `low_humidity` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `deepest_snow` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `max_snow_hour` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `max_snow_day` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_solar` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_pm25` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_pm10_in` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_co2` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_tvoc` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_aqi_pm25` | `*AlmanacRecord` | `omitempty` (pointer) | — |
| `high_aqi_pm10` | `*AlmanacRecord` | `omitempty` (pointer) | — |

#### `AlmanacRecord` — `/almanac` → each record

| JSON key | Go type | Emitted | Website |
|---|---|---|---|
| `value` | `float32` | always | — |
| `timestamp` | `time.Time` | always | — |
| `wind_dir` | `*float32` | `omitempty` (pointer) | — |

#### `RemoteStationResponse`

*handlers.go, inside GetRemoteStations -- a function-local type, so it is invisible to any tool that scans package-level declarations.*

| JSON key | Go type | Emitted |
|---|---|---|
| `station_id` | `string` | always |
| `station_name` | `string` | always |
| `station_type` | `string` | always |
| `last_seen` | `time.Time` | always |
| `services` | `anonymous struct` | always |
| `services.aprs_enabled` | `bool` | always |
| `services.wu_enabled` | `bool` | always |
| `services.aeris_enabled` | `bool` | always |
| `services.pws_enabled` | `bool` | always |
| `status` | `string` | always |

#### `AerisWeatherForecastPeriod`

*internal/controllers/aerisweather/controller.go; marshalled into the jsonb column that /forecast returns verbatim.*

| JSON key | Go type | Emitted |
|---|---|---|
| `dateTimeISO` | `time.Time` | always |
| `maxTempF` | `int16` | always |
| `minTempF` | `int16` | always |
| `avgTempF` | `int16` | always |
| `pop` | `int16` | always |
| `precipIN` | `float32` | always |
| `iceaccumIN` | `float32` | always |
| `snowIN` | `float32` | always |
| `maxFeelslikeF` | `int16` | always |
| `minFeelslikeF` | `int16` | always |
| `windSpeedMPH` | `int16` | always |
| `windSpeedMaxMPH` | `int16` | always |
| `windDir` | `string` | always |
| `windDirDEG` | `uint16` | always |
| `weather` | `string` | always |
| `weatherPrimaryCoded` | `string` | always |
| `weatherIcon` | `string` | always |
| `compactWeather` | `string` | always |

#### Swift decode targets, for reference

| Struct | Properties | Non-optional | Optional |
|---|---|---|---|
| `ForecastPeriod` | 18 | 18 | 0 |
| `ForecastResponse` | 2 | 2 | 0 |
| `SnowResponse` | 18 | 8 | 10 |
| `Station` | 3 | 3 | 0 |
| `StationInfo` | 10 | 4 | 6 |
| `WeatherResponse` | 88 | 70 | 18 |

### 3.1 What the two client-visible station types actually are

`/stationinfo` → `stations[].type` is the **configured device type** (`campbellscientific`, `davis`,
`snowgauge`, `airgradient`, `ambient-customized`, `grpcreceiver`, `weatherlink-live`). The iOS
client's `weatherStations` filter and the portal JS both test against these strings.

`/latest` → `stationtype` is the **driver's self-reported name**, and for Campbell it is
`campbell`, not `campbellscientific`. The two vocabularies are not the same set and must not be
unified.

### 3.2 `/forecast`'s body is not a Go struct

`/forecast` returns `{"lastUpdated": <string>, "data": <stored jsonb>}`.

* `lastUpdated` is `record.UpdatedAt.String()` — Go's `time.Time.String()` layout,
  `2006-01-02 15:04:05.999999999 -0700 MST`. **Not RFC3339.** `weather-utils.js`
  `formatLastUpdated` parses it with the moment format `YYYY-MM-DD HH:mm:ss.SSSSSS-ZZ`, so the
  layout is load-bearing. Observed: `2026-10-28 15:45:12.482913 +0000 UTC`.
* `data` is whatever is in the `aeris_weather_forecasts.data` jsonb column, written back verbatim.
  The *shape* is nonetheless server-controlled: the Aeris controller stores
  `json.Marshal([]AerisWeatherForecastPeriod)`, so the eighteen keys above are exactly what any
  record contains. The *serialisation* is Postgres's, not Go's.
* With `?period=N`, `data` becomes a single **object** and is re-encoded by `json.Marshal`, so that
  one response mixes Postgres's whitespace style in the wrapper with Go's compact style in the body.

---

## 4. The compatibility hazard table

**The computation.** The intersection of {Go fields tagged `omitempty`} and {Swift properties
declared non-optional}, taken per response type and matched by JSON key. A non-optional Swift
`Double` or `String` fails the *entire* `JSONDecoder` decode when its key is absent, so every member
of this set is a field whose zero value breaks iOS.

**The result is two fields, both on `/stationinfo`.**

| JSON key | Endpoint | Go | Swift | When the key is absent | Effect |
|---|---|---|---|---|---|
| `about_text` | `/stationinfo` | `StationInfoResponse.AboutText string` `omitempty` | `StationInfo.aboutText: String` **non-optional** | the website's `about_station_html` is empty | `JSONDecoder` throws `keyNotFound(about_text)`; the whole `StationInfo` decode fails, so the iOS app gets `WeatherError.invalidResponse` and has no station list, no snow device and no radar token |
| `weather_device` | `/stationinfo` | `StationInfoResponse.WeatherDevice *int` `omitempty` | `StationInfo.weatherDevice: Int` **non-optional** | the website has no `device_id` — which is the normal state of a **portal** website | same: total `StationInfo` decode failure |

Both are reproduced by real server responses:

```
$ curl -H 'Host: minimal.test' .../stationinfo          # fixture stationinfo-about-text-omitted
{"website_name":"Minimal Station","stations":[…],"weather_device":2,"latitude":39.8,"longitude":-105.4}
                      ^ about_text absent

$ curl -H 'Host: portal.test'  .../stationinfo          # fixture stationinfo-no-weather-device
{"website_name":"Weather Portal","stations":[…]}
                      ^ about_text AND weather_device both absent
```

**Record current behaviour. Do not fix it.** The new implementation must reproduce what shipped.
If a fix is warranted it is a separate, deliberate decision — see Open question OQ-W1-1.

### 4.1 Why `/latest` is *not* on this list, and why that is fragile

`types.go` carries the comment *"omitempty removed from numeric fields to ensure zeros are included
in JSON output"*, and the result is that `WeatherReading`'s eighteen `omitempty` keys
(`stationtype`, `windcard`, `extratext1`–`10`, `sunrise`, `sunset`, `moonPhaseName`,
`moonIllumination`, `moonAge`, `moonCrescentAngle`) line up **exactly** with
`WeatherResponse.swift`'s eighteen optional properties. The intersection is empty by construction,
not by luck — somebody already hit this bug on `/latest` and fixed it there.

That alignment is the real hazard: **70 of `WeatherResponse`'s 88 properties are non-optional, so
adding `omitempty` to any one of the 70 corresponding Go fields breaks the iOS app completely.**
`SnowReading` has the same property: zero `omitempty` tags, and eight non-optional Swift
properties. The conformance suite asserts key presence precisely so that this cannot regress
silently.

`ForecastPeriod` is a third case: all 18 of its Swift properties are non-optional, and the shape
comes from a jsonb column rather than a live struct, so a record written by an older server version
with fewer keys would break the iOS forecast decode. Nothing in the server prevents that today.

### 4.2 The full client crosswalk

`contract/legacy-v1.json` → `client_field_crosswalk` carries all **109** rows where a Go JSON key
and a Swift property meet, each with its `go_omitempty` and `swift_optional` flags. That array, not
this prose, is the auditable artefact; the table above is the subset where both flags are set the
wrong way round.

---

## 5. Quirks that must be preserved

Twenty, each with a fixture. `contract/legacy-v1.json` → `quirks` is the machine-readable copy.
These exist so that nobody later "cleans them up".

| # | Quirk |
|---|---|
| Q-1 | **`ohum` and `outhumidity` are the same measurement, emitted twice.** `transform.go` sets both from `r.OutHumidity`. The website reads `ohum`; iOS decodes both as non-optional. Neither may be dropped. |
| Q-2 | **Naming is inconsistent and stays that way.** `otemp`, `bar`, `winds`, `windd`, `itemp`, `ihum`, `windch`, `heatidx` run together; `period_rain` is snake_case; the four `moon*` keys are camelCase; everything else is lower-concatenated. |
| Q-3 | **`ts` is milliseconds since the epoch, as a bare JSON integer.** `UnixMilli()` on `Timestamp` for `/latest` and on `Bucket` for `/span`. 13 digits at present; Swift decodes `Int64`. Confirmed and pinned. |
| Q-4 | **`snowdepth` is millimetres on `/latest` and inches on `/snow` and `/span`.** `fetchLatestReading` selects `base_distance - snowdistance` in mm and passes it through unconverted; `transformSpanReadings` and `GetSnowLatest` both apply `mmToInchesWithThreshold`. Fixtures: `latest-snow-device` → `120`, `snow-reporting` → `4.7`. |
| Q-5 | **`windcard` has `omitempty` but is never absent.** `headingToCardinalDirection` always returns one of sixteen non-empty strings, `N` for 0°. |
| Q-6 | **`/span` never populates `windgust`, `rainfall24h`, `rainfall48h`, `rainfall72h` or `rainfallstorm`.** `transformSpanReadings` does not set them and they have no `omitempty`, so every element carries them as `0`. Only `/latest` computes them. |
| Q-7 | **`/span` overwrites `extrafloat1` with a smoothed snow-depth estimate for snow stations.** `fetchWeatherSpan` joins `snow_depth_est_5m` and writes `snow_depth_est_in` into `ExtraFloat1` when the station has a base snow distance. The same key means PM1.0 on an AirGradient device. `SnowResponse.swift` documents the asymmetry in a comment. |
| Q-8 | **`/snow` reports the website's snow device name even when another station was queried.** `stationname` is always `website.SnowDeviceName`; `station`/`station_id` redirect only the depth lookup. Fixture `snow-station-param-overrides-depth-only` shows `snowgauge`'s name and totals with `davis-full`'s (absent) depth. |
| Q-9 | **A gap in `/span` data is missing array elements.** No gap-filling anywhere. |
| Q-10 | **Bad-`period` errors on `/forecast` are JSON bodies labelled `text/plain`.** `w.WriteHeader(400)` runs before `formatter.WriteResponse`, so Go has already committed the sniffed content type. |
| Q-11 | **Three `/forecast` failure modes return zero bytes and no `Content-Type`.** A span failing the 1-to-4-digit regex, a span with no record, and a `station_id` with no device. |
| Q-12 | **The `encoding/json` endpoints end with a newline; `/forecast` does not.** `json.NewEncoder(w).Encode` appends `\n`; `WriteRawJSON` writes the jsonb bytes with no terminator. A real byte-level difference, asserted by the conformance suite. |
| Q-13 | **Numeric literals follow Go's shortest-round-trip rule, which makes integer-or-decimal a property of the value, not of the field.** Section 6. |
| Q-14 | **`format=msgpack` is a parallel encoding of the same contract, and it loses number types.** Section 6.4. |
| Q-15 | **`/span` is 500, not 200, for a station whose `winddir` is NULL throughout the window.** Section 10. |
| Q-16 | **`/api/remote-stations` is 500 for a `remote_stations` row with any NULL text column.** Section 10. |
| Q-17 | **Every endpoint accepts every HTTP method.** |
| Q-18 | **An unknown `Host` silently serves the first configured website.** |
| Q-19 | **Almanac snow metrics are permanently absent.** `deepest_snow`, `max_snow_hour` and `max_snow_day` are declared in `AlmanacData` and never assigned; `handlers.go:1416` reads *"Snow metrics temporarily disabled due to performance issues"*. They are `omitempty`, so the keys simply never appear. |
| Q-20 | **Directory listings are served for the asset trees.** `http.FileServer` renders an HTML index for `/js/`, `/css/`, `/images/` and `/fonts/`. |

---

## 6. Numeric and string formatting, determined empirically

Deliverable 5 asks two specific questions. Both were answered against the running server rather
than reasoned about.

### 6.1 Does Go emit `0` or `0.0`?

**`0`.** And the question is subtler than it looks: `encoding/json` writes the shortest decimal that
round-trips the value, so the choice depends on the **value**, not on the field's Go type.

| Go value (`float32`) | Emitted |
|---|---|
| `0` | `0` |
| `57` | `57` |
| `65.3` | `65.3` |
| `23.730469` | `23.730469` |
| `29.991` | `29.991` |

So a decimal point appears **if and only if** the value has a fractional part. Exponent notation
appears only outside `[1e-6, 1e21)`. There is no `0.0` anywhere in the eight `encoding/json`
endpoints — and no field of which "is an integer literal" is a stable property.

**This is why the conformance runner checks a rule rather than a recorded literal.** Comparing
`0` versus `0.0` field-by-field against a fixture fails on live data the moment a temperature
happens to land on a whole degree; the first draft of the runner did exactly that and produced
eleven false failures. The runner now asserts the rule itself: decimal point iff fractional value.

### 6.2 `/forecast` is the exception

`/forecast`'s `data` is Postgres `jsonb` text, which preserves the numeric literal as written.
Fixture `forecast-240-ten-daily-periods` contains `"snowIN": 0.0` and `"precipIN": 0.0`. **The same
contract therefore contains both `0` and `0.0`**, in different endpoints, for the same kind of
quantity. Fixtures for `/forecast` carry `"numeric_convention": "as_stored"` and the runner does not
apply the Go rule to them.

### 6.3 Is either client sensitive to field order?

**No.** `Foundation.JSONDecoder` is keyed, and `Response.json()` produces a JS object. Neither can
observe order.

Order is nonetheless **recorded and asserted** for the eight `encoding/json` endpoints, because there
it is free: Go emits struct fields in declaration order, deterministically, so a diff in key order
is a cheap signal that a schema was reordered or regenerated. For `/forecast` it is recorded and
**not** asserted: jsonb normalises keys by length and then bytewise, so the stored order is
unrecoverable and reproducing it would be an accident of the storage engine rather than a contract.
Compare the two forms of the same endpoint:

```
/forecast/240          {"pop": 10, "snowIN": 0.0, "weather": "Mostly Sunny", "windDir": "NW", …
/forecast/240?period=3 {"avgTempF":33,"compactWeather":"Cloudy","dateTimeISO":"2026-10-31T00:00:00-06:00",…
```

Postgres's jsonb writer puts a space after `:` and `,`; Go's `json.Marshal` does not. Both bodies
come out of the same handler.

### 6.4 Other formatting facts

* **Timestamps in JSON structs** are Go `time.Time`, i.e. RFC3339 with **variable** sub-second
  precision — trailing zeros are trimmed. `alerts-active` carries both
  `"issued_at":"2026-10-28T15:00:00Z"` and `"begins_at":"2026-10-28T14:13:32.049952Z"`. A client
  parsing with a fixed fractional-digit format would break on the first whole second.
* **`lastUpdated` on `/forecast` is not RFC3339** — see §3.2.
* **HTML is escaped in JSON strings.** `about_text` comes back as
  `"<p>A Davis…</p>"`: Go's encoder escapes `<`, `>` and `&` by default. Any
  Rust encoder must do the same or the bytes differ.
* **`format=msgpack`** switches to `vmihailenco/msgpack` with `SetCustomStructTag("json")`, so key
  names match. On `/forecast` the jsonb is decoded into `interface{}` first, so **every number
  becomes a float64** — `0xcb` doubles throughout. `/span` ignores the parameter entirely. No
  shipped client sends it; the fixtures record it because it is part of the frozen surface.

---

## 7. Golden fixtures

**70 fixtures** in [`contract/fixtures/`](../contract/fixtures/), one JSON file each, carrying the
request that produced it, the response, and the assertions that apply.

```json
{
  "name": "stationinfo-about-text-omitted",
  "description": "HAZARD: about_station_html is empty, so about_text is omitted. …",
  "provenance": "derived from the SQLite configuration database only",
  "captured_at": "2026-10-28T…Z",
  "request":  { "method": "GET", "path": "/stationinfo", "query": "", "headers": { "Host": "minimal.test" } },
  "response": { "status": 200, "headers": { … }, "body_kind": "json", "body_bytes": 401,
                "body_sha256": "…", "trailing_newline": true, "body": "…" },
  "assert":   { "status": true, "headers": ["Access-Control-Allow-Origin", "Content-Type"],
                "json_shape": true, "json_key_order": true, "numeric_format": true,
                "numeric_convention": "go_encoder", "trailing_newline": true,
                "body_equals": false, "required_substrings": [] }
}
```

Coverage against the deliverable-4 list:

| Required case | Fixtures |
|---|---|
| a station reporting a full sensor complement | `latest-full-complement`, `span-24h-five-minute-buckets` (Davis: soil and leaf temps, UV, solar, storm/day/month/year rain, battery voltage) |
| a station reporting a minimal one | `latest-minimal-complement`, `span-1h-one-minute-buckets` (Campbell: temperature, humidity, wind, barometer, solar, rain) |
| snow enabled | `snow-reporting`, `snow-by-station-id`, `latest-snow-device`, `span-72h-hourly-with-gap` |
| snow disabled | `snow-disabled-for-website` (website `snow_enabled` false → 404) |
| **snow device configured but absent or offline — `/snow`** | `snow-device-never-reported` (no row with `snowdistance > 0` ever: every field `0`, status still **200**) and `snow-device-stale` (gauge silent for three days: `/snow` has no recency filter, so it keeps returning `snowdepth: 2.8` indefinitely) |
| **snow device configured but absent or offline — `/latest`** | `latest-snow-device-offline` (last reading older than the 10-minute window → **404** `no recent weather data available for this station`) |
| a `/span/` response covering a gap | `span-72h-hourly-with-gap` (a deliberate six-hour hole; the `ts` sequence jumps and no element is emitted for the missing buckets) |
| at least one empty/no-data case per endpoint | `span-empty` (`[]`), `almanac-empty` (`{}`), `alerts-empty` (`[]`), `forecast-empty-data` (`"data": []`), `api-remote-stations-forbidden`/`api-stations-forbidden` (403 is the only reachable "nothing" for the portal APIs on a non-portal site), `snow-device-never-reported` (all-zero object), `latest-no-recent-data` (404) |
| a representative response for each website-only endpoint | `almanac-populated`, `api-stations-portal`, `api-remote-stations-populated`, `html-weather-station`, `html-portal`, `html-privacy`, `html-support`, `asset-weather-app-js`, `asset-static-js`, `asset-static-css`, `asset-webmanifest`, `asset-font`, `asset-favicon` |

**Note the asymmetry that fixture pair exposes:** an unknown `station_id` on `/forecast` returns
an empty 404 body, while an unknown `station` *name* on the same endpoint returns a JSON `{"error"}`
object labelled `text/plain`. One endpoint, two 404 shapes, chosen by which parameter you used.

Beyond that list the suite covers every error branch in §2.2, both `station`/`station_id` forms on
every endpoint that takes them, the three `msgpack` variants, all four `/span` aggregate levels, and
both `/stationinfo` hazard cases.

**On fixture honesty.** Two span fixtures were 325 kB each; their bodies are stored truncated to the
first six array elements, with `body_array_length`, `body_array_truncated_to` and a note recording
that `body_sha256` and `body_bytes` describe the untruncated capture. Element shape is uniform, and
the runner compares shape rather than length, so nothing is lost. Every other fixture stores its
complete body except the five large static assets, which store status, headers, length and SHA-256
only.

The snow-absent case is **not** time-boxed: the fixtures were produced by configuring a snow device
with no readings and one with stale readings, so the state is reproducible from
`contract/tools/rig/` at any time without touching hardware.

---

## 8. The conformance test

[`contract/conformance/`](../contract/conformance/) is a standalone Rust crate. It replays every
fixture against any HTTP base URL.

```
cargo run --release -- --base-url http://127.0.0.1:8080
CONFORMANCE_BASE_URL=http://127.0.0.1:8080 cargo test --release
```

It is standalone — its own `[workspace]` — on purpose: the crate and workspace layout of the new
daemon is W3's decision (`DESIGN.md` R-4), and this runner has to work **before any Rust daemon
exists**. Dependencies are `serde_json` (with `preserve_order` and `arbitrary_precision`, so key
order and numeric literals survive parsing) and `ureq`. The `cargo test` wrapper skips itself when
`CONFORMANCE_BASE_URL` is unset, so a plain `cargo test` in a fresh checkout stays green.

### What it asserts

| Property | How |
|---|---|
| status code | exact |
| headers the fixture recorded | exact, except `max-age=<n>` which is compared as a pattern because `/span` recomputes it per request |
| JSON key set | exact at every level, in both directions: a missing key **and** an unexpected key both fail |
| JSON value types | exact |
| numeric formatting | against Go's shortest-round-trip rule (§6.1), not against the fixture's literal; skipped for `/forecast`'s jsonb passthrough |
| JSON key order | asserted for the eight `encoding/json` endpoints, recorded but not asserted for `/forecast` |
| trailing newline | exact — this is what separates `json.Encoder` from `WriteRawJSON` |
| plain-text error bodies | byte-exact |
| HTML and script bodies | required substrings, including the rendered template variables |

Heterogeneous arrays are handled properly: `/api/stations` emits the nested `website` object only for
stations bound to one, so the runner collects the distinct element shapes on each side and requires
every shape the response produced to match one the fixture recorded.

Requests are sent with `Accept-Encoding: identity`, because `CompressHandler` wraps every route and
the fixtures describe handler output rather than gzip envelopes.

### The green run

```
$ ./target/release/legacy-conformance --base-url http://127.0.0.1:18080
replaying 70 fixtures from …/contract/fixtures against http://127.0.0.1:18080
ok   alerts-active
…
70 passed, 0 failed, 0 skipped
```

against the Go daemon at `c3ec9f7`, PostgreSQL 17.11 / TimescaleDB 2.31.0. `cargo test --release`
with `CONFORMANCE_BASE_URL` set passes the same suite.

### The negative control

A green suite proves nothing on its own — a runner that asserts nothing is also green. So the suite
was run against a deliberately broken server:
[`contract/tools/rig/mangle_proxy.py`](../contract/tools/rig/mangle_proxy.py) forwards to the Go
daemon and introduces four realistic regressions — drops the `ohum` key, writes `0.0` where Go
writes `0`, changes `ts` from a number to a string, and strips the trailing newline.

```
44 passed, 26 failed
  - $.ohum: key MISSING from the response
  - $.windd: numeric literal "0.0" has a decimal point for an integral value; Go's encoder writes 0
  - $.ts: TYPE differs: expected number, got string
  - trailing newline: expected true, got false
```

All four classes are caught, at the right fixtures, with actionable messages. That is what makes the
green run meaningful.

---

## 9. HTML and asset surface

Per the prompt, the contract here is **weaker than byte equality on rendered HTML**: what must hold
is that routes resolve, content types match, cache headers match, and the template variables the
pages consume are present.

| Route | Content-Type | Cache-Control | Template |
|---|---|---|---|
| `/` (station website) | `text/html` | none | `weather-station.html.tmpl` |
| `/` (portal website) | `text/html` | none | `portal.html.tmpl` |
| `/portal` | `text/html` | none | `portal.html.tmpl` |
| `/privacy` | `text/html; charset=utf-8` | none | none — `privacy.html` verbatim |
| `/support` | `text/html; charset=utf-8` | none | none — `support.html` verbatim |
| `/js/weather-app.js` | `text/javascript` *(no charset)* | none | `js/weather-app.js.tmpl` |
| `/js/*.js` (static) | `text/javascript; charset=utf-8` | none | — |
| `/css/*.css` | `text/css; charset=utf-8` | none | — |
| `/fonts/*` | `font/ttf`, `font/woff2`, … | `public, max-age=31536000, immutable` | — |
| `/images/*`, `/favicon.ico` | sniffed (`image/vnd.microsoft.icon`, …) | none | — |
| `/site.webmanifest` | `text/plain; charset=utf-8` | none | — |
| `/js/`, `/css/`, `/images/`, `/fonts/` | `text/html; charset=utf-8` — a directory index | `/fonts/` only | — |

Points worth flagging:

* **Two different content types for `.js` on the same site.** The templated
  `/js/weather-app.js` has no charset; everything the file server returns does.
* **`/fonts/` is the only tree with `Cache-Control`.** Every other asset is served with no caching
  headers at all, on a payload that never changes.
* **`site.webmanifest` is `text/plain`**, not `application/manifest+json`, because Go's mime table
  has no entry for the extension.
* **`/privacy` and `/support` do not look at the website at all.** They are the only two *handler*
  routes that skip the "is a website configured?" check, so they behave like the static file server
  rather than like the API. The `websiteMiddleware` 503 for an unknown host still applies when no
  website is configured at all.

### Template variables — three templates, and what they actually consume

**`portal.html.tmpl` consumes nothing.** The handler passes `PageTitle` and the template never
references it; the page title is hard-coded `Weather Management Portal`. Zero `{{ }}` actions in the
file.

**`weather-station.html.tmpl`** is passed 18 fields and consumes 10:

| Consumed | Not consumed (passed and ignored) |
|---|---|
| `StationName`, `Version`, `AppleAppID`, `Latitude`, `Longitude`, `RadarToken`, and as `if` conditions `AerisWeatherEnabled`, `AirQualityEnabled`, `RadarEnabled`, `SnowEnabled` | `StationID`, `PullFromDevice`, `SnowDevice`, `SnowBaseDistance`, `AirQualityDevice`, `AirQualityDeviceID`, `PageTitle`, `AboutStationHTML` |

`Latitude`, `Longitude` and `RadarToken` are interpolated into a `<script
id="radar-config" type="application/json">` block, which is how the browser radar module gets its
tile token. That makes the token **public in the page markup** — the same token `/stationinfo` hands
to native clients.

**`js/weather-app.js.tmpl`** is passed 11 fields and consumes 9: `PullFromDevice`, `StationID`,
`SnowEnabled`, `SnowDevice`, `AirQualityEnabled`, `AirQualityDevice`, `AirQualityDeviceID`,
`StationName`, `AerisWeatherEnabled`. `PageTitle` and `AboutStationHTML` are ignored.

**How these get served from Rust is W3's problem, not W1's.** `DESIGN.md` D-38 already decides that
`weather-app.js.tmpl` is replaced by static JavaScript plus a JSON configuration endpoint; the nine
values above are exactly what that endpoint must carry. The two HTML templates are recorded here as
"what they currently emit" and nothing more.

### The external radar service is not part of this contract

`maps.nw5w.com` serves the radar basemap, tiles and `radar/manifest.json`, and both the website
(`weather-radar.js`) and the iOS app (`RadarMapsClient.swift`) talk to it **directly**. This server's
only involvement is handing out `radar_token` via `/stationinfo` and the `radar-config` block. The
manifest's `{"schema_version": 1, "frames": [{"ts","iso"}]}` shape and the `?t=<token>` tile
convention belong to that service, not to the rewrite.

---

## 10. Latent defects found while capturing. Recorded, not fixed.

Two were reproduced during this work item. Neither is fixed here, both matter to later work items,
and both would be invisible from a code read alone.

### W1-BUG-1. `/span` is a 500 for any station with no non-NULL `winddir` in the window

`circular_avg_final` (`internal/storage/timescaledb/sql.go:268`) computes
`state.sin_sum / state.accum`. The aggregate's `INITCOND` is `'(0,0,0)'` and its transition function
is `STRICT`, so a bucket that saw no non-NULL `winddir` leaves `accum = 0` and the finaliser divides
by zero. Every `/span` level reads the aggregate, so the whole endpoint returns
`500 error fetching weather data` with `SQLSTATE 22012` in the log.

**It is latent in the shipping system and only latent by accident.** The Go daemon inserts its
entire `Reading` struct on every write, zeros included, so no column it writes is ever NULL. The
defect becomes live for any writer that leaves an unpopulated metric NULL.

**That is precisely what the new design plans to do.** The wide-table decision (D-3) rests on a
NULL costing one bit, and adding a metric is a *nullable* `ADD COLUMN` (D-9) — both are arguments
for storing NULL rather than 0 for a metric a station does not report. A Rust writer that takes that option, against the current `circular_avg`, makes
`/span` return 500 for every snow gauge and every air-quality monitor. See proposed amendment A-1 and open question OQ-W1-2.

### W1-BUG-2. `/api/remote-stations` is a 500 for a `remote_stations` row with any NULL text column

`GetRemoteStations` scans the row into Go `string` fields, so a NULL `aprs_password` produces
`sql: Scan error … converting NULL to string is unsupported`, surfaced as
`500 error loading remote station data`. The SQLite schema declares those columns nullable with no
default, so any row not written by the normal registration path breaks the endpoint.

### Two further observations, outside the contract but worth not losing

* **The Go Davis driver stops polling after one successful LOOP batch.**
  `internal/weatherstations/davis/station.go:200` — `GetLoopPackets` loops only on *error* and
  `return`s on success, so after `GetDavisLoopPackets(20)` succeeds the station never requests
  another batch. Reproduced against `cmd/davis-emulator`: readings stop after exactly 20 packets.
  This is a collector concern (W7), not an API concern, and it is a good example of the W0 finding
  that the Go emulators are a stronger oracle than an emulator written by the driver's author.
* **The daemon's startup provisioning and `migrations/weather/` collide.** `migrate -command up`
  against a database the daemon has already provisioned fails at migration 001 with
  `function update_rainfall_summary() is not unique`, because the daemon creates
  `update_rainfall_summary(job_id INT, config JSONB)` and the migration creates the no-argument
  form. The two schema sources are not composable, which is a data point for W4 and W9 and the
  reason D-8 ("schema changes happen solely through migrations; there is no DDL at startup and no
  `IF NOT EXISTS` idiom") exists.

---
## 11. Reproducing the rig

[`contract/tools/`](../contract/tools/) holds everything used to produce the fixtures:

| File | Purpose |
|---|---|
| `rig/tsdb.yaml` | the throwaway `timescale/timescaledb-ha:pg17` pod and service |
| `rig/seed.sql` | snow-gauge and air-quality readings (no Go driver for either can be emulated locally) |
| `rig/backfill.sql` | 30 h of history for the two emulator-driven stations, cloning the real rows' populated-column pattern so only `time` moves |
| `rig/snowstale.sql` | the "offline for three days" snow gauge |
| `rig/aeris.sql` | forecast and alert records, shaped exactly as `json.Marshal([]AerisWeatherForecastPeriod)` writes them |
| `rig/mangle_proxy.py` | the negative control of §8 |
| `capture.py` | the fixture capture harness — the fixture list lives here, as code |
| `endpoints.py` | the endpoint inventory, quirk list and the two response types no source scanner can see |
| `genschema.py` | builds `legacy-v1.json` by parsing the Go and Swift sources; nothing in the schema is hand-transcribed |

The six configured websites, five devices and two remote-station rows are described in
`capture.py`'s fixture `Host` values and in `contract/legacy-v1.json`.

**The deliberate gap:** the snow gauge (a gRPC client), the AirGradient monitor (an HTTP poller) and
the Aeris forecast and alert fetches were seeded directly rather than driven through their Go
drivers, because the first two need peer services this runtime has no emulator for and the third
needs a paid API key. Every fixture produced that way says so in its `provenance` field. The JSON
*shape* of those responses is still entirely the Go handler's and the Go encoder's; only the values'
origin differs.

---

# Decisions

1. **The frozen contract is `legacy-v1`**, defined by `contract/legacy-v1.json` plus the 70 fixtures in `contract/fixtures/`. The prose in this document is background; those two artefacts are the specification.
2. **The contract is the union of the two clients' needs, with the stricter client winning.** The Rust server may emit a field nobody reads; it may never omit or null a field either client requires.
3. **Conformance means status, headers, key set, value types, numeric formatting, key order where the encoder is deterministic, and trailing-newline presence — not byte equality.** Values change every two seconds; byte equality is untestable against a live server.
4. **Numeric formatting is asserted as a rule, not as a recorded literal.** Go writes the shortest decimal that round-trips, so a decimal point appears iff the value has a fractional part. Comparing a fixture's `0` against a live `65.3` field is a false failure, and the first draft of the runner produced eleven of them.
5. **`/forecast` is exempt from the key-order and numeric-format assertions.** Its body is Postgres `jsonb` written back verbatim; reproducing jsonb's key normalisation and stored literals would be reproducing an accident of the storage engine.
6. **The fixture format is one self-describing JSON file per case**, carrying request, response and its own `assert` block, so the runner needs no second source of truth and a new case is one file.
7. **The conformance runner is a standalone Rust crate with its own `[workspace]`.** It must run against the Go binary before any Rust daemon exists, and the workspace layout is W3's decision (R-4).
8. **The runner's dependencies are `serde_json` (`preserve_order`, `arbitrary_precision`) and `ureq`.** `ureq` rather than `reqwest` because the runner is blocking test infrastructure with no need for tokio. This is not a substitution against D-32, which governs the daemon.
9. **The runner sends `Accept-Encoding: identity`** and the fixtures record uncompressed bodies. Compression is a transport property of `CompressHandler`, not of any handler's output.
10. **A green conformance run is only accepted alongside a passing negative control.** `contract/tools/rig/mangle_proxy.py` is kept in-tree for that purpose, and CI should run it.
11. **The hazard in §4 is recorded and not fixed.** `about_text` and `weather_device` keep their `omitempty` semantics in the Rust implementation; iOS keeps failing to decode `/stationinfo` for a website with no about text or no device. Changing that is a separate, deliberate decision (OQ-W1-1).
12. **Both unit inconsistencies and both duplicate fields are preserved**: `snowdepth` in millimetres on `/latest` and inches elsewhere; `ohum` and `outhumidity` carrying the same value; `extrafloat1` meaning smoothed snow depth on `/span` for snow stations and PM1.0 on an AirGradient device.
13. **`/alerts` and `/api/remote-stations` are reproduced despite having no shipped consumer.** "Frozen" is about the surface, not about observed traffic, and a client outside these two repositories cannot be ruled out.
14. **The HTML and asset surface is specified as routes, content types, cache headers and consumed template variables — not as rendered bytes.** The fixtures record the full rendered bodies anyway, as a reference for W3, but the assertions are substring-level.
15. **The 10-minute recency window on `/latest` is part of the contract.** A station silent for eleven minutes returns 404, and the iOS client turns that into `WeatherError.serverError`. Serving a stale reading instead would be a behaviour change.
16. **`/snow`'s lack of a recency filter is also part of the contract.** An offline gauge returns its last known depth indefinitely, which is what the owner's currently-down hardware does today.
17. **The server was built and run locally, from `remoteweather` @ `c3ec9f7`, against a throwaway TimescaleDB 2.31.0 / PostgreSQL 17.11 container**, and the commit is recorded in `legacy-v1.json` so every fixture is traceable to an exact server.
18. **Fixtures produced by direct database seeding are labelled as such** in a per-fixture `provenance` field, rather than being presented as emulator-driven.

---

# Open questions

Stated as decisions for the owner, with options and a recommendation.

**OQ-W1-1. Fix the `/stationinfo` iOS decode hazard, or keep reproducing it?**

Today, a website with no about text or no bound device returns a `/stationinfo` body that the iOS app
cannot decode *at all* — it loses the station list, the snow device, the air-quality device and the
radar token, not just the missing field. Any portal website is permanently in this state.

*Options.* (a) Reproduce it exactly; iOS keeps breaking on those websites. (b) Drop `omitempty` from
`about_text` and make `weather_device` emit `0` when unset — strictly additive from iOS's point of
view, since a present key can only help a non-optional property, but it changes bytes the website
also sees (the website guards both reads, so it is unaffected). (c) Ship an iOS release making both
properties optional, then option (a) is harmless.

*Recommendation:* **(b), as an explicit amendment after W1 merges, not inside W1.** It is a
two-character change to the Rust schema, it cannot break the tolerant client, and it fixes a real
user-visible failure without waiting on an App Store review. But it is a deliberate deviation from
"reproduce what shipped", so it needs the owner's signature and a new fixture, not an agent's
judgement. W1 therefore ships the faithful version and this question.

**OQ-W1-2. Does the Rust writer store NULL for metrics a station does not report?**

If it does, `/span` returns 500 for every snow gauge and air-quality monitor under the current
`circular_avg` (§10, W1-BUG-1). The Go daemon avoids this only by writing its whole struct, zeros
included.

*Options.* (a) Keep writing zeros for every column, as Go does — gives up the NULL density
advantage D-3 is partly argued on. (b) Write NULLs and make the circular-average finaliser
NULL-safe (`CASE WHEN accum = 0 THEN NULL`), which changes `windd` on `/span` from `0` to `null`
for those stations — a **contract change**, because `windd` is a non-optional Swift `Double`. (c)
Write NULLs, make the finaliser return `0` when `accum = 0`, preserving today's output exactly.

*Recommendation:* **(c).** It keeps the frozen contract byte-identical, keeps the NULL density
advantage, and localises the change to one SQL function. **W4 decides and records**; W1 flags it
because the defect is invisible from either document alone.

**OQ-W1-3. Are `/alerts` and `/api/remote-stations` actually dead, or is there a consumer outside these three repositories?**

Neither has a working in-repo client. `/api/remote-stations`' only reference is a management-UI fetch
against a server with no such route.

*Recommendation:* **reproduce both and ask.** One line of access-log analysis on production settles
it; if both are genuinely unused, W6 can record them as candidates for the v2 API's deprecation list
rather than carrying them forward forever. The cost of being wrong is a broken third-party client,
so the default must be to keep them.

**OQ-W1-4. Should the Rust server reproduce `http.FileServer`'s directory listings?**

`/js/`, `/css/`, `/images/` and `/fonts/` currently return an HTML index (Q-20). No client requests
them; it is a minor information disclosure.

*Recommendation:* **do not reproduce them; return 404.** Record it as a deliberate deviation in W6's
ledger rather than discovering it as a diff. No client is affected, and this is the one place where
"reproduce exactly" costs something and buys nothing. The owner should confirm, since it is a
deviation from D-37 however small.

**OQ-W1-5. Is `format=msgpack` kept?**

It is implemented on four endpoints, skipped on `/span`, loses number types on `/forecast`, and no
shipped client sends it.

*Recommendation:* **keep it, because D-37 says frozen, and raise it as a v2 deprecation candidate.**
It is cheap in Rust — one `Accept`-like switch over the same serialisable types — and removing a
working query parameter is exactly the class of change the freeze exists to prevent.

---

# Assumptions about other work items

Every assumption this document makes about a document it did not write.

**About W3 (service architecture)**

1. W3 owns how the three Go templates and the asset trees are served, and will treat §9 as the
   specification of what they must emit. W1 does not propose a serving mechanism.
2. The JSON configuration endpoint that replaces `weather-app.js.tmpl` (D-38) must carry exactly the
   nine template values §9 lists as consumed. If W3's config endpoint carries a different set, one of
   the two documents is wrong.
3. `/latest`'s 10-minute recency window and `/snow`'s absence of one survive whatever
   precompute-and-publish scheme W3 §2a adopts. A published snapshot that keeps serving a reading
   older than ten minutes changes a 404 into a 200.
4. The conformance suite is run in CI against both binaries, with the Go target as the
   fixture-validity check (R-3). W8 owns the gate; W1 provides the runner and its exit code.
5. Multi-hostname routing preserves the *fallback* behaviour of Q-18, not just the matching
   behaviour — an unknown `Host` serves the first configured website rather than 404.

**About W2 (metric registry)**

6. The registry's generated REST-JSON schema can express, per field: the exact JSON key (including
   the inconsistent names of Q-2), whether the key is omitted when the value is zero, and the fact
   that two keys (`ohum`/`outhumidity`) carry one metric. If the generator cannot emit two JSON keys
   from one registry entry, the frozen contract cannot be generated and must be hand-written.
7. The registry can express a field whose unit differs between endpoints (`snowdepth`, Q-4), or the
   legacy endpoints are hand-written rather than generated.
8. Identifier allocation (D-21) does not rename any of the 163 existing JSON keys.

**About W4 (database schema)**

9. W4 decides OQ-W1-2 — whether unreported metrics are NULL — and if they are, makes the
   circular-average finaliser NULL-safe in a way that still emits `0`, not `null`, for `windd`.
10. `/span`'s four aggregate levels and their exact span boundaries (§2.3) are preserved, or the
    `ts` spacing in a `/span` response changes and every client's chart resolution changes with it.
11. `period_rain` exists as an aggregate-level column, as it does today; it has no counterpart in the
    raw table.
12. `almanac_cache` and `snow_totals_cache` continue to exist as precomputed tables, because
    `/almanac` and `/snow` read them directly and `/almanac` returns `{}` rather than computing
    anything when the cache is empty.

**About W7 (collector)**

13. Whatever writes readings either writes every column (as Go does) or coordinates with W4 on
    assumption 9. This is the same assumption as 9, from the writer's side, and it is the one most
    likely to be violated independently.

**About W6 (reconciliation)**

14. W6 records the v2 API as a named future work item (D-39) and carries OQ-W1-1, OQ-W1-3, OQ-W1-4
    and OQ-W1-5 forward as decisions the owner still owes, rather than as W1 having settled them.
15. W6's ledger records OQ-W1-4's outcome as a deliberate deviation from D-37 if the owner accepts
    the recommendation.

**About the iOS client**

16. `RemoteWeather.app` @ `77967ab` is the version that must keep working. If a release ships that
    makes `aboutText` or `weatherDevice` optional, OQ-W1-1 changes answer, and `legacy-v1.json`'s
    `clients.ios.commit` must be bumped with a new crosswalk.

---

# Proposed amendments to `DESIGN.md`

Four. None is applied here; `DESIGN.md` decisions are never edited in place.

> **A-1 — 2026-10-28 — D-3 and D-9 need a caveat, not a replacement.**
> *What was found.* The density argument for the wide table is that PostgreSQL stores a NULL as one
> bit, and D-9 adds a metric as a nullable `ADD COLUMN`. Both are correct and both encourage a writer
> to store NULL for a metric a station does not report. Against the current `circular_avg`
> aggregate, a window in which `winddir` is entirely NULL makes `/span` return **500** — the
> finaliser divides by a zero accumulator. The shipping Go daemon is only safe because it writes its
> whole reading struct, zeros included, so no column it writes is ever NULL. The rewrite is expected
> to be more parsimonious and would therefore hit this.
> *What replaces it.* Nothing is replaced. D-3 and D-9 stand; they gain the caveat that **a
> NULL-tolerant writer requires NULL-tolerant aggregate finalisers**, and that the frozen contract
> (D-37) constrains the fix to one that still emits `0` rather than `null`. W4 owns the SQL.
> *Why this is more than inconvenient.* It is the only place found where two merged decisions
> combine into a user-visible 500 that neither decision's own document would predict.

> **A-2 — 2026-10-28 — D-37 should name its artefacts.**
> *What was found.* D-37 says the legacy contract is frozen and that W1 captures the surface, which
> leaves "frozen to what?" answerable only by reading prose.
> *What replaces it.* A sentence naming `contract/legacy-v1.json` and `contract/fixtures/` as the
> definition of the frozen surface, and a green conformance run against the Rust daemon as the
> acceptance test for any PR that touches a legacy endpoint.
> *Why this is more than inconvenient.* A freeze with no executable definition is a freeze that
> nobody can be found to have broken.

> **A-3 — 2026-10-28 — D-38 should record the nine values the JSON config endpoint must carry.**
> *What was found.* D-38 decides that template-generated JavaScript becomes static JavaScript plus a
> JSON configuration endpoint, but does not say what that endpoint carries. `weather-app.js.tmpl`
> consumes exactly nine of the eleven values the handler passes it (§9).
> *What replaces it.* D-38 gains a pointer to §9's list. W3 and W5 both read D-38 and would
> otherwise each re-derive it from the Go template.
> *Why this is more than inconvenient.* It is the one decision in `DESIGN.md` that deliberately
> changes the website's bytes, so the exact replacement payload is load-bearing rather than detail.

> **A-4 — 2026-10-28 — D-32's crate list should say it governs the daemon, not test infrastructure.**
> *What was found.* D-32 names `axum`/`tonic` for HTTP and lists no HTTP *client*. The conformance
> runner needs a blocking client and uses `ureq`, which is neither a substitution for a listed crate
> nor covered by the list.
> *What replaces it.* D-32 gains a scope sentence: it governs the daemon's dependencies; test
> harnesses may choose their own, recorded where they are used.
> *Why this is more than inconvenient.* Without it, every piece of test infrastructure either drags
> in the daemon's async stack or files an amendment, and W6 reconciles noise.
