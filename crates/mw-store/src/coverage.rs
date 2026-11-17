//! **The check nobody was running**: a configured station key against the history the
//! weather database already holds under it.
//!
//! # The failure this exists for, which has now happened twice
//!
//! A station key is the domain identity in both directions — it is `config.stations.
//! station_key` and it is `observations.station_id`, and nothing relates the two except
//! equality. So a configuration whose key differs from the key its own history is under
//! produces a station that is *working*: it collects, it stores, `/latest` answers, and
//! every chart and the whole almanac start at the moment the key was typed. There is no
//! error anywhere, because nothing is wrong with either half on its own.
//!
//! | | what was configured | where the history was |
//! |---|---|---|
//! | 2026-11-13 ([`crate::registry`], docs/data-migration.md §2c) | `csi` | `CSI` |
//! | 2026-11-16 (the AirGradient) | `airgradient` | `suncrest-air-quality` |
//!
//! The first was caught because the migrated ids could not be station keys *at all*, so
//! the live station minted a second one beside them and somebody noticed a one-bucket
//! chart. The second could not be caught that way: `airgradient` is a perfectly legal key,
//! the station ran correctly under it for three hours, and the only symptom was a year of
//! air-quality history that nothing addressed any more. Two occurrences with one shape is
//! a pattern, and the shape is **detectable** — which is what this module is.
//!
//! # Both directions, because each one is a different sentence
//!
//! [`CoverageProblem::NoHistory`] is "this station's past is not where it is looking", and
//! [`CoverageProblem::NoStation`] is "this history has no future". They are not two
//! renderings of one fact: `snow` is the second without being the first (its gauge is not
//! configured on this node and its readings stop in October), and a station genuinely
//! configured for the first time is the first without being the second. Reporting only the
//! pairing would have said nothing about either.
//!
//! The one sentence worth the whole module is the pair *together*: when a station has no
//! history **and** there are keys with history nobody writes under, those keys are named in
//! the message. That is the line that turns "your chart is short" into "your readings are
//! under `suncrest-air-quality`".
//!
//! # `stations`, not `SELECT DISTINCT station_id`
//!
//! The recorded half is read from the **`stations` projection** (**D-56**), which is one row
//! per station and therefore one page, against a `DISTINCT` over seventeen million rows that
//! no index serves. That is a cheap substitution only because [`crate::writer`] upserts the
//! projection in the **same transaction** as the readings and before them — its module
//! documentation states the consequence outright: *"there is no window in which
//! `observations` has rows for a station `stations` does not have"*. So the projection is
//! not an approximation of the distinct keys; it is the same set, maintained.
//!
//! It also carries `first_seen` and `last_seen`, which `DISTINCT` does not, and those are
//! what make [`CoverageProblem::NoStation`] worth reading: *"ends 2026-10-02"* is a
//! different problem from *"ends four seconds ago"*.
//!
//! # Why `w9.station_map` is not consulted here, although it is the third input
//!
//! It is the migration's own name-to-key mapping and it is the record of what the import
//! decided, so an operator comparing three things by hand should read it. It is also
//! **scaffolding**: `w9` is a schema the migration left on the target and cutover removes
//! (docs/data-migration.md's closing section). A product check that read it would fail, or
//! silently stop checking, on the first database that never had a migration — which is
//! every database this daemon creates from `migrations/` alone. The two durable inputs are
//! the configuration and the projection, and they are sufficient: `w9.station_map` can only
//! ever explain a disagreement the two of them already found.
//!
//! # A warning, not a refusal
//!
//! Contrast [`crate::registry::ZoneProblem`], which `crates/modernweather/src/boot.rs`
//! turns into a startup refusal. A zone that does not parse is a configuration error with a
//! wrong answer behind it — a rainfall day rolling over in UTC. This is not that: both
//! halves are internally consistent, the daemon is collecting correctly, and the operator
//! is the only one who can say which key is the right one. A daemon that refused to start
//! over it would be unstartable in exactly the window somebody was trying to fix it in,
//! which is the shape **D-27** rejects for an unreachable database and rejects here for the
//! same reason.

