#!/usr/bin/env bash
# Apply the migrations twice and prove the two results are the same schema.
#
#   path A   empty database            -> every migration at HEAD
#   path B   previous release's schema -> seed rows -> every migration at HEAD
#
# TWO databases, not one. `migrations/` is the weather schema on PostgreSQL/TimescaleDB;
# `migrations/config/` is the configuration schema on SQLite. DESIGN.md D-8 as amended by
# A-W6-2 governs both -- "schema changes solely through migrations, no DDL at startup, no
# `IF NOT EXISTS`" is about EVERY schema this system owns -- and until implementation-plan.md
# M2-2 the second one did not exist, so this gate had nothing to say about the database that
# is the actual instance of the defect D-8 was written about (docs/reconciliation.md G-2).
# Each database gets the same two paths, and the config half adds a third check the weather
# half cannot make: re-run the applier against the finished schema and assert that NOTHING
# ran. See "the configuration database" below.
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
# Requires: psql, pg_dump, sqlite3, and DATABASE_URL pointing at a server this script may
# create and drop databases on. No Rust toolchain -- which is why the config half shells out
# to sqlite3 rather than to the daemon's own `mw_store::config_db`. That module's tests drive
# the real `sqlx` migrator over the same files; this script and those tests check different
# things, and neither is the other's duplicate.
#
# Arms itself when migrations/ exists (see ci/gates.toml).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

: "${DATABASE_URL:?set DATABASE_URL to a server where databases may be created and dropped}"
MIGRATIONS_DIR="${MIGRATIONS_DIR:-migrations}"
DB_A="${DB_A:-mw_migrate_a}"
DB_B="${DB_B:-mw_migrate_b}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Set before the preflight below, which scopes part of its rule to this directory.
CONFIG_MIGRATIONS_DIR="${CONFIG_MIGRATIONS_DIR:-$MIGRATIONS_DIR/config}"

if [ ! -d "$MIGRATIONS_DIR" ]; then
  echo "no $MIGRATIONS_DIR/ — nothing to migrate. This script is armed by its existence." >&2
  exit 1
fi

missing=""
for tool in psql pg_dump sqlite3; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "missing required tool(s):$missing" >&2
  exit 1
fi

