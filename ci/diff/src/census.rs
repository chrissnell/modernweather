//! How many readings went into each bucket, which is the only thing DEV-4's bound and
//! DEV-7's relationship cannot be checked without.
//!
//! The bound on `period_rain` and `windd` is `n * 2^-24` relative, where n is the number of
//! additions the Go view's `real` accumulator made -- so n is a property of the data, not of
//! the endpoint, and nothing in a `/span` response carries it. The census is therefore a
//! third input to mechanism (c), taken from the **Go** side: the bound is on how wrong the
//! old side is allowed to be, so the old side's reading count is the one that bounds it.
//!
//! A bucket with no census entry is reported as `no_census` and is **not** clean. The
//! alternative -- deriving a worst case from the bucket width and an assumed cadence --
//! would turn an unknown bound into a loose one and a loose bound into a green run, and the
//! station whose cadence the assumption was wrong about is the one it would matter for.

use std::collections::BTreeMap;
use std::path::Path;

use crate::clock::parse_duration_secs;
use crate::expected::Band;
use crate::psql::{quote_station, Psql};

/// Readings per bucket, per band, for one side.
#[derive(Debug, Default, Clone)]
pub struct Census {
    counts: BTreeMap<(String, i64), u64>,
    /// What produced it, for the report.
    pub provenance: String,
}

/// Both sides' counts.
///
/// The Go side's is what DEV-4's and DEV-7's bounds need. The Rust side's answers a different
/// and sharper question: **did the two buckets contain the same readings?** A bucket that
/// exists on both sides but was fed 720 readings on one and 61 on the other is not a
/// comparison of two implementations, it is a comparison of two windows -- and without this
/// the difference between the means is reported as a value difference in a dozen metrics,
/// which is how a coverage problem gets investigated as an arithmetic one. Measured: it is
/// exactly what the trailing bucket of every live band looks like while one daemon has been
/// running for less time than the other.
#[derive(Debug, Default, Clone)]
pub struct Censuses {
    pub go: Census,
    pub rust: Census,
}

impl Censuses {
    /// Readings in the Go side's bucket, which is what bounds the Go side's accumulator.
    pub fn n_go(&self, band: &str, ts: i64) -> Option<u64> {
        self.go.get(band, ts)
    }

    /// Whether the two sides' buckets were fed a different number of readings. `None` when
    /// the Rust side's census was not supplied and the question cannot be answered.
    pub fn uneven(&self, band: &str, ts: i64) -> Option<(u64, u64)> {
        let (a, b) = (self.go.get(band, ts)?, self.rust.get(band, ts)?);
        if a == b {
            None
        } else {
            Some((a, b))
        }
    }

    pub fn describe(&self) -> String {
        match (self.go.is_empty(), self.rust.is_empty()) {
            (true, _) => "none -- DEV-4's and DEV-7's bounds have no n, so every bucket they \
                          apply to is reported as no_census"
                .to_string(),
            (false, true) => format!(
                "{} Go buckets from {}; no Rust census, so an unevenly populated bucket cannot \
                 be told from a value difference",
                self.go.len(),
                self.go.provenance
            ),
            (false, false) => format!(
                "{} Go buckets from {}, {} Rust buckets from {}",
                self.go.len(),
                self.go.provenance,
                self.rust.len(),
                self.rust.provenance
            ),
        }
    }
}

impl Census {
    pub fn get(&self, band: &str, bucket_ts_ms: i64) -> Option<u64> {
        self.counts.get(&(band.to_string(), bucket_ts_ms)).copied()
    }

    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Count the Go side's raw readings into each band's grid.
    ///
    /// `time_bucket` rather than arithmetic on the epoch: the buckets have to land on the
    /// same grid the Go continuous aggregates used, and that grid is TimescaleDB's, so the
    /// census asks TimescaleDB for it instead of reproducing it.
    pub fn from_psql(
        psql: &Psql,
        table: &str,
        ident: &str,
        station: &str,
        bands: &[Band],
        now_ms: i64,
    ) -> Result<Census, String> {
        let name = quote_station(station)?;
        let mut counts = BTreeMap::new();
        for band in bands {
            let span = parse_duration_secs(&band.span)?;
            // One bucket of slack at the lower bound, because DEV-12 puts one bucket on the
            // new side that Go's unaligned predicate excludes, and that bucket still needs
            // an n.
            let from = (now_ms / 1000) - span - band.bucket_seconds as i64;
            let sql = format!(
                "SELECT (extract(epoch from time_bucket(interval '{} seconds', time)) * 1000)::bigint, \
                 count(*) FROM {} WHERE {} = {} AND time >= to_timestamp({}) GROUP BY 1",
                band.bucket_seconds, table, ident, name, from
            );
            for row in psql.rows(&sql)? {
                if row.len() < 2 {
                    continue;
                }
                let ts: i64 = row[0].parse().map_err(|_| {
                    format!("census {}: `{}` is not a bucket timestamp", band.id, row[0])
                })?;
                let n: u64 = row[1]
                    .parse()
                    .map_err(|_| format!("census {}: `{}` is not a count", band.id, row[1]))?;
                counts.insert((band.id.clone(), ts), n);
            }
        }
        Ok(Census {
            counts,
            provenance: format!("{} ({table})", psql.label),
        })
    }

    /// A recorded census: `band<TAB>bucket_ts_ms<TAB>n`, `#` comments allowed.
    pub fn from_tsv(path: &Path) -> Result<Census, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut counts = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 3 {
                return Err(format!(
                    "{}:{}: expected band<TAB>bucket_ts_ms<TAB>n",
                    path.display(),
                    i + 1
                ));
            }
            let ts: i64 = f[1].parse().map_err(|_| {
                format!(
                    "{}:{}: `{}` is not a timestamp",
                    path.display(),
                    i + 1,
                    f[1]
                )
            })?;
            let n: u64 = f[2]
                .parse()
                .map_err(|_| format!("{}:{}: `{}` is not a count", path.display(), i + 1, f[2]))?;
            counts.insert((f[0].to_string(), ts), n);
        }
        Ok(Census {
            counts,
            provenance: format!("tsv:{}", path.display()),
        })
    }
}
