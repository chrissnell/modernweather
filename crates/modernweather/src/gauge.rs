//! Reading a snow gauge's own calibration, for the management API.
//!
//! `mw_api_mgmt::device::GaugeProbe` is declared there and implemented here, which is
//! [`crate::hostinfo`]'s arrangement and `ci/layers.toml`'s reason: that crate is the
//! management API over `ConfigAdmin` and `ComponentControl`, and an outbound request to an
//! appliance is neither. This is the one crate that may name both it and
//! `mw_station::http`.
//!
//! # Three requests, and why `/data` is the one that decides
//!
//! `/data` carries the calibration and the depth, and it is the endpoint the driver polls —
//! so a gauge answering it is a gauge this daemon can collect from, and that is what
//! `reachable` means. `/health` and `/calibration` are additive: a gauge that answers the
//! first and refuses the others still has a base distance worth showing. All three are
//! unauthenticated on the device; only `POST /calibrate` and `POST /ota` need the admin
//! credential, and this build does neither (the owner runs a calibration from the gauge's
//! own web interface).

use mw_api_mgmt::device::{GaugeDeviceView, GaugeProbe};
use mw_api_mgmt::error::ApiError;
use mw_component::BoxFuture;
use mw_station::snowgauge::{HEALTH_TIMEOUT, REQUEST_TIMEOUT};
use mw_types::DisplayChain;

/// The probe, over the configuration this process already holds.
pub struct Gauges {
    /// The configuration database, for the station's `type` and `transport_url`.
    pool: sqlx::SqlitePool,
}

impl std::fmt::Debug for Gauges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gauges").finish_non_exhaustive()
    }
}

impl Gauges {
    /// Over the configuration pool the binary owns.
    #[must_use]
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }
}

impl GaugeProbe for Gauges {
    fn read<'a>(
        &'a self,
        station_key: &'a str,
    ) -> BoxFuture<'a, Result<GaugeDeviceView, ApiError>> {
        Box::pin(async move {
            let row: Option<(String, Option<String>)> =
                sqlx::query_as("SELECT type, transport_url FROM stations WHERE station_key = ?")
                    .bind(station_key)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|_| ApiError::Internal {
                        summary: "The configuration database could not be read.".to_string(),
                        detail: None,
                    })?;

            // A station that is not a gauge, or one with no address, is a question this
            // route cannot answer -- as distinct from a gauge that is merely off, which is
            // an answer (`mw_api_mgmt::device`).
            let url = match row {
                Some((kind, Some(url)))
                    if kind == mw_station::snowgauge::STATION_TYPE && !url.trim().is_empty() =>
                {
                    url
                }
                Some((kind, _)) if kind == mw_station::snowgauge::STATION_TYPE => {
                    return Err(ApiError::NotFound {
                        what: "a snow gauge with an address",
                        id: station_key.to_string(),
                    })
                }
                _ => {
                    return Err(ApiError::NotFound {
                        what: "a snow gauge",
                        id: station_key.to_string(),
                    })
                }
            };
            let endpoint = mw_station::Endpoint::parse(&url).map_err(|_| ApiError::NotFound {
                what: "a snow gauge whose address this build can read",
                id: station_key.to_string(),
            })?;

            // `/data` decides reachability, because it is the endpoint the driver polls.
            //
            // `snow_data` and not `SnowGaugeData::read`: the collection path refuses a body
            // that carries no storable measurement, and this one must not. A gauge whose
            // sensor is dead still reports what it is calibrated to, and that is precisely
            // when an operator opens this panel.
            let data = match get(&endpoint, mw_codec::SNOW_DATA, REQUEST_TIMEOUT).await {
                Ok(frame) => match mw_codec::snow_data(&frame) {
                    Ok(data) => data,
                    Err(e) => {
                        return Ok(GaugeDeviceView::unreachable(format!(
                            "{url} answered something this build could not read: {}",
                            DisplayChain(&e)
                        )))
                    }
                },
                Err(e) => return Ok(GaugeDeviceView::unreachable(DisplayChain(&e).to_string())),
            };

            // Additive: a gauge that answered `/data` is reachable whatever these do. Both
            // at once, because they are independent and an operator waiting on a management
            // request should wait for the slower of the two rather than their sum.
            let (health, calibration) = tokio::join!(
                get(&endpoint, mw_codec::SNOW_HEALTH, HEALTH_TIMEOUT),
                get(&endpoint, mw_codec::SNOW_CALIBRATION, HEALTH_TIMEOUT),
            );
            let health = health.ok().and_then(|f| mw_codec::snow_health(&f).ok());
            let calibration = calibration
                .ok()
                .and_then(|f| mw_codec::snow_calibration(&f).ok());

            Ok(view_of(&data, health.as_ref(), calibration.as_ref()))
        })
    }
}

