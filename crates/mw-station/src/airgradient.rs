//! The AirGradient driver: one `GET /measures/current` per occurrence.
//!
//! W2a §4a.5's row, and the simplest of the seven: `HttpOut`, no session, `Fixed` identity,
//! `Every { 3s }`. An AirGradient ONE or Open Air is a small air-quality monitor on the LAN
//! with a local HTTP API, and the whole driver is a request, a decode and an admission.
//!
//! # Three seconds, and why it is not clamped per occurrence
//!
//! [`DEFAULT_POLL`] is the Go driver's `pollInterval` and the device's own measurement
//! cadence — `station.go:67` with the comment *"AirGradient supports fast polling"*. The
//! period is **not configurable** here, because there is no column for it: the schema's only
//! interval column is `wll_poll_interval_seconds`, which is the WeatherLink Live's. Adding
//! one is a migration and an owner decision about what a sensible floor is; three seconds is
//! what both implementations do today, and recording that is better than inventing a column.
//!
//! The floor is still declared ([`POLL_FLOOR`]) and is still enforced **once, in `plan()`**
//! (**W2a Decision 56**) rather than clamped per occurrence, so that a period below it is a
//! startup refusal naming the station rather than a timer that silently does something else.
//!
//! # What this driver does not do that the Go one does
//!
//! `pollDevice` logs an error and returns on a failed request, on a non-`200` and on a body
//! that does not decode — three `logger.Errorw` calls, three times a second if the device is
//! off, and nothing an operator can alert on. Here each of those is a **failed occurrence**:
//! `mw_component_ticks_total{component, outcome="failed"}` moves,
//! `ComponentStatus::last_error` carries the sentence, and the component's own state says
//! what is wrong. There is no retry loop in this file for the same reason there is none in
//! `crate::driver` — the occurrence is the retry.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mw_codec::{AirGradientJson, Codec, Frame, MEASURES_CURRENT};
use mw_component::{BoxFuture, ComponentError, ComponentId, ComponentIdError, Cx, Ingress, Tick};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use tracing::{debug, error, warn};

use crate::endpoint::Endpoint;
use crate::http;
use crate::identity::StationIdentity;
use crate::metrics::{self as m, FrameOutcome};
use crate::transport::HttpMethod;

/// The `type` column value this driver serves.
pub const STATION_TYPE: &str = "airgradient";

/// The poll period, which is the device's own measurement cadence.
pub const DEFAULT_POLL: Duration = Duration::from_secs(3);

/// The floor W2a §4a.5's "the measured default" implies.
///
/// One second: the device answers in a few milliseconds on a LAN and a sub-second poll
/// would be a request every time the previous one finished. Declared here and enforced in
/// `plan()`.
pub const POLL_FLOOR: Duration = Duration::from_secs(1);

/// How long one request may take.
///
/// Comfortably inside the occurrence's deadline so that a device answering slowly produces
/// the request timeout's message -- which names the device -- rather than
/// `DeadlineExceeded`, which names nothing. Shorter than the Go client's ten seconds,
/// because ten seconds is longer than three and an occurrence that outlived its own period
/// would be skipped and counted rather than answered.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The component id of one station's poll.
pub fn station_id(station_key: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("station:airgradient:{station_key}"))
}

/// One `GET /measures/current` per occurrence.
pub struct Poll {
    endpoint: Endpoint,
    station: StationKey,
    codec: AirGradientJson,
    ingress: Arc<dyn Ingress>,
}

impl fmt::Debug for Poll {
    /// The endpoint and the station. Not the handle, which would print a spool.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Poll")
            .field("endpoint", &self.endpoint)
            .field("station", &self.station)
            .finish_non_exhaustive()
    }
}

impl Poll {
    /// The poll for one station.
    pub fn new(endpoint: Endpoint, station: StationKey, ingress: Arc<dyn Ingress>) -> Self {
        Self {
            endpoint,
            station,
            codec: AirGradientJson,
            ingress,
        }
    }

    /// Decode one response body and admit what it carried.
    ///
    /// The seam the transcript-level test drives: a recorded sequence of bodies, asserting
    /// that **every** one produces an admission. The same assertion the WeatherLink Live
    /// poll makes, for the same reason -- W3 **D-82** names the level that catches a driver
    /// which admits the first body and then quietly stops.
    pub async fn on_body(&self, id: &ComponentId, frame: &Frame) -> Result<(), ComponentError> {
        let batch = match self.codec.decode(frame) {
            Ok(batch) => batch,
            Err(e) => {
                m::frame(id, FrameOutcome::Undecodable);
                return Err(ComponentError::tick("decoding an AirGradient response", e));
            }
        };
        // `Fixed`: configuration says which station, so the frame presents nothing.
        let meta = StationIdentity::Fixed.frame_meta(id, None, Utc::now());
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    station = %self.station,
                    accepted = admitted.accepted,
                    rejected = admitted.rejected,
                    "admitted an AirGradient poll"
                );
                Ok(())
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                match e.class() {
                    // A station pointed at the wrong endpoint is a counted signal, not a
                    // failure of the component that received it (§4.4).
                    Class::Ignorable => {
                        debug!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "an AirGradient reading was refused"
                        );
                        Ok(())
                    }
                    Class::Fatal => {
                        error!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "the spool could not be written, so this node cannot durably \
                             accept readings"
                        );
                        Err(ComponentError::tick("admitting an AirGradient reading", e))
                    }
                    _ => {
                        warn!(
                            station = %self.station,
                            error = %DisplayChain(&e),
                            "an AirGradient reading could not be admitted"
                        );
                        Err(ComponentError::tick("admitting an AirGradient reading", e))
                    }
                }
            }
        }
    }
}

