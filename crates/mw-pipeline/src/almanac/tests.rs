//! The plan and the body, against a recording store.
//!
//! The seam is the two capability handle traits, which is what `ci/layers.toml`'s ban on
//! `sqlx` and `mw-store` in this crate buys: the twelve records, their levels, their
//! directions, their filters and the carry-forward are all asserted with no database
//! anywhere. Go's equivalent is one PL/pgSQL function reachable only through a live
//! TimescaleDB, which is why none of it has a test today.
//!
//! What is **not** here, and is deliberately elsewhere: that these queries read the columns
//! the registry declares and that the twelve `record` names satisfy `almanac_cache`'s
//! `CHECK`. Neither is answerable without a server, and both are asserted by
//! `crates/mw-store/tests/db.rs` against a real one.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use mw_component::{
    Access, BucketLevel, Capability, ComponentId, ExtremeBucket, ExtremeQuery, FirstRun, Need,
    RawExtreme, RawExtremeQuery, Strength, Trigger,
};
use mw_types::StationKey;

use super::*;

// ---------------------------------------------------------------------------------------
// The configuration and the two handles, recorded.

struct Cfg {
    database: bool,
    stations: Vec<AlmanacStation>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            database: true,
            stations: vec![station("csi", "campbellscientific")],
        }
    }
}

impl AlmanacConfig for Cfg {
    fn has_weather_database(&self) -> bool {
        self.database
    }
    fn almanac_stations(&self) -> Vec<AlmanacStation> {
        self.stations.clone()
    }
}

fn station(key: &str, kind: &str) -> AlmanacStation {
    AlmanacStation {
        key: StationKey::new(key),
        kind: kind.to_string(),
    }
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<chrono::FixedOffset>>()
        .expect("an RFC 3339 instant")
        .with_timezone(&Utc)
}

/// What one `extreme` call asked for, as a comparable string.
fn asked_for(q: &ExtremeQuery) -> String {
    format!(
        "{} {:?}/{:?} {:?} positive_only={} carry={:?}",
        q.level, q.column.0, q.column.1, q.direction, q.positive_only, q.carry
    )
}

/// A `ReadingStore` that answers canned extremes keyed by what was asked for.
#[derive(Debug, Default)]
struct Store {
    /// Keyed on `"<level> <metric>/<agg>"`, which is enough to identify a record's read.
    extremes: BTreeMap<String, ExtremeBucket>,
    raw: BTreeMap<String, RawExtreme>,
    cached: Vec<AlmanacRecord>,
    asked: Mutex<Vec<String>>,
    raw_asked: Mutex<Vec<String>>,
    almanac_reads: Mutex<usize>,
    broken: bool,
}

/// The key both maps use: the level and the column, which is what distinguishes
/// `max_rain_hour` from `max_rain_day`.
fn key(level: BucketLevel, metric: MetricKey, agg: Agg) -> String {
    format!("{level} {metric:?}/{agg:?}")
}

impl Store {
    fn with(mut self, level: BucketLevel, metric: MetricKey, agg: Agg, b: ExtremeBucket) -> Self {
        self.extremes.insert(key(level, metric, agg), b);
        self
    }

    fn with_raw(mut self, metric: MetricKey, r: RawExtreme) -> Self {
        self.raw.insert(format!("{metric:?}"), r);
        self
    }

    fn cached(mut self, records: Vec<AlmanacRecord>) -> Self {
        self.cached = records;
        self
    }

