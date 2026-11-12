//! `POST /v1/ingest` — the **same** contract, not a second tier (§4.5, W7 **D-30**).
//!
//! W3 treats transport as a codec choice (**D-31**), so REST ingest must carry the same
//! contract or the fleet acquires two reliability tiers and the model is unclear again.
//! Everything that makes the gRPC path reliable is present here and is **the same code**:
//!
//! | | How |
//! |---|---|
//! | same admission | [`crate::service::Handlers::resolve`], the same three layers |
//! | same `accept`, same dedup gate | [`crate::service::Handlers::ingest_batch`] verbatim |
//! | same atom | one request is one `IngestBatch` is one append is one `fsync` |
//! | same idempotency | a retried POST after a lost response re-acks rather than duplicating |
//! | same cursor semantics | the sender advances to the response's `through`, which may be lower than the request's |
//!
//! What REST does **not** get is server-initiated flow control, so a REST sender paces
//! itself: one request in flight, and `Retry-After` honoured. The cost is throughput, not
//! reliability.
//!
//! **`known_through` is on every response** rather than once per stream, because there is
//! no stream to open; the sender uses the most recent one. That is the one place the two
//! codecs differ in what they send, and it follows from the absence of a stream rather than
//! from a decision.
//!
//! # The status codes, which are §4.5's table
//!
//! ```text
//! 200 OK   IngestAck { .., status: ACK_OK }
//! 200 OK   IngestAck { through: <unchanged>, status: ACK_RESOURCE_EXHAUSTED }   + Retry-After
//! 401/403  admission refused -- no body is read
//! 413      over 256 envelopes or 1 MiB
//! 422      IngestAck { status: ACK_REJECTED } -- every envelope failed a check
//! 503      not ready to accept                                                 + Retry-After
//! ```
//!
//! `Content-Type: application/x-protobuf` only. §4.5 also offers the protobuf **JSON**
//! mapping, and that is deliberately absent: `mw-proto`'s own module documentation records
//! that `pbjson-build` is not yet a dependency because "the protobuf JSON mapping has no
//! consumer until a REST surface needs it", and arming it means generating `Serialize` for
//! two packages and gating the output. One codec now, the second when a sender asks —
//! which is recorded in this item's PR rather than only here. A request with any other
//! content type gets `415` naming the one that works.

use std::sync::Arc;

use axum::response::IntoResponse as _;
use http::{header, HeaderValue, StatusCode};
use mw_proto::wire::collector::v1 as pb;
use prost::Message as _;
use tracing::{debug, warn};

use crate::service::ServiceState;

/// The one content type this handler decodes.
pub const CONTENT_TYPE: &str = "application/x-protobuf";

/// How long a sender should wait after a `503` or an `ACK_RESOURCE_EXHAUSTED`.
///
/// Thirty seconds. The condition both answers describe is a collector whose spool is at its
/// ceiling or whose component is not running, and neither clears in a second — a tighter
/// value would only make a stalled fleet retry faster.
pub const RETRY_AFTER_SECONDS: u32 = 30;