use chrono::{DateTime, Utc};
use mw_types::StationKey;

/// How many orphaned keys [`CoverageProblem::NoHistory`] names before it starts counting.
///
/// Not a style choice: every one of these lines carries the same list, so an uncapped list
/// multiplies by the number of stations reported. The rest are not lost -- the other
/// direction reports each orphan in its own line, with its extent.
const ORPHANS_SHOWN: usize = 5;

/// A station this node has a configuration row for.
///
/// Built from `config.stations` rather than from the published [`crate::RegistrySnapshot`]:
/// the snapshot is keyed for *resolution* — by claim, by receiver, by presented key — and
/// enumerating it would be reading an index backwards. The three fields here are the three
/// the two messages need and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    /// `config.stations.station_key`, which is what the writer stores as `station_id`.
    pub station: StationKey,
    /// `config.stations.name`, because that is what the operator typed and what
    /// `/stationinfo` shows; the key alone is not what they will recognise.
    pub name: String,
    /// `config.stations.enabled`.
    ///
    /// A disabled station still **claims** its key — it is configuration, and deleting its
    /// history would be wrong — so it silences [`CoverageProblem::NoStation`] for that key.
    /// It does not raise [`CoverageProblem::NoHistory`], because a station that is switched
    /// off and has no readings yet is not a mismatch; it is a station that is switched off.
    pub collecting: bool,
}

/// A station the weather database holds history for, as the `stations` projection has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// `stations.station_id`.
    pub station: String,
    /// `stations.first_seen`, which the schema declares `NOT NULL DEFAULT now()`.
    ///
    /// Not an `Option`, because the column cannot be one: a projection row exists only
    /// because the writer upserted it beside a batch of readings, and that upsert supplies
    /// the instant. Modelling it as absent would mean a `None` arm no database can reach
    /// and a test pinning a sentence nothing can produce.
    pub first_seen: DateTime<Utc>,
    /// `stations.last_seen`, which **is** nullable.
    ///
    /// The asymmetry is the schema's (`migrations/20261029234500_w4_...`): `first_seen`
    /// has a default and this does not, so a row written by anything other than the
    /// writer's upsert -- `w9.populate_stations()` during the migration, for one -- has a
    /// beginning and no end.
    pub last_seen: Option<DateTime<Utc>>,
}

/// A configured key and a recorded key that should have been the same one.
///
/// `thiserror` for the `Display` alone — neither variant is ever returned as an error, and
/// that is the point of the module's closing note. The derive is what keeps the sentence
/// beside the fields it is built from instead of in a `warn!` somewhere else.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoverageProblem {
    /// A station is collecting under a key the weather database has no readings for.
    #[error(
        "station `{station}` ({name}) is collecting and the weather database holds no \
         readings under that key, so its charts and its almanac begin at the moment the \
         key was configured. {whence} A station configured for the first time reports \
         this until its first reading lands, and then stops."
    )]
    NoHistory {
        /// The configured key.
        station: StationKey,
        /// `config.stations.name`, as the operator typed it.
        name: String,
        /// The clause naming the keys that do have history, pre-rendered.
        whence: String,
    },
    /// The weather database holds history under a key no configured station writes to.
    #[error(
        "`{station}` has history in the weather database ({span}) and no station on this \
         node is configured with that key, so nothing extends it and the frozen surface \
         cannot be asked for it -- `/span` and `/latest` address a station by its \
         configured name. Either a station should carry this key, or this is history from \
         a device that has been retired."
    )]
    NoStation {
        /// The recorded `station_id`.
        station: String,
        /// The extent of what is there, pre-rendered.
        span: String,
    },
    /// A collecting station and an orphaned key that look like one series, split in two.
    ///
    /// **The variant the 2026-11-16 defect actually needed.** [`CoverageProblem::NoHistory`]
    /// stops firing the moment the first reading lands under the wrong key -- which on that
    /// day was three seconds after the save, and hours before anybody looked. What survives
    /// is the shape: a station whose history *begins* where an unclaimed key's history
    /// *ends*.
    #[error(
        "station `{station}` ({name}) has been collecting since {began}, and `{orphan}` \
         has history that ends at {ended} and that no configured station carries. One \
         series under two keys looks exactly like this -- a station key is \
         `observations.station_id`, so re-keying a station leaves its past addressed \
         by the old key and nothing pointing at it. If they are the same device the \
         two keys have to become one; if they are not, `{orphan}` is a retired device."
    )]
    Split {
        /// The configured key.
        station: StationKey,
        /// `config.stations.name`, as the operator typed it.
        name: String,
        /// When the configured key's own history begins.
        began: DateTime<Utc>,
        /// The unclaimed key whose history ends just before that.
        orphan: String,
        /// When the unclaimed key's history ends.
        ended: DateTime<Utc>,
    },
}

