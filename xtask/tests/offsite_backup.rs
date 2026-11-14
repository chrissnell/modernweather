//! `ci/db/offsite-backup.sh`, driven without a database and without R2.
//!
//! An integration test rather than a unit test because the subject is a shell script, and it
//! is testable at all because the script takes `PSQL`, `PG_DUMP` and `RCLONE` as whole
//! invocations — the same indirection `ci/db/rebuild-aggregates.sh` already has, and the same
//! reason: these scripts have to work where the database is reachable only through
//! `kubectl exec`. Here the indirection is pointed at two stubs, so the dump's *plan* can be
//! asserted with nothing running anywhere.
//!
//! What is under test is the part of M0-2 that is a decision rather than a measurement, and
//! that `ci/db/restore-rehearsal.sh` cannot check because it only ever sees a database at
//! HEAD:
//!
//!   * the four aggregate levels are never written to the archive (D-15, and A-W9-2 for why
//!     they could not be loaded back even if they were);
//!   * `observations` is sliced one object per calendar year, and only the current year is
//!     re-dumped when the history has not moved;
//!   * the legacy archive tables go through `pg_dump` WITH their schema, because
//!     `migrations/` does not create them;
//!   * a table that exists and is not declared STOPS the dump. That is the only mechanism
//!     that beats "the legacy archive is the one that gets forgotten", and it is a property
//!     no live database can demonstrate, because a live database at HEAD has no undeclared
//!     table in it.
//!   * a missing configuration database stops the dump unless it is explicitly tolerated.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .to_path_buf()
}

/// A fake `psql`. It answers by matching the query text the script sends, which keeps the
/// stub honest in one specific way: a query the script changes stops being answered, and the
/// test fails rather than quietly asserting against a stale plan.
const PSQL_STUB: &str = r#"#!/usr/bin/env bash
# args: [--set ...] [-X] [-q] [-At] -c "<query>"
q=""
while [ $# -gt 0 ]; do
  case "$1" in
    -c) q="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n--\n' "$q" >> "$MW_STUB_DIR/queries"
case "$q" in
  *"FROM pg_class c JOIN pg_namespace n"*)   cat "$MW_STUB_DIR/relations" ;;
  *"string_agg(quote_ident(attname)"*)       echo "col_a,col_b" ;;
  *"year from (min(time) AT TIME ZONE 'UTC')"*) cat "$MW_STUB_DIR/year_lo" ;;
  *"year from (max(time) AT TIME ZONE 'UTC')"*) cat "$MW_STUB_DIR/year_hi" ;;
  *"count(*) FROM observations"*)            echo 1000 ;;
  *"count(*) FROM public."*)                 echo 7 ;;
  *"COPY (SELECT"*)                          printf 'a-row\n' ;;
  *) echo "STUB-PSQL: no answer for: $q" >&2; exit 1 ;;
esac
"#;

/// A fake `rclone`. Records every object it is asked to write, and drains stdin so the
/// upstream of the pipeline never dies of SIGPIPE.
const RCLONE_STUB: &str = r#"#!/usr/bin/env bash
case "$1" in
  rcat) printf '%s\n' "$2" >> "$MW_STUB_DIR/written"
        cat > "$MW_STUB_DIR/objects/$(printf '%s' "$2" | tr '/:' '__')" ;;
  cat)  cat "$MW_STUB_DIR/objects/$(printf '%s' "$2" | tr '/:' '__')" 2>/dev/null || exit 1 ;;
  size) cat "$MW_STUB_DIR/size" 2>/dev/null || echo '{"count":1,"bytes":4242}' ;;
  delete) printf '%s\n' "$*" >> "$MW_STUB_DIR/deleted" ;;
  lsl)  : ;;
  *)    : ;;
esac
"#;

/// A fake `pg_dump`. Emits something recognisable so the archive's legacy objects can be
/// told apart from the COPY ones.
const PG_DUMP_STUB: &str = r#"#!/usr/bin/env bash
printf 'PGDMP-STUB %s\n' "$*"
"#;

struct Harness {
    dir: PathBuf,
}

