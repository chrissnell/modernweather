-- The alert table the `aeris:alerts:*` components write — implementation-plan.md M6-3.
--
-- **Nothing reads it on the frozen surface, and that is settled rather than overlooked.**
-- `docs/reconciliation.md` §6.3 retired `/alerts` (no in-repo client, not in either
-- client's endpoint list) and **A-M6-3-1** confirms it against M6-3's older done-criterion:
-- the alert *components* are in scope because "alerts failing, forecasts fine" has to be
-- expressible (W2a **D-33**), the alert *endpoint* is not. So this table exists to be
-- written, swept, and read later by whatever surface the owner decides alerts deserve —
-- today that is the management UI's "last alert fetch" (W5 §1544), which reads
-- `updated_at` and nothing else.
--
-- **Not a hypertable, and that is `C-W2a-15`.** OQ-C offered two homes for the
-- expired-alert sweep: a TimescaleDB retention policy if this became a hypertable, or the
-- `Calendar` component W2a specified. W4 never addressed alerts at all, so the arm that is
-- operative is settled by elimination — this is an ordinary table and
-- `aeris:alerts:gc` is a component. Which is also the right shape for the data: an alert
-- is identified by its upstream id and is **updated in place** as the issuing office
-- revises it, and an upsert against a hypertable's unique index is the one write pattern
-- hypertables are bad at.
--
-- **W4 did not specify this table**, for the same reason it did not specify `forecasts`
-- (`20261109120000_forecasts_the_read_side_of_the_frozen_endpoint.sql`): `docs/database-schema.md`
-- mentions no alert anywhere and W1 assumption 12 names `almanac_cache` and
-- `snow_totals_cache` as the two precomputed caches and stops. That is the `G-2` class
-- again — several documents assumed a relation into existence and none created one — and it
-- is created here by the item that first writes it.

-- The Go table is `aeris_weather_alerts` (`internal/controllers/aerisweather/controller.go:159`).
-- Three of its columns are deliberately not carried forward and one key changes shape:
--
--   * no `id` surrogate and no `deleted_at`. `gorm.Model` supplies both; the soft delete is
--     actively harmful here, because the sweep's whole job is to make rows go away and a
--     `deleted_at` stamp makes "expired more than 24 h ago" grow without bound.
--   * no `location`. The Go column holds `39.900000,-105.500000` — the coordinates the
--     fetch was made for — and nothing reads it back. A station's coordinates are
--     `config.stations`' (W4 assumption 24); a second copy here would be a second answer to
--     where a station is. `forecasts` dropped the same column for the same reason.
--   * `created_at` becomes `fetched_at`, which is what it actually means: when this node
--     first saw the alert. The upstream's own `issued`/`added`/`created` timestamps are
--     inside `data`, and three of them are already columns below.
--
-- **The primary key is `(station_id, alert_id)` and the Go one was `alert_id` alone.** Go
-- declares `AlertID string gorm:"uniqueIndex"`, so one alert row can belong to exactly one
-- station — and a county-level warning covering two configured stations is therefore a row
-- whose `station_id` is rewritten by whichever component fetched last, every fifteen
-- minutes, forever. Two stations in one warning polygon is the ordinary case for this
-- program's deployment (the owner's stations are in one county), so the natural key is the
-- pair. The deviation is database-only: no response on any surface reads this table, which
-- is what makes correcting it free here and a behaviour change later.
CREATE TABLE alerts (
    -- The station the alert was fetched for. `text` referencing the read-side projection,
    -- exactly as `forecasts`, `almanac_cache` and `snow_totals_cache` do (**D-65**: this
    -- side is the projection; the frozen contract's integer `?station_id=` is
    -- `config.stations.id` and is resolved to a station key before this table is reached).
    station_id text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    -- The upstream's own identifier for the alert, which is what makes a revision an
    -- update rather than a duplicate.
    alert_id   text        NOT NULL CHECK (length(alert_id) > 0),

    -- The three timestamps the sweep and the UI need as columns rather than as `jsonb`
    -- reads. Nullable because the upstream omits them: Go tests each for `> 0` before
    -- taking a pointer to it, so an absent timestamp is a NULL here and not the epoch.
    issued_at  timestamptz,
    begins_at  timestamptz,
    -- `aeris:alerts:gc` deletes on this column, which is why it is indexed below. A NULL
    -- expiry is an alert that never expires by the upstream's own account, so the sweep
    -- must not take it -- `expires_at < $1` is already NULL-safe in that direction, and it
    -- is stated here because the obvious "clean up anything old" rewrite is not.
    expires_at timestamptz,

    -- The four fields the Go row promotes out of the payload, for a list view that does not
    -- parse `jsonb` per row. `details.name`, `details.color`, `details.body`,
    -- `details.bodyFull`.
    name       text        NOT NULL DEFAULT '',
    color      text        NOT NULL DEFAULT '',
    body       text        NOT NULL DEFAULT '',
    body_full  text        NOT NULL DEFAULT '',

    -- The whole upstream alert object, as fetched. An OBJECT, where `forecasts.data` is an
    -- array: `/alerts` paged over the array and this is one alert per row, so the shape
    -- constraint differs from that table's on purpose.
    data       jsonb       NOT NULL CHECK (jsonb_typeof(data) = 'object'),

    -- When this node first saw the alert, and when it last wrote it. `updated_at` is what
    -- W5 §1544's "last alert fetch time" renders through `Freshness`, so an
    -- `ON CONFLICT ... DO UPDATE` that forgets to set it makes that display wrong in the
    -- one direction that matters -- the same trap `forecasts.updated_at` carries a note
    -- about, and for the same reason: gorm managed this field automatically on the Go side.
    fetched_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (station_id, alert_id)
);

COMMENT ON TABLE alerts IS
    'Written by the aeris:alerts:<station> components and swept by aeris:alerts:gc '
    '(implementation-plan.md M6-3). Deliberately read by no frozen endpoint: '
    'docs/reconciliation.md A-M6-3-1 retired /alerts and kept the components.';

-- The sweep's whole query: `DELETE FROM alerts WHERE expires_at < now() - interval '24
-- hours'`. Without this it is a sequential scan once a day, which at this table's size is
-- nothing -- and the index is here anyway because the alternative to measuring is a
-- sequential scan on a table whose size is bounded by how many alerts an operator's
-- counties issue, which is not a number this repository gets to choose.
CREATE INDEX alerts_expires_at ON alerts (expires_at);