impl CoverageProblem {
    /// The station key this problem is about, whichever side it came from.
    ///
    /// **Not a convenience.** `mw_obs::dedupe` keys repetition control on the callsite,
    /// the level and the `station` field, and the whole-pipeline reach of that layer means
    /// a caller that does not record `station` gives every one of these lines the same key
    /// -- so a deployment with a split and three other stations emits the policy's burst of
    /// three and silently drops the rest, including the one that matters. Measured: against
    /// a database with twenty-one problems in it, three lines reached the log.
    ///
    /// `mw_obs::dedupe`'s own documentation states the rule this satisfies: *"`station` is
    /// in the key so one failing device does not suppress another's identical error"*.
    pub fn station(&self) -> &str {
        match self {
            Self::NoHistory { station, .. } | Self::Split { station, .. } => station.as_str(),
            Self::NoStation { station, .. } => station.as_str(),
        }
    }

    /// The clause that names where the history actually is.
    ///
    /// **This is the useful half of `NoHistory`**, and it is built from
    /// the other direction's findings rather than from a second query: a key with history
    /// nobody writes under, in a database that also has a station with no history, is very
    /// likely the same station under its previous name. Likely, not certain — so the
    /// sentence offers the candidates and does not assert the pairing.
    fn whence(orphans: &[&str]) -> String {
        match orphans {
            [] => "No other key in this database has history nobody writes to, so this is \
                   a station with no past rather than a station looking in the wrong place."
                .to_owned(),
            _ => {
                // Capped, and measured rather than guessed: run against a database a test
                // suite had left nineteen keys in, the uncapped list was most of a screen
                // and was repeated verbatim for every station reported. A log line nobody
                // finishes reading is the same as no log line. Five is enough to contain
                // the answer in the case this exists for -- one station, one orphan -- and
                // the count keeps the sentence honest about what it is not showing.
                let shown = orphans.len().min(ORPHANS_SHOWN);
                let more = match orphans.len() - shown {
                    0 => String::new(),
                    n => format!(" (and {n} more)"),
                };
                format!(
                    "These keys DO have history that no configured station writes under, \
                     and one of them may be this station under an earlier name: {}{more}.",
                    orphans[..shown].join(", ")
                )
            }
        }
    }

    /// The extent of a recorded station.
    ///
    /// `last_seen` is nullable where `first_seen` is not, so the open-ended case is the one
    /// that needs words: *"has history (2025-10-30 .. )"* reads as a rendering bug, and a
    /// row written by `w9.populate_stations()` during the migration genuinely has a
    /// beginning and no end until the writer touches it.
    fn span(first: DateTime<Utc>, last: Option<DateTime<Utc>>) -> String {
        match last {
            Some(l) => format!("{first} .. {l}"),
            None => format!("from {first}, with no last reading recorded"),
        }
    }
}

