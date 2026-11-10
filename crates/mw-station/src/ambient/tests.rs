//! The Ambient endpoint: two stations on one route, and the refusals.
//!
//! Every test here drives [`Handler::upload`] over a query string, which is the whole of
//! what the endpoint does — the `axum` route is `mw_http::upload_table`'s and has its own
//! test over there. The one that matters is
//! [`tests::two_consoles_on_one_endpoint_resolve_to_two_stations`]: the Go driver cannot
//! express it without a second listener, and the identity it would use is the port.

use super::*;
use crate::testing::RecordingIngress;
use mw_codec::WindDirCorrection;
use mw_component::Reject;

/// A console's upload, with whatever parameters the test wants appended.
fn upload(id: &str, password: &str, extra: &str) -> String {
    format!(
        "ID={id}&PASSWORD={password}&action=updateraw\
         &dateutc=2026-11-10+22%3A49%3A09{extra}"
    )
}

fn receiver() -> ComponentId {
    endpoint_id().expect("a legal component id")
}

fn unit(claim: &str, station: &str, correction: i16) -> AmbientUnit {
    AmbientUnit {
        claim: claim.to_string(),
        station: StationKey::new(station),
        codec: AmbientQuery::new(WindDirCorrection::new(correction)),
    }
}

fn at() -> DateTime<Utc> {
    DateTime::from_timestamp(1_762_814_949, 0).expect("a literal instant")
}

fn handler(units: Vec<AmbientUnit>, ingress: Arc<dyn Ingress>) -> Handler {
    Handler::new(receiver(), units, ingress)
}

/// **The structural property.** One component, one route, two consoles, two stations — and
/// each decodes with its own vane calibration.
#[tokio::test]
async fn two_consoles_on_one_endpoint_resolve_to_two_stations() {
    let ingress = RecordingIngress::accepting();
    let handler = handler(
        vec![
            unit("roof", "suncrest:roof", 0),
            unit("meadow", "suncrest:meadow", 10),
        ],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    assert_eq!(handler.units(), 2);

    let roof = handler
        .upload(&upload("roof", "p1", "&tempf=68.4&winddir=355"), at())
        .await;
    let meadow = handler
        .upload(&upload("meadow", "p2", "&tempf=61.2&winddir=355"), at())
        .await;
    assert_eq!(roof, UploadReply::accepted(ACCEPTED_BODY));
    assert_eq!(meadow, UploadReply::accepted(ACCEPTED_BODY));

    let admissions = ingress.admissions();
    assert_eq!(admissions.len(), 2);
    // The claim travels as a claim, and `resolve` is what would turn it into a station.
    assert_eq!(admissions[0].presented_key.as_deref(), Some("roof"));
    assert_eq!(admissions[1].presented_key.as_deref(), Some("meadow"));
    // Both arrived at the SAME receiver, which is what `Cardinality::Many` means.
    for admission in &admissions {
        assert_eq!(admission.receiver, ID);
    }
    assert_eq!(admissions[0].batch[0].values.out_temp, Some(68.4));
    assert_eq!(admissions[1].batch[0].values.out_temp, Some(61.2));
    // Each console's own calibration, which is why the map holds a codec per station.
    assert_eq!(admissions[0].batch[0].values.wind_dir, Some(355.0));
    assert_eq!(admissions[1].batch[0].values.wind_dir, Some(5.0));
}

/// The upload's own timestamp, not the arrival instant.
#[tokio::test]
async fn the_uploads_dateutc_is_the_observation_instant() {
    let ingress = RecordingIngress::accepting();
    handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    )
    .upload(&upload("roof", "p", "&tempf=68.4"), at())
    .await;
    let admission = &ingress.admissions()[0];
    assert_eq!(
        admission.batch[0]
            .observed_at
            .expect("`dateutc` is required")
            .to_rfc3339(),
        "2026-11-10T22:49:09+00:00"
    );
    assert_eq!(
        admission.received_at,
        at(),
        "and the arrival instant is kept"
    );
}

