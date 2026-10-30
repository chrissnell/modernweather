#!/usr/bin/env bash
# The lab harness. Runs migrate-from-go.sh end to end against two databases inside a
# throwaway TimescaleDB pod, so that every statement the procedure sends to a production
# database has already been executed against a faithful copy of the old schema.
#
# Production is unreachable from the agent runtime (W0 OQ-1), which is why this exists: a
# migration procedure that has never been run is a proposal.
#
#   ci/db/migrate-from-go/lab/run-in-pod.sh up        create the pod, both schemas, seed
#   ci/db/migrate-from-go/lab/run-in-pod.sh migrate   run the whole procedure
#   ci/db/migrate-from-go/lab/run-in-pod.sh down      delete the pod
#
# POD and NS override the pod name and namespace. The image is pinned to the one the
# `database` CI job uses, so a result here is a result there.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
POD="${POD:-w9-migration-lab}"
NS="${NS:-default}"
IMAGE="${IMAGE:-timescale/timescaledb-ha:pg17}"
OLD_DB=weather_v2_0_0
NEW_DB=modernweather

kex() { kubectl exec -i "$POD" -n "$NS" -- "$@"; }
psql_old() { kex psql -U postgres -d "$OLD_DB" -X -q -v ON_ERROR_STOP=1 "$@"; }
psql_new() { kex psql -U postgres -d "$NEW_DB" -X -q -v ON_ERROR_STOP=1 "$@"; }

case "${1:-help}" in
up)
  kubectl run "$POD" -n "$NS" --image "$IMAGE" --restart=Never \
    --env POSTGRES_PASSWORD=w9lab --env PGDATA=/var/lib/postgresql/pgdata
  kubectl wait --for=condition=Ready "pod/$POD" -n "$NS" --timeout=300s
  until kubectl exec "$POD" -n "$NS" -- pg_isready -q; do sleep 2; done
  kex psql -U postgres -c "CREATE DATABASE $OLD_DB"
  kex psql -U postgres -c "CREATE DATABASE $NEW_DB"
  # The OLD schema, built from the Go source's own constants rather than retyped.
  "$(dirname "${BASH_SOURCE[0]}")/emit-old-schema.sh" "${REMOTEWEATHER:?set REMOTEWEATHER to a remoteweather checkout}" \
    | psql_old -f -
  psql_old -f - < "$(dirname "${BASH_SOURCE[0]}")/00-old-harness.sql"
  # The NEW schema, from the migrations, so this is the schema that ships.
  for f in "$ROOT"/migrations/*.sql; do psql_new -1 -f - < "$f"; done
  echo "lab up: $OLD_DB (old schema) and $NEW_DB (migrations applied)"
  echo "now seed: see lab/10-seed.sh"
  ;;
migrate)
  export PSQL_SOURCE="kubectl exec -i $POD -n $NS -- psql -U postgres -d $OLD_DB -X -q -v ON_ERROR_STOP=1"
  export PSQL_TARGET="kubectl exec -i $POD -n $NS -- psql -U postgres -d $NEW_DB -X -q -v ON_ERROR_STOP=1"
  export TARGET="postgresql://postgres:w9lab@localhost/$NEW_DB"
  shift
  "$ROOT/ci/db/migrate-from-go/migrate-from-go.sh" "${@:-all}"
  ;;
down)
  kubectl delete pod "$POD" -n "$NS" --wait=false
  ;;
*)
  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 2
  ;;
esac
