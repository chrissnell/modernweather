#!/usr/bin/env bash
# Seed the lab's OLD database into the *shape* of the owner's production database.
#
# The shape is the point, not the volume. Production's distinguishing feature is not that
# it is 19 GB; it is that `weather_1d` reaches back 660 days while raw reaches back 366,
# because an active 365-day retention policy deleted the raw rows the old daily buckets
# were computed from. That window — the ~294 days that cannot be recomputed from anything —
# is the whole reason W9 exists, and it is reproduced here by the same mechanism production
# used: materialise the aggregates over the full range, then let retention drop the raw
# chunks underneath them.
#
# What is NOT reproduced: production's row count, its 2-second cadence and therefore its
# consecutive-identical ("hold") rate, which is the single variable that predicts the
# compression ratio (D-10, OQ-2). Nothing in docs/data-migration.md quotes a compression
# figure measured here for that reason.
set -euo pipefail
POD="${POD:-w9-migration-lab}"
NS="${NS:-default}"
DB=weather_v2_0_0
psql_old() { kubectl exec -i "$POD" -n "$NS" -- psql -U postgres -d "$DB" -X -q -v ON_ERROR_STOP=1 "$@"; }

FROM="${FROM:-2025-01-01}"
TO="${TO:-2026-10-30}"
CADENCE="${CADENCE:-40}"

echo "=== raw: $FROM .. $TO at ${CADENCE}s, 3 stations"
# 40 seconds, not 60: it puts 1 or 2 readings in each 1-minute bucket rather than exactly
# one, which is D-12's uneven-bucket condition — the condition under which a mean of means
# is wrong and a sum-and-count hierarchy is not. A 60-second cadence would make every
# reconciliation below pass for the wrong reason.
psql_old -c "SET synchronous_commit = off" \
         -c "CALL w9_lab.fill('$FROM'::timestamptz, '$TO'::timestamptz, $CADENCE)"

echo "=== materialise all four levels over the full range"
for lv in 1m 5m 1h 1d; do
  psql_old -c "SET synchronous_commit = off" \
           -c "CALL refresh_continuous_aggregate('weather_$lv', '$FROM'::timestamptz, '$TO'::timestamptz)"
done

echo "=== now let retention do what it has been doing in production since 2025"
# raw 365 days, 1m 1 month, 5m 6 months. weather_1h (2 years) and weather_1d (10 years)
# keep everything, which is exactly how 660 days of rollup ends up sitting on 366 days of
# raw. drop_chunks takes a literal rather than a subquery over the relation it is dropping
# from: the subquery holds a reference and the drop fails.
RAW_CUT="$(psql_old -tAc "SELECT (max(time) - INTERVAL '365 days')::date FROM weather")"
M1_CUT="$(psql_old -tAc "SELECT (max(bucket) - INTERVAL '1 month')::date FROM weather_1m")"
M5_CUT="$(psql_old -tAc "SELECT (max(bucket) - INTERVAL '6 months')::date FROM weather_5m")"
psql_old -tAc "SELECT drop_chunks('weather',    older_than => '$RAW_CUT'::timestamptz)" >/dev/null
psql_old -tAc "SELECT drop_chunks('weather_1m', older_than => '$M1_CUT'::timestamptz)"  >/dev/null
psql_old -tAc "SELECT drop_chunks('weather_5m', older_than => '$M5_CUT'::timestamptz)"  >/dev/null

echo "=== the state this produced"
kubectl exec "$POD" -n "$NS" -- psql -U postgres -d "$DB" -X -c "
SELECT 'weather' AS relation, count(*)::text AS rows, min(time)::date::text AS lo,
       max(time)::date::text AS hi, (max(time)::date - min(time)::date)::text AS days FROM weather
UNION ALL SELECT 'weather_1m', count(*)::text, min(bucket)::date::text, max(bucket)::date::text,
       (max(bucket)::date - min(bucket)::date)::text FROM weather_1m
UNION ALL SELECT 'weather_5m', count(*)::text, min(bucket)::date::text, max(bucket)::date::text,
       (max(bucket)::date - min(bucket)::date)::text FROM weather_5m
UNION ALL SELECT 'weather_1h', count(*)::text, min(bucket)::date::text, max(bucket)::date::text,
       (max(bucket)::date - min(bucket)::date)::text FROM weather_1h
UNION ALL SELECT 'weather_1d', count(*)::text, min(bucket)::date::text, max(bucket)::date::text,
       (max(bucket)::date - min(bucket)::date)::text FROM weather_1d;
SELECT w9_lab.hold_rate() AS hold_rate_7d;"
