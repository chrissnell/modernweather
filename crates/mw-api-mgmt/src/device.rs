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
//!
//! # Why a route that fetches a configured address is not a hole
//!
//! Stated once so that the next reader does not have to derive it. This is an outbound
//! request to an address the request body does not choose: the target is the station's own
//! `transport_url`, which the collection path already polls every five seconds, and the
//! paths are the three constants in `mw_codec`. It is behind the admin session guard, and an
//! operator who can reach it can already set `transport_url` to anything -- so the route adds
//! no reach that the configuration page does not already have. `mw_station::http` is what
//! bounds the rest: no redirects followed, a 1 MiB body ceiling, the status checked before
//! the body is read, TLS refused, and `Endpoint::parse` refusing userinfo, a path and
//! anything it cannot resolve. The residue is that an admin can use the panel to learn
//! whether a port on the LAN answers, which is true of the configuration page too.

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
    /// Whether the **sensor** behind the gauge is producing readings.
    ///
    /// A gauge whose ultrasonic sensor has stopped keeps serving HTTP perfectly: it answers
    /// `/data` with `status: "no_data"`, a frozen `readings` count and `bytes_rx: 0`, and it
    /// still reports what it is calibrated to. So `reachable` cannot carry this — a panel
    /// that showed only reachability would render a dead sensor as a healthy device, which
    /// is precisely the state an operator opens this panel in.
    pub sensor_ok: bool,
    /// One line about the sensor, when it is not producing readings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_detail: Option<String>,
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
            // A device that did not answer is not delivering readings either, so `false` is
            // true as far as anything can tell. The panel reads `reachable` first and says
            // the gauge is unreachable, so this does not become a second claim about why.
            sensor_ok: false,
            sensor_detail: None,
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