/// The secret reaches the gate and is not compared here.
#[tokio::test]
async fn the_password_travels_to_the_gate_as_a_secret() {
    use mw_component::{AdmitError, Admitted, FrameMeta, Reject};
    use std::sync::Mutex as StdMutex;

    /// An `Ingress` that records what the frame presented, including the secret, so that
    /// "the driver hands the secret over" is assertable without the registry.
    #[derive(Debug, Default)]
    struct Recorder(StdMutex<Vec<(Option<String>, Option<String>)>>);
    impl Ingress for Recorder {
        fn admit<'a>(
            &'a self,
            _batch: &'a [mw_types::PartialReading],
            meta: &'a FrameMeta,
        ) -> BoxFuture<'a, Result<Admitted, AdmitError>> {
            Box::pin(async move {
                use secrecy::ExposeSecret;
                self.0.lock().expect("not poisoned").push((
                    meta.presented_key.clone(),
                    meta.presented_secret
                        .as_ref()
                        .map(|s| s.expose_secret().to_string()),
                ));
                assert_eq!(meta.source, mw_component::IdentitySource::FromQuery);
                Ok(Admitted {
                    accepted: 1,
                    rejected: 0,
                    seq_high: mw_types::Seq::new(1),
                })
            })
        }
        fn accept<'a>(
            &'a self,
            _batch: &'a [mw_types::Envelope],
            _from: mw_types::OriginId,
        ) -> BoxFuture<'a, Result<mw_component::Accepted, mw_component::AcceptError>> {
            Box::pin(async {
                Err(mw_component::AcceptError::Rejected(
                    Reject::UnauthorizedPeer,
                ))
            })
        }
    }

    let recorder = Arc::new(Recorder::default());
    handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&recorder) as Arc<dyn Ingress>,
    )
    .upload(&upload("roof", "hunter2", "&tempf=68.4"), at())
    .await;
    assert_eq!(
        *recorder.0.lock().expect("not poisoned"),
        vec![(Some("roof".to_string()), Some("hunter2".to_string()))]
    );
}

