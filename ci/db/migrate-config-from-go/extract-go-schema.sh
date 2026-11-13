#!/usr/bin/env bash
# Regenerate `go-config-schema.sql` from the read-only Go tree.
#
# WHY THIS FILE EXISTS AT ALL
#
# `crates/mw-store/src/config_go.rs` reads the Go daemon's own configuration database, and
# every test of it needs one to read. Hand-writing that fixture would mean transcribing
# fifty-three `devices` columns, and a transposed or forgotten column in a fixture is a test
# that passes against a schema production does not have.
#
# It is also not one schema. `pkg/config/provider_sqlite.go` passes
# `CREATE TABLE IF NOT EXISTS` to one `Exec` at every start, so a database's shape is frozen
# at its own first-run shape; and `migrations/config/` both ADDS columns that first-run DDL
# does not declare (`019_add_wll_fields` adds five to `devices`) and REMOVES columns it does
# (`013_remove_controller_configs` drops every per-service credential). So the widest real
# shape is "the first-run DDL, plus the `ADD COLUMN`s it does not already declare", and that
# is what this emits: the union a long-lived deployment actually has.
#
# The emitted file is a FIXTURE and a record, not something the daemon applies. Nothing in
# the shipped binary reads it.
#
# Usage: ci/db/migrate-config-from-go/extract-go-schema.sh <path-to-remoteweather-checkout>
set -euo pipefail

RW="${1:?usage: extract-go-schema.sh <path-to-remoteweather-checkout>}"
SRC="$RW/pkg/config/provider_sqlite.go"
MIG="$RW/migrations/config"
[ -f "$SRC" ] || { echo "not a remoteweather checkout: $SRC missing" >&2; exit 2; }
[ -d "$MIG" ] || { echo "not a remoteweather checkout: $MIG missing" >&2; exit 2; }

SQLITE="${SQLITE:-$(command -v sqlite3 || true)}"
[ -x "$SQLITE" ] || { echo "sqlite3 not found; set SQLITE=<path>" >&2; exit 2; }

OUT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/go-config-schema.sql"
COMMIT="$(git -C "$RW" rev-parse HEAD)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The first-run DDL: everything between `schema := \`` and the closing backtick. Extracted
# rather than copied, so a column added to that block in the Go tree arrives here by
# re-running this and not by somebody noticing.
awk '
  /schema := `/ { inside = 1; next }
  inside && /^`$/ { exit }
  inside { print }
' "$SRC" > "$WORK/ddl.sql"

# It carries two INSERTs of its own -- the `default` configuration row and a disabled
# snowcache controller. They are the Go daemon's own first-run state, so they belong in a
# fixture of a Go database; they are separated out only so the file reads as schema then
# data.
grep -c 'CREATE TABLE' "$WORK/ddl.sql" > /dev/null || {
  echo "the DDL block was not found in $SRC" >&2; exit 2; }

"$SQLITE" "$WORK/probe.db" < "$WORK/ddl.sql"

# Every `ADD COLUMN` in version order, kept only where BOTH hold:
#
#   * the first-run DDL does not already declare the column -- that difference is the whole
#     point, because it is exactly the set of columns a long-lived database has and a
#     freshly created one does not; and
#   * no migration later renames it away, drops it, or rebuilds its table without it. A
#     scratch column is as mechanical a fact as a kept one: `001` adds
#     `weather_websites.is_dashboard` and `003` renames it to `is_portal`, and `002` adds
#     `device_ref_id` and then rebuilds the table without it -- so neither is in any
#     migrated database, and a fixture carrying them would be testing against a shape
#     nothing has.
: > "$WORK/added.sql"

