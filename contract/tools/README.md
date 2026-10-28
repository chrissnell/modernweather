# `contract/tools/` — how the fixtures were made

`DESIGN.md` OQ-1 records that production is not reachable from an agent runtime, so the fixtures were
captured from a server built and run locally: the Go daemon from
`chrissnell/remoteweather` @ `c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf` against a throwaway
`timescale/timescaledb-ha:pg17` container, driven by the Go station emulators in that repo's `cmd/`.

That is not a downgrade. Byte-level JSON compatibility is a property of the Go encoder and
`restserver/types.go`, not of production's data, and a locally driven server is the only way to
*produce* the awkward cases the contract needs — minimal sensor complement, a gap in the data, empty
responses, and a snow device configured but absent.

## The rig

```sh
# 1. database
kubectl apply -f rig/tsdb.yaml
kubectl -n mw-w1-contract port-forward pod/tsdb 15432:5432 &

# 2. server, from a checkout of the Go repository at the recorded commit
go build -mod=vendor -o rw ./cmd/remoteweather
go build -mod=vendor -o davis-emulator ./cmd/davis-emulator
go build -mod=vendor -o campbell-emulator ./cmd/campbell-emulator
go build -mod=vendor -o rwmigrate ./cmd/migrate

# 3. configuration: ./rw -config config.db bootstraps an empty SQLite config database.
#    Then insert five devices and six websites (see "Configuration" below).

# 4. stations
./davis-emulator -port 22222 &
./campbell-emulator -port 8123 -interval 2s &
./rw -config config.db &          # provisions the whole TimescaleDB schema on start

# 5. the tables the daemon does NOT create
psql ... -f <go-repo>/migrations/weather/005_create_almanac_cache.up.sql
psql ... -f <go-repo>/migrations/weather/028_complete_almanac_metrics.up.sql
psql ... -f <go-repo>/migrations/weather/029_almanac_store_pm_only.up.sql
psql ... -f <go-repo>/migrations/weather/030_almanac_airgradient_pm10.up.sql
psql ... -f <go-repo>/migrations/weather/025_create_snow_depth_est.up.sql

# 6. data the local drivers cannot produce
psql ... -f rig/seed.sql        # snow gauge + air-quality readings
psql ... -f rig/backfill.sql    # 30 h of history for the two emulator-driven stations
psql ... -f rig/snowstale.sql   # a snow gauge that went silent three days ago
psql ... -f rig/aeris.sql       # forecast and alert records

# 7. refresh the "live" stations so /latest's 10-minute window hits, then capture
psql ... -f rig/topup.sql
RW_CAPTURE_HOST=127.0.0.1 RW_CAPTURE_PORT=18080 \
  python3 capture.py ../fixtures

RW_GO_REPO=<go-repo> RW_IOS_REPO=<ios-repo> RW_FIXTURE_DIR=../fixtures \
  RW_SERVER_VERSION="$(./rw -version)" \
  RW_DATABASE_VERSION="$(psql ... -At -c "select 'PostgreSQL ' || current_setting('server_version') || ' + TimescaleDB ' || extversion from pg_extension where extname='timescaledb'")" \
  python3 genschema.py ../legacy-v1.json
```

`genschema.py` is deterministic: re-running it against the same sources and fixtures reproduces
`legacy-v1.json` byte for byte, which is what makes it safe to regenerate in a CI drift check.

`migrate -command up` against the whole `migrations/weather` directory **fails** at migration 001
with `function update_rainfall_summary() is not unique`, because the daemon's startup provisioning
already created a two-argument form of that function. Apply only the five migrations listed above.
That collision is itself a finding — see `docs/legacy-api-contract.md` §10.

## Configuration

Five devices and six websites, chosen so that every branch of the contract is reachable:

| Device | Type | Driven by | Why |
|---|---|---|---|
| `davis-full` | `davis` | `davis-emulator` over TCP | full sensor complement; Aeris enabled, which is what registers `/forecast` and `/alerts` |
| `csi-minimal` | `campbellscientific` | `campbell-emulator` over TCP | minimal sensor complement |
| `snowgauge` | `snowgauge` | `rig/seed.sql` | reporting snow gauge, base distance 1500 mm |
| `snowgauge-stale` | `snowgauge` | `rig/snowstale.sql` | gauge offline for three days |
| `snowgauge-dead` | `snowgauge` | nothing | gauge configured but never reported |
| `aqmon` | `airgradient` | `rig/seed.sql` | PM2.5, PM10, CO2, TVOC, NOx |
| `nolocation` | `campbellscientific` | nothing | a configured device with no location and no readings |

| Website (`Host`) | Reaches |
|---|---|
| `localhost` | the happy path: snow on, air quality on, radar on, about text set |
| `minimal.test` | the `about_text` hazard — `about_station_html` empty |
| `portal.test` | the `weather_device` hazard, plus `/portal`, `/api/stations`, `/api/remote-stations` |
| `nosnow.test` | `/snow` 404 for a website with snow disabled |
| `stalesnow.test` | `/snow` returning a three-day-old depth |
| `deadsnow.test` | `/snow` all-zero for a gauge that never reported |

## Keeping `/latest` capturable

`/latest` only returns a reading from the last ten minutes, and the Go Davis driver stops polling
after one successful 20-packet LOOP batch (`docs/legacy-api-contract.md` §10), so the capture script
first clones each live station's newest row forward to `NOW()`. The clone copies the whole row, so
the set of populated columns stays exactly what the Go driver wrote; only `time` moves.

`rig/topup.sql` does both. The directly-seeded rows are also NULL-filled to zero before capture, because the Go daemon inserts
its entire reading struct and therefore never writes a NULL. Leaving them NULL triggers W1-BUG-1 and
makes `/span` return 500 — which is how that defect was found.