/// A claim case-folds, which is what `ConfigRegistry::resolve` does on its own side.
#[tokio::test]
async fn a_claim_resolves_regardless_of_case() {
    let ingress = RecordingIngress::accepting();
    let handler = handler(
        vec![unit("SunCrest:Roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    for claim in ["suncrest:roof", "SUNCREST:ROOF", "SunCrest:Roof"] {
        assert_eq!(
            handler
                .upload(&upload(claim, "p", "&tempf=68.4"), at())
                .await,
            UploadReply::accepted(ACCEPTED_BODY),
            "`{claim}`"
        );
    }
    assert_eq!(ingress.readings(), 3);
}

/// The protocol's required set, each refused by name and none of them admitted.
#[tokio::test]
async fn an_upload_missing_a_required_parameter_is_malformed_and_admits_nothing() {
    let ingress = RecordingIngress::accepting();
    let handler = handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    for query in [
        // No `action`.
        "ID=roof&PASSWORD=p&dateutc=2026-11-10+22%3A49%3A09",
        // The wrong `action`.
        "ID=roof&PASSWORD=p&action=updateraww&dateutc=2026-11-10+22%3A49%3A09",
        // No `ID`.
        "PASSWORD=p&action=updateraw&dateutc=2026-11-10+22%3A49%3A09",
        // An empty `ID`.
        "ID=&PASSWORD=p&action=updateraw&dateutc=2026-11-10+22%3A49%3A09",
        // No `PASSWORD`.
        "ID=roof&action=updateraw&dateutc=2026-11-10+22%3A49%3A09",
        // No `dateutc`, which the codec refuses.
        "ID=roof&PASSWORD=p&action=updateraw&tempf=68.4",
        // A `dateutc` that is not one.
        "ID=roof&PASSWORD=p&action=updateraw&dateutc=now",
    ] {
        assert_eq!(
            handler.upload(query, at()).await,
            UploadReply::refused(UploadStatus::Malformed, MALFORMED_BODY),
            "`{query}`"
        );
    }
    assert!(ingress.admissions().is_empty());
}

/// The Go defect, as the test that distinguishes the two: a console presenting an
/// identifier nothing is configured for is refused rather than trusted.
#[tokio::test]
async fn an_upload_claiming_a_station_this_node_has_no_row_for_is_refused() {
    let ingress = RecordingIngress::accepting();
    let reply = handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    )
    .upload(&upload("somebody-elses-console", "p", "&tempf=68.4"), at())
    .await;
    assert_eq!(
        reply,
        UploadReply::refused(UploadStatus::Unauthorised, UNAUTHORISED_BODY)
    );
    assert!(ingress.admissions().is_empty());
}

/// And the other half: the registry refusing a claim this endpoint *could* decode.
#[tokio::test]
async fn a_refusal_from_the_gate_is_unauthorised_and_not_a_bad_request() {
    for reason in [Reject::UnknownStation, Reject::BadSecret, Reject::Disabled] {
        let ingress = RecordingIngress::refusing(reason);
        let reply = handler(
            vec![unit("roof", "suncrest:roof", 0)],
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .upload(&upload("roof", "wrong", "&tempf=68.4"), at())
        .await;
        assert_eq!(
            reply,
            UploadReply::refused(UploadStatus::Unauthorised, UNAUTHORISED_BODY),
            "{reason:?}"
        );
    }
}

/// `totalrainin`, which is the one piece of state the driver holds.
#[tokio::test]
async fn the_rain_counter_becomes_an_increment_across_uploads() {
    let ingress = RecordingIngress::accepting();
    let handler = handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    for total in ["118.70", "118.75", "118.75"] {
        handler
            .upload(
                &upload("roof", "p", &format!("&rainin=0.02&totalrainin={total}")),
                at(),
            )
            .await;
    }
    let increments: Vec<Option<f32>> = ingress
        .admissions()
        .iter()
        .map(|a| a.batch[0].values.rain_incremental)
        .collect();
    assert_eq!(
        increments,
        vec![None, Some(0.050_003_05), Some(0.0)],
        "the first upload has no previous total, so the increment is absent rather than 0"
    );
}

/// Two consoles' counters do not interfere, which is what keying by claim buys.
#[tokio::test]
async fn each_consoles_rain_counter_is_its_own() {
    let ingress = RecordingIngress::accepting();
    let handler = handler(
        vec![
            unit("roof", "suncrest:roof", 0),
            unit("meadow", "suncrest:meadow", 0),
        ],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    handler
        .upload(&upload("roof", "p", "&totalrainin=100.00"), at())
        .await;
    handler
        .upload(&upload("meadow", "p", "&totalrainin=50.00"), at())
        .await;
    handler
        .upload(&upload("roof", "p", "&totalrainin=100.25"), at())
        .await;
    handler
        .upload(&upload("meadow", "p", "&totalrainin=50.50"), at())
        .await;
    let increments: Vec<Option<f32>> = ingress
        .admissions()
        .iter()
        .map(|a| a.batch[0].values.rain_incremental)
        .collect();
    assert_eq!(increments.len(), 4);
    assert_eq!(increments[0], None);
    assert_eq!(increments[1], None);
    assert_eq!(increments[2], Some(0.25));
    assert_eq!(increments[3], Some(0.5));
}

/// With no counter, `rainin` stays where the codec put it.
#[tokio::test]
async fn an_upload_with_no_rain_counter_keeps_the_hourly_value() {
    let ingress = RecordingIngress::accepting();
    handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    )
    .upload(&upload("roof", "p", "&rainin=0.02"), at())
    .await;
    assert_eq!(
        ingress.admissions()[0].batch[0].values.rain_incremental,
        Some(0.02)
    );
}

/// An upload that arrives before the component is running, or after it stops.
#[tokio::test]
async fn an_upload_with_no_handler_installed_is_unavailable_rather_than_a_404() {
    let state = Arc::new(UploadState::new());
    let endpoint = state.endpoint();
    assert!(state.handler().is_none());
    let reply = endpoint(Upload {
        query: upload("roof", "p", "&tempf=68.4"),
    })
    .await;
    assert_eq!(
        reply,
        UploadReply::refused(UploadStatus::Unavailable, UNAVAILABLE_BODY)
    );

    // ... and once it is installed the same upload is accepted through the same closure.
    let ingress = RecordingIngress::accepting();
    state.install_for_test(Arc::new(handler(
        vec![unit("roof", "suncrest:roof", 0)],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    )));
    let reply = endpoint(Upload {
        query: upload("roof", "p", "&tempf=68.4"),
    })
    .await;
    assert_eq!(reply, UploadReply::accepted(ACCEPTED_BODY));
    assert_eq!(ingress.readings(), 1);
}

#[test]
fn the_manifest_declares_the_mount_and_the_id_is_the_one_the_design_names() {
    let manifest = manifest(DEFAULT_MOUNT);
    assert_eq!(manifest.scope, Scope::Any);
    assert_eq!(
        manifest.entries,
        vec![RouteEntry::exact(DEFAULT_MOUNT, Methods::READ)]
    );
    assert_eq!(
        endpoint_id().expect("a legal id").as_str(),
        "station:ambient"
    );
    assert_eq!(STATION_TYPE, "ambient-customized");
    assert_eq!(DEFAULT_MOUNT, "/ingest/ambient");
}

#[test]
fn the_handlers_debug_representation_carries_no_handle_and_no_secret() {
    let rendered = format!(
        "{:?}",
        handler(
            vec![unit("roof", "suncrest:roof", 0)],
            RecordingIngress::accepting()
        )
    );
    assert!(rendered.contains("station:ambient"), "{rendered}");
    assert!(rendered.contains("units: 1"), "{rendered}");
    assert!(!rendered.contains("Ingress"), "{rendered}");
}
