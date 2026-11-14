//! The latest reading per station, and **N-11**'s fleet numbers.
//!
//! Both are traits `mw-api-mgmt` declares and this binary implements, for the reason
//! `ConfigAdmin` and [`crate::hostinfo`] exist: the data lives in `mw-pipeline`,
//! `mw-store` and `mw-collector`, and `ci/layers.toml` forbids the management API from
//! naming any of the three. The binary is the one place all of them may be named
//! (docs/service-architecture.md §1.3 rule 1).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use mw_api_mgmt::error::ApiError;
use mw_api_mgmt::host::{
    EnrolmentToken, EnrolmentView, Fleet, FleetView, LatestReading, PeerView, Readings, SpoolView,
};
use mw_component::BoxFuture;
use mw_pipeline::Spool;
use mw_types::Envelope;

/// The latest reading per station, read off the spool on demand.
///
/// Fed from [`Spool::subscribe_latest`] rather than from the `LatestSnapshot` capability,
/// and the reason is which question each one answers. `LatestSnapshot` is the
/// **pre-serialised** current conditions the frozen `/latest` path serves, keyed by
/// website and shaped by that contract; this needs "which station produced a reading, and
/// when", for every station including ones no website shows. The spool carries exactly
/// that, and carries it for peer-received readings too -- which is what makes a
/// satellite's station appear on Home.
///
/// # No task of its own
///
/// The accumulation happens inside [`Readings::latest`], which the live stream calls when
/// [`Readings::changes`] fires. A follower task would be a second task in a process whose
/// §3.2 invariant is that `mw_supervisor::task` owns every one of them -- and it would be
/// accumulating readings for nobody whenever no browser is connected.
///
/// **One consequence, stated rather than discovered.** A `watch` holds only the newest
/// value, so two stations reporting between two reads leave only the second in the
/// channel. The stream reads on every change signal, so the window is one signal wide;
/// and the thing being rendered is "the latest reading per station", for which an
/// intermediate reading that was never drawn is not information. The weather database --
/// not this -- is what nothing is allowed to drop.
#[derive(Debug)]
pub struct Latest {
    /// The spool's newest envelope. `borrow()` takes `&self`, which is what lets the
    /// accumulation happen on the read path.
    newest: watch::Receiver<Option<Arc<Envelope>>>,
    by_station: Mutex<BTreeMap<String, LatestReading>>,
}

impl Latest {
    /// Over one spool.
    pub fn over(spool: &Arc<Spool>) -> Arc<Self> {
        Arc::new(Latest {
            newest: spool.subscribe_latest(),
            by_station: Mutex::new(BTreeMap::new()),
        })
    }

    fn record(&self, envelope: &Envelope) {
        let key = envelope.station.as_str().to_string();
        let r = &envelope.reading;
        let reading = LatestReading {
            station_key: key.clone(),
            // Filled in by the stream from the vocabulary (D-36). Empty here on purpose:
            // this half of the system does not know what a station is called.
            station_name: String::new(),
            ts: envelope.observed_at,
            temperature_f: r.out_temp,
            humidity_pct: r.out_humidity,
            barometer_inhg: r.barometer,
            wind_speed_mph: r.wind_speed,
            wind_dir_degrees: r.wind_dir,
            solar_wm2: r.solar_watts,
            rain_day_in: r.day_rain,
            snow_depth_in: r.snow_depth,
        };
        self.by_station
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, reading);
    }
}

impl Readings for Latest {
    fn latest(&self) -> Vec<LatestReading> {
        // Fold whatever is in the channel now, then answer. Idempotent: the same envelope
        // produces the same entry, so a read with no new reading behind it changes nothing.
        let newest = self.newest.borrow().clone();
        if let Some(envelope) = newest {
            self.record(&envelope);
        }
        self.by_station
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn changes(&self) -> Option<watch::Receiver<Option<Arc<Envelope>>>> {
        // The fan-out's own channel, forwarded. The stream uses the edge and reads the
        // value back through `latest()`, which is where the accumulation lives.
        Some(self.newest.clone())
    }
}

/// How long the write rate is averaged over before it is reported.
///
/// Thirty seconds, because the advisory it feeds compares a ceiling against a `max_lag`
/// measured in days: a rate taken over two appends would make "your ceiling does not cover
/// your lag window" flicker on and off, which is worse than not saying it.
const RATE_WINDOW_SECONDS: u64 = 30;

/// **N-11**: W7's fleet numbers, assembled from the three crates that hold them.
pub struct FleetNumbers {
    spool: Option<Arc<Spool>>,
    peers: Option<Arc<mw_store::PeerStore>>,
    /// The depth and the instant it was taken, for the observed rate.
    baseline: Mutex<Option<(u64, std::time::Instant)>>,
}

impl std::fmt::Debug for FleetNumbers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetNumbers")
            .field("spool", &self.spool.is_some())
            .field("peers", &self.peers.is_some())
            .finish_non_exhaustive()
    }
}

impl FleetNumbers {
    /// Over whatever this node has. A node with neither answers an empty resource rather
    /// than a `501`: having no peers is a configuration, not a missing feature.
    pub fn new(spool: Option<Arc<Spool>>, peers: Option<Arc<mw_store::PeerStore>>) -> Arc<Self> {
        Arc::new(FleetNumbers {
            spool,
            peers,
            baseline: Mutex::new(None),
        })
    }

