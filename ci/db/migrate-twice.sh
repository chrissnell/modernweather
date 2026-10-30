#!/usr/bin/env bash
# Apply the migrations twice and prove the two results are the same schema.
#
#   path A   empty database            -> every migration at HEAD
#   path B   previous release's schema -> seed rows -> every migration at HEAD
#
# Why both. A migration set is almost always tested the way path A tests it, from nothing,
# and that is the path no production database ever takes. Path B is the one that matters,
# and the two have to agree: if they do not, a fresh install and an upgraded install are
# running different schemas, and every later migration is written against whichever one
# its author happened to have.
#
# The W8 prompt asked for "the union-view assertions W4 defines". There are none to make:
# DESIGN.md D-5 rejects the versioned-aggregate and union-view scheme outright, because
# raw data is retained indefinitely (D-4) so there is no history trapped in an old
# aggregate version to union over. The equivalent assertion on the design as decided is
# the one made below — the aggregate definitions and the values they return are identical
# on both paths — and it is strictly stronger than comparing view texts.
#
# Requires: psql, pg_dump, and DATABASE_URL pointing at a server this script may create
# and drop databases on. No Rust toolchain. Arms itself when migrations/ exists (see ci/gates.toml).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

: "${DATABASE_URL:?set DATABASE_URL to a server where databases may be created and dropped}"
MIGRATIONS_DIR="${MIGRATIONS_DIR:-migrations}"
DB_A="${DB_A:-mw_migrate_a}"
DB_B="${DB_B:-mw_migrate_b}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ ! -d "$MIGRATIONS_DIR" ]; then
  echo "no $MIGRATIONS_DIR/ — nothing to migrate. This script is armed by its existence." >&2
  exit 1
fi

# The server URL with the database component stripped, so psql can target each scratch
# database and the maintenance database in turn.
BASE_URL="${DATABASE_URL%/*}"
admin() { psql --set ON_ERROR_STOP=1 -q "$BASE_URL/postgres" "$@"; }
on() { psql --set ON_ERROR_STOP=1 -q "$BASE_URL/$1" "${@:2}"; }

echo "=== asserting the server version (DESIGN.md D-42)"
on postgres -f ci/sql/assert-timescaledb-version.sql

recreate() {
  admin -c "DROP DATABASE IF EXISTS $1 WITH (FORCE)"
  admin -c "CREATE DATABASE $1"
  on "$1" -c "CREATE EXTENSION IF NOT EXISTS timescaledb"
}

# One applier, used for both paths. The daemon applies migrations with sqlx-migrate
# (DESIGN.md D-8); this script does not shell out to sqlx-cli, for two reasons. It must be
# runnable before the daemon's toolchain exists, and — more importantly — path B only means
# anything if the second apply runs the *delta*. A loop that re-applies every file in
# filename order makes path B fail on `relation already exists` and tells you nothing about
# the schema, so the ledger below is not a convenience, it is the semantics being tested.
#
# Matches sqlx-migrate on the two properties that matter here: each migration is applied
# exactly once, in filename order, inside a transaction.
LEDGER="_mw_ci_applied"

apply_migrations() {
  local db="$1" dir="$2" f base
  on "$db" -c "CREATE TABLE IF NOT EXISTS $LEDGER (filename text PRIMARY KEY,
                                                   applied_at timestamptz NOT NULL DEFAULT now())"
  for f in $(find "$dir" -name '*.sql' | sort); do
    base="$(basename "$f")"
    if [ -n "$(on "$db" -At -c "SELECT 1 FROM $LEDGER WHERE filename = '$base'")" ]; then
      echo "  $base (already applied)"
      continue
    fi
    echo "  $base"
    # -1 wraps the file in a single transaction, so a migration that fails half way
    # through leaves nothing behind and the ledger stays truthful.
    on "$db" -1 -f "$f"
    on "$db" -c "INSERT INTO $LEDGER (filename) VALUES ('$base')"
  done
}