# ----------------------------------------------------------------------------------------
# D-8's third clause, checked before anything is applied because it needs no database and
# because it is the clause the other two depend on: if a migration tolerates being applied
# twice, then "a second application is a no-op" is green for the wrong reason and says
# nothing. That is exactly how the Go configuration schema came to be frozen at first-run
# shape -- `CREATE TABLE IF NOT EXISTS` in a startup path, which never errors and never
# updates.
#
# TWO scopes, because the two schemas are not in the same position.
#
#   everywhere under migrations/   `IF NOT EXISTS` and `if_not_exists`. This is the work
#                                  item's done-criterion verbatim, and both schemas satisfy
#                                  it.
#   migrations/config/ only        plus `OR REPLACE` and `IF EXISTS`. The configuration
#                                  schema is written from scratch and has no legitimate use
#                                  for either. The WEATHER schema does and always will:
#                                  `cargo xtask codegen` emits `CREATE OR REPLACE FUNCTION
#                                  mw_mean` (xtask/src/gen.rs:67) and `cargo xtask migrate`
#                                  emits `DROP MATERIALIZED VIEW IF EXISTS ... CASCADE`
#                                  (xtask/src/migrate.rs:216), so forbidding them repo-wide
#                                  would be a rule the repository's own generator cannot
#                                  satisfy. `DROP ... IF EXISTS` is not conditional-create
#                                  DDL anyway.
#
# These are the scopes `crates/mw-store/src/config_db.rs` checks too, so the shell half and
# the Rust half of one rule agree.
#
# Line comments are stripped first, so a migration that SAYS "no IF NOT EXISTS,
# deliberately" is not reported as using it. Both migration sets say exactly that, and a
# grep that could not tell prose from DDL would have to be either wrong or ignored.
#
# No pipe into `grep -q`. Under `pipefail` a matching `grep -q` exits 0 and closes the pipe,
# the writer dies of SIGPIPE, and 141 propagates as the pipeline's status -- so
# `if printf '%s' "$body" | grep -qF "$idiom"` is FALSE exactly when the idiom is present,
# on any input large enough that the writer has not already finished. That is how this check
# came to report `clean` on three migrations that do contain `OR REPLACE`. A `case` pattern
# does the same job with no pipeline and no race.
echo "=== no conditional DDL under $MIGRATIONS_DIR/ (D-8, A-W6-2)"
conditional=0
while IFS= read -r f; do
  body="$(sed -E 's/--.*$//' "$f" | tr '[:lower:]' '[:upper:]')"
  idioms=('IF NOT EXISTS' 'IF_NOT_EXISTS')
  case "$f" in
    "$CONFIG_MIGRATIONS_DIR"/*) idioms+=('OR REPLACE' 'IF EXISTS') ;;
  esac
  for idiom in "${idioms[@]}"; do
    case "$body" in
      *"$idiom"*)
        echo "  $f: \`$idiom\` in a statement" >&2
        conditional=1
        ;;
    esac
  done
done < <(find "$MIGRATIONS_DIR" -name '*.sql' | sort)
if [ "$conditional" -ne 0 ]; then
  echo >&2
  echo "FAIL: a migration that tolerates being applied twice makes the apply-twice check" >&2
  echo "      meaningless. DESIGN.md D-8, amended by A-W6-2. Write the unconditional form;" >&2
  echo "      the ledger is what stops it running a second time." >&2
  exit 1
fi
echo "    clean"

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
  # -maxdepth 1 is load-bearing. migrations/config/ is a SECOND database (D-8 as amended
  # by A-W6-2), and a recursive find would hand its SQLite DDL to psql.
  for f in $(find "$dir" -maxdepth 1 -name '*.sql' | sort); do
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
if [ -n "$(find "$PREV_DIR" -maxdepth 1 -name '*.sql' 2>/dev/null)" ]; then
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
prev_n=$(find "$PREV_DIR" -maxdepth 1 -name '*.sql' 2>/dev/null | wc -l | tr -d ' ')
head_n=$(find "$MIGRATIONS_DIR" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')
echo "=== path B applied $prev_n migration(s), then $((head_n - prev_n)) more to reach HEAD"
if [ "$prev_n" -eq 0 ]; then
  echo "    WEAK: no previous migrations, so path B was a second path A." >&2
elif [ "$prev_n" -eq "$head_n" ]; then
  echo "    WEAK: HEAD adds no migration, so both paths ran the same set." >&2
else
  echo "    path B exercised a real upgrade over $( [ "$seeded" -eq 1 ] && echo seeded || echo empty ) tables."
fi

if [ "$status" -eq 0 ]; then
  echo "=== the weather schema: both paths produced the same schema"
fi

admin -c "DROP DATABASE IF EXISTS $DB_A WITH (FORCE)" >/dev/null
admin -c "DROP DATABASE IF EXISTS $DB_B WITH (FORCE)" >/dev/null

# ========================================================================================
# The configuration database
#
# Same two paths, on SQLite. The third check below is the one this half adds, and it is the
# work item's headline done-criterion: apply HEAD, seed rows, run the applier AGAIN over the
# finished schema, and assert that no migration ran and that the rows are still there. On
# the PostgreSQL side that check would be redundant with path B; here it is the direct test
# of "a second application of every migration is a no-op", and it is the thing a startup DDL
# path would pass by being conditional rather than by being ledgered -- which is why the
# `IF NOT EXISTS` preflight above runs first.
# ========================================================================================

if [ ! -d "$CONFIG_MIGRATIONS_DIR" ]; then
  echo "=== no $CONFIG_MIGRATIONS_DIR/ — the configuration schema is not in this tree" >&2
  echo "    implementation-plan.md M2-2 creates it; this gate covers it from then on." >&2
  exit "$status"
fi

CFG_A="$WORK/config-a.db"
CFG_B="$WORK/config-b.db"
CFG_C="$WORK/config-c.db"
CFG_LEDGER="_mw_ci_applied"

# sqlite3 exits 0 on a parse error without -bail, so every invocation has it. Foreign keys
# are per-connection and OFF by default, so every invocation sets them: the references in
# migrations/config/ are decorative on a connection that forgets, and `mw_store::config_db`
# sets the same pragma for the daemon.
sq() { sqlite3 -bail -cmd 'PRAGMA foreign_keys = ON' "$@"; }

cfg_apply() {
  local db="$1" dir="$2" f base applied=0
  sq "$db" "CREATE TABLE IF NOT EXISTS $CFG_LEDGER (filename TEXT PRIMARY KEY) STRICT" >/dev/null
  for f in $(find "$dir" -maxdepth 1 -name '*.sql' | sort); do
    base="$(basename "$f")"
    if [ -n "$(sq "$db" "SELECT 1 FROM $CFG_LEDGER WHERE filename = '$base'")" ]; then
      echo "  $base (already applied)"
      continue
    fi
    echo "  $base"
    # One transaction per file, matching sqlx-migrate: a migration that fails half way
    # through leaves nothing behind and the ledger stays truthful.
    sq "$db" "BEGIN" ".read $f" "INSERT INTO $CFG_LEDGER (filename) VALUES ('$base')" "COMMIT"
    applied=$((applied + 1))
  done
  echo "$applied" > "$WORK/cfg-applied-count"
}

# Every object the schema defines, as the SQL that defines it. Newlines and tabs collapse so
# that a reformatted migration is not a schema difference; the CI ledger is excluded because
# it is this script's bookkeeping and not schema. Compared instead of `.schema` output
# because `.schema` prints in creation order, which the two paths legitimately differ on.
cfg_schema() {
  sq "$1" \
    "SELECT type, name, replace(replace(replace(COALESCE(sql,''), char(10),' '), char(13),' '), char(9),' ')
       FROM sqlite_master
      WHERE name NOT LIKE 'sqlite_%' AND name <> '$CFG_LEDGER'
      ORDER BY type, name"
}

echo "=== the configuration database, path A: empty -> HEAD"
cfg_apply "$CFG_A" "$CONFIG_MIGRATIONS_DIR"

echo "=== the configuration database, path B: previous schema -> seeded rows -> HEAD"
cfg_prev_dir="$PREV_DIR/$(basename "$CONFIG_MIGRATIONS_DIR")"
cfg_seeded=0
cfg_prev_n=0
if [ -n "$(find "$cfg_prev_dir" -maxdepth 1 -name '*.sql' 2>/dev/null)" ]; then
  cfg_apply "$CFG_B" "$cfg_prev_dir"
  cfg_prev_n=$(find "$cfg_prev_dir" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')
  if [ -f ci/db/config-seed.sql ]; then
    echo "    seeding"
    sq "$CFG_B" ".read ci/db/config-seed.sql"
    cfg_seeded=1
  else
    echo "    note: ci/db/config-seed.sql does not exist, so the upgrade path runs against" >&2
    echo "          empty tables, which is weaker than it should be." >&2
  fi
else
  echo "    note: no previous $CONFIG_MIGRATIONS_DIR/, so path B is currently a second path A." >&2
fi
cfg_apply "$CFG_B" "$CONFIG_MIGRATIONS_DIR"

echo "=== comparing"
cfg_schema "$CFG_A" > "$WORK/cfg-a.schema"
cfg_schema "$CFG_B" > "$WORK/cfg-b.schema"
if ! diff -u "$WORK/cfg-a.schema" "$WORK/cfg-b.schema"; then
  echo >&2
  echo "FAIL: a fresh install and an upgraded install do not have the same configuration" >&2
  echo "      schema." >&2
  status=1
fi

# The third check: the applier, run again over a schema that is already at HEAD.
echo "=== the configuration database: a second application"
cfg_apply "$CFG_C" "$CONFIG_MIGRATIONS_DIR"
cfg_schema "$CFG_C" > "$WORK/cfg-c.before"
# The seed is applied here as well as on path B, so these rows are exercised on every run
# rather than only once there is a release to upgrade from.
sq "$CFG_C" ".read ci/db/config-seed.sql"
cfg_rows_before="$(sq "$CFG_C" "SELECT (SELECT count(*) FROM stations) || '/' ||
                                       (SELECT count(*) FROM uploader) || '/' ||
                                       (SELECT count(*) FROM station_service) || '/' ||
                                       (SELECT count(*) FROM website) || '/' ||
                                       (SELECT count(*) FROM peers) || '/' ||
                                       (SELECT count(*) FROM component_schedule)")"
cfg_apply "$CFG_C" "$CONFIG_MIGRATIONS_DIR"
cfg_ran="$(cat "$WORK/cfg-applied-count")"
cfg_schema "$CFG_C" > "$WORK/cfg-c.after"
cfg_rows_after="$(sq "$CFG_C" "SELECT (SELECT count(*) FROM stations) || '/' ||
                                      (SELECT count(*) FROM uploader) || '/' ||
                                      (SELECT count(*) FROM station_service) || '/' ||
                                      (SELECT count(*) FROM website) || '/' ||
                                      (SELECT count(*) FROM peers) || '/' ||
                                      (SELECT count(*) FROM component_schedule)")"

if [ "$cfg_ran" -ne 0 ]; then
  echo "FAIL: the second application ran $cfg_ran migration(s). Applied-by-name means each" >&2
  echo "      file runs exactly once." >&2
  status=1
fi
if ! diff -u "$WORK/cfg-c.before" "$WORK/cfg-c.after"; then
  echo "FAIL: the second application changed the configuration schema." >&2
  status=1
fi
if [ "$cfg_rows_before" != "$cfg_rows_after" ]; then
  echo "FAIL: the seeded configuration rows did not survive. $cfg_rows_before -> $cfg_rows_after" >&2
  status=1
fi

# And the converse, which is what makes the check above worth having: the statements must NOT
# tolerate a second application. If a migration body re-applies cleanly then the no-op above
# is green because the DDL is conditional rather than because the ledger held, and the gate
# has stopped testing anything. This is the half DESIGN.md D-64's reasoning applies to a
# property rather than to a path: a check that has never been seen able to fail proves
# nothing.
#
# What it sees: `sqlite3 -bail` stops at the first error, so this proves the FIRST statement
# is unconditional and nothing about the rest. The preflight above is what covers every
# statement in the file; this covers the case the preflight cannot see, which is a statement
# that is re-appliable without naming one of the four idioms.
echo "=== the configuration database: every migration body must REFUSE to re-apply"
for f in $(find "$CONFIG_MIGRATIONS_DIR" -maxdepth 1 -name '*.sql' | sort); do
  if sq "$CFG_A" ".read '$f'" >/dev/null 2>&1; then
    echo "  FAIL: $(basename "$f") applied twice without error." >&2
    echo "        Something in it is conditional, so the ledger is no longer load-bearing." >&2
    status=1
  else
    echo "  $(basename "$f") refused, as it must"
  fi
done

echo "=== the configuration database: path B applied $cfg_prev_n migration(s) before HEAD"
cfg_head_n=$(find "$CONFIG_MIGRATIONS_DIR" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')
if [ "$cfg_prev_n" -eq 0 ]; then
  echo "    WEAK: no previous $CONFIG_MIGRATIONS_DIR/, so path B was a second path A. The" >&2
  echo "          second-application check above is the strong one on this run." >&2
elif [ "$cfg_prev_n" -eq "$cfg_head_n" ]; then
  echo "    WEAK: HEAD adds no configuration migration, so both paths ran the same set." >&2
else
  echo "    path B exercised a real upgrade over $( [ "$cfg_seeded" -eq 1 ] && echo seeded || echo empty ) tables."
fi

if [ "$status" -eq 0 ]; then
  echo "=== both schemas: both paths agree, and a second application is a no-op"
fi
exit "$status"
