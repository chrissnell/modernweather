-- The forecast table `/forecast` reads, and nothing else.
--
-- `docs/implementation-plan.md` M4-2 owns the `/forecast` HANDLER -- "caches the stored
-- `jsonb` text byte for byte and splices it between a prefix and a suffix (D-39) --
-- nothing parses or re-emits it" -- and `/forecast/{hours}` is one of the five frozen
-- endpoints (**D-68**). M6-3 owns the Aeris COMPONENTS that write it.
--
-- **W4 did not specify this table.** `docs/database-schema.md` does not mention a forecast
-- anywhere; W1 assumption 12 names `almanac_cache` and `snow_totals_cache` as the two
-- precomputed caches the frozen contract reads directly and stops there. So a frozen
-- endpoint whose handler this item owns had no relation to read from, which is the same
-- class of gap `docs/reconciliation.md` G-2 records for the configuration store: four
-- documents assumed tables into existence and none of them created one. It is created here,
-- by the item that first needs it, rather than left for M6-3 -- a handler with nothing to
-- read is not a deliverable, and the alternative was five frozen-class fixtures failing
-- because of a missing relation rather than because of the contract.
--
-- Narrow on purpose. It carries exactly what the frozen response needs and not the Go
-- table's shape:
--
--   * no `id` surrogate. `aeris_weather_forecasts` has one and gorm's `First()` orders by
--     it; the natural key here is `(station_id, span_hours)`, which is also the only
--     lookup either the handler or the writer makes.
--   * no `location`. The Go column holds `39.900000,-105.500000` -- the coordinates the
--     fetch was made for -- and nothing reads it back. A station's coordinates are
--     `config.stations`' (W4 assumption 24), and a second copy here would be a second
--     answer to where a station is.
--   * no `created_at`. `updated_at` is the one the contract serialises, as `lastUpdated`.
--
-- M6-3 may need columns this does not have -- a fetch cursor, the upstream request id. An
-- `ADD COLUMN` of a nullable column is **D-9** and costs 11.6 ms; recreating a table whose
-- primary key is already the right one would not be cheaper.

CREATE TABLE forecasts (
    -- The station the forecast was fetched for. `text` referencing the read-side
    -- projection, exactly as `almanac_cache` and `snow_totals_cache` do -- the frozen
    -- contract's integer `?station_id=` is `config.stations.id` and is resolved to a
    -- station key before this table is reached (**D-65**: this side is the projection).
    station_id text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    -- `forecast_span_hours`: 24 is the hourly forecast, 240 the ten-day one. NOT a
    -- duration. The endpoint's grammar is `^\d{1,4}$`, so four digits is the widest value
    -- that can ever be asked for and a row outside that range is one no request can reach.
    span_hours integer     NOT NULL CHECK (span_hours BETWEEN 1 AND 9999),
    -- jsonb, and the read is `data::text`: key order is jsonb-normalised (key length then
    -- bytewise) and the numeric text is whatever was stored, so the same contract contains
    -- both `0` and `0.0` (Q-13). That is a property of this type and the reason the handler
    -- splices the text rather than re-encoding a parsed document (**D-39**).
    --
    -- An ARRAY, which the `?period=` branch depends on: it indexes the array and the
    -- out-of-range message names `len(data) - 1`. A stored object would make that branch
    -- unexpressible, so the shape is a constraint rather than a convention.
    data       jsonb       NOT NULL CHECK (jsonb_typeof(data) = 'array'),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, span_hours)
);

COMMENT ON TABLE forecasts IS
    'Read verbatim by the frozen /forecast endpoint; written by the aeris:forecast:* '
    'components (implementation-plan.md M6-3). data::text is served byte for byte.';

-- `/forecast/{span}` with no station names only the span, and the frozen handler answers
-- with any station's row for it. Without this index that is a scan of the table; with it,
-- the same index also serves the station-and-span lookup's second column.
CREATE INDEX forecasts_span_hours ON forecasts (span_hours, station_id);