impl Harness {
    /// `relations` is what the stub reports as present in `public`.
    fn new(name: &str, relations: &[&str], year_lo: &str, year_hi: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("mw-offsite-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("stub dir");
        fs::create_dir_all(dir.join("objects")).expect("objects dir");
        write_exec(&dir.join("bin/psql-stub"), PSQL_STUB);
        write_exec(&dir.join("bin/rclone-stub"), RCLONE_STUB);
        write_exec(&dir.join("bin/pgdump-stub"), PG_DUMP_STUB);
        fs::write(dir.join("relations"), relations.join("\n") + "\n").expect("relations");
        fs::write(dir.join("year_lo"), format!("{year_lo}\n")).expect("lo");
        fs::write(dir.join("year_hi"), format!("{year_hi}\n")).expect("hi");
        fs::write(dir.join("written"), "").expect("written");
        fs::write(dir.join("size"), "{\"count\":1,\"bytes\":4242}\n").expect("size");
        fs::write(dir.join("deleted"), "").expect("deleted");
        fs::write(dir.join("queries"), "").expect("queries");
        Harness { dir }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
        let root = repo_root();
        let mut cmd = Command::new("bash");
        cmd.arg(root.join("ci/db/offsite-backup.sh"))
            .args(args)
            .current_dir(&root)
            .env("MW_STUB_DIR", &self.dir)
            .env("PSQL", self.dir.join("bin/psql-stub"))
            .env("RCLONE", self.dir.join("bin/rclone-stub"))
            .env("PG_DUMP", self.dir.join("bin/pgdump-stub"))
            .env("MW_BACKUP_REMOTE", "stub:bucket/modernweather")
            .env_remove("DATABASE_URL")
            .env_remove("MW_CONFIG_DB")
            .env_remove("MW_BACKUP_ALLOW_NO_CONFIG_DB");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("bash runs the script");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Every `rclone delete` invocation, verbatim.
    fn deletes(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("deleted"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every object path `rclone rcat` was handed, in order.
    fn written(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("written"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// What `rclone size` reports for every object. `0` is how an object that is no longer
    /// in the archive looks from here.
    fn set_stored_size(&self, bytes: u64) {
        fs::write(
            self.dir.join("size"),
            format!("{{\"count\":1,\"bytes\":{bytes}}}\n"),
        )
        .expect("size");
    }

    /// Every query the script sent to `psql`, in order.
    fn queries(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("queries"))
            .unwrap_or_default()
            .split("\n--\n")
            .map(str::to_owned)
            .collect()
    }

    /// The body of an object the script wrote, so the manifest can be read rather than
    /// inferred from the paths.
    fn object_body(&self, path: &str) -> String {
        let name: String = path
            .chars()
            .map(|c| if c == '/' || c == ':' { '_' } else { c })
            .collect();
        fs::read_to_string(self.dir.join("objects").join(name)).unwrap_or_default()
    }

    /// Seeds the archive with a manifest a later run will read as "the previous run".
    fn put_object(&self, path: &str, body: &str) {
        let name: String = path
            .chars()
            .map(|c| if c == '/' || c == ':' { '_' } else { c })
            .collect();
        fs::write(self.dir.join("objects").join(name), body).expect("put object");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
    }
}

const AT_HEAD: &[&str] = &[
    "_sqlx_migrations",
    "almanac_cache",
    "forecasts",
    "observations",
    "snow_totals_cache",
    "station_metrics",
    "stations",
];

#[test]
fn the_aggregate_levels_are_never_written_to_the_archive() {
    let h = Harness::new("no-aggregates", AT_HEAD, "2024", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    for level in [
        "observations_1m",
        "observations_5m",
        "observations_1h",
        "observations_1d",
    ] {
        assert!(
            !h.written().iter().any(|p| p.contains(level)),
            "{level} was written to the archive. D-15 makes the levels disposable and A-W9-2 \
             makes them unloadable; they are rebuilt, never backed up.\nwritten: {:?}",
            h.written()
        );
        assert!(
            out.contains(&format!("{level}          skipped"))
                || out.contains(&format!("{level}           skipped")),
            "the dump did not report {level} as deliberately skipped:\n{out}"
        );
    }
}

#[test]
fn raw_is_one_object_per_calendar_year() {
    let h = Harness::new("per-year", AT_HEAD, "2024", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    let written = h.written();
    for year in ["2024", "2025", "2026"] {
        assert_eq!(
            written
                .iter()
                .filter(
                    |p| **p == format!("stub:bucket/modernweather/raw/observations-{year}.copy.gz")
                )
                .count(),
            1,
            "expected exactly one object for {year}\nwritten: {written:?}"
        );
    }
    assert!(
        !written.iter().any(|p| p.contains("observations-2023")),
        "a year outside [min, max] was dumped\nwritten: {written:?}"
    );
}

#[test]
fn an_unchanged_historical_year_is_not_re_uploaded_and_the_current_one_always_is() {
    let (last_year, this_year) = years();
    let h = Harness::new("incremental", AT_HEAD, &last_year, &this_year);
    // A previous manifest that agrees with the stub about the historical year's row count and
    // its stored size, so the only reason left to re-dump it would be the schedule.
    with_a_previous_manifest(&h, &last_year, &this_year, 4242);

    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    let written = h.written();
    assert!(
        !written
            .iter()
            .any(|p| p.contains(&format!("observations-{last_year}"))),
        "{last_year} has not moved and was re-uploaded anyway. Raw is append-only, so the \
         weekly run is supposed to cost the current year and not the whole decade.\n\
         written: {written:?}\n{out}"
    );
    assert!(
        written
            .iter()
            .any(|p| p.contains(&format!("observations-{this_year}"))),
        "the current year was not re-dumped\nwritten: {written:?}\n{out}"
    );
    assert!(
        out.contains("unchanged, and the existing object is still there"),
        "the dump did not say why it skipped a year:\n{out}"
    );
}

/// A manifest entry for a year that was **not** re-dumped has to describe the object that is
/// in the archive, not today's database.
///
/// Both halves of this were wrong. The entry was rebuilt with the *current* `observations`
/// column list, so one added metric column made `COPY observations (<new cols>) FROM STDIN`
/// fail for every historical year — the exact case an explicit column list exists to survive.
/// And it recorded the *live* row count beside the *stored* object's digest, so with
/// `MW_BACKUP_VERIFY_HISTORY=0` the manifest described an object that does not exist, which
/// the rehearsal then reports as a MISMATCH against a faithful restore.
#[test]
fn a_carried_years_manifest_entry_describes_the_stored_object() {
    let this_year = current_year();
    let last_year = (this_year.parse::<i32>().expect("a year") - 1).to_string();
    let h = Harness::new("carried", AT_HEAD, &last_year, &this_year);
    // `stored_bytes` is 4242 because that is what the `rclone size` stub answers: a year is
    // only carried if `history_object_intact` finds the object still there at the recorded
    // length, so a fixture that disagreed with the stub would be testing the re-dump path.
    //
    // The stored object was dumped when `observations` had two different columns, and it has
    // 1000 rows. The stub's live `count(*)` answers 1000 as well, so the counts are made to
    // differ by the only mechanism that carries a year without comparing them:
    // MW_BACKUP_VERIFY_HISTORY=0, with a recorded count that is deliberately not the live one.
    h.put_object(
        "stub:bucket/modernweather/manifest/latest.json",
        &format!(
            r#"{{"layout_version":1,"run":"PREVIOUS","remote":"stub:bucket/modernweather",
                 "history_verified":true,"absent":[],"not_backed_up":{{}},
                 "config_db":{{"state":"unconfigured","sha256":null,"bytes":null,"stored_bytes":null}},
                 "objects":{{"raw":{{"{last_year}":{{
                     "path":"stub:bucket/modernweather/raw/observations-{last_year}.copy.gz",
                     "rows":4242,"bytes":10,"stored_bytes":4242,"sha256":"deadbeef",
                     "columns":["time","station_id","outtemp_only_back_then"],
                     "carried_from_a_previous_run":false}}}},"tables":{{}},"pgdump":{{}}}}}}"#
        ),
    );

    let (ok, out, err) = h.run(
        &["dump"],
        &[
            ("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1"),
            ("MW_BACKUP_VERIFY_HISTORY", "0"),
        ],
    );
    assert!(ok, "the dump failed\n{out}\n{err}");

    let man = h.object_body("stub:bucket/modernweather/manifest/latest.json");
    let entry = raw_entry(&man, &last_year);
    assert!(
        entry.contains("\"outtemp_only_back_then\""),
        "the carried entry was rebuilt with today's columns. `COPY observations (<cols>) FROM \
         STDIN` would then name columns the stored object does not have, and every historical \
         year would fail to restore after one schema change.\nentry: {entry}"
    );
    assert!(
        entry.contains("\"rows\": 4242") || entry.contains("\"rows\":4242"),
        "the carried entry recorded the live row count beside the stored object's digest, so \
         the manifest describes an object that is not in the archive.\nentry: {entry}"
    );
    assert!(
        !entry.contains("\"rows\": 1000") && !entry.contains("\"rows\":1000"),
        "the live count reached the carried entry\nentry: {entry}"
    );

    // And the count it no longer records is a count it no longer pays for: that is the whole
    // purpose of MW_BACKUP_VERIFY_HISTORY=0 over a decade of history.
    let per_year_counts = h
        .queries()
        .iter()
        .filter(|q| q.contains("count(*) FROM observations") && q.contains(&last_year))
        .count();
    assert_eq!(
        per_year_counts,
        0,
        "MW_BACKUP_VERIFY_HISTORY=0 still ran a per-year count(*) for {last_year}, which is \
         the cost the flag exists to avoid\nqueries: {:?}",
        h.queries()
    );
}

/// A dump overwrites objects in place, so one that dies half way leaves new objects behind a
/// manifest that describes the old ones. The marker is what makes that detectable instead of
/// being reported as a restore defect.
#[test]
fn the_archive_records_whether_the_dump_finished() {
    let h = Harness::new("state", AT_HEAD, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");

    let written = h.written();
    let state = "stub:bucket/modernweather/manifest/state.json";
    let first_state = written.iter().position(|p| p == state);
    let first_object = written
        .iter()
        .position(|p| p.contains("/raw/observations-"));
    let latest = written
        .iter()
        .position(|p| p.ends_with("manifest/latest.json"));
    let last_state = written.iter().rposition(|p| p == state);

    assert!(
        matches!((first_state, first_object), (Some(s), Some(o)) if s < o),
        "the marker has to be written before the first object, or an interrupted dump leaves \
         nothing to tell it apart from a complete one\nwritten: {written:?}"
    );
    assert!(
        matches!((latest, last_state), (Some(l), Some(s)) if l < s),
        "the marker has to be rewritten after latest.json, so `complete` means the manifest \
         and the objects are from the same run\nwritten: {written:?}"
    );
    assert!(
        h.object_body(state).contains("\"status\": \"complete\"")
            || h.object_body(state).contains("\"status\":\"complete\""),
        "the finished dump did not mark the archive complete: {:?}",
        h.object_body(state)
    );
}

/// The digest is byte-exact, so the COPY that produces it needs a **total** order.
///
/// `ORDER BY 1` is not one for `station_metrics`, `almanac_cache` or `forecasts` — they all
/// have duplicate first columns — and the tie order differs between the statistics-rich
/// source and the freshly restored copy, which turns reconciliation into a coin toss.
#[test]
fn a_tables_copy_is_ordered_by_every_column() {
    let h = Harness::new("order", AT_HEAD, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");

    let queries = h.queries();
    let copies: Vec<&String> = queries
        .iter()
        .filter(|q| q.contains("COPY (SELECT") && q.contains("FROM public."))
        .collect();
    assert!(
        !copies.is_empty(),
        "no table was copied\nqueries: {queries:?}"
    );
    for q in copies {
        assert!(
            q.contains("ORDER BY col_a,col_b"),
            "a table's COPY is not ordered by every column, so its digest depends on the \
             plan's tie order:\n{q}"
        );
        assert!(
            !q.contains("ORDER BY 1)"),
            "a table's COPY still orders by the first column alone:\n{q}"
        );
    }
}

/// `date -u +%Y`, as the script sees it.
fn current_year() -> String {
    let out = Command::new("date")
        .args(["-u", "+%Y"])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The raw entry for one year, pulled out of a manifest body without a JSON dependency.
fn raw_entry(manifest: &str, year: &str) -> String {
    let flat: String = manifest.split_whitespace().collect::<Vec<_>>().join(" ");
    let key = format!("\"{year}\":");
    let at = flat
        .find(&key)
        .unwrap_or_else(|| panic!("no raw entry for {year} in {flat}"));
    let rest = &flat[at + key.len()..];
    let end = rest.find('}').map(|e| e + 1).unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[test]
fn the_legacy_archive_tables_are_dumped_with_their_schema() {
    let mut present: Vec<&str> = AT_HEAD.to_vec();
    present.push("observations_1d_legacy");
    present.push("observations_1h_legacy");
    let h = Harness::new("legacy", &present, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    for t in ["observations_1d_legacy", "observations_1h_legacy"] {
        assert!(
            h.written()
                .iter()
                .any(|p| *p == format!("stub:bucket/modernweather/legacy/{t}.pgdump")),
            "{t} was not dumped as a pg_dump archive. migrations/ does not create these \
             tables, so an archive without their schema has nowhere to put the only copy of \
             the ~294 rollup-only days.\nwritten: {:?}",
            h.written()
        );
    }
}

#[test]
fn a_table_nobody_declared_stops_the_dump() {
    let mut present: Vec<&str> = AT_HEAD.to_vec();
    present.push("observations_1s");
    let h = Harness::new("undeclared", &present, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(
        !ok,
        "a table nobody declared did not stop the dump. That check is the whole mechanism \
         against a future migration adding a table the backup silently omits.\n{out}\n{err}"
    );
    assert!(
        err.contains("observations_1s") && err.contains("RELATIONS does not name it"),
        "the failure did not name the undeclared table:\n{err}"
    );
    assert!(
        h.written().is_empty(),
        "the dump wrote objects before checking the inventory, so a partial archive with a \
         missing table exists.\nwritten: {:?}",
        h.written()
    );
}

#[test]
fn a_missing_configuration_database_stops_the_dump_unless_it_is_tolerated() {
    let h = Harness::new("no-config", AT_HEAD, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_CONFIG_DB", "/nonexistent/config.db")]);
    assert!(
        !ok,
        "a missing configuration database did not stop the dump. It is tiny and \
         irreplaceable (docs/reconciliation.md 7.5).\n{out}\n{err}"
    );
    assert!(
        err.contains("no configuration database to back up"),
        "the failure did not say what was missing:\n{err}"
    );

    let h = Harness::new("no-config-ok", AT_HEAD, "2026", "2026");
    let (ok, out, err) = h.run(
        &["dump"],
        &[
            ("MW_CONFIG_DB", "/nonexistent/config.db"),
            ("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1"),
        ],
    );
    assert!(ok, "the explicit tolerance did not work\n{out}\n{err}");
    assert!(
        err.contains("no configuration database in this archive"),
        "a tolerated absence was silent. It has to be loud: the flag belongs in the CronJob \
         only until the daemon owns a config.db.\n{err}"
    );
}

#[test]
fn plan_touches_nothing() {
    let h = Harness::new("plan", AT_HEAD, "2025", "2026");
    let (ok, out, err) = h.run(&["plan"], &[]);
    assert!(ok, "plan failed\n{out}\n{err}");
    assert!(
        h.written().is_empty(),
        "plan wrote to the archive\nwritten: {:?}",
        h.written()
    );
    // Every declared relation is accounted for with a mode, present or not. `plan` is how
    // the closed inventory is read, so an inventory it does not print is an inventory
    // nobody checks.
    for r in [
        "observations",
        "observations_1d_legacy",
        "observations_1h_legacy",
        "stations",
        "station_metrics",
        "almanac_cache",
        "snow_totals_cache",
        "forecasts",
        "_sqlx_migrations",
    ] {
        assert!(out.contains(r), "plan did not mention {r}:\n{out}");
    }
    assert!(
        out.contains("ABSENT"),
        "plan did not mark the absent tables:\n{out}"
    );
}

/// Seeds a previous manifest that agrees with the stub about both years, so the only reason
/// left to re-dump a historical year is the state of the archive itself.
fn with_a_previous_manifest(h: &Harness, last_year: &str, this_year: &str, stored: u64) {
    let entries: Vec<String> = [last_year, this_year]
        .iter()
        .map(|y| {
            format!(
                r#""{y}": {{"path":"stub:bucket/modernweather/raw/observations-{y}.copy.gz",
                           "rows":1000,"bytes":10,"stored_bytes":{stored},"sha256":"deadbeef",
                           "columns":["col_a","col_b"],"carried_from_a_previous_run":false}}"#
            )
        })
        .collect();
    h.put_object(
        "stub:bucket/modernweather/manifest/latest.json",
        &format!(
            r#"{{"layout_version":1,"run":"PREVIOUS","remote":"stub:bucket/modernweather",
                 "history_verified":true,"absent":[],"not_backed_up":{{}},
                 "config_db":{{"state":"unconfigured","sha256":null,"bytes":null,"stored_bytes":null}},
                 "objects":{{"raw":{{{}}},"tables":{{}},"pgdump":{{}}}}}}"#,
            entries.join(",")
        ),
    );
}

fn years() -> (String, String) {
    let out = Command::new("date")
        .args(["-u", "+%Y"])
        .output()
        .expect("date");
    let this_year = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let last_year = (this_year.parse::<i32>().expect("a year") - 1).to_string();
    (last_year, this_year)
}

/// The check that stops the per-year design from degrading into one year of backup.
///
/// Skipping an unchanged year trusts the previous manifest, and a manifest describes what was
/// written rather than what survived. The row counts agree either way — they are counts of
/// rows in the *database* — so nothing else in the dump can see an object that has been
/// removed out of band. This is not hypothetical: the bucket this archive originally shared
/// with the Vault raft snapshots was found holding zero objects, every snapshot that job had
/// ever written gone, with the job itself failing unnoticed since 2026-09-09.
#[test]
fn a_historical_year_missing_from_the_archive_is_re_dumped() {
    let (last_year, this_year) = years();
    let h = Harness::new("vanished", AT_HEAD, &last_year, &this_year);
    with_a_previous_manifest(&h, &last_year, &this_year, 4242);
    // Nothing is there any more, however confidently the manifest describes it.
    h.set_stored_size(0);

    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    assert!(
        h.written()
            .iter()
            .any(|p| p.contains(&format!("observations-{last_year}"))),
        "{last_year} is gone from the archive and the dump skipped it anyway, because the row \
         count still agreed. That is how a decade of history becomes one year without anything \
         going red.\nwritten: {:?}\n{out}\n{err}",
        h.written()
    );
    assert!(
        err.contains("NOT IN THE ARCHIVE") && err.contains(&last_year),
        "the dump re-dumped the year without saying why. A missing object is worth a loud \
         line: a lifecycle rule on the bucket would take the other years next week.\n{err}"
    );
}

/// The row-count check is skippable because it is a sequential scan per year. The
/// object-presence check is one HEAD, and is not.
#[test]
fn verify_history_off_still_checks_that_the_objects_are_there() {
    let (last_year, this_year) = years();
    let h = Harness::new("unverified-vanished", AT_HEAD, &last_year, &this_year);
    with_a_previous_manifest(&h, &last_year, &this_year, 4242);
    h.set_stored_size(0);

    let (ok, out, err) = h.run(
        &["dump"],
        &[
            ("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1"),
            ("MW_BACKUP_VERIFY_HISTORY", "0"),
        ],
    );
    assert!(ok, "the dump failed\n{out}\n{err}");
    assert!(
        err.contains("NOT IN THE ARCHIVE"),
        "MW_BACKUP_VERIFY_HISTORY=0 turned off the object-presence check as well as the row \
         count. It must only turn off the row count — the cheap check is the one that catches \
         history disappearing.\n{err}"
    );
}

/// An object of the wrong length is not the object the recorded digest was computed over.
#[test]
fn a_historical_year_whose_size_disagrees_with_the_manifest_is_re_dumped() {
    let (last_year, this_year) = years();
    let h = Harness::new("wrong-size", AT_HEAD, &last_year, &this_year);
    with_a_previous_manifest(&h, &last_year, &this_year, 4242);
    h.set_stored_size(999);

    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");
    assert!(
        h.written()
            .iter()
            .any(|p| p.contains(&format!("observations-{last_year}"))),
        "the object is 999 bytes and the manifest recorded 4242, and the year was skipped \
         anyway\nwritten: {:?}\n{out}",
        h.written()
    );
    assert!(
        err.contains("999") && err.contains("4242"),
        "the mismatch was not reported with both lengths:\n{err}"
    );
}

/// The retention rule, and the two things it must never be able to reach.
///
/// `delete` is the one verb in this script that destroys data, so its scope is asserted
/// rather than reviewed. A historical year's object is written once and never rewritten, so
/// anything age-based that can see it will eventually delete it — which is why the prune is
/// pinned to the manifest prefix AND excludes `latest.json` by name, and why both are checked
/// here. One of the two would be enough to be correct and neither alone is enough to be
/// obviously correct.
#[test]
fn the_prune_is_scoped_to_timestamped_manifests_and_nothing_else() {
    let h = Harness::new("prune-scope", AT_HEAD, "2020", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(ok, "the dump failed\n{out}\n{err}");

    let deletes = h.deletes();
    assert_eq!(
        deletes.len(),
        1,
        "expected exactly one delete — the manifest prune\ndeletes: {deletes:?}"
    );
    let d = &deletes[0];
    assert!(
        d.contains("stub:bucket/modernweather/manifest"),
        "the prune is not scoped to the manifest prefix, so it can see the raw objects: {d}"
    );
    for forbidden in ["/raw", "/tables", "/legacy", "/config"] {
        assert!(
            !d.contains(forbidden),
            "the prune's path can reach {forbidden}: {d}"
        );
    }
    assert!(
        d.contains("--exclude") && d.contains("latest.json"),
        "the prune does not exclude latest.json, which is the entry point `restore` reads — \
         an archive that loses it is unreadable: {d}"
    );
    assert!(
        d.contains("--min-age 90d"),
        "the prune did not default to 90 days: {d}"
    );
}

/// Zero means "keep them all", and it has to mean that without issuing a delete at all.
#[test]
fn manifest_retention_can_be_switched_off_entirely() {
    let h = Harness::new("prune-off", AT_HEAD, "2026", "2026");
    let (ok, out, err) = h.run(
        &["dump"],
        &[
            ("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1"),
            ("MW_BACKUP_MANIFEST_RETENTION_DAYS", "0"),
        ],
    );
    assert!(ok, "the dump failed\n{out}\n{err}");
    assert!(
        h.deletes().is_empty(),
        "retention is off and a delete was issued anyway: {:?}",
        h.deletes()
    );
    assert!(
        out.contains("manifest retention off"),
        "switching retention off was silent:\n{out}"
    );
}

/// A non-numeric retention is a typo in a CronJob, and a typo that silently becomes "off"
/// is a retention rule nobody notices is absent.
#[test]
fn a_nonsense_retention_value_stops_the_dump() {
    let h = Harness::new("prune-bad", AT_HEAD, "2026", "2026");
    let (ok, _out, err) = h.run(
        &["dump"],
        &[
            ("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1"),
            ("MW_BACKUP_MANIFEST_RETENTION_DAYS", "90 days"),
        ],
    );
    assert!(!ok, "a nonsense retention value was accepted\n{err}");
    assert!(
        err.contains("whole number of days"),
        "the failure did not say what was wrong:\n{err}"
    );
}

/// The three tables implementation-plan.md M6-3 added are declared, and declared as data
/// rather than as something a refresh rebuilds.
///
/// `snow_depth_estimates` is the one that matters: each estimate carries the rate limit
/// forward from the one before it, so it is recursive over its own output and nothing in the
/// database can rebuild it from raw. An archive without it loses the series `/snow`'s numbers
/// are computed from.
#[test]
fn the_tables_m6_3_added_are_backed_up() {
    let mut present: Vec<&str> = AT_HEAD.to_vec();
    present.extend(["alerts", "snow_depth_estimates", "snow_events"]);
    let h = Harness::new("m6-3", &present, "2026", "2026");
    let (ok, out, err) = h.run(&["dump"], &[("MW_BACKUP_ALLOW_NO_CONFIG_DB", "1")]);
    assert!(
        ok,
        "the dump failed, so M6-3's tables are still undeclared\n{out}\n{err}"
    );
    for t in ["alerts", "snow_depth_estimates", "snow_events"] {
        assert!(
            h.written()
                .iter()
                .any(|p| *p == format!("stub:bucket/modernweather/tables/{t}.copy.gz")),
            "{t} was not written to the archive\nwritten: {:?}",
            h.written()
        );
    }
}