# Path B starts from the previous release's schema. That is reconstructed from git rather
# than from a committed dump, so it cannot go stale: the previous release is whatever the
# most recent reachable tag says it is.
previous_ref() {
  if [ -n "${MW_PREVIOUS_REF:-}" ]; then
    echo "$MW_PREVIOUS_REF"
  elif ! command -v git >/dev/null 2>&1 || ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo ""
  elif git describe --tags --abbrev=0 --match 'v*' HEAD^ >/dev/null 2>&1; then
    git describe --tags --abbrev=0 --match 'v*' HEAD^
  else
    # No release yet: the merge base with the default branch is the closest thing to
    # "the schema that already exists somewhere". Empty if even that is unavailable, in
    # which case path B degrades to path A and says so.
    git merge-base HEAD "origin/${MW_DEFAULT_BRANCH:-main}" 2>/dev/null || echo ""
  fi
}

PREV="$(previous_ref)"
PREV_DIR="$WORK/prev-migrations"
mkdir -p "$PREV_DIR"
if [ -n "${MW_PREVIOUS_MIGRATIONS_DIR:-}" ]; then
  # Escape hatch for running this outside a git checkout, and for testing the script
  # itself against a hand-made "previous" set.
  echo "=== previous schema comes from $MW_PREVIOUS_MIGRATIONS_DIR"
  rm -rf "$PREV_DIR"
  cp -r "$MW_PREVIOUS_MIGRATIONS_DIR" "$PREV_DIR"
elif [ -z "$PREV" ]; then
  echo "=== no previous ref available; path B will start from an empty database"
  echo "    This run is weaker than it will be once there is a release to upgrade from." >&2
else
  echo "=== previous schema comes from $PREV"
  if git archive "$PREV" -- "$MIGRATIONS_DIR" 2>/dev/null | tar -x -C "$WORK"; then
    rm -rf "$PREV_DIR"
    mv "$WORK/$MIGRATIONS_DIR" "$PREV_DIR"
  else
    echo "note: $PREV has no $MIGRATIONS_DIR/, so path B starts from an empty database too." >&2
    echo "      That makes this run weaker than it will be at the next release; it is not" >&2
    echo "      a reason to skip it." >&2
  fi
fi

echo "=== path A: empty -> HEAD"
recreate "$DB_A"
apply_migrations "$DB_A" "$MIGRATIONS_DIR"

echo "=== path B: previous schema -> seeded rows -> HEAD"
recreate "$DB_B"
seeded=0
if [ -n "$(find "$PREV_DIR" -name '*.sql' 2>/dev/null)" ]; then
  apply_migrations "$DB_B" "$PREV_DIR"
  # Seeded rows exist so that the upgrade path is exercised against data rather than
  # against an empty table — a migration that rewrites a hypertable behaves differently
  # when there is something in it, and that is the case production is in.
  #
  # Seeding only makes sense once the previous schema is in place. With no previous
  # schema there is nothing to seed into, and path B degenerates into a second path A;
  # that is reported rather than papered over with an empty seed.
  if [ -f ci/db/seed.sql ]; then
    echo "    seeding"
    on "$DB_B" -f ci/db/seed.sql
    seeded=1
  else
    echo "    note: ci/db/seed.sql does not exist. The upgrade path runs against an" >&2
    echo "          empty table, which is weaker than it should be." >&2
  fi
else
  echo "    note: no previous migrations, so path B is currently a second path A." >&2
fi
apply_migrations "$DB_B" "$MIGRATIONS_DIR"

echo "=== comparing"
# TimescaleDB's own schemas are excluded rather than filtered: they hold the chunk tables,
# and the two paths legitimately have different chunks because only path B has rows in it.
# Chunks are not schema. What is being compared is the schema the application sees.
TS_EXCLUDES=(
  --exclude-schema='_timescaledb_internal'
  --exclude-schema='_timescaledb_catalog'
  --exclude-schema='_timescaledb_config'
  --exclude-schema='_timescaledb_cache'
  --exclude-schema='_timescaledb_debug'
  --exclude-schema='_timescaledb_functions'
  --exclude-schema='timescaledb_information'
  --exclude-schema='timescaledb_experimental'
)
# pg_dump emits comments, blank lines, the database name and — since PostgreSQL 17 — a
# random \restrict token per dump. None of those say anything about schema equality.
normalise() {
  sed -E \
    -e '/^--/d' \
    -e '/^$/d' \
    -e '/^\\(un)?restrict /d' \
    -e "s/\\b($DB_A|$DB_B)\\b/DBNAME/g" \
    -e "/$LEDGER/d"
}
pg_dump --schema-only --no-owner --no-privileges "${TS_EXCLUDES[@]}" \
  "$BASE_URL/$DB_A" | normalise > "$WORK/a.sql"