    fn broken() -> Self {
        Self {
            broken: true,
            ..Self::default()
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("not poisoned").clone()
    }

    fn raw_asked(&self) -> Vec<String> {
        self.raw_asked.lock().expect("not poisoned").clone()
    }

    fn almanac_reads(&self) -> usize {
        *self.almanac_reads.lock().expect("not poisoned")
    }
}

impl ReadingStore for Store {
    fn buckets<'a>(
        &'a self,
        _q: mw_component::BucketQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucket>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn totals<'a>(
        &'a self,
        _q: mw_component::TotalsQuery,
    ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn series<'a>(
        &'a self,
        _q: mw_component::SeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucketed>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn mean_series<'a>(
        &'a self,
        _q: mw_component::MeanSeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::MeanBucket>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn latest<'a>(
        &'a self,
        _q: mw_component::LatestQuery,
    ) -> BoxFuture<'a, Result<Option<mw_types::Reading>, StoreError>> {
        Box::pin(async { Ok(None) })
    }
    fn span<'a>(
        &'a self,
        _q: mw_component::SpanQuery,
    ) -> BoxFuture<'a, Result<mw_component::Span, StoreError>> {
        Box::pin(async {
            Err(StoreError::rejected(std::io::Error::other(
                "almanac:records reads no span",
            )))
        })
    }
    fn almanac<'a>(
        &'a self,
        _q: AlmanacQuery,
    ) -> BoxFuture<'a, Result<Vec<AlmanacRecord>, StoreError>> {
        *self.almanac_reads.lock().expect("not poisoned") += 1;
        let out = self.cached.clone();
        Box::pin(async move { Ok(out) })
    }
    fn extreme<'a>(
        &'a self,
        q: ExtremeQuery,
    ) -> BoxFuture<'a, Result<Option<ExtremeBucket>, StoreError>> {
        self.asked.lock().expect("not poisoned").push(asked_for(&q));
        if self.broken {
            return Box::pin(async {
                Err(StoreError::unavailable(std::io::Error::other(
                    "connection reset by peer",
                )))
            });
        }
        let found = self
            .extremes
            .get(&key(q.level, q.column.0, q.column.1))
            .cloned();
        // The carried column is only answered when it was asked for, which is what makes
        // "a record with no `carry` never gets a `wind_dir`" assertable here.
        let found = found.map(|b| ExtremeBucket {
            carried: if q.carry.is_some() { b.carried } else { None },
            ..b
        });
        Box::pin(async move { Ok(found) })
    }
    fn raw_extreme<'a>(
        &'a self,
        q: RawExtremeQuery,
    ) -> BoxFuture<'a, Result<Option<RawExtreme>, StoreError>> {
        self.raw_asked
            .lock()
            .expect("not poisoned")
            .push(format!("{:?} [{}, {})", q.metric, q.since, q.until));
        let found = self.raw.get(&format!("{:?}", q.metric)).cloned();
        Box::pin(async move { Ok(found) })
    }
}

/// An `AlmanacCache` that remembers what it was told to write.
#[derive(Debug, Default)]
struct Cache {
    writes: Mutex<Vec<(String, Vec<AlmanacRecord>)>>,
}

impl Cache {
    fn writes(&self) -> Vec<(String, Vec<AlmanacRecord>)> {
        self.writes.lock().expect("not poisoned").clone()
    }

    /// The one write this tick made, or a failure naming what it did instead.
    fn only_write(&self) -> Vec<AlmanacRecord> {
        let writes = self.writes();
        assert_eq!(writes.len(), 1, "expected exactly one write: {writes:?}");
        writes[0].1.clone()
    }
}

impl AlmanacCache for Cache {
    fn replace_almanac<'a>(
        &'a self,
        station: &'a StationKey,
        records: &'a [AlmanacRecord],
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        // The real implementation's no-op on an empty slice is its own; the component is
        // asserted not to call with one, so recording every call is what makes that
        // assertable.
        self.writes
            .lock()
            .expect("not poisoned")
            .push((station.to_string(), records.to_vec()));
        let n = records.len() as u64;
        Box::pin(async move { Ok(n) })
    }
}

/// The body, built the way the spec's builder builds it.
fn body(stations: Vec<AlmanacStation>, store: Arc<Store>, cache: Arc<Cache>) -> Records {
    Records {
        stations: Arc::from(stations),
        store: store as Arc<dyn ReadingStore>,
        cache: cache as Arc<dyn AlmanacCache>,
    }
}