/// One `GET` against the gauge.
async fn get(
    endpoint: &mw_station::Endpoint,
    path: &str,
    timeout: std::time::Duration,
) -> Result<mw_codec::Frame, mw_station::http::HttpError> {
    mw_station::http::request(endpoint, mw_station::HttpMethod::Get, path, None, timeout).await
}

/// The three bodies as one view. Pure, so the mapping is testable with no device.
fn view_of(
    data: &mw_codec::SnowData,
    health: Option<&mw_codec::SnowHealth>,
    calibration: Option<&mw_codec::SnowCalibration>,
) -> GaugeDeviceView {
    let (state, detail) = match calibration {
        Some(mw_codec::SnowCalibration::Complete {
            base_distance,
            samples_used,
            ..
        }) => (
            "complete",
            Some(format!("{base_distance} mm from {samples_used} samples")),
        ),
        Some(mw_codec::SnowCalibration::Running {
            remaining_ms,
            samples,
            ..
        }) => (
            "running",
            Some(format!(
                "{samples} samples so far, about {}s left",
                remaining_ms / 1000
            )),
        ),
        Some(mw_codec::SnowCalibration::Failed { reason, .. }) => ("failed", Some(reason.clone())),
        // A firmware newer than this daemon. Named rather than blanked.
        Some(mw_codec::SnowCalibration::Other) => (
            "unknown",
            Some("this gauge reports a calibration state this build does not know".to_string()),
        ),
        // `idle` and "the endpoint did not answer" are told apart by the base distance,
        // because that is the fact an operator is actually asking about: a gauge with one
        // has been calibrated at some point, whatever its state says now.
        Some(mw_codec::SnowCalibration::Idle) | None => (
            "idle",
            if data.is_calibrated() {
                None
            } else {
                Some(
                    "this gauge has never been calibrated, so it reports a distance and no \
                     depth"
                        .to_string(),
                )
            },
        ),
    };
    GaugeDeviceView {
        reachable: true,
        // **The same `> 0` test `is_calibrated` applies**, and applied here for the same
        // reason: the firmware's NVS read can yield `0` for "unset", and publishing that
        // raw would render "calibrated distance to the ground: 0 mm" beside "this gauge has
        // never been calibrated" -- the panel contradicting itself in two adjacent lines.
        base_distance_mm: data.base_distance.filter(|mm| *mm > 0),
        snow_depth_mm: data.snow_depth,
        calibration: state.to_string(),
        calibration_detail: detail,
        firmware_version: health.and_then(|h| h.fw_version.clone()),
        firmware_slot: health.and_then(|h| h.fw_slot.clone()),
        // **Zero is the firmware's "the lookup failed", not a strong signal.** Filtered
        // here rather than rendered, which is the correction the firmware source turned up.
        rssi_dbm: data
            .rssi
            .or_else(|| health.and_then(|h| h.rssi))
            .filter(|dbm| *dbm != 0),
        sensor_ok: data.is_ok(),
        sensor_detail: sensor_detail(data, health),
        unreachable_because: None,
    }
}