/// `POST /v1/ingest`.
pub async fn ingest(
    state: Arc<ServiceState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    // §4.5's `503` + `Retry-After`: the route exists and the component is not running,
    // which the sender's own retry is the right answer to.
    let Some(handlers) = state.handlers() else {
        return retry_after(
            StatusCode::SERVICE_UNAVAILABLE,
            "this node's ingest endpoint is not running",
        );
    };
    let (parts, body) = request.into_parts();
    // **Admission before the body is read**, which is §4.5's "401/403 -- no body is read"
    // and the same ordering the gRPC path has. A rejected sender costs one map lookup.
    let peer = match handlers.resolve(connection_from(&parts), &parts.headers) {
        Ok(peer) => peer,
        Err(status) => {
            // `403`, not `401`: there is no challenge to issue. A `WWW-Authenticate` on a
            // PSK endpoint would advertise the scheme to an unauthenticated caller.
            debug!(%status, "a REST ingest request was refused by admission");
            return (
                StatusCode::FORBIDDEN,
                "this node does not accept readings from you\n",
            )
                .into_response();
        }
    };

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    if content_type.as_deref() != Some(CONTENT_TYPE) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("POST {CONTENT_TYPE}\n"),
        )
            .into_response();
    }

    // Bounded by the batch's own byte limit before anything is buffered: without a limit
    // the body is an unbounded allocation a peer controls.
    let bytes = match axum::body::to_bytes(body, mw_pipeline::MAX_BATCH_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "a batch may carry at most {} bytes encoded\n",
                    mw_pipeline::MAX_BATCH_BYTES
                ),
            )
                .into_response()
        }
    };
    let batch = match pb::IngestBatch::decode(bytes) {
        Ok(batch) => batch,
        Err(e) => {
            debug!(peer = %peer.name, error = %e, "a REST ingest body did not decode");
            return (
                StatusCode::BAD_REQUEST,
                "the body is not an encoded collector.v1.IngestBatch\n",
            )
                .into_response();
        }
    };
    if batch.envelopes.len() > mw_pipeline::MAX_BATCH_ENVELOPES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "a batch may carry at most {} envelopes\n",
                mw_pipeline::MAX_BATCH_ENVELOPES
            ),
        )
            .into_response();
    }

    // `first = true` on every request, which is what puts `known_through` on every
    // response: there is no stream, so there is no "first ack of a stream" to put it on.
    let mut acked_through = 0u64;
    match handlers
        .ingest_batch(&peer, &batch, true, &mut acked_through)
        .await
    {
        Ok(ack) if ack.status == pb::AckStatus::AckResourceExhausted as i32 => {
            // §4.5: `200` with the unchanged `through` and a `Retry-After`, not an error
            // code -- the batch was understood and this node cannot hold it yet.
            let mut response = encoded(StatusCode::OK, &ack);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            response
        }
        Ok(ack) if ack.status == pb::AckStatus::AckRejected as i32 => {
            encoded(StatusCode::UNPROCESSABLE_ENTITY, &ack)
        }
        Ok(ack) => encoded(StatusCode::OK, &ack),
        Err(status) => {
            warn!(
                peer = %peer.name,
                %status,
                "a REST ingest request could not be acknowledged"
            );
            match status.code() {
                tonic::Code::InvalidArgument => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("{}\n", status.message()),
                )
                    .into_response(),
                _ => retry_after(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "this node cannot durably accept readings",
                ),
            }
        }
    }
}

/// What a request's connection presented, after the request has been split.
///
/// `None` for a request that did not arrive over the ingest socket, which
/// `crate::service::Handlers::presented` refuses -- the shared router means this path is
/// also routable on the website's port, and a request there has no handshake behind it.
fn connection_from(parts: &http::request::Parts) -> Option<&mw_http::PeerCerts> {
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<mw_http::PeerCerts>>()
        .map(|info| &info.0)
}

/// One `IngestAck` as a protobuf body.
fn encoded(status: StatusCode, ack: &pb::IngestAck) -> axum::response::Response {
    let mut body = Vec::with_capacity(ack.encoded_len());
    // Infallible into a `Vec`: `encode` only fails when the buffer cannot grow.
    if ack.encode(&mut body).is_err() {
        return retry_after(
            StatusCode::SERVICE_UNAVAILABLE,
            "the acknowledgement could not be encoded",
        );
    }
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE))],
        body,
    )
        .into_response()
}

/// A plain-text refusal with §4.5's `Retry-After`.
fn retry_after(status: StatusCode, message: &str) -> axum::response::Response {
    (
        status,
        [(header::RETRY_AFTER, HeaderValue::from_static("30"))],
        format!("{message}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Retry-After` the two transports advertise has to be the same number, or a
    /// sender that honours one and reads the other backs off wrongly.
    #[test]
    fn the_retry_after_header_matches_the_constant() {
        assert_eq!(RETRY_AFTER_SECONDS, 30);
        let response = retry_after(StatusCode::SERVICE_UNAVAILABLE, "not running");
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(RETRY_AFTER_SECONDS.to_string().as_str())
        );
    }

    #[test]
    fn an_ack_is_encoded_as_protobuf_with_the_content_type_it_claims() {
        let ack = pb::IngestAck {
            through: 4711,
            status: pb::AckStatus::AckOk as i32,
            ..Default::default()
        };
        let response = encoded(StatusCode::OK, &ack);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(CONTENT_TYPE)
        );
    }

    /// §4.5's limits are the gRPC path's limits, read from the same constants rather than
    /// re-spelled -- which is what "the same contract, not a second tier" means in code.
    #[test]
    fn the_rest_limits_are_the_same_constants_accept_enforces() {
        assert_eq!(mw_pipeline::MAX_BATCH_ENVELOPES, 256);
        assert_eq!(mw_pipeline::MAX_BATCH_BYTES, 1024 * 1024);
    }
}