/// How far apart an orphan's last reading and a station's first may be and still be called
/// one series.
///
/// The two occurrences this module exists for measured **3 h 31 m** (the AirGradient: the
/// migration's import ended at 19:53 and the station's first live reading was at 23:24) and
/// very nearly zero (the 2026-11-13 re-key, where the live station started as the migrated
/// history ended). A week is generous against both and is the figure that keeps the one
/// standing orphan on this deployment out of it: `snow` ends 2026-10-02 and no station is
/// configured within a week either side of that.
///
/// A bound rather than none at all, because the alternative pairs every station added from
/// now on with every device ever retired -- which is a sentence that is wrong more often
/// than it is right, said on every boot.
const HANDOVER: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// Compare the two sets and say what does not line up.
///
/// Pure, total, and the whole of the logic: [`recorded`] is the only part that needs a
/// database, which is what lets the table of cases below be unit tests rather than a
/// fixture. An empty result is the ordinary answer and the one a correct deployment gives.
///
/// Ordered — configured-side problems first, then recorded-side, each alphabetically by
/// key — because the caller writes one log line per problem and an unordered `Vec` would
/// reorder a boot log between restarts for no reason.
pub fn problems(configured: &[Configured], recorded: &[Recorded]) -> Vec<CoverageProblem> {
    let history = |key: &str| recorded.iter().find(|r| r.station == key);
    // Every configured key claims its own, enabled or not: a station switched off through
    // the admin UI has not given up its history.
    let claimed = |key: &str| configured.iter().any(|c| c.station.as_str() == key);

    // The orphans, computed once. They are the other direction's answer, the candidate list
    // every `NoHistory` message carries, and the right-hand side of every `Split`.
    let mut orphans: Vec<&Recorded> = recorded.iter().filter(|r| !claimed(&r.station)).collect();
    orphans.sort_unstable_by(|a, b| a.station.cmp(&b.station));
    let orphan_keys: Vec<&str> = orphans.iter().map(|r| r.station.as_str()).collect();

    let mut out = Vec::new();
    let mut collecting: Vec<&Configured> = configured.iter().filter(|c| c.collecting).collect();
    collecting.sort_unstable_by(|a, b| a.station.as_str().cmp(b.station.as_str()));
    for c in collecting {
        match history(c.station.as_str()) {
            // No readings at all under this key.
            None => out.push(CoverageProblem::NoHistory {
                station: c.station.clone(),
                name: c.name.clone(),
                whence: CoverageProblem::whence(&orphan_keys),
            }),
            // Readings under this key, and an unclaimed key whose own readings stop just
            // before they start. **This is the arm that matters**: the station is working,
            // `NoHistory` has gone quiet, and the pairing is still visible in the instants.
            //
            // The nearest qualifying orphan rather than all of them -- the shape being
            // reported is a handover, and a handover has one predecessor. A second line
            // naming a second candidate would be a guess competing with a measurement.
            Some(mine) => {
                let nearest = orphans
                    .iter()
                    .filter_map(|o| o.last_seen.map(|ended| (o, ended)))
                    .filter(|(_, ended)| {
                        *ended <= mine.first_seen && mine.first_seen - *ended <= HANDOVER
                    })
                    .max_by_key(|(_, ended)| *ended);
                if let Some((o, ended)) = nearest {
                    out.push(CoverageProblem::Split {
                        station: c.station.clone(),
                        name: c.name.clone(),
                        began: mine.first_seen,
                        orphan: o.station.clone(),
                        ended,
                    });
                }
            }
        }
    }

    for r in orphans {
        out.push(CoverageProblem::NoStation {
            station: r.station.clone(),
            span: CoverageProblem::span(r.first_seen, r.last_seen),
        });
    }
    out
}

