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
# POD, NS, IMAGE, SRC_IMAGE, SRC_POD and lab_adopt_existing_source_pod, shared with
# run-in-pod.sh so that one environment cannot select two different source pods.
# shellcheck source=ci/db/migrate-from-go/lab/lab-env.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lab-env.sh"
lab_adopt_existing_source_pod
DB=weather_v2_0_0
psql_old() { kubectl exec -i "$SRC_POD" -n "$NS" -- psql -U postgres -d "$DB" -X -q -v ON_ERROR_STOP=1 "$@"; }

FROM="${FROM:-2025-01-01}"
TO="${TO:-2026-10-30}"
CADENCE="${CADENCE:-40}"
# How much raw to keep, which is what sets the width of the rollup-only window: the
# window is (TO - FROM) - RETAIN. Production's value is 365 days and that is the default,
# so it has to be lowered along with a shortened FROM..TO — at the default a span of 365
# days or less produces NO rollup-only window at all, and then `rollup` imports nothing,
# `assert_legacy_disjoint()` passes over an empty table and the reconciliation is green
# on a lab that never exercised the legacy path. The check below refuses that.
RETAIN="${RETAIN:-365 days}"

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
# raw RETAIN, 1m 1 month, 5m 6 months. weather_1h (2 years) and weather_1d (10 years)
# keep everything, which is exactly how 660 days of rollup ends up sitting on 366 days of
# raw. drop_chunks takes a literal rather than a subquery over the relation it is dropping
# from: the subquery holds a reference and the drop fails.
#
# An explicit drop_chunks and NOT a retention policy, because the lab's data sits in the
# past while now() does not: a policy with production's `drop_after` is immediately due
# over the whole seeded range and deletes all of it within minutes. The policies added
# below exist for `pause-source-retention` to find and are sized so that they cannot fire
# on anything.
RAW_CUT="$(psql_old -tAc "SELECT (max(time) - INTERVAL '$RETAIN')::date FROM weather")"
M1_CUT="$(psql_old -tAc "SELECT (max(bucket) - INTERVAL '1 month')::date FROM weather_1m")"
M5_CUT="$(psql_old -tAc "SELECT (max(bucket) - INTERVAL '6 months')::date FROM weather_5m")"
psql_old -tAc "SELECT drop_chunks('weather',    older_than => '$RAW_CUT'::timestamptz)" >/dev/null
psql_old -tAc "SELECT drop_chunks('weather_1m', older_than => '$M1_CUT'::timestamptz)"  >/dev/null
psql_old -tAc "SELECT drop_chunks('weather_5m', older_than => '$M5_CUT'::timestamptz)"  >/dev/null

# The rollup-only window is the whole reason W9 exists. Measure it rather than assume it:
# a seed that produced none looks like every other successful seed, and the reconciliation
# downstream would pass without the legacy path having moved a single bucket.
ROLLUP_ONLY="$(psql_old -tAc "SELECT (SELECT min(time)::date FROM weather)
                                   - (SELECT min(bucket)::date FROM weather_1d)")"
if [ "${ROLLUP_ONLY:-0}" -le 0 ]; then
  cat >&2 <<EOM

  SEED PRODUCED NO ROLLUP-ONLY WINDOW. raw begins no later than weather_1d does, so
  \`rollup\` has nothing to import and assert_legacy_disjoint() would pass over an empty
  table. RETAIN is '$RETAIN' and the seeded span is $FROM .. $TO; the window is the
  difference, so lower RETAIN or widen the span.
EOM
  exit 1
fi
echo "=== rollup-only window: $ROLLUP_ONLY days of daily buckets with no raw behind them"

# Retention policies, so that `pause-source-retention` has the thing it pauses and the
# four aggregate policies it must leave alone — production has five (W6 O-2: 1004 on raw
# plus 1005–1008 on the levels). `drop_after` is derived to reach past the oldest row in
# ANY of the five relations, so no policy run can remove anything; production's own values
# would empty the lab within minutes, for the reason given above.
#
# The oldest row is NOT raw's: the whole point of the seed is that weather_1d reaches
# further back than `weather` does, and a drop_after derived from raw alone would leave
# the 1d policy free to delete exactly the rollup-only history this seed exists to build.
# LEAST over all five, and it ignores the NULLs an empty level would contribute.
KEEP_ALL="$(psql_old -tAc "SELECT ((now() - LEAST(
             (SELECT min(time)   FROM weather),
             (SELECT min(bucket) FROM weather_1m),
             (SELECT min(bucket) FROM weather_5m),
             (SELECT min(bucket) FROM weather_1h),
             (SELECT min(bucket) FROM weather_1d))) + INTERVAL '30 days')::text")"
#
# Removed and re-added rather than `if_not_exists`, so that a second seed run over a wider
# range gets the wider `drop_after` instead of silently keeping the first run's.
for lv in weather weather_1m weather_5m weather_1h weather_1d; do
  psql_old -tAc "SELECT remove_retention_policy('$lv', if_exists => true)" >/dev/null
  psql_old -tAc "SELECT add_retention_policy('$lv', INTERVAL '$KEEP_ALL')" >/dev/null
done
echo "=== retention policies, none of which can fire on this data"
psql_old -c "SELECT job_id, hypertable_name, scheduled, config ->> 'drop_after' AS drop_after
               FROM timescaledb_information.jobs
              WHERE proc_name = 'policy_retention' ORDER BY job_id"

echo "=== the state this produced"
kubectl exec "$SRC_POD" -n "$NS" -- psql -U postgres -d "$DB" -X -c "
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
