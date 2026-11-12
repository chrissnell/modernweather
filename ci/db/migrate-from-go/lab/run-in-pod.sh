#!/usr/bin/env bash
# The lab harness. Runs migrate-from-go.sh end to end against two throwaway TimescaleDB
# databases, so that every statement the procedure sends to a production database has
# already been executed against a faithful copy of the old schema.
#
# A migration procedure that has never been run is a proposal, which is why this exists.
#
# W0 OQ-1 recorded that production is unreachable from the agent runtime. That was wrong at
# the network layer — the blocker was a credential — and production is PostgreSQL 16.6 with
# TimescaleDB 2.17.2, not the pg17/2.31 this lab defaults to. The halves differ on purpose:
# the TARGET is pg17/2.31 because that is what ships (D-2, asserted by
# ci/sql/assert-timescaledb-version.sql), while the SOURCE is whatever pitcairn runs.
#
# ONE POD OR TWO. By default both databases live in one pod, which is the cheap case and
# what every earlier run used. Set SRC_IMAGE to a different image and the source moves to
# its OWN pod at that version — which is the only way to seed and migrate across the real
# version pair, because the target's migrations assert pg17/2.31 and refuse to build on
# 16.6. The 2026-11-01 source-side run could only check DDL and catalogue paths on an
# unseeded database for exactly that reason.
#
#   ci/db/migrate-from-go/lab/run-in-pod.sh up        create the pod(s), both schemas
#   ci/db/migrate-from-go/lab/run-in-pod.sh migrate   run the whole procedure
#   ci/db/migrate-from-go/lab/run-in-pod.sh down      delete the pod(s)
#
# POD, SRC_POD and NS override the pod names and namespace. The image is pinned to the one
# the `database` CI job uses, so a result here is a result there.
#
#   SRC_IMAGE=timescale/timescaledb-ha:pg16.6-ts2.17.2 \
#     REMOTEWEATHER=../remoteweather lab/run-in-pod.sh up
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
POD="${POD:-w9-migration-lab}"
NS="${NS:-default}"
IMAGE="${IMAGE:-timescale/timescaledb-ha:pg17}"
# The source half only. Defaults to IMAGE, in which case the source shares the target's
# pod and nothing about the earlier behaviour changes. Set it to production's version to
# run the whole procedure across the pair it will actually cross.
SRC_IMAGE="${SRC_IMAGE:-$IMAGE}"
# `10-seed.sh` reads SRC_POD too, so a split lab seeds the right database.
if [ "$SRC_IMAGE" = "$IMAGE" ]; then
  SRC_POD="${SRC_POD:-$POD}"
else
  SRC_POD="${SRC_POD:-$POD-src}"
fi
OLD_DB=weather_v2_0_0
NEW_DB=modernweather

kex() { kubectl exec -i "$POD" -n "$NS" -- "$@"; }
kex_src() { kubectl exec -i "$SRC_POD" -n "$NS" -- "$@"; }
psql_old() { kex_src psql -U postgres -d "$OLD_DB" -X -q -v ON_ERROR_STOP=1 "$@"; }
psql_new() { kex psql -U postgres -d "$NEW_DB" -X -q -v ON_ERROR_STOP=1 "$@"; }

# Bring up one pod and wait until its server answers.
start_pod() {
  local pod="$1" image="$2"
  kubectl run "$pod" -n "$NS" --image "$image" --restart=Never \
    --env POSTGRES_PASSWORD=w9lab --env PGDATA=/var/lib/postgresql/pgdata
  kubectl wait --for=condition=Ready "pod/$pod" -n "$NS" --timeout=300s
  until kubectl exec "$pod" -n "$NS" -- pg_isready -q; do sleep 2; done
}

case "${1:-help}" in
up)
  start_pod "$POD" "$IMAGE"
  if [ "$SRC_POD" != "$POD" ]; then
    start_pod "$SRC_POD" "$SRC_IMAGE"
    echo "source is its own pod: $SRC_POD at $SRC_IMAGE"
  fi
  kex_src psql -U postgres -c "CREATE DATABASE $OLD_DB"
  kex psql -U postgres -c "CREATE DATABASE $NEW_DB"
  # The OLD schema, built from the Go source's own constants rather than retyped.
  "$(dirname "${BASH_SOURCE[0]}")/emit-old-schema.sh" "${REMOTEWEATHER:?set REMOTEWEATHER to a remoteweather checkout}" \
    | psql_old -f -
  psql_old -f - < "$(dirname "${BASH_SOURCE[0]}")/00-old-harness.sql"
  # The NEW schema, from the migrations, so this is the schema that ships.
  for f in "$ROOT"/migrations/*.sql; do psql_new -1 -f - < "$f"; done
  echo "lab up: $OLD_DB on $SRC_POD (old schema) and $NEW_DB on $POD (migrations applied)"
  echo "now seed: see lab/10-seed.sh"
  ;;
migrate)
  export PSQL_SOURCE="kubectl exec -i $SRC_POD -n $NS -- psql -U postgres -d $OLD_DB -X -q -v ON_ERROR_STOP=1"
  export PSQL_TARGET="kubectl exec -i $POD -n $NS -- psql -U postgres -d $NEW_DB -X -q -v ON_ERROR_STOP=1"
  export TARGET="postgresql://postgres:w9lab@localhost/$NEW_DB"
  shift
  "$ROOT/ci/db/migrate-from-go/migrate-from-go.sh" "${@:-all}"
  ;;
down)
  # --ignore-not-found, because with two pods a missing first one must not stop `set -e`
  # from reaching the second.
  kubectl delete pod "$POD" -n "$NS" --wait=false --ignore-not-found
  [ "$SRC_POD" = "$POD" ] || kubectl delete pod "$SRC_POD" -n "$NS" --wait=false --ignore-not-found
  ;;
*)
  sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 2
  ;;
esac