    /// The observed write rate, or `None` until the window has elapsed.
    ///
    /// Measured against the depth rather than against an append counter because the
    /// question is "how fast does the ceiling fill", and a reclaimed segment makes those
    /// two different numbers. A depth that went **down** -- a peer caught up -- is
    /// reported as no rate rather than as a negative one: the advisory is about filling.
    fn rate_bytes_per_second(&self, depth: u64) -> Option<f64> {
        let mut baseline = self
            .baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        match *baseline {
            None => {
                *baseline = Some((depth, now));
                None
            }
            Some((then_depth, then)) => {
                let elapsed = now.duration_since(then).as_secs_f64();
                if elapsed < RATE_WINDOW_SECONDS as f64 {
                    return None;
                }
                *baseline = Some((depth, now));
                let grew = depth.checked_sub(then_depth)?;
                if grew == 0 {
                    return None;
                }
                Some(grew as f64 / elapsed)
            }
        }
    }

    fn spool_view(&self) -> Option<SpoolView> {
        let spool = self.spool.as_ref()?;
        let config = spool.config();
        let depth_bytes = spool.depth_bytes();
        let ceiling_bytes = config.ceiling_bytes;
        let max_lag_seconds = config.max_lag.as_secs();
        let rate = self.rate_bytes_per_second(depth_bytes);
        // W7's `spool_time_remaining_seconds`: how long the room that is LEFT covers.
        let time_remaining_seconds = rate.map(|r| {
            let left = ceiling_bytes.saturating_sub(depth_bytes) as f64;
            left / r
        });
        // W7 §3.5's advisory, as the comparison rather than as a sentence.
        let needed = rate.map(|r| (r * max_lag_seconds as f64).round() as u64);
        // The depth in envelopes and the last delivery both come from the uplink's own
        // gauges, retained beside them in `mw_collector::metrics` -- the one writer of
        // each number. The ack lag is reported as an **age**, so the absolute instant is
        // the moment it was recorded minus that age. Several uplinks means several
        // answers; the newest delivery is the one the page means by "last".
        let lags = mw_collector::metrics::ack_lags();
        let last_delivery = lags
            .values()
            .filter_map(|r| {
                chrono::Duration::try_milliseconds((r.value * 1000.0) as i64).map(|age| r.at - age)
            })
            .max();
        let depth_envelopes = mw_collector::metrics::unacked_envelopes()
            .values()
            .map(|r| r.value.max(0.0) as u64)
            .max();
        Some(SpoolView {
            path: config.path.display().to_string(),
            ceiling_bytes,
            max_lag_seconds,
            depth_bytes,
            depth_envelopes,
            bytes_per_second: rate,
            time_remaining_seconds,
            needed_for_max_lag_bytes: needed,
            ceiling_covers_max_lag: needed.map(|n| ceiling_bytes >= n),
            last_delivery,
        })
    }
}

impl Fleet for FleetNumbers {
    fn view(&self) -> BoxFuture<'_, Result<FleetView, ApiError>> {
        Box::pin(async move {
            let spool = self.spool_view();
            let mut peers = Vec::new();
            let mut enrolment = None;
            // W7 D-40: peer-reported, and `mw-collector` is its one writer.
            let reported = mw_collector::metrics::peer_reported_depths();
            if let Some(store) = &self.peers {
                let rows = store.fleet().await.map_err(|e| ApiError::Internal {
                    summary: "Couldn't read which peers this machine knows.".to_string(),
                    detail: Some(e.to_string()),
                })?;
                for row in rows {
                    peers.push(PeerView {
                        peer_reported_depth_envelopes: reported
                            .get(&row.name)
                            .map(|r| r.value.max(0.0) as u64),
                        name: row.name,
                        node_id: hex(&row.node_id),
                        last_seen: row.last_seen,
                        stations: row.stations,
                    });
                }
                enrolment = Some(self.enrolment(store).await?);
            }
            Ok(FleetView {
                spool,
                peers,
                enrolment,
            })
        })
    }
}

impl FleetNumbers {
    /// Outstanding enrolment and the log, from the one table that holds both.
    ///
    /// W7 D-33, as W5 is required to surface it: enrolment is served only while an
    /// unexpired, unconsumed token exists, so an outstanding row **is** an open door.
    /// Deriving the log from the same rows rather than from a separate audit table is
    /// deliberate -- a history with a different source than the live state is a history
    /// that can disagree with it.
    async fn enrolment(&self, store: &mw_store::PeerStore) -> Result<EnrolmentView, ApiError> {
        let rows = store
            .enrolment_tokens()
            .await
            .map_err(|e| ApiError::Internal {
                summary: "Couldn't read which enrolment codes are outstanding.".to_string(),
                detail: Some(e.to_string()),
            })?;
        let now = chrono::Utc::now();
        let log: Vec<EnrolmentToken> = rows
            .into_iter()
            .map(|row| {
                let state = if row.consumed_at.is_some() {
                    "used"
                } else if row.expires_at <= now {
                    "expired"
                } else {
                    "outstanding"
                };
                EnrolmentToken {
                    id: hex(&row.id),
                    peer_name: row.peer_name,
                    issued_at: row.issued_at,
                    expires_at: row.expires_at,
                    state: state.to_string(),
                    consumed_at: row.consumed_at,
                    consumed_by: row.consumed_by.as_deref().map(hex),
                }
            })
            .collect();
        let outstanding = log
            .iter()
            .filter(|t| t.state == "outstanding")
            .cloned()
            .collect();
        Ok(EnrolmentView { outstanding, log })
    }
}

/// Lower-case hex, for the two identifiers that are shown only under `System → Advanced`.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A node with no spool, which produces no readings of its own.
///
/// An explicit type rather than an `Option<Arc<dyn Readings>>` on the API's side: "this
/// node measures nothing" is a configuration a node can be in, and the tiles' answer to it
/// is the same empty list they show before the first reading arrives.
#[derive(Debug, Default)]
pub struct NoReadings;

impl Readings for NoReadings {
    fn latest(&self) -> Vec<LatestReading> {
        Vec::new()
    }
}
