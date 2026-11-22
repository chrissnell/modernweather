//! What a snow gauge says about itself, for the operator's benefit.
//!
//! The gauge persists its own calibration in NVS and reports the depth derived from it, so
//! the base distance is no longer configuration this API writes — it is **device state**,
//! like a firmware version. This module is the read of it, and the answer to "what is my
//! gauge actually set to" without opening the device's own web interface.
//!
//! # A trait, not a dependency
//!
//! `ci/layers.toml` makes this crate the management API over `ConfigAdmin` and
//! `ComponentControl`, and an outbound HTTP request to an appliance on the LAN is neither.
//! [`crate::host`] takes the same shape for the log rings and `/dev` — declare what is
//! needed, let the binary supply it — and the binary is the one crate that may name both
//! this and `mw-station`'s request path.
//!
//! # Read on demand, not polled
//!
//! One request when an operator opens the page, not a background poll. The gauge serves at
//! most seven concurrent sockets and its own UI spends one every 500 ms per open tab, so a
//! second poller for a number that changes when somebody presses a button would be spending
//! that budget on nothing. Nothing here is stored: a calibration an operator can read off
//! the device is not a measurement, and `metrics/registry.toml` describes no metric for it.

use mw_component::BoxFuture;

use crate::error::ApiError;

/// One snow gauge's own account of itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct GaugeDeviceView {
    /// Whether the device answered at all.
    pub reachable: bool,
    /// The gauge's calibrated sensor-to-ground distance, in millimetres.
    ///
    /// **Read, never written.** Absent is a gauge that has never been calibrated, which is
    /// the state in which it reports a distance and no depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_distance_mm: Option<u32>,
    /// The depth the device last derived, in millimetres.
    ///
    /// Signed: a negative is a stable reading that overshot the calibrated base, which the
    /// firmware documents as frost heave or a calibration coming due.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snow_depth_mm: Option<i32>,
    /// `idle`, `running`, `complete`, `failed`, or `unknown` for a firmware newer than
    /// this daemon.
    pub calibration: String,
    /// One line about that state: how many samples it used, how long is left, or why it
    /// failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_detail: Option<String>,
    /// From `/health.fw_version`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
    /// From `/health.fw_slot` — which OTA partition is running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_slot: Option<String>,
    /// Signal strength in dBm.
    ///
    /// Absent rather than zero when the device could not look it up: the firmware's
    /// `sta_rssi()` yields `0` for that, which would read as a very strong signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<i32>,
    /// Why the device could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreachable_because: Option<String>,
}

impl GaugeDeviceView {
    /// The answer for a gauge that did not respond.
    ///
    /// **A `200`, not an error.** "The gauge is off" is an answer to the question the page
    /// asked, and a `500` would make a station whose sensor is unplugged look like a broken
    /// daemon.
    #[must_use]
    pub fn unreachable(because: impl Into<String>) -> Self {
        Self {
            reachable: false,
            base_distance_mm: None,
            snow_depth_mm: None,
            calibration: "unknown".to_string(),
            calibration_detail: None,
            firmware_version: None,
            firmware_slot: None,
            rssi_dbm: None,
            unreachable_because: Some(because.into()),
        }
    }
}

/// Read one gauge's live state. Implemented by the binary.
pub trait GaugeProbe: Send + Sync + 'static {
    /// Read the gauge configured for `station_key`.
    ///
    /// `Err` is for a question this API cannot answer — a station that is not a snow gauge,
    /// or one with no transport configured. A gauge that is simply off is `Ok` with
    /// `reachable: false`, because that is an answer rather than a failure.
    fn read<'a>(&'a self, station_key: &'a str)
        -> BoxFuture<'a, Result<GaugeDeviceView, ApiError>>;
}