/// Why the gauge is not producing readings, when it is not.
///
/// `status` is the firmware's own verdict and `readings`/`bytes_rx` are the evidence behind
/// it: a sensor that never spoke has both at zero, and one whose UART died mid-run has a
/// non-zero `readings` that has stopped moving. The second cannot be told from one response
/// -- that is what the driver's own liveness state is for -- so this says what this response
/// shows and no more.
fn sensor_detail(
    data: &mw_codec::SnowData,
    health: Option<&mw_codec::SnowHealth>,
) -> Option<String> {
    if data.is_ok() {
        return None;
    }
    let bytes = health.and_then(|h| h.bytes_rx);
    Some(match (data.readings, bytes) {
        (0, Some(0) | None) => format!(
            "the gauge reports `{}` and has parsed no sensor frames since booting -- \
             check the sensor's wiring",
            data.status
        ),
        (0, Some(rx)) => format!(
            "the gauge reports `{}`: {rx} bytes arrived from the sensor but no frame \
             parsed from them",
            data.status
        ),
        (n, _) => format!(
            "the gauge reports `{}` after {n} sensor frames: the sensor stopped rather \
             than never started",
            data.status
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(body: &str) -> mw_codec::SnowData {
        mw_codec::snow_data(&mw_codec::Frame::new(body.as_bytes().to_vec()))
            .expect("the fixture is a `/data` body")
    }

    /// A calibrated gauge, mid-season: the three bodies become the panel.
    #[test]
    fn the_three_bodies_become_one_view() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1288,"trim_mean":1700.0,
                    "base_distance":1771,"snow_depth":71,"rssi":-63,"uptime_ms":2369755}"#,
            ),
            Some(
                &mw_codec::snow_health(&mw_codec::Frame::new(
                    br#"{"wifi":"connected","ip":"10.50.0.138","rssi":-63,
                         "hostname":"snowgauge-de18","bytes_rx":7728,"heap_free":162428,
                         "fw_version":"0.1.0","fw_slot":"ota_0"}"#
                        .to_vec(),
                ))
                .expect("decodes"),
            ),
            Some(&mw_codec::SnowCalibration::Complete {
                base_distance: 1771,
                samples_used: 10,
                finished_at_ms: 2_342_978,
            }),
        );
        assert!(view.reachable);
        assert_eq!(view.base_distance_mm, Some(1771));
        assert_eq!(view.snow_depth_mm, Some(71));
        assert_eq!(view.calibration, "complete");
        assert_eq!(
            view.calibration_detail.as_deref(),
            Some("1771 mm from 10 samples")
        );
        assert_eq!(view.firmware_version.as_deref(), Some("0.1.0"));
        assert_eq!(view.firmware_slot.as_deref(), Some("ota_0"));
        assert_eq!(view.rssi_dbm, Some(-63));
        assert_eq!(view.unreachable_because, None);
    }

    /// `/health` and `/calibration` are additive: a gauge that answered only `/data` is
    /// still reachable and still has a base distance to show.
    #[test]
    fn a_gauge_that_answered_only_data_is_still_reachable() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1288,"trim_mean":1771.0,
                    "base_distance":1771,"snow_depth":0,"uptime_ms":1}"#,
            ),
            None,
            None,
        );
        assert!(view.reachable);
        assert_eq!(view.base_distance_mm, Some(1771));
        // Calibrated, so `idle` needs no apology: the gauge has been calibrated at some
        // point, which is the fact being asked about.
        assert_eq!(view.calibration, "idle");
        assert_eq!(view.calibration_detail, None);
        assert_eq!(view.firmware_version, None);
    }

    /// An uncalibrated gauge is the state the panel exists to make obvious, and it says
    /// what to do about it.
    #[test]
    fn an_uncalibrated_gauge_says_so() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1288,"trim_mean":1771.0,
                    "base_distance":null,"snow_depth":null,"rssi":-63,"uptime_ms":1}"#,
            ),
            None,
            Some(&mw_codec::SnowCalibration::Idle),
        );
        assert_eq!(view.base_distance_mm, None);
        assert_eq!(view.snow_depth_mm, None);
        assert_eq!(view.calibration, "idle");
        assert!(view
            .calibration_detail
            .as_deref()
            .expect("a sentence")
            .contains("never been calibrated"));
    }

    /// **The NVS "unset" sentinel.** `base_distance: 0` is what the firmware's NVS read can
    /// yield for a gauge that has never been calibrated, and it is the same `> 0` test
    /// `is_calibrated` applies — so the panel must not publish it as a calibration. Without
    /// the filter the two lines beside each other read "calibrated distance to the ground:
    /// 0 mm" and "this gauge has never been calibrated".
    #[test]
    fn a_zero_base_distance_is_absent_rather_than_a_calibration() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1288,"trim_mean":1771.0,
                    "base_distance":0,"snow_depth":null,"rssi":-63,"uptime_ms":1}"#,
            ),
            None,
            Some(&mw_codec::SnowCalibration::Idle),
        );
        assert_eq!(view.base_distance_mm, None, "not `Some(0)`");
        assert_eq!(view.calibration, "idle");
        assert!(
            view.calibration_detail
                .as_deref()
                .expect("a sentence")
                .contains("never been calibrated"),
            "and the two agree"
        );
    }

    /// **A dead sensor behind a healthy web server**, which is the state the panel is most
    /// often opened in and the one `reachable` cannot express. The gauge answers every
    /// request, reports what it is calibrated to, and is measuring nothing.
    #[test]
    fn a_gauge_whose_sensor_is_dead_is_reachable_and_not_ok() {
        let view = view_of(
            &data(
                r#"{"distance":0,"status":"no_data","readings":0,"trim_mean":null,
                    "base_distance":1771,"snow_depth":null,"rssi":-63,"uptime_ms":90000}"#,
            ),
            Some(
                &mw_codec::snow_health(&mw_codec::Frame::new(
                    br#"{"wifi":"connected","ip":"10.50.0.138","rssi":-63,
                         "hostname":"snowgauge-de18","bytes_rx":0,"heap_free":162428,
                         "fw_version":"0.1.0","fw_slot":"ota_0"}"#
                        .to_vec(),
                ))
                .expect("decodes"),
            ),
            None,
        );
        assert!(view.reachable, "it answered everything");
        assert!(!view.sensor_ok, "and measured nothing");
        assert_eq!(
            view.base_distance_mm,
            Some(1771),
            "which is the number the operator came for"
        );
        let detail = view.sensor_detail.expect("a sentence about the sensor");
        assert!(detail.contains("no_data"), "{detail}");
        assert!(detail.contains("wiring"), "{detail}");
    }

    /// A sensor that spoke and **then** stopped: `readings` is not zero, so the sentence
    /// says so rather than sending the operator to check wiring that worked this morning.
    #[test]
    fn a_sensor_that_stopped_is_distinguished_from_one_that_never_started() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"no_data","readings":412,"trim_mean":1771.0,
                    "base_distance":1771,"snow_depth":0,"rssi":-63,"uptime_ms":900000}"#,
            ),
            None,
            None,
        );
        assert!(!view.sensor_ok);
        let detail = view.sensor_detail.expect("a sentence");
        assert!(detail.contains("412"), "{detail}");
        assert!(
            detail.contains("stopped rather than never started"),
            "{detail}"
        );
    }

    /// A working sensor has nothing to explain.
    #[test]
    fn a_working_sensor_carries_no_apology() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1288,"trim_mean":1771.0,
                    "base_distance":1771,"snow_depth":0,"rssi":-63,"uptime_ms":1}"#,
            ),
            None,
            None,
        );
        assert!(view.sensor_ok);
        assert_eq!(view.sensor_detail, None);
    }

    /// **The firmware's RSSI sentinel.** `0` means the access-point lookup failed, so
    /// rendering it would show a dead gauge as the strongest possible signal.
    #[test]
    fn a_zero_signal_strength_is_absent_rather_than_rendered() {
        let view = view_of(
            &data(
                r#"{"distance":1771,"status":"ok","readings":1,"trim_mean":null,
                    "base_distance":1771,"snow_depth":0,"rssi":0,"uptime_ms":1}"#,
            ),
            None,
            None,
        );
        assert_eq!(view.rssi_dbm, None);
    }

    /// A calibration in progress, and a failed one, both read as sentences rather than as
    /// a state word on its own.
    #[test]
    fn a_running_or_failed_calibration_says_what_is_happening() {
        let body = r#"{"distance":1771,"status":"ok","readings":1,"trim_mean":null,
                       "base_distance":null,"snow_depth":null,"uptime_ms":1}"#;
        let running = view_of(
            &data(body),
            None,
            Some(&mw_codec::SnowCalibration::Running {
                elapsed_ms: 12_000,
                remaining_ms: 18_000,
                samples: 7,
            }),
        );
        assert_eq!(running.calibration, "running");
        assert_eq!(
            running.calibration_detail.as_deref(),
            Some("7 samples so far, about 18s left")
        );

        let failed = view_of(
            &data(body),
            None,
            Some(&mw_codec::SnowCalibration::Failed {
                reason: "no samples collected".to_string(),
                finished_at_ms: 82_000,
            }),
        );
        assert_eq!(failed.calibration, "failed");
        assert_eq!(
            failed.calibration_detail.as_deref(),
            Some("no samples collected")
        );

        // A firmware newer than this build is named, not blanked.
        let future = view_of(&data(body), None, Some(&mw_codec::SnowCalibration::Other));
        assert_eq!(future.calibration, "unknown");
        assert!(future.calibration_detail.is_some());
    }

    /// A gauge that is off is an answer, not a failure: the page asked "what is my gauge
    /// set to" and "it is not answering" is a reply to that.
    #[test]
    fn an_unreachable_gauge_is_an_answer() {
        let view =
            GaugeDeviceView::unreachable("snowgauge-f878.local did not answer within 3000ms");
        assert!(!view.reachable);
        assert_eq!(view.calibration, "unknown");
        assert_eq!(view.base_distance_mm, None);
        assert!(view
            .unreachable_because
            .as_deref()
            .expect("a sentence")
            .contains("did not answer"));
    }

    // -----------------------------------------------------------------------------------
    // `Gauges::read`'s four station branches, which decide 404 against 200 before any
    // socket is opened. The rule they encode: **"not a gauge I can address" is a 404 and
    // "a gauge that is not answering" is a 200**, because only one of the two is a question
    // this route cannot answer.

    /// A configuration store with one `stations` row, holding only the two columns the
    /// probe reads. In memory, so this is a unit test with a database in it rather than a
    /// fixture.
    async fn store(kind: &str, url: Option<&str>) -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("an in-memory database");
        sqlx::query("CREATE TABLE stations (station_key TEXT, type TEXT, transport_url TEXT)")
            .execute(&pool)
            .await
            .expect("the table is creatable");
        sqlx::query("INSERT INTO stations (station_key, type, transport_url) VALUES (?, ?, ?)")
            .bind("snowgauge")
            .bind(kind)
            .bind(url)
            .execute(&pool)
            .await
            .expect("the row is insertable");
        pool
    }

    #[tokio::test]
    async fn a_station_that_is_not_a_gauge_is_a_404() {
        let probe = Gauges::new(store("davis", Some("http://10.50.0.138")).await);
        let e = probe.read("snowgauge").await.expect_err("not a gauge");
        assert!(
            matches!(e, ApiError::NotFound { what, .. } if what == "a snow gauge"),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn a_station_that_does_not_exist_is_a_404() {
        let probe = Gauges::new(store("snowgauge", Some("http://10.50.0.138")).await);
        let e = probe
            .read("nothing-here")
            .await
            .expect_err("no such station");
        assert!(
            matches!(e, ApiError::NotFound { what, .. } if what == "a snow gauge"),
            "{e:?}"
        );
    }

    /// A gauge with no address: configured, not yet told where it is. The 404 names the
    /// address rather than the gauge, because the station does exist.
    #[tokio::test]
    async fn a_gauge_with_no_address_is_a_404_that_says_which_half_is_missing() {
        for url in [None, Some(""), Some("   ")] {
            let probe = Gauges::new(store("snowgauge", url).await);
            let e = probe.read("snowgauge").await.expect_err("no address");
            assert!(
                matches!(e, ApiError::NotFound { what, .. } if what.contains("an address")),
                "{url:?}: {e:?}"
            );
        }
    }

    /// An address this build cannot turn into a request at all. `Endpoint::parse` refuses
    /// userinfo (a credential it must not quote back), a path (the paths here are fixed
    /// constants) and text with no scheme — and none of those is a gauge being off, so none
    /// of them is a `200`.
    #[tokio::test]
    async fn an_address_that_cannot_be_parsed_is_a_404() {
        for url in [
            "http://user:pass@snowgauge.local",
            "http://snowgauge.local/data",
            "not a url at all",
        ] {
            let probe = Gauges::new(store("snowgauge", Some(url)).await);
            let e = probe.read("snowgauge").await.expect_err("unparseable");
            assert!(
                matches!(e, ApiError::NotFound { what, .. } if what.contains("this build can read")),
                "{url}: {e:?}"
            );
        }
    }

    /// **An `https://` address parses and then fails at the request**, so it is a `200`
    /// whose reason names TLS. That is the useful answer: the operator has a configuration
    /// mistake, and the panel is where they will read about it, whereas a `404` would say
    /// only that there is no gauge here.
    #[tokio::test]
    async fn an_https_address_is_reported_on_the_panel_rather_than_as_a_404() {
        let probe = Gauges::new(store("snowgauge", Some("https://snowgauge.local")).await);
        let view = probe.read("snowgauge").await.expect("an answer");
        assert!(!view.reachable);
        let why = view.unreachable_because.expect("a sentence");
        assert!(why.contains("HTTPS"), "{why}");
    }

    /// **The case that is a `200`.** A real address with nothing behind it: the station is
    /// a gauge, this build can address it, and it is not answering. That is the panel's
    /// "unreachable" state and not an error, so a station whose sensor is unplugged does
    /// not render as a broken daemon.
    #[tokio::test]
    async fn a_gauge_that_is_not_answering_is_a_200() {
        // Port 1 on the loopback: a connection refused rather than a timeout, so the test
        // does not wait out `REQUEST_TIMEOUT`.
        let probe = Gauges::new(store("snowgauge", Some("http://127.0.0.1:1")).await);
        let view = probe
            .read("snowgauge")
            .await
            .expect("an answer, not an error");
        assert!(!view.reachable);
        assert!(!view.sensor_ok);
        assert_eq!(view.base_distance_mm, None);
        assert!(
            view.unreachable_because.is_some(),
            "and it says why: {view:?}"
        );
    }
}