impl Tick for Poll {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            m::arm(cx.id());
            let frame = http::request(
                &self.endpoint,
                HttpMethod::Get,
                MEASURES_CURRENT,
                None,
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| ComponentError::tick("polling an AirGradient monitor", e))?;
            self.on_body(cx.id(), &frame).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingIngress;
    use mw_component::Reject;

    const BODY: &str = r#"{"atmpCompensated":20.0,"rhumCompensated":46.0,"rco2":612.0,
                           "pm02Compensated":2.5,"serialno":"ecda3bbd1f2c"}"#;

    fn poll(ingress: Arc<dyn Ingress>) -> Poll {
        Poll::new(
            Endpoint::parse("http://aqmon.lan").expect("a literal address"),
            StationKey::new("suncrest:aqmon"),
            ingress,
        )
    }

    fn id() -> ComponentId {
        station_id("suncrest:aqmon").expect("a legal id")
    }

    /// **Every** body produces an admission, which is the D-82 property for a polling
    /// driver: four occurrences, four readings.
    #[tokio::test]
    async fn every_occurrence_produces_a_reading() {
        let ingress = RecordingIngress::accepting();
        let poll = poll(Arc::clone(&ingress) as Arc<dyn Ingress>);
        for co2 in [612.0, 615.0, 611.0, 620.0f32] {
            let body = format!(r#"{{"rco2":{co2},"atmpCompensated":20.0}}"#);
            poll.on_body(&id(), &Frame::new(body.into_bytes()))
                .await
                .expect("the body is admitted");
        }
        let admissions = ingress.admissions();
        assert_eq!(admissions.len(), 4);
        assert_eq!(
            admissions
                .iter()
                .map(|a| a.batch[0].values.co2)
                .collect::<Vec<_>>(),
            vec![Some(612.0), Some(615.0), Some(611.0), Some(620.0)]
        );
        for admission in &admissions {
            assert_eq!(admission.receiver, "station:airgradient:suncrest:aqmon");
            assert_eq!(admission.presented_key, None, "a Fixed identity");
        }
    }

    #[tokio::test]
    async fn a_body_that_decodes_lands_the_device_metadata_too() {
        let ingress = RecordingIngress::accepting();
        poll(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_body(&id(), &Frame::new(BODY.as_bytes().to_vec()))
            .await
            .expect("admitted");
        let r = &ingress.admissions()[0].batch[0].values;
        assert_eq!(r.out_temp, Some(68.0));
        assert_eq!(r.co2, Some(612.0));
        assert_eq!(r.extra_text1.as_deref(), Some("ecda3bbd1f2c"));
    }

    /// A body the device should not have sent is a **failed occurrence**, where the Go
    /// driver logs and returns.
    #[tokio::test]
    async fn a_body_that_is_not_json_fails_the_occurrence() {
        let ingress = RecordingIngress::accepting();
        let e = poll(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_body(&id(), &Frame::new(b"<html>404</html>".to_vec()))
            .await
            .expect_err("not a measurement");
        assert!(
            e.to_string().contains("decoding an AirGradient response"),
            "{e}"
        );
        assert!(ingress.admissions().is_empty());
    }

    /// An unknown station is a counted refusal and **not** a failed occurrence: the
    /// component is working and the configuration is not (§4.4).
    #[tokio::test]
    async fn an_unknown_station_is_a_counted_refusal_rather_than_a_failed_occurrence() {
        let ingress = RecordingIngress::refusing(Reject::UnknownStation);
        poll(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_body(&id(), &Frame::new(BODY.as_bytes().to_vec()))
            .await
            .expect("an Ignorable refusal is not an occurrence failure");
    }

    #[test]
    fn the_period_the_floor_and_the_request_timeout_are_consistent() {
        assert_eq!(DEFAULT_POLL, Duration::from_secs(3));
        assert!(POLL_FLOOR <= DEFAULT_POLL);
        // `deadline <= period` is enforced at plan time, and the request has to finish
        // inside the deadline for its own message to be the one an operator reads.
        assert!(REQUEST_TIMEOUT < DEFAULT_POLL);
        assert_eq!(
            station_id("suncrest:aqmon").expect("a legal id").as_str(),
            "station:airgradient:suncrest:aqmon"
        );
        assert!(station_id("Air Monitor").is_err());
        assert_eq!(STATION_TYPE, "airgradient");
    }

    #[test]
    fn the_debug_representation_carries_no_handle() {
        let rendered = format!("{:?}", poll(RecordingIngress::accepting()));
        assert!(rendered.contains("aqmon.lan"), "{rendered}");
        assert!(rendered.contains("suncrest:aqmon"), "{rendered}");
        assert!(!rendered.contains("Ingress"), "{rendered}");
    }
}
