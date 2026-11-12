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

use std::collections::BTreeMap;
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
case "$q" in
  *"FROM pg_class c JOIN pg_namespace n"*)   cat "$MW_STUB_DIR/relations" ;;
  *"string_agg(quote_ident(attname)"*)       echo "col_a,col_b" ;;
  *"extract(year from min(time))"*)          cat "$MW_STUB_DIR/year_lo" ;;
  *"extract(year from max(time))"*)          cat "$MW_STUB_DIR/year_hi" ;;
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
  rcat) printf '%s\n' "$2" >> "$MW_STUB_DIR/written"; cat >/dev/null ;;
  cat)  cat "$MW_STUB_DIR/objects/$(printf '%s' "$2" | tr '/:' '__')" 2>/dev/null || exit 1 ;;
  size) echo '{"count":1,"bytes":4242}' ;;
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

    /// Every object path `rclone rcat` was handed, in order.
    fn written(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("written"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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
    let this_year = {
        let out = Command::new("date")
            .args(["-u", "+%Y"])
            .output()
            .expect("date");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let last_year = (this_year.parse::<i32>().expect("a year") - 1).to_string();
    let h = Harness::new("incremental", AT_HEAD, &last_year, &this_year);
    // A previous manifest that agrees with the stub's row count for the historical year, so
    // the only reason to re-dump it would be the schedule rather than the data.
    let mut raw = BTreeMap::new();
    for y in [&last_year, &this_year] {
        raw.insert(
            y.clone(),
            format!(
                r#"{{"path":"stub:bucket/modernweather/raw/observations-{y}.copy.gz",
                     "rows":1000,"bytes":10,"stored_bytes":10,"sha256":"deadbeef",
                     "columns":["col_a","col_b"],"carried_from_a_previous_run":false}}"#
            ),
        );
    }
    let objects: Vec<String> = raw.iter().map(|(y, e)| format!("\"{y}\": {e}")).collect();
    h.put_object(
        "stub:bucket/modernweather/manifest/latest.json",
        &format!(
            r#"{{"layout_version":1,"run":"PREVIOUS","remote":"stub:bucket/modernweather",
                 "history_verified":true,"absent":[],"not_backed_up":{{}},
                 "config_db":{{"state":"unconfigured","sha256":null,"bytes":null,"stored_bytes":null}},
                 "objects":{{"raw":{{{}}},"tables":{{}},"pgdump":{{}}}}}}"#,
            objects.join(",")
        ),
    );

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
        out.contains("unchanged, the existing object stands"),
        "the dump did not say why it skipped a year:\n{out}"
    );
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