# Does any `CREATE TABLE <t>_new ( ... )` block omit column <c>? The block and not the file:
# `002` mentions `device_ref_id` in an `UPDATE` and omits it from the rebuild, which is
# precisely how a scratch column reads.
rebuilt_without() {
  local tbl="$1" col="$2" f
  for f in "$MIG"/*.up.sql; do
    awk -v tbl="$tbl" -v col="$col" '
      $0 ~ ("CREATE TABLE " tbl "_new") { inside = 1; found = 0; next }
      inside && /^\);/ { if (!found) { print "omitted" } ; inside = 0; next }
      inside && $0 ~ ("(^|[^A-Za-z0-9_])" col "([^A-Za-z0-9_]|$)") { found = 1 }
    ' "$f" | grep -q omitted && return 0
  done
  return 1
}

for f in "$MIG"/*.up.sql; do
  while IFS= read -r line; do
    tbl="$(printf '%s' "$line" | sed -n 's/.*ALTER TABLE \([A-Za-z_][A-Za-z0-9_]*\) ADD COLUMN .*/\1/p')"
    col="$(printf '%s' "$line" | sed -n 's/.*ADD COLUMN \([A-Za-z_][A-Za-z0-9_]*\).*/\1/p')"
    [ -n "$tbl" ] && [ -n "$col" ] || continue
    have="$("$SQLITE" "$WORK/probe.db" \
      "SELECT count(*) FROM pragma_table_info('$tbl') WHERE name = '$col'")"
    [ "$have" = "0" ] || continue
    if grep -hq "RENAME COLUMN $col TO" "$MIG"/*.up.sql; then
      printf -- '-- %s.%s: added by a migration and renamed away by a later one.\n' \
        "$tbl" "$col" >> "$WORK/skipped.txt"
      continue
    fi
    if grep -hq "DROP COLUMN $col" "$MIG"/*.up.sql; then
      printf -- '-- %s.%s: added by a migration and dropped by a later one.\n' \
        "$tbl" "$col" >> "$WORK/skipped.txt"
      continue
    fi
    if rebuilt_without "$tbl" "$col"; then
      printf -- '-- %s.%s: added by a migration and omitted from a later rebuild of %s.\n' \
        "$tbl" "$col" "$tbl" >> "$WORK/skipped.txt"
      continue
    fi
    printf '%s\n' "$line" >> "$WORK/added.sql"
    "$SQLITE" "$WORK/probe.db" "$line"
  done < <(grep -h '^ALTER TABLE .* ADD COLUMN ' "$f" || true)
done

{
  echo "-- GENERATED by ci/db/migrate-config-from-go/extract-go-schema.sh. DO NOT EDIT."
  echo "--"
  echo "-- The RemoteWeather configuration database's schema, as the widest shape a real"
  echo "-- deployment has: the first-run DDL of pkg/config/provider_sqlite.go, plus every"
  echo "-- \`ALTER TABLE ... ADD COLUMN\` in migrations/config/ that the first-run DDL does"
  echo "-- not already declare. See the extractor's header for why those are two different"
  echo "-- things."
  echo "--"
  echo "-- A FIXTURE AND A RECORD. Nothing in the shipped binary applies this; it is what the"
  echo "-- tests of crates/mw-store/src/config_go.rs read, and what a reviewer of that file"
  echo "-- reads to check it against."
  echo "--"
  echo "-- source_repo   github.com/chrissnell/remoteweather"
  echo "-- source_commit $COMMIT"
  echo "-- source_file   pkg/config/provider_sqlite.go, migrations/config/*.up.sql"
  echo ""
  cat "$WORK/ddl.sql"
  if [ -s "$WORK/added.sql" ]; then
    echo ""
    echo "-- The columns migrations/config/ adds that the first-run DDL does not declare."
    cat "$WORK/added.sql"
  fi
  if [ -s "$WORK/skipped.txt" ]; then
    echo ""
    echo "-- Added by a migration and gone again before HEAD, so no database has them:"
    cat "$WORK/skipped.txt"
  fi
} > "$OUT"

echo "wrote $OUT"
echo "  tables:        $(grep -c 'CREATE TABLE' "$OUT")"
echo "  added columns: $(wc -l < "$WORK/added.sql" | tr -d ' ')"
echo "  skipped:       $(wc -l < "$WORK/skipped.txt" 2>/dev/null | tr -d ' ' || echo 0)"
