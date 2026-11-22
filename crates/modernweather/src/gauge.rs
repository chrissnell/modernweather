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

            // Additive: a gauge that answered `/data` is reachable whatever these do.
            let health = get(&endpoint, mw_codec::SNOW_HEALTH, HEALTH_TIMEOUT)
                .await
                .ok()
                .and_then(|f| mw_codec::snow_health(&f).ok());
            let calibration = get(&endpoint, mw_codec::SNOW_CALIBRATION, HEALTH_TIMEOUT)
                .await
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
        base_distance_mm: data.base_distance,
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
        unreachable_because: None,
    }
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
}