fn cx() -> Cx {
    let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
    Cx::new(
        ComponentId::new(ID).expect("a valid id"),
        tokio_util::sync::CancellationToken::new(),
        mw_component::Generation::ZERO,
        rx,
    )
}

fn bucket(value: f64, at_: &str) -> ExtremeBucket {
    ExtremeBucket {
        bucket: at(at_),
        value,
        carried: None,
    }
}

/// A store with one answer for every record a Campbell logger can produce.
fn a_full_campbell_history() -> Store {
    Store::default()
        .with(
            BucketLevel::Day,
            MetricKey::OutTemp,
            Agg::Max,
            bucket(95.73979, "2025-08-20T00:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::OutTemp,
            Agg::Min,
            bucket(12.097401, "2026-02-19T00:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::WindSpeed,
            Agg::Max,
            ExtremeBucket {
                carried: Some(184.01839),
                ..bucket(46.014404, "2026-02-17T00:00:00Z")
            },
        )
        .with(
            BucketLevel::Hour,
            MetricKey::RainIncremental,
            Agg::Sum,
            bucket(0.23000003, "2026-04-28T16:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::RainIncremental,
            Agg::Sum,
            bucket(0.90999943, "2025-10-04T00:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::Barometer,
            Agg::Min,
            bucket(29.440966, "2026-02-18T00:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::OutHumidity,
            Agg::Min,
            bucket(6.698, "2025-08-04T00:00:00Z"),
        )
        .with(
            BucketLevel::Day,
            MetricKey::SolarWatts,
            Agg::Max,
            bucket(1514.0381, "2026-05-18T00:00:00Z"),
        )
        .with_raw(
            MetricKey::SolarWatts,
            RawExtreme {
                time: at("2026-05-18T19:04:28.657385Z"),
                value: 1514.0381,
            },
        )
}

// ---------------------------------------------------------------------------------------
// The plan.

#[test]
fn a_node_with_no_weather_database_plans_nothing() {
    let mut out = Vec::new();
    register(
        &Cfg {
            database: false,
            ..Cfg::default()
        },
        &mut out,
    )
    .expect("no refusal");
    assert!(out.is_empty(), "D-44: there is no db:postgres to read");
}

#[test]
fn a_node_with_no_station_plans_nothing() {
    let mut out = Vec::new();
    register(
        &Cfg {
            stations: Vec::new(),
            ..Cfg::default()
        },
        &mut out,
    )
    .expect("no refusal");
    assert!(out.is_empty());
}

/// **One component for every station, not one per station.**
///
/// The three `snow:*` shared components have this shape and `snow:season:<station>` does
/// not, because that one is `Calendar { TzSource::Station }` and a zone binds it to one
/// station. Nothing here is zone-bound: an all-time maximum has no local boundary.
#[test]
fn one_component_covers_every_station_and_needs_both_handles() {
    let mut out = Vec::new();
    register(
        &Cfg {
            stations: vec![
                station("csi", "campbellscientific"),
                station("aq", "airgradient"),
            ],
            ..Cfg::default()
        },
        &mut out,
    )
    .expect("no refusal");
    assert_eq!(out.len(), 1);
    let spec = &out[0];
    assert_eq!(spec.id.as_str(), "almanac:records");
    assert_eq!(
        spec.requires,
        vec![
            Need {
                capability: Capability::AlmanacCache,
                strength: Strength::Hard,
                access: Access::Write,
            },
            Need {
                capability: Capability::ReadingStore,
                strength: Strength::Hard,
                access: Access::Read,
            },
        ],
        "`AlmanacCache` H W and `ReadingStore` H -- it writes one table and reads another"
    );
    assert!(
        matches!(
            spec.trigger,
            Trigger::Every {
                period: PERIOD,
                first: FirstRun::Immediate,
                ..
            }
        ),
        "Go's hourly job, and an immediate first run so a fresh deployment does not serve \
         `{{}}` for an hour: {:?}",
        spec.trigger
    );
}

/// The freshness declaration is the operator-visible half of this item: "Almanac records
/// have never been calculated" is the sentence that was missing while the cache was empty.
#[test]
fn the_freshness_declaration_names_the_cache_and_this_component() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("no refusal");
    let fresh = &out[0].freshness;
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].capability, Capability::AlmanacCache);
    assert_eq!(fresh[0].populated_by.as_str(), "almanac:records");
    assert_eq!(fresh[0].expected_max_age, PERIOD * 2);
}

/// The digest carries the **type** as well as the key, because the type is what selects
/// `high_pm10_in`'s column.
#[test]
fn retyping_a_station_changes_the_digest() {
    let of = |kind: &str| {
        let mut out = Vec::new();
        register(
            &Cfg {
                stations: vec![station("aq", kind)],
                ..Cfg::default()
            },
            &mut out,
        )
        .expect("no refusal");
        out[0].config_digest
    };
    assert_ne!(of("airgradient"), of("ambient-customized"));
    assert_eq!(of("airgradient"), of("airgradient"));
}

// ---------------------------------------------------------------------------------------
// The record table.

/// **Eleven records for a station that is not an AirGradient, twelve for one that is.**
///
/// `high_tvoc` is `AND stationtype = 'airgradient'` in Go's statement, so it is not a
/// record other devices can produce — not even an absent one.
#[test]
fn the_record_table_is_eleven_records_or_twelve() {
    let names = |kind: &str| -> Vec<&'static str> {
        records_for(kind).into_iter().map(|r| r.name).collect()
    };
    assert_eq!(
        names("campbellscientific"),
        vec![
            "high_temp",
            "low_temp",
            "high_wind_speed",
            "max_rain_hour",
            "max_rain_day",
            "low_barometer",
            "low_humidity",
            "high_solar",
            "high_pm25",
            "high_pm10_in",
            "high_co2",
        ]
    );
    assert_eq!(names("airgradient").len(), RECORD_COUNT);
    assert!(names("airgradient").contains(&"high_tvoc"));
}

/// **The three snow records are never written**, which is Q-19: they are declared in Go and
/// never assigned, `handlers.go:1416` carrying "Snow metrics temporarily disabled due to
/// performance issues". Writing them here would add three keys to a frozen response.
#[test]
fn no_snow_record_is_ever_written() {
    for kind in ["campbellscientific", "airgradient", "snowgauge"] {
        for spec in records_for(kind) {
            assert!(
                !spec.name.contains("snow"),
                "{kind} would write {}",
                spec.name
            );
        }
    }
}

/// **The two AQI records are derived by the handler and never stored.**
///
/// `handlers.go:1402-1413` computes them from the PM extremes with `getOrCalculateAQIPM25`,
/// and `mw_api_legacy::almanac` reproduces that — it ignores a stored row for either. A
/// writer here would be a second place deciding, and its value could drift from the PM
/// extreme beside it.
#[test]
fn no_aqi_record_is_ever_written() {
    for kind in ["campbellscientific", "airgradient"] {
        for spec in records_for(kind) {
            assert!(!spec.name.starts_with("high_aqi"), "{}", spec.name);
        }
    }
}

/// Go's `> 0` against its `IS NOT NULL`, record by record. The two are not interchangeable:
/// a barometer reporting `0` is a dead sensor and `low_barometer` must not be it, while a
/// `low_temp` of 0 °F is a January morning.
#[test]
fn the_positive_only_filter_is_the_one_go_writes_per_record() {
    let by_name: BTreeMap<&str, RecordSpec> = records_for("airgradient")
        .into_iter()
        .map(|r| (r.name, r))
        .collect();
    for name in [
        "high_temp",
        "low_temp",
        "high_wind_speed",
        "max_rain_hour",
        "max_rain_day",
    ] {
        assert!(
            !by_name[name].positive_only,
            "{name} is `IS NOT NULL` in migration 030"
        );
    }
    for name in [
        "low_barometer",
        "low_humidity",
        "high_solar",
        "high_pm25",
        "high_pm10_in",
        "high_co2",
        "high_tvoc",
    ] {
        assert!(
            by_name[name].positive_only,
            "{name} is `> 0` in migration 030"
        );
    }
}

/// **PM10 is `extrafloat2` on an AirGradient and `pm10_in_aqin` on everything else**, which
/// is the whole of migration 030: "The almanac only read pm10_in_aqin, so PM10 (and the
/// derived AQI PM10) were always blank for AirGradient devices."
#[test]
fn pm10_is_read_from_the_column_the_device_reports_it_in() {
    let column = |kind: &str| {
        records_for(kind)
            .into_iter()
            .find(|r| r.name == "high_pm10_in")
            .expect("every device has this record")
            .column
    };
    assert_eq!(
        column("airgradient"),
        (MetricKey::ExtraFloat2, Agg::Max),
        "the AirGradient station mapping packs PM10 into extrafloat2"
    );
    assert_eq!(
        column("ambient-customized"),
        (MetricKey::Pm10InAqin, Agg::Max)
    );
}

/// `max_rain_hour` and `max_rain_day` are the **same column at two levels**, and only
/// `max_rain_hour` carries the retention flag — `observations_1h` is kept for 90 days and
/// `observations_1d` is unbounded.
#[test]
fn only_the_hourly_rain_record_is_marked_as_coming_from_a_retained_level() {
    let specs = records_for("airgradient");
    let retained: Vec<&str> = specs
        .iter()
        .filter(|r| r.retained)
        .map(|r| r.name)
        .collect();
    assert_eq!(retained, vec!["max_rain_hour"]);
    for r in &specs {
        if r.name == "max_rain_hour" {
            assert_eq!(r.level, BucketLevel::Hour);
        }
        if r.name == "max_rain_day" {
            assert_eq!(r.level, BucketLevel::Day);
            assert_eq!(r.column, (MetricKey::RainIncremental, Agg::Sum));
        }
    }
}

// ---------------------------------------------------------------------------------------
// The body.

/// A record already in the cache.
///
/// At parent scope rather than inside `driven` below, because the carry-forward's
/// comparison tests are synchronous and stay in the loom build while `driven` does not.
fn held(record: &str, value: f32, at_: &str) -> AlmanacRecord {
    AlmanacRecord {
        record: record.to_string(),
        value,
        observed_at: at(at_),
        wind_dir: None,
    }
}

/// The body, which needs a **real `tokio` runtime** and therefore cannot run under
/// `--cfg loom`.
///
/// That flag applies to every crate in the graph, and `tokio` gates its whole `net`
/// module behind `#![cfg(not(loom))]` — so `runtime::Builder::build` asserts the I/O
/// driver is disabled and a `#[tokio::test]` panics before its body runs, inside tokio
/// rather than in anything this crate wrote:
///
/// ```text
/// tokio-1.54.1/src/runtime/driver.rs:165:9: assertion failed: !enabled
/// ```
///
/// `super::super::writer`'s `adapter` module, `super::super::snapshot::tests`' driven
/// module and `super::super::rollup::tests`' all carry the same guard for the same
/// reason; `tests/spool.rs`, `tests/fanout.rs` and `tests/ingress.rs` carry it at file
/// level. `.github/workflows/pr.yml`'s loom job explains the flag's reach in full.
///
/// Nothing excluded here is a concurrency model, so nothing is lost by excluding it:
/// this module asserts which records a history supports and what the carry-forward
/// does, both of which are sequential. The eleven plan tests above and the two
/// comparison tests below stay in the loom build because they are synchronous.
#[cfg(not(loom))]
mod driven {
    use super::*;

    #[tokio::test]
    async fn one_tick_writes_every_record_the_history_supports() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");

        let written = cache.only_write();
        let names: Vec<&str> = written.iter().map(|r| r.record.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "high_temp",
                "low_temp",
                "high_wind_speed",
                "max_rain_hour",
                "max_rain_day",
                "low_barometer",
                "low_humidity",
                "high_solar",
            ],
            "the three particulate records have no column on a Campbell logger, so they are \
             absent rather than zero -- which is what makes /almanac eight keys for this \
             station and fourteen for a site with an air-quality device"
        );
        // Eleven reads for eleven records, including the three that answered nothing: a record
        // is absent because the query found no bucket, not because this component decided a
        // Campbell logger has no PM2.5.
        assert_eq!(store.asked().len(), 11);
    }

    /// The value and the timestamp a record carries, down to the digit the contract serves.
    #[tokio::test]
    async fn a_day_level_record_carries_the_buckets_own_instant() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let written = cache.only_write();
        let high_temp = written
            .iter()
            .find(|r| r.record == "high_temp")
            .expect("written");
        assert_eq!(high_temp.value, 95.73979);
        assert_eq!(high_temp.observed_at, at("2025-08-20T00:00:00Z"));
        assert_eq!(
            high_temp.wind_dir, None,
            "only `high_wind_speed` carries a direction, and an absent one is an absent key"
        );
    }

    /// **`high_wind_speed` carries the bucket's mean direction, not the direction at the gust.**
    ///
    /// Go selects `max_windspeed, bucket, winddir` from one `weather_1d` row, so the direction
    /// beside the gust is `mw_circular_mean` over the whole day. Reproducing the contract means
    /// reading it the same way, and the test that matters is that the carry is *asked for* with
    /// `CircularMean` at the record's own level.
    #[tokio::test]
    async fn the_wind_record_carries_the_buckets_circular_mean_direction() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let wind = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "high_wind_speed")
            .expect("written");
        assert_eq!(wind.value, 46.014404);
        assert_eq!(wind.wind_dir, Some(184.01839));
        assert!(
            store.asked().iter().any(|q| q
                == "1d WindSpeed/Max Greatest positive_only=false carry=Some((WindDir, \
                    CircularMean))"),
            "{:?}",
            store.asked()
        );
    }

    /// **`high_solar`'s timestamp is a reading's, and the raw lookup is bounded by the day the
    /// day level named.**
    ///
    /// The live Go server answers `2026-05-18T13:04:28.657385-06:00` for this record, which is
    /// not a bucket boundary — Go reads the raw table because "aggregates keep only a daily
    /// average". What makes that bounded here is the two-step: the day level finds the peak day
    /// out of one row per day, and raw is read only inside it.
    #[tokio::test]
    async fn the_solar_record_is_the_raw_peak_inside_the_peak_day() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let solar = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "high_solar")
            .expect("written");
        assert_eq!(solar.observed_at, at("2026-05-18T19:04:28.657385Z"));
        assert_eq!(solar.value, 1514.0381);
        assert_eq!(
            store.raw_asked(),
            vec!["SolarWatts [2026-05-18 00:00:00 UTC, 2026-05-19 00:00:00 UTC)"],
            "one day of one station, not all of history"
        );
    }

    /// A day bucket whose raw rows are gone leaves the record out rather than serving a
    /// midnight timestamp for a solar peak. Unreachable while **D-4** holds — `observations`
    /// has no retention policy — and answered anyway, because the alternative is a visibly
    /// wrong instant on a frozen response.
    #[tokio::test]
    async fn a_peak_day_with_no_raw_reading_in_it_produces_no_record() {
        let mut store = a_full_campbell_history();
        store.raw.clear();
        let store = Arc::new(store);
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        assert!(
            !cache.only_write().iter().any(|r| r.record == "high_solar"),
            "no instant is better than midnight"
        );
    }

    /// **A station with no history writes nothing at all**, rather than emptying its records.
    ///
    /// Go's `DELETE` is unconditional, so a refresh that computes nothing empties the cache and
    /// `/almanac` serves `{}` — the state this component exists to prevent. The departure is
    /// deliberate and `mw_component::AlmanacCache::replace_almanac` documents it; this asserts
    /// the component's half, which is that it does not even call.
    #[tokio::test]
    async fn a_station_with_no_history_leaves_the_cache_alone() {
        let store = Arc::new(Store::default());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("no read failed");
        assert!(cache.writes().is_empty());
    }

    /// A failed read aborts the station and writes nothing — Go's transaction boundary. A
    /// partial set would replace eleven records with however many were read before the database
    /// stopped answering.
    #[tokio::test]
    async fn a_failed_read_writes_nothing_and_names_the_station() {
        let store = Arc::new(Store::broken());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        let err = records.tick(&cx()).await.expect_err("the store is broken");
        assert!(
            format!("{err}").contains("csi"),
            "an operator reading `last_error` has no other way to learn which station: {err}"
        );
        assert!(cache.writes().is_empty());
    }

    /// Every station gets its own write, and one station's failure is the whole tick's — the
    /// supervisor's retry re-reads all of them, which is cheap and keeps "did this tick
    /// succeed" a single fact.
    #[tokio::test]
    async fn each_station_is_written_separately() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![
                station("csi", "campbellscientific"),
                station("aq", "airgradient"),
            ],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let writes = cache.writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, "csi");
        assert_eq!(writes[1].0, "aq");
    }

    // ---------------------------------------------------------------------------------------
    // The carry-forward, which is `max_rain_hour` and nothing else.

    /// **An all-time record does not un-set itself when its source ages out.**
    ///
    /// `observations_1h` is kept for 90 days. The hour that set `max_rain_hour` leaves the
    /// level after that, and the extreme over what is left is smaller — so a writer that simply
    /// replaced the record would *lower* an all-time maximum every quarter, which is the rotting
    /// this component was written to prevent.
    #[tokio::test]
    async fn the_hourly_rain_record_is_never_lowered_by_the_levels_retention() {
        let store = Arc::new(
            a_full_campbell_history()
                .with(
                    BucketLevel::Hour,
                    MetricKey::RainIncremental,
                    Agg::Sum,
                    bucket(0.04, "2026-11-01T18:00:00Z"),
                )
                .cached(vec![held(
                    "max_rain_hour",
                    0.23000003,
                    "2026-04-28T16:00:00Z",
                )]),
        );
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let rain = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "max_rain_hour")
            .expect("written");
        assert_eq!(rain.value, 0.23000003, "the standing record is kept");
        assert_eq!(rain.observed_at, at("2026-04-28T16:00:00Z"));
        assert_eq!(
            store.almanac_reads(),
            1,
            "the records as they stand are read once per station, not once per record"
        );
    }

    /// And an hour that *does* beat the standing record replaces it.
    #[tokio::test]
    async fn an_hour_that_beats_the_standing_record_replaces_it() {
        let store = Arc::new(a_full_campbell_history().cached(vec![held(
            "max_rain_hour",
            0.1,
            "2026-04-28T16:00:00Z",
        )]));
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let rain = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "max_rain_hour")
            .expect("written");
        assert_eq!(rain.value, 0.23000003);
    }

    /// **A station whose every level read answers nothing writes nothing — even when it has a
    /// carried-forward record to offer.**
    ///
    /// The regression test for the defect the first code review of this component found. `out`
    /// being non-empty is not the same question as "did the history answer": a station with a
    /// standing `max_rain_hour` and no other readable bucket produces exactly that one record,
    /// which looked like a successful computation and replaced the other seven with it — and
    /// every later tick found the same nothing and rewrote the same one row, so `/almanac` fell
    /// from eight keys to one permanently. The state is reachable: the `warn` beside the write
    /// names it, an aggregate level rebuilt without being materialised
    /// (`ci/db/rebuild-aggregates.sh`).
    #[tokio::test]
    async fn a_station_whose_levels_answer_nothing_writes_nothing_even_with_a_record_to_carry() {
        let store = Arc::new(Store::default().cached(vec![
            held("high_temp", 95.0, "2025-08-20T00:00:00Z"),
            held("low_temp", 12.0, "2026-02-19T00:00:00Z"),
            held("max_rain_hour", 0.23, "2026-04-28T16:00:00Z"),
            held("low_barometer", 29.44, "2026-02-18T00:00:00Z"),
        ]));
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("no read failed");
        assert!(
            cache.writes().is_empty(),
            "the four standing records must survive a tick that could read nothing: {:?}",
            cache.writes()
        );
    }

    /// And the converse, so the guard above is not "never write when anything is missing": one
    /// readable level is enough, and the carried-forward record travels with it.
    #[tokio::test]
    async fn one_readable_record_is_enough_to_write_and_the_carry_travels_with_it() {
        let store = Arc::new(
            Store::default()
                .with(
                    BucketLevel::Day,
                    MetricKey::OutTemp,
                    Agg::Max,
                    bucket(95.73979, "2025-08-20T00:00:00Z"),
                )
                .cached(vec![held("max_rain_hour", 0.23, "2026-04-28T16:00:00Z")]),
        );
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("no read failed");
        let written = cache.only_write();
        let names: Vec<&str> = written.iter().map(|r| r.record.as_str()).collect();
        assert_eq!(names, vec!["high_temp", "max_rain_hour"]);
    }

    /// A record whose level is unbounded is **not** carried forward: `observations_1d` can
    /// always still see the day that set it, so a cached value that is greater than the data
    /// means the data changed and the record should follow it.
    #[tokio::test]
    async fn a_day_level_record_is_not_carried_forward() {
        let store = Arc::new(a_full_campbell_history().cached(vec![held(
            "high_temp",
            120.0,
            "2020-01-01T00:00:00Z",
        )]));
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let high = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "high_temp")
            .expect("written");
        assert_eq!(high.value, 95.73979, "the data wins over the cached value");
    }

    /// A station whose records have never been computed has nothing to carry forward, and the
    /// first tick is not a special case.
    #[tokio::test]
    async fn a_first_tick_with_an_empty_cache_takes_what_the_level_has() {
        let store = Arc::new(a_full_campbell_history());
        let cache = Arc::new(Cache::default());
        let mut records = body(
            vec![station("csi", "campbellscientific")],
            Arc::clone(&store),
            Arc::clone(&cache),
        );
        records.tick(&cx()).await.expect("the reads all answered");
        let rain = cache
            .only_write()
            .into_iter()
            .find(|r| r.record == "max_rain_hour")
            .expect("written");
        assert_eq!(rain.value, 0.23000003);
    }
}

/// A tie keeps the stored observation, because `/almanac`'s `timestamp` is part of the
/// answer: an hour that merely equals the standing record has not broken it.
#[test]
fn a_tie_keeps_the_earlier_observation() {
    let kept = more_extreme(
        Extremum::Greatest,
        Some(held("max_rain_hour", 0.23, "2026-11-01T00:00:00Z")),
        Some(held("max_rain_hour", 0.23, "2026-04-28T16:00:00Z")),
    )
    .expect("one of the two");
    assert_eq!(kept.observed_at, at("2026-04-28T16:00:00Z"));
}

/// A `Least` record carried forward keeps the *smaller* value, not the larger. No record
/// uses this combination today — `max_rain_hour` is the only retained one and it is a
/// maximum — and the function is total so that adding one is not a second decision.
#[test]
fn a_least_record_carried_forward_keeps_the_smaller() {
    let kept = more_extreme(
        Extremum::Least,
        Some(held("low_barometer", 30.0, "2026-11-01T00:00:00Z")),
        Some(held("low_barometer", 29.44, "2026-02-18T00:00:00Z")),
    )
    .expect("one of the two");
    assert_eq!(kept.value, 29.44);
}
