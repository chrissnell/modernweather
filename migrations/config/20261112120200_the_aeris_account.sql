-- The Aeris account — implementation-plan.md M6-3.
--
-- `20261102230000_config_schema_initial.sql` already named this file's subject, twice:
--
--     -- Aeris is NOT here. It is four fetching components (W2a D-33, M6-3) rather than an
--     -- egress, so its account configuration belongs to M6-3's own migration [...]
--
--     -- "Which stations get forecasts and alerts" is a station/service pairing like any
--     -- other; the Aeris ACCOUNT (client id, secret, endpoint, refresh interval) is
--     -- M6-3's, and it is not an uploader, so the pairing carries no `uploader_id`.
--
-- This is that migration. The split it completes is the one `uploader`/`station_service`
-- makes: `station_service (service = 'aeris')` says WHICH stations get forecasts and
-- alerts, and this table says WHOSE account fetches them. The `CHECK ((service = 'aeris') =
-- (uploader_id IS NULL))` on that table is what makes the two unable to disagree -- an
-- Aeris pairing cannot name an `uploader` row, so it has nowhere to point but here.
--
-- **One row, not one per station, and that is a normalisation rather than a simplification.**
-- The Go `devices` row carries `aeris_enabled`, `aeris_api_client_id`,
-- `aeris_api_client_secret`, `aeris_api_endpoint` and `aeris_refresh_interval` **per
-- device**, so an operator with three stations types the same credential three times and a
-- rotated secret is three `UPDATE`s, two of which will be forgotten. The credential is a
-- property of the *account*, which this node has one of -- exactly the argument the
-- `uploader` table is built on (D-66, D-67), and exactly what the initial schema's comment
-- means by "the Aeris ACCOUNT". M7-4's data migration collapses the per-device copies; a
-- disagreement between them is a refusal there rather than a silently-chosen winner.
--
-- **A behaviour this changes, stated because it is observable:** under Go, two stations
-- could fetch from two different Aeris accounts, or one could be enabled and the other not
-- with different credentials. Here they share the account and the pairing is the switch.
-- The second half of that was already true in Go in practice -- the website's
-- `aerisWeatherEnabled` is derived from the pairing -- and the first half is a capability
-- nothing in the shipped configuration uses.

CREATE TABLE aeris_account (
    -- One row. `CHECK (id = 1)` is how the `node` table says the same thing, and for the
    -- same reason: a singleton enforced by a constraint is a singleton, and one enforced by
    -- a convention is a table that eventually has two rows and a component that picks one.
    id                       INTEGER PRIMARY KEY CHECK (id = 1),

    -- W5 §1.4's "Stop fetching forecasts". Under W2a §2.5 this produces a plan diff that
    -- stops all four components; the station pairings are left alone, so turning it back on
    -- does not require re-pairing every station.
    --
    -- Deliberately **not** redundant with the pairings: "the account is off" and "no
    -- station is paired" are different operator intentions, and `plan()` reads the
    -- conjunction. `uploader.enabled` makes the same distinction against
    -- `station_service`'s rows.
    enabled                  INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),

    -- The credential pair. A `SecretString` from the moment it is read (**D-33**); no
    -- endpoint returns the secret. The client id is not a secret -- it travels in a query
    -- string beside the secret, which is why the SECRET travelling in a query string is
    -- worth a note of its own: that is the upstream's own authentication scheme
    -- (`client_id=`/`client_secret=` on a GET), so the request is over TLS and the URL is
    -- never logged. `mw_output::form::Form`'s `Debug` redacting is what enforces the second
    -- half.
    client_id                TEXT,
    client_secret            TEXT,

    -- `https://data.api.xweather.com` when NULL, which is the Go default
    -- (`controller.go:402`). An override exists because the vendor has renamed the host once
    -- already (`aerisapi.com` -> `xweather.com`) and a rename should not need a release.
    api_endpoint             TEXT,

    -- `Trigger::Every`'s period for the two forecast components, in seconds. NULL means the
    -- derived one -- a quarter of the forecast's own period, which is `docs/component-model.md`
    -- §4.3's `Every { span/4, >= 1h }`.
    --
    -- The one-hour floor is deliberately **not** a CHECK, for the reason
    -- `uploader.interval_seconds`' floor is not one: W2a OQ-A's upgrade path requires a
    -- sub-floor value to be RAISED with a one-line report rather than rejected, which a
    -- constraint here would make impossible. The floor is `Trigger::Every { floor }`, which
    -- `plan()` enforces once.
    --
    -- It does not apply to the alerts component. Go's alert interval is a hard-coded
    -- fifteen minutes (`controller.go:716`) with no per-device override, and an alert is the
    -- one thing here where staleness is the whole cost -- so there is no column for it.
    refresh_interval_seconds INTEGER CHECK (refresh_interval_seconds IS NULL
                                            OR refresh_interval_seconds > 0),

    created_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at               INTEGER NOT NULL DEFAULT (unixepoch()),

    -- An enabled account with no credential is a component that will fail every tick
    -- forever with a `401`, which is the failure mode W2a Decision 30 forbids giving up on
    -- and therefore the one worth rejecting at the schema. `plan()` cannot be the only
    -- check: the row is also written by `ConfigAdmin` (M7-1), and a constraint is the only
    -- kind of check both paths share.
    CHECK (enabled = 0 OR (client_id IS NOT NULL AND length(client_id) > 0
                           AND client_secret IS NOT NULL AND length(client_secret) > 0))
) STRICT;
