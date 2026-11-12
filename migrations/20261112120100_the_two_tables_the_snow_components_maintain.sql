-- The two intermediate tables the `snow:*` components maintain — implementation-plan.md M6-3.
--
-- `snow_totals_cache` already exists (W4, `20261029234500_...`) and is the seven numbers
-- `/snow` serves. It is the **output**. These two are the working set behind it, and both
-- have a Go ancestor:
--
--   snow_depth_estimates   <- `snow_depth_est_5m` (Go migration 025)
--   snow_events            <- `snow_events_cache` (Go migration 023)
--
-- **Why the estimates table is not an optimisation.** The number `/snow` serves as
-- `snowlast24` is `max(estimate) - min(estimate)` over 24 hours of a **smoothed**,
-- **rate-limited** depth series -- not of the raw gauge distance. A sonic snow gauge
-- pointed at settling snow reports a series that rises in steps and falls continuously, so
-- `max - min` over the raw signal counts every gust and every melt cycle as new snowfall.
-- The smoothing (local upper quantile) and the rate limit (4 in/h up, 1.5 in/h down) are
-- what make the difference physical rather than statistical, and they are a function of a
-- *window* of the series, so they cannot be computed per request and cannot be computed at
-- ingest (**R1**/**R2**, docs/service-architecture.md §2.5). A maintained table is the only
-- remaining shape.
--
-- **Why the events table is not the same thing twice.** The estimates feed the totals; the
-- events feed the website's accumulation bands, and they are detected by a different
-- algorithm on purpose (PELT changepoint detection over a median-filtered series, with a
-- segment classifier). W3 §2a.7 gives them separate components for separate reasons, and
-- `docs/component-model.md` §4.3's table gives each its own row: `snow:smoothing` writes
-- the first table, `snow:events` writes the second.

-- ---------------------------------------------------------------------------------------
-- snow_depth_estimates -- `snow:smoothing`'s output and `snow:smoothing`/`snow:season`'s input.

-- **A hypertable, which the Go table also is.** One row per 5-minute bucket per snow
-- station is ~105 000 rows a year per station, the reads are always a time range, and the
-- write pattern is "delete a trailing window and re-insert it" -- which is a chunk-local
-- operation on a hypertable and a whole-table index churn without one.
--
-- Not a continuous aggregate, which is the other thing it looks like: a continuous
-- aggregate is a pure function of its source rows and this is not one. Each estimate
-- depends on the estimate before it (the rate limit carries state forward), so it is
-- recursive over its own output and no `CREATE MATERIALIZED VIEW` can express it.
CREATE TABLE snow_depth_estimates (
    -- The read-side projection's key, exactly as the caches do (**D-65**).
    station_id text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    -- The 5-minute bucket this estimate is for. Named `bucket` rather than the Go table's
    -- `time`, because it IS a bucket -- it comes from `observations_5m` and `bucket` is what
    -- every aggregate level in this schema calls that column.
    bucket     timestamptz NOT NULL,
    -- **Inches**, which is the unit `/snow` serves and the unit the rate limit is expressed
    -- in (4.0 in/h up, 1.5 in/h down). NOT the millimetres `observations.snowdepth` carries
    -- (Q-4, W3 assumption 23). `real` and not `double precision`: the number this feeds is
    -- a Go `float32` on the wire and is rounded to tenths of an inch before it is served, so
    -- eight significant digits is six more than can survive.
    depth_in   real        NOT NULL,
    PRIMARY KEY (station_id, bucket)
);

SELECT create_hypertable('snow_depth_estimates', 'bucket', chunk_time_interval => INTERVAL '30 days');

COMMENT ON TABLE snow_depth_estimates IS
    'Smoothed, rate-limited snow depth in INCHES per 5-minute bucket, written by '
    'snow:smoothing (implementation-plan.md M6-3). snowlast24/snowlast72 are max-min over '
    'this series and snowseason is max-less-first; nothing serves it directly.';

COMMENT ON COLUMN snow_depth_estimates.depth_in IS
    'Inches. (base_distance_mm - snowdistance) / 25.4, then local upper-quantile smoothing '
    'over a +/-30 minute window at the 85th percentile, then a rate limit of 4.0 in/h up '
    'and 1.5 in/h down carried forward from the previous estimate.';

-- `(station_id, bucket DESC)` is the shape of every read: the newest estimate (the
-- incremental start point), the estimate immediately before a boundary (the rate limit's
-- carried state), and a min/max over a range. The primary key's ascending index serves the
-- range scans; this one serves the two `ORDER BY bucket DESC LIMIT 1` reads, which are the
-- ones on the 15-minute path.
CREATE INDEX snow_depth_estimates_newest ON snow_depth_estimates (station_id, bucket DESC);

-- ---------------------------------------------------------------------------------------
-- snow_events -- `snow:events`' output.

-- An ordinary table, not a hypertable: it holds at most a few dozen rows per
-- `(station, window)` and is replaced wholesale on every 15-minute tick, which is the write
-- pattern a hypertable is worst at.
CREATE TABLE snow_events (
    station_id      text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    -- The chart range this row was detected for: 24, 72, 168 (7 d) or 744 (31 d). The same
    -- four the website's `CHART_RANGES` asks for, which is why they are a CHECK rather than
    -- a convention -- a fifth range is a code change in `snow:events` and this constraint is
    -- where the two are kept in agreement.
    hours           integer     NOT NULL CHECK (hours IN (24, 72, 168, 744)),
    start_time      timestamptz NOT NULL,
    end_time        timestamptz NOT NULL CHECK (end_time >= start_time),
    -- The segment classifier's verdict. Exhaustive, because a fifth class would change
    -- which segments contribute snow and that is a decision, not a string.
    event_type      text        NOT NULL CHECK (event_type IN (
                        'accumulation', 'plateau', 'redistribution', 'spike_then_settle')),
    -- **Millimetres**, which is what the detector works in -- it reads
    -- `base_distance - snowdistance` straight out of the aggregate and never converts.
    -- Deliberately a different unit from `snow_depth_estimates.depth_in` above, and the
    -- column names carry it for `G-2`'s reason: a mistyped unit here produces plausible
    -- wrong numbers rather than an error.
    start_depth_mm  real        NOT NULL,
    end_depth_mm    real        NOT NULL,
    -- How much snow this segment contributed. NULL for a segment that contributed none,
    -- which is `plateau` and `redistribution` -- a zero would be indistinguishable from an
    -- accumulation event that was measured at zero.
    accumulation_mm real,
    computed_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, hours, start_time),
    -- The two classes that contribute snow are the two that carry an accumulation, and the
    -- two that do not, do not. Go writes `event.SnowMM` unconditionally and then filters
    -- `SnowMM > 0` at both the insert and the read, which is the same rule applied twice in
    -- two places; here it is the table's.
    CHECK ((accumulation_mm IS NOT NULL)
           = (event_type IN ('accumulation', 'spike_then_settle')))
);

COMMENT ON TABLE snow_events IS
    'Classified snow accumulation segments per chart range, written by snow:events '
    '(implementation-plan.md M6-3). PELT changepoint detection over a median-filtered '
    'depth series; replaced wholesale per (station_id, hours) on every tick.';

-- The read is always one `(station, hours)` pair in `start_time` order, which the primary
-- key already serves. What it does not serve is the replace: `DELETE ... WHERE station_id =
-- $1 AND hours = $2` is a prefix of the same key, so there is deliberately no second index
-- here -- the Go table has two and neither adds a plan the primary key does not already
-- give.