/// Read the recorded half from the `stations` projection.
///
/// One row per station, so this is a sequential scan of a table that holds single digits of
/// rows — which is the entire reason the projection is read instead of `observations`. It
/// runs on `db:postgres`' own first connection, beside the version assertion, and nowhere
/// on an ingest path.
pub async fn recorded(conn: &mut sqlx::PgConnection) -> Result<Vec<Recorded>, sqlx::Error> {
    // `FromRow` rather than the three-tuple the query would otherwise decode as: the tuple
    // is `clippy::type_complexity` with no name on any of its three positions, and the
    // derive binds each field to the column it is named after -- so a column renamed in a
    // migration is a compile-time rename here and not a silently reordered pair of
    // instants.
    #[derive(sqlx::FromRow)]
    struct Row {
        station_id: String,
        first_seen: DateTime<Utc>,
        last_seen: Option<DateTime<Utc>>,
    }

    // Ordered in SQL rather than in `problems`, so that a caller reading this list for any
    // other reason gets the same order the log lines come out in.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT station_id, first_seen, last_seen FROM stations ORDER BY station_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Recorded {
            station: r.station_id,
            first_seen: r.first_seen,
            last_seen: r.last_seen,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> StationKey {
        StationKey::new(k)
    }

    fn cfg(k: &str, collecting: bool) -> Configured {
        Configured {
            station: key(k),
            name: k.to_ascii_uppercase(),
            collecting,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    /// A station recorded over a long span ending at `end`.
    fn rec_until(k: &str, end: i64) -> Recorded {
        Recorded {
            station: k.to_owned(),
            first_seen: at(end - 31_536_000),
            last_seen: Some(at(end)),
        }
    }

    /// A station recorded from `start` to a little after it -- a young series.
    fn rec_from(k: &str, start: i64) -> Recorded {
        Recorded {
            station: k.to_owned(),
            first_seen: at(start),
            last_seen: Some(at(start + 3_600)),
        }
    }

    /// 2026-11-16 19:53 UTC, where the AirGradient's imported history ends.
    const IMPORT_END: i64 = 1_794_858_782;
    /// 2026-11-16 23:24 UTC, where it started writing under the other key.
    const LIVE_START: i64 = 1_794_871_459;

    /// The ordinary deployment: every configured station has history and every recorded
    /// station is configured. Silence is the correct output and the one this will produce
    /// on almost every boot, so it is asserted first.
    #[test]
    fn a_configuration_that_matches_its_history_reports_nothing() {
        let problems = problems(
            &[cfg("csi", true), cfg("suncrest-air-quality", true)],
            &[
                rec_until("csi", IMPORT_END),
                rec_until("suncrest-air-quality", IMPORT_END),
            ],
        );
        assert_eq!(problems, Vec::new());
    }

    /// **The 2026-11-16 defect before the station had written anything.** A legal key, no
    /// readings yet, and a year of history addressed by a key nothing points at. The
    /// message names the key the history is under, which is the sentence that would have
    /// turned a three-hour mistake into a thirty-second one.
    #[test]
    fn a_station_with_no_readings_yet_names_the_key_the_history_is_under() {
        let problems = problems(
            &[cfg("csi", true), cfg("airgradient", true)],
            &[
                rec_until("csi", LIVE_START),
                rec_until("suncrest-air-quality", IMPORT_END),
            ],
        );
        let rendered: Vec<String> = problems.iter().map(ToString::to_string).collect();
        assert_eq!(rendered.len(), 2, "both directions, in {rendered:?}");
        assert!(
            rendered[0].contains("`airgradient`") && rendered[0].contains("suncrest-air-quality"),
            "the station with no history names where the history is: {}",
            rendered[0]
        );
        assert!(
            rendered[1].contains("`suncrest-air-quality`")
                && rendered[1].contains("no station on this node is configured"),
            "the orphaned history is reported in its own right: {}",
            rendered[1]
        );
    }

    /// **The same defect three seconds later, which is when it was actually discoverable.**
    /// The station has written, so there IS a projection row under `airgradient` and
    /// `NoHistory` has gone quiet -- the state the deployment was in for the whole of the
    /// day somebody went looking. The pairing survives in the instants: one series stops
    /// at 19:53 and another starts at 23:24 under a key nobody configured.
    #[test]
    fn the_pairing_survives_the_first_reading_landing_under_the_wrong_key() {
        let problems = problems(
            &[cfg("csi", true), cfg("airgradient", true)],
            &[
                rec_until("csi", LIVE_START),
                rec_from("airgradient", LIVE_START),
                rec_until("suncrest-air-quality", IMPORT_END),
            ],
        );
        assert!(
            !problems
                .iter()
                .any(|p| matches!(p, CoverageProblem::NoHistory { .. })),
            "the station has readings now, so the no-history arm is silent: {problems:?}"
        );
        let split = problems
            .iter()
            .find(|p| matches!(p, CoverageProblem::Split { .. }))
            .unwrap_or_else(|| panic!("the split is still visible: {problems:?}"));
        let rendered = split.to_string();
        assert!(
            rendered.contains("`airgradient`") && rendered.contains("`suncrest-air-quality`"),
            "{rendered}"
        );
        assert_eq!(split.station(), "airgradient");
    }

    /// The bound is what keeps the pairing a measurement rather than a guess. A station
    /// configured long after a device was retired is not that device under a new key, and
    /// saying so on every boot is how a check stops being read.
    #[test]
    fn a_station_configured_long_after_a_device_retired_is_not_paired_with_it() {
        let month_later = IMPORT_END + 30 * 86_400;
        let problems = problems(
            &[cfg("newgauge", true)],
            &[
                rec_from("newgauge", month_later),
                rec_until("snow", IMPORT_END),
            ],
        );
        assert!(
            !problems
                .iter()
                .any(|p| matches!(p, CoverageProblem::Split { .. })),
            "a month apart is not a handover: {problems:?}"
        );
        assert_eq!(problems.len(), 1, "only `snow`'s own line: {problems:?}");
    }

    /// The nearest predecessor, not all of them: the shape being reported is a handover and
    /// a handover has one. A second candidate would be a guess competing with a
    /// measurement.
    #[test]
    fn the_split_names_the_nearest_predecessor_only() {
        let problems = problems(
            &[cfg("live", true)],
            &[
                rec_from("live", LIVE_START),
                rec_until("far", LIVE_START - 500_000),
                rec_until("near", IMPORT_END),
            ],
        );
        let split = problems
            .iter()
            .find_map(|p| match p {
                CoverageProblem::Split { orphan, .. } => Some(orphan.as_str()),
                _ => None,
            })
            .expect("a split");
        assert_eq!(split, "near");
        assert_eq!(
            problems
                .iter()
                .filter(|p| matches!(p, CoverageProblem::Split { .. }))
                .count(),
            1
        );
    }

    /// History under a key nothing is configured for, with **no** station to pair it with
    /// -- `snow` on this deployment. Only one direction fires, and the message does not
    /// invite a pairing that does not exist.
    #[test]
    fn retired_history_is_reported_without_a_station_to_blame_it_on() {
        let problems = problems(
            &[cfg("csi", true)],
            &[
                rec_until("csi", IMPORT_END),
                rec_until("snow", IMPORT_END - 4_000_000),
            ],
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(matches!(
            &problems[0],
            CoverageProblem::NoStation { station, .. } if station == "snow"
        ));
    }

    /// A station added a moment ago has no readings yet, and that is not a mismatch. It is
    /// still reported -- the check cannot tell the two apart and must not guess -- but the
    /// message says so, and the candidate clause says there is nowhere else to look.
    #[test]
    fn a_brand_new_station_is_reported_as_having_no_past_rather_than_a_misplaced_one() {
        let problems = problems(
            &[cfg("csi", true), cfg("newgauge", true)],
            &[rec_until("csi", IMPORT_END)],
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        let rendered = problems[0].to_string();
        assert!(
            rendered.contains(
                "a station with no past rather than a station looking in the wrong place"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("until its first reading lands"),
            "{rendered}"
        );
    }

    /// A station switched off through the admin UI keeps its claim on its key, so its
    /// history is not reported as orphaned -- and it is not asked for readings either,
    /// because a station that is not collecting is not expected to be writing.
    #[test]
    fn a_disabled_station_claims_its_key_and_is_not_asked_for_readings() {
        assert_eq!(
            problems(&[cfg("csi", false)], &[rec_until("csi", IMPORT_END)]),
            Vec::new(),
            "a disabled station with history is nobody's problem"
        );
        assert_eq!(
            problems(&[cfg("csi", false)], &[]),
            Vec::new(),
            "a disabled station with no history is switched off, not misconfigured"
        );
    }

    /// An empty database is the first boot of a new deployment, and the first boot of a
    /// new deployment is not a defect. Every configured station is reported once, which is
    /// the honest answer, and nothing claims a key is missing.
    #[test]
    fn an_empty_database_reports_each_station_once_and_invents_no_orphans() {
        let problems = problems(&[cfg("csi", true), cfg("snow", true)], &[]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems
            .iter()
            .all(|p| matches!(p, CoverageProblem::NoHistory { .. })));
    }

    /// The candidate list is capped, and says how many it is not showing. Measured on a
    /// database a test suite had left nineteen unclaimed keys in: the line was most of a
    /// screen and was repeated for every station reported, which is how a diagnostic stops
    /// being read.
    #[test]
    fn the_candidate_list_is_capped_and_counts_what_it_leaves_out() {
        let orphans: Vec<Recorded> = (0..19)
            .map(|i| rec_until(&format!("orphan-{i:02}"), IMPORT_END - 4_000_000))
            .collect();
        let problems = problems(&[cfg("newgauge", true)], &orphans);
        let rendered = problems[0].to_string();
        assert!(
            rendered.contains("orphan-00, orphan-01, orphan-02, orphan-03, orphan-04"),
            "{rendered}"
        );
        assert!(!rendered.contains("orphan-05"), "{rendered}");
        assert!(rendered.contains("(and 14 more)"), "{rendered}");
        // Nothing is lost: the other direction still reports every one of them.
        assert_eq!(problems.len(), 1 + 19, "one per orphan, plus the station");
    }

    /// Every problem answers with the key it is about, because the repetition-control
    /// layer keys on a `station` field and a caller that omits it collapses every one of
    /// these lines into one bucket with a burst of three.
    #[test]
    fn every_problem_names_the_station_the_dedupe_layer_keys_on() {
        let problems = problems(
            &[cfg("airgradient", true)],
            &[
                rec_from("airgradient", LIVE_START),
                rec_until("suncrest-air-quality", IMPORT_END),
                rec_until("snow", IMPORT_END - 4_000_000),
            ],
        );
        let keys: Vec<&str> = problems.iter().map(CoverageProblem::station).collect();
        assert_eq!(keys, ["airgradient", "snow", "suncrest-air-quality"]);
    }

    /// **No message contains a double space.** A `\`-continued string literal in an
    /// `#[error]` attribute strips the newline and the next line's indentation, and one
    /// that loses its continuation silently bakes ten spaces into the middle of a
    /// sentence. That happened to the split message once, reached a log, and is invisible
    /// in a diff -- so it is asserted rather than re-read.
    #[test]
    fn no_rendered_message_has_a_run_of_spaces_in_it() {
        let every = problems(
            &[cfg("airgradient", true), cfg("newgauge", true)],
            &[
                rec_from("airgradient", LIVE_START),
                rec_until("suncrest-air-quality", IMPORT_END),
                Recorded {
                    station: "open-ended".to_owned(),
                    first_seen: at(IMPORT_END),
                    last_seen: None,
                },
            ],
        );
        // Every variant is present, so the assertion below covers all of them.
        assert!(every
            .iter()
            .any(|p| matches!(p, CoverageProblem::NoHistory { .. })));
        assert!(every
            .iter()
            .any(|p| matches!(p, CoverageProblem::Split { .. })));
        assert!(every
            .iter()
            .any(|p| matches!(p, CoverageProblem::NoStation { .. })));
        for p in &every {
            let rendered = p.to_string();
            assert!(
                !rendered.contains("  "),
                "a run of spaces in a rendered message: {rendered:?}"
            );
        }
    }

    /// `last_seen` is the nullable one, and the open-ended case needs words rather than an
    /// empty half of a range: a row `w9.populate_stations()` wrote during the migration has
    /// a beginning and no end until the writer touches it.
    #[test]
    fn a_projection_row_with_no_last_reading_still_renders_a_sentence() {
        let open = Recorded {
            station: "ghost".to_owned(),
            first_seen: at(IMPORT_END),
            last_seen: None,
        };
        let problems = problems(&[], &[open]);
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0]
                .to_string()
                .contains("with no last reading recorded"),
            "{}",
            problems[0]
        );
    }
}