pg_dump --schema-only --no-owner --no-privileges "${TS_EXCLUDES[@]}" \
  "$BASE_URL/$DB_B" | normalise > "$WORK/b.sql"

status=0
if ! diff -u "$WORK/a.sql" "$WORK/b.sql"; then
  echo >&2
  echo "FAIL: a fresh install and an upgraded install do not have the same schema." >&2
  status=1
fi

# The assertion that replaces the prompt's union-view check: the continuous aggregates are
# defined identically on both paths. Comparing the catalog rather than the dump text means
# a difference in how a view was *reached* cannot hide behind identical SQL.
cagg_query="SELECT view_schema, view_name, materialized_only, compression_enabled,
                   regexp_replace(view_definition, '[[:space:]]+', ' ', 'g')
            FROM timescaledb_information.continuous_aggregates
            ORDER BY view_schema, view_name"
on "$DB_A" -At -c "$cagg_query" > "$WORK/a.caggs"
on "$DB_B" -At -c "$cagg_query" > "$WORK/b.caggs"
if ! diff -u "$WORK/a.caggs" "$WORK/b.caggs"; then
  echo "FAIL: the continuous aggregates differ between the two paths." >&2
  status=1
fi

# The schema assertions, on BOTH paths. These are the properties that a dump comparison
# cannot see because both paths are equally wrong about them: a count column that became
# numeric, a finaliser that returns NULL where the frozen contract needs 0, a level that
# started reading raw, a retention policy on the raw table. W4 owns them and
# ci/db/schema-assertions.sql explains each one. Three of the nine are there because they
# were violated.
echo "=== schema assertions, path A"
on "$DB_A" -f ci/db/schema-assertions.sql || status=1
echo "=== schema assertions, path B"
on "$DB_B" -f ci/db/schema-assertions.sql || status=1

# And that the seeded rows are still there and still readable through the aggregates. A
# migration that silently drops data passes a schema comparison. This is also where D-12,
# D-13 and D-14 stop being spike results and become assertions the build runs.
if [ "$seeded" -eq 1 ] && [ -f ci/db/verify-seed.sql ]; then
  echo "=== verifying seeded data survived"
  on "$DB_B" -f ci/db/verify-seed.sql || status=1
fi

# How strong was this run? Path B is only a real upgrade test if it applied a *delta*: a
# previous migration set that is a strict prefix of HEAD's. Saying so out loud is the
# difference between a gate that is green and a gate that is known to have tested
# something — the state this check was in before W4 landed, with one migration in the
# tree, was "green because both paths were the same path".
prev_n=$(find "$PREV_DIR" -name '*.sql' 2>/dev/null | wc -l | tr -d ' ')
head_n=$(find "$MIGRATIONS_DIR" -name '*.sql' | wc -l | tr -d ' ')
echo "=== path B applied $prev_n migration(s), then $((head_n - prev_n)) more to reach HEAD"
if [ "$prev_n" -eq 0 ]; then
  echo "    WEAK: no previous migrations, so path B was a second path A." >&2
elif [ "$prev_n" -eq "$head_n" ]; then
  echo "    WEAK: HEAD adds no migration, so both paths ran the same set." >&2
else
  echo "    path B exercised a real upgrade over $( [ "$seeded" -eq 1 ] && echo seeded || echo empty ) tables."
fi

if [ "$status" -eq 0 ]; then
  echo "=== both paths produced the same schema"
fi

admin -c "DROP DATABASE IF EXISTS $DB_A WITH (FORCE)" >/dev/null
admin -c "DROP DATABASE IF EXISTS $DB_B WITH (FORCE)" >/dev/null
exit "$status"
