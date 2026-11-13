//! `ingest:grpc` — `collector.v1.Collector`, and the ack that is the point of it.
//!
//! > `Ingest(stream IngestBatch) returns (stream IngestAck)`, replacing
//! > `SendWeatherReadings(stream WeatherReading) returns (Empty)` — under which no sender
//! > could ever learn what its collector persisted, with or without a local buffer.
//!
//! # What an ack means, which is the one thing to get right
//!
//! **An `IngestAck` means "durably appended to this collector's spool". It does not mean
//! "written to PostgreSQL" and it does not mean "accepted"** (W7 **D-21**).
//!
//! That is forced rather than chosen. The collector's own database write is a *cursor* on
//! the same spool (W3 §2), so making the ack wait for PostgreSQL would reintroduce exactly
//! the coupling W3 removed: a stalled database would back-pressure every satellite in the
//! fleet. The spool is itself durable and its writer cursor cannot lose data, so "spooled"
//! is a complete promise — and `Ingress::accept`'s own contract is that one call is one
//! append, one `fsync` and one atom, which is why there is no partial ack to express.
//!
//! The boundary between the three claims is what `tests/ack.rs` demonstrates: an ack
//! issued, the process killed, and the reading still present on restart — with no database
//! anywhere in the test.
//!
//! **`through` is in the *sender's* coordinate space** (W7 **D-22**). `IngestBatch.through`
//! is the sender's spool `seq` of the last envelope in the batch and the ack echoes it;
//! for a leaf satellite that is numerically the last envelope's own `seq`, and for a relay
//! it is a number no envelope carries, because a relay's log interleaves origins and a
//! cursor is a position in one log.
//!
//! # The three admission layers, and where the stream is
//!
//! | Layer | Where | What it refuses |
//! |---|---|---|
//! | 1 | `crate::handshake`, inside `rustls` | an unknown key — before a frame is decoded |
//! | 2 | [`Handlers::resolve`], before any method body | an unknown, revoked or wrongly-credentialled peer — **no stream is created** |
//! | 3 | `crate::admit::Gate`, per envelope | an origin this peer may not speak for, a station it may not write into, a clock too far out |
//!
//! Layer 2 is a function every method body takes its `PeerEntry` **from**, so there is no
//! method here that can run without one: the type is the interceptor. `tonic`'s own
//! interceptor API is not used because these services are `axum` routes on the shared
//! router (`mw_grpc::codec` has why nothing here is generated), and a function that returns
//! the thing the body needs is a stronger guarantee than a layer that runs beside it.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use axum::Router;
use chrono::Utc;
use mw_component::{
    AcceptError, BoxFuture, ComponentError, ComponentId, ComponentIdError, ComponentSpec,
    ConfigDigest, Cx, Enrolment, Ingress, Methods, Need, PeerEntry, PeerName, PeerRegistry,
    PeerStation, PeerWriteError, Provision, Publisher, Reject, RouteEntry, RouteManifest,
    RouteProvider, Scope, Serve, Trigger,
};
use mw_proto::wire::collector::v1 as pb;
use mw_types::{Class, Classify, DisplayChain, Envelope, OriginId};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use crate::admit::{Gate, Presented};
use crate::handshake::PeerAuthKind;
use crate::metrics as m;
use crate::wire;

/// The component's id. W7 **D-2**: `ingest:` rather than `station:`, because a peer is not
/// a station — it is a node that has stations.
pub const ID: &str = "ingest:grpc";

/// The gRPC metadata key a `psk` peer announces its `NodeId` in, as lower-case hex.
///
/// **A header rather than a message field, and that is layer 2's requirement rather than a
/// preference**: the peer has to be resolved *before* any method body runs, so the identity
/// cannot come out of a body nobody has decoded yet. Under either `mtls-*` kind this header
/// is **not consulted at all** — the certificate is the identity — so it cannot be used to
/// claim somebody else.
pub const NODE_ID_HEADER: &str = "x-mw-node-id";

/// How often one source address may attempt enrolment (§5.3).
pub const ENROL_RATE: Duration = Duration::from_secs(5);

/// How many distinct source addresses the limiter remembers.
///
/// Reaching it means a flood: more than this many distinct sources attempted enrolment
/// inside one [`ENROL_RATE`] window. Enrolment is one operator action on one machine, so
/// 1024 is three orders of magnitude above anything legitimate and still a trivial amount
/// of memory.
pub const ENROL_SOURCES: usize = 1024;

/// How many stations one node may declare in a `Hello`.
///
/// The owner's fleet is four nodes with a handful of stations each, so this is two orders
/// of magnitude above anything real — and it is what stops an unbounded repeated field
/// from being an unbounded `config.stations` table. A peer that needs more than this has a
/// topology nobody designed for, and should say so to an operator rather than to a `for`
/// loop.
pub const MAX_PEER_STATIONS: usize = 256;

/// What `HelloAck.max_batch_envelopes` advertises, which is what `accept` enforces.
pub const MAX_BATCH_ENVELOPES: u32 = mw_pipeline::MAX_BATCH_ENVELOPES as u32;

/// Everything one connection is answered from.
pub struct Handlers {
    receiver: ComponentId,
    peers: Arc<dyn PeerRegistry>,
    ingress: Arc<dyn Ingress>,
    spool: Arc<mw_pipeline::Spool>,
    gate: Gate,
    auth: PeerAuthKind,
    /// The last enrolment attempt per source address (§5.3's rate limit).
    enrolments: Mutex<std::collections::HashMap<IpAddr, Instant>>,
    /// The `version` label each peer's `peer_software_info` series currently carries, so
    /// that a peer which reports a new version does not leave the old series reading 1.
    /// Bounded by the `peers` table for `last_seen`'s reason.
    software: Mutex<std::collections::BTreeMap<PeerName, String>>,
    /// When each peer was last heard from, for §7's `peer_last_seen_seconds`.
    ///
    /// **Bounded by the `peers` table**, not by what connects: a key is written only for a
    /// peer that layer 2 already resolved against a configured row, so an unenrolled
    /// caller cannot grow this map. Seeded at mount with every configured peer, so a peer
    /// that has never connected reads as silent-since-start rather than as an absent
    /// series.
    last_seen: Mutex<std::collections::BTreeMap<PeerName, Instant>>,
}

impl fmt::Debug for Handlers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handlers")
            .field("receiver", &self.receiver)
            .field("auth", &self.auth)
            .field("node", &self.gate.node())
            .finish_non_exhaustive()
    }
}

impl Handlers {
    /// The handlers for one ingest endpoint.
    pub fn new(
        receiver: ComponentId,
        peers: Arc<dyn PeerRegistry>,
        ingress: Arc<dyn Ingress>,
        spool: Arc<mw_pipeline::Spool>,
        gate: Gate,
        auth: PeerAuthKind,
    ) -> Self {
        Self {
            receiver,
            peers,
            ingress,
            spool,
            gate,
            auth,
            enrolments: Mutex::new(std::collections::HashMap::new()),
            last_seen: Mutex::new(std::collections::BTreeMap::new()),
            software: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Record what `peer` reports itself to be, replacing the series it carried before.
    fn note_software(&self, peer: &PeerName, version: &str) {
        let label = m::software_label(version);
        let mut held = self
            .software
            .lock()
            .expect("the software map is not held across an await");
        m::peer_software(peer, version, held.get(peer).map(String::as_str));
        held.insert(peer.clone(), label.to_string());
    }

    /// Note that `peer` has just been heard from: the gauge resets and the sweep below has
    /// an instant to measure from.
    fn seen(&self, peer: &PeerName) {
        m::peer_seen(peer);
        self.last_seen
            .lock()
            .expect("the last-seen map is not held across an await")
            .insert(peer.clone(), Instant::now());
    }

    /// Recompute `peer_last_seen_seconds` for every peer this collector knows.
    ///
    /// **Swept on the scrape path, which is M4-1's decision reused** (see
    /// `mw_pipeline::metrics::sweep_snapshot_ages`, whose documentation states it): the
    /// gauge is written when a peer arrives, so a peer that goes quiet freezes it at zero
    /// and an alert on `peer_last_seen_seconds > 300` never fires for the silence it exists
    /// to catch. §7 names silence as "the failure mode a stream-based protocol hides", so a
    /// gauge that cannot show it is the one gauge not worth having.
    ///
    /// Sweeping rather than running a timer for the reason M4-1 recorded: a timer is a
    /// second clock and costs work on a daemon nobody scrapes, while the scrape is the
    /// moment the number is read. The cost is one iteration over the `peers` table.
    pub fn sweep_last_seen(&self) {
        for (peer, age) in self.ages() {
            m::peer_last_seen(&peer, age.as_secs_f64());
        }
    }

    /// What [`Handlers::sweep_last_seen`] is about to report, as a value a test can read.
    ///
    /// Separate because a gauge cannot be read back without a recorder, and this workspace
    /// has no metrics test dependency (**D-32**).
    fn ages(&self) -> Vec<(PeerName, std::time::Duration)> {
        let seen = self
            .last_seen
            .lock()
            .expect("the last-seen map is not held across an await");
        seen.iter()
            .map(|(peer, at)| (peer.clone(), at.elapsed()))
            .collect()
    }

    /// Seed the map so that a configured peer which has never connected is a series rather
    /// than an absence. Its age then runs from the mount, which is the honest reading: this
    /// node has not heard from it since it started.
    fn seed_last_seen(&self, peers: &[PeerEntry]) {
        let now = Instant::now();
        let mut seen = self
            .last_seen
            .lock()
            .expect("the last-seen map is not held across an await");
        for peer in peers {
            seen.entry(peer.name.clone()).or_insert(now);
        }
    }

    /// **Layer 2**: what this connection presented, or why it is not a peer.
    ///
    /// `connection` is `None` for a request that did not arrive over the ingest socket at
    /// all — the shared router means `/collector.v1.Collector/*` is also routable on the
    /// website's port (§6.1: "the unit that is singular is the router"), and a request
    /// there has no handshake behind it. That is refused rather than falling back to the
    /// header, which would make the whole of layer 1 optional.
    pub fn presented(
        &self,
        connection: Option<&mw_http::PeerCerts>,
        headers: &http::HeaderMap,
    ) -> Result<Presented, Reject> {
        let connection = connection.ok_or(Reject::UnauthorizedPeer)?;
        // **The two `mtls-*` kinds require an encrypted socket, and that is checked here
        // rather than inferred from the absence of a certificate.** A plaintext ingest
        // socket exists -- W7 **D-35**'s explicit opt-out puts one there -- and on it a
        // certificate cannot have been presented, so without this the refusal below would
        // read as "this peer sent no certificate" when the truth is "this socket could not
        // have received one". `psk` is the one kind that works there, which is the whole
        // content of the opt-out.
        if self.auth.wants_client_certificate() && !connection.tls() {
            warn!(
                component = %self.receiver,
                auth = self.auth.as_str(),
                "a connection reached the ingest endpoint over a socket with no TLS, so no \
                 certificate could have been presented and this `peer_auth` cannot \
                 authenticate anybody. Only `peer_auth = 'psk'` is usable with \
                 `config.ingest.allow_insecure = 1`."
            );
            return Err(Reject::UnauthorizedPeer);
        }
        match self.auth {
            PeerAuthKind::MtlsCa => {
                let leaf = connection.leaf().ok_or(Reject::UnauthorizedPeer)?;
                crate::x509::node_from_san(&leaf)
                    .map(|node| Presented::CaValidated { node })
                    .ok_or(Reject::UnauthorizedPeer)
            }
            PeerAuthKind::MtlsPinned => {
                let leaf = connection.leaf().ok_or(Reject::UnauthorizedPeer)?;
                // The handshake already matched this key against a `peers` row -- that is
                // what let the connection exist -- so this re-derives *which* row, from the
                // same bytes and the same digest. It cannot admit a key the verifier
                // refused.
                let spki = crate::x509::spki(&leaf).ok_or(Reject::UnauthorizedPeer)?;
                self.peers
                    .peer_by_spki(&crate::handshake::sha256(&spki))
                    .map(|peer| Presented::Pinned { node: peer.node })
                    .ok_or(Reject::UnauthorizedPeer)
            }
            PeerAuthKind::Psk => {
                let node = headers
                    .get(NODE_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(node_id_from_hex)
                    .ok_or(Reject::UnauthorizedPeer)?;
                let secret = bearer(headers).ok_or(Reject::BadSecret)?;
                Ok(Presented::Psk { node, secret })
            }
        }
    }

    /// [`Handlers::presented`] and then `crate::admit::resolve`, as the one call every
    /// method body makes before it does anything else.
    pub fn resolve(
        &self,
        connection: Option<&mw_http::PeerCerts>,
        headers: &http::HeaderMap,
    ) -> Result<PeerEntry, Status> {
        let presented = self
            .presented(connection, headers)
            .and_then(|p| crate::admit::resolve(self.peers.as_ref(), &p).map(|peer| (p, peer)));
        match presented {
            Ok((presented, peer)) => {
                debug!(
                    peer = %peer.name,
                    auth = presented.kind(),
                    "a peer was resolved before any method body ran"
                );
                Ok(peer)
            }
            Err(reason) => {
                // **No peer label.** There is no resolved peer here by definition, and
                // labelling a counter with what an unauthenticated request claimed is
                // exactly §5.10's cardinality rule being broken. The node-wide handshake
                // counter is where this lands.
                m::handshake_refused();
                warn!(
                    component = %self.receiver,
                    reason = reason.as_str(),
                    auth = self.auth.as_str(),
                    "a connection to the ingest endpoint is not a peer, so no stream was \
                     created and nothing was read"
                );
                Err(Status::permission_denied(
                    "this node does not accept readings from you",
                ))
            }
        }
    }

    /// `Hello` — identity only. No secrets, ever (§5.2).
    pub async fn hello(
        &self,
        peer: &PeerEntry,
        identity: &pb::NodeIdentity,
    ) -> Result<pb::HelloAck, Status> {
        // The body's claimed identity must be the one the connection proved. A peer
        // announcing another node's id is not a naming mistake: `known_through` and the
        // station registration below are both written against it.
        let claimed = node_id(&identity.node_id)
            .ok_or_else(|| Status::invalid_argument("`node_id` is not the 16 bytes a NodeId is"))?;
        if claimed != peer.node {
            warn!(
                peer = %peer.name,
                claimed = %claimed,
                "a peer's Hello announced a different node id from the one it \
                 authenticated as"
            );
            return Err(Status::permission_denied(
                "`node_id` is not the node this connection authenticated as",
            ));
        }
        // **Bounded, and refused rather than truncated.** `NodeIdentity.stations` is an
        // unbounded repeated field, and each entry becomes a `config.stations` row and a
        // resident snapshot entry -- so a compromised leaf looping `Hello` with fresh keys
        // would grow a table and this process's memory at one transaction per call.
        // Refusing is what makes the limit visible to whoever has to raise it; truncating
        // would register some of a satellite's stations and silently refuse envelopes for
        // the rest.
        if identity.stations.len() > MAX_PEER_STATIONS {
            warn!(
                peer = %peer.name,
                declared = identity.stations.len(),
                limit = MAX_PEER_STATIONS,
                "a peer declared more stations than one node may own, so nothing was \
                 registered"
            );
            return Err(Status::invalid_argument(format!(
                "a node may declare at most {MAX_PEER_STATIONS} stations"
            )));
        }
        let stations: Vec<PeerStation> =
            identity.stations.iter().filter_map(wire::station).collect();
        // **The registration path** (§5.1, W2a Decision 45). Without it a satellite that
        // authenticates perfectly stores nothing, because every envelope it sends names a
        // station key this node has no row for.
        self.peers
            .record_hello(peer.node, &identity.software, &stations)
            .await
            .map_err(|e| peer_write_status(&e, "registering your stations"))?;

        self.seen(&peer.name);
        // A `Hello` is the only thing that can replace `unknown` with a version, which is
        // what makes the label mean "this peer speaks the new protocol".
        // `software_label` handles the empty case (a peer that sent no version reads as
        // `unknown`, which is §9's Go-era signal) and bounds what a peer can put in a label.
        self.note_software(&peer.name, &identity.software);
        let accepted: Vec<String> = stations
            .iter()
            .filter(|s| self.gate.station_owner(&s.station) == Some(peer.node))
            .map(|s| s.station.as_str().to_string())
            .collect();
        if accepted.len() != stations.len() {
            // A key this peer does not own -- one of this node's own stations, or another
            // peer's. The upsert left it alone and envelopes naming it will be refused, so
            // saying so at connect is what stops that becoming a mystery per envelope.
            warn!(
                peer = %peer.name,
                declared = stations.len(),
                accepted = accepted.len(),
                "some of a peer's declared stations are owned by another node, so they \
                 were not registered and envelopes naming them will be refused"
            );
        }
        info!(
            peer = %peer.name,
            software = %identity.software,
            stations = accepted.len(),
            "a peer said hello"
        );
        Ok(pb::HelloAck {
            collector_node_id: self.gate.node().as_bytes().to_vec(),
            accepted_stations: accepted,
            known_through: self.known_through(peer),
            server_time: wire::millis(Utc::now()),
            max_batch_envelopes: MAX_BATCH_ENVELOPES,
        })
    }

    /// The collector's per-origin high water, for this peer's origins only (§4.2).
    ///
    /// **This peer's origins and not every origin this node has ever seen**: the vector is
    /// advisory fast-resume, a sender can only use the entries for origins it sends for,
    /// and handing over the rest would tell one satellite how far along every other node in
    /// the fleet is.
    fn known_through(&self, peer: &PeerEntry) -> Vec<pb::OriginHigh> {
        std::iter::once(peer.node)
            .chain(peer.origins.iter().copied())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|origin| pb::OriginHigh {
                origin: origin.as_bytes().to_vec(),
                seq: self.spool.high_water(origin).get(),
            })
            .collect()
    }

    /// One `IngestBatch`: §4.4's steps 2–11, and the ack that says how far the sender got.
    ///
    /// `Err` ends the stream. `Ok` with a non-`ACK_OK` status does not: a satellite whose
    /// collector is full must back off and keep the connection, because reconnecting would
    /// cost a `Hello` and change nothing.
    pub async fn ingest_batch(
        &self,
        peer: &PeerEntry,
        batch: &pb::IngestBatch,
        first: bool,
        acked_through: &mut u64,
    ) -> Result<pb::IngestAck, Status> {
        self.seen(&peer.name);
        m::peer_depth(&peer.name, batch.sender_depth);
        // The count bound before anything is decoded, because decoding is the work a peer
        // can make this node do by asking. The encoded-size bound is `accept`'s, where the
        // append is -- one limit, enforced where it matters.
        if batch.envelopes.len() > mw_pipeline::MAX_BATCH_ENVELOPES {
            m::rejected(&peer.name, Reject::BatchTooLarge, 1);
            warn!(
                peer = %peer.name,
                envelopes = batch.envelopes.len(),
                limit = mw_pipeline::MAX_BATCH_ENVELOPES,
                "a peer's batch is over the envelope limit, so the stream is closed: \
                 trimming it would make the ack a claim about envelopes never seen"
            );
            return Err(Status::invalid_argument(format!(
                "a batch may carry at most {} envelopes",
                mw_pipeline::MAX_BATCH_ENVELOPES
            )));
        }

        let now = Utc::now();
        let mut rejected = 0u32;
        // Refusals this node's own state produced, as against refusals the bytes produced
        // (`Reject::is_transient`). One of these is enough to stop the ack advancing, and
        // that distinction is the difference between a bounded stall and silent loss.
        let mut transient = 0u32;
        let mut candidates: Vec<Envelope> = Vec::with_capacity(batch.envelopes.len());
        for wire in &batch.envelopes {
            // The decode is where provenance is **re-sealed**: three of the six receive
            // clauses are a property of `Provenance::sealed` and cannot be bypassed by any
            // bytes a peer sends (`mw_proto::envelope`).
            let envelope = match Envelope::try_from(wire) {
                Ok(envelope) => envelope,
                Err(e) => {
                    // A malformed envelope is dropped and counted with the batch still
                    // acked (W7 **D-8**): a batch retried forever over one bad member is a
                    // satellite that never drains.
                    m::rejected(&peer.name, Reject::MalformedPath, 1);
                    rejected += 1;
                    debug!(
                        peer = %peer.name,
                        error = %DisplayChain(&e),
                        "an envelope did not decode and was dropped"
                    );
                    continue;
                }
            };
            // The three positional receive clauses (W7 **D-7**), then layer 3 and §4.6.
            if let Err(reason) =
                mw_pipeline::receive_check(envelope.provenance.path(), self.gate.node(), peer.node)
                    .and_then(|()| self.gate.check(&envelope, peer, now))
            {
                m::rejected(&peer.name, reason, 1);
                rejected += 1;
                if reason.is_transient() {
                    transient += 1;
                    // At `WARN`, not `DEBUG`: every reason in this branch is something an
                    // operator can fix, and the sender is about to be told to back off
                    // rather than to carry on -- so the line that says why has to be
                    // visible at the default level.
                    warn!(
                        peer = %peer.name,
                        reason = reason.as_str(),
                        origin = %envelope.provenance.origin(),
                        seq = envelope.provenance.seq().get(),
                        station = %envelope.station,
                        "an envelope was refused for a reason this node's own configuration \
                         or clock decides, so the sender is told to retry rather than to \
                         advance past it"
                    );
                } else {
                    debug!(
                        peer = %peer.name,
                        reason = reason.as_str(),
                        origin = %envelope.provenance.origin(),
                        seq = envelope.provenance.seq().get(),
                        "an envelope was refused before anything was stored"
                    );
                }
                continue;
            }
            candidates.push(envelope);
        }

        let accepted = match self.ingress.accept(&candidates, peer.node).await {
            Ok(accepted) => accepted,
            Err(AcceptError::Rejected(reason)) => {
                m::rejected(&peer.name, reason, 1);
                warn!(
                    peer = %peer.name,
                    reason = reason.as_str(),
                    "a peer's batch was refused whole, so the stream is closed"
                );
                return Err(Status::invalid_argument(format!(
                    "this batch was refused: {reason}"
                )));
            }
            Err(e) if matches!(e.class(), Class::Retryable { .. }) => {
                // **The ceiling, and one of the two statuses whose `through` does not
                // move.** The collector's spool being full is the collector's `Failed`, not
                // the satellite's (§4.3), so this answers rather than closing the stream
                // and the sender is told to send the batch again.
                //
                // Whether the *connection* then survives is the sender's call and not
                // this one's: `mw_collector::uplink` drops the session, because a
                // `Trigger::OnReading` body reports a failed batch by returning `Err` and
                // the supervisor's answer to that is `Failed -> Backoff -> Starting`
                // (W2a Decisions 28 and 29) -- which tears the component down whatever this
                // end does with the stream. Keeping it open here costs nothing and is what
                // a different sender, or a REST one, would want.
                warn!(
                    peer = %peer.name,
                    error = %DisplayChain(&e),
                    "this collector cannot durably accept more envelopes, so the batch is \
                     refused and the sender is told to back off"
                );
                return Ok(pb::IngestAck {
                    through: *acked_through,
                    known_through: Vec::new(),
                    status: pb::AckStatus::AckResourceExhausted as i32,
                    duplicates: 0,
                    rejected,
                });
            }
            Err(e) => {
                error!(
                    peer = %peer.name,
                    error = %DisplayChain(&e),
                    "the spool could not be written, so this node cannot acknowledge \
                     anything"
                );
                return Err(Status::unavailable(
                    "this node cannot durably accept readings; please retry",
                ));
            }
        };

        // Labelled with the **origin** and not the station: the origin is bounded by this
        // peer's own grant, which is configuration, and the station key is not
        // (`crate::metrics`).
        count_by_origin(
            &peer.name,
            &candidates,
            accepted.accepted,
            accepted.duplicates,
        );
        // The spool's own capacity refusal is a function of this node's state too, so it
        // joins the transient count rather than the permanent one.
        let rejected = rejected + accepted.rejected;
        let transient = transient + accepted.rejected;

        // **The one thing an ack must never do is advance a sender past an envelope this
        // node might have accepted.**
        //
        // `through` is cumulative in the sender's log, so acknowledging a batch licenses the
        // sender to advance past *every* envelope in it. That is correct for a refusal the
        // envelope's own bytes earned -- a forged path does not become valid by being
        // re-sent, and a sender that kept re-sending it would never drain, which is the
        // stall W7 **D-9** forbids. It is **wrong** for a refusal this node's state earned:
        // a wrong clock here, a station not yet registered here, an origin grant an operator
        // has not written here. `Reject::is_transient` is that distinction and its
        // documentation has the worked example.
        //
        // So: any transient refusal in the batch and `through` does not move. The sender
        // backs off, the batch replays, and the envelopes that *were* accepted are absorbed
        // by the dedup gate on the way through (W7 **D-29**) -- which is what makes the
        // replay cheap enough for this to be the default.
        let status = if transient > 0 {
            // Not `ACK_REJECTED`: that tells a sender "do not retry blindly", and retrying
            // is exactly what it should do. `ACK_RESOURCE_EXHAUSTED` already means "I could
            // not take this; back off and send it again", which is the whole of what a
            // transient refusal needs to say.
            warn!(
                peer = %peer.name,
                transient,
                permanent = rejected - transient,
                accepted = accepted.accepted,
                "{transient} envelope(s) in this batch were refused for a reason this node \
                 can stop refusing, so the acknowledgement does NOT advance the sender's \
                 cursor: it will re-send them. The per-envelope lines above name each one."
            );
            pb::AckStatus::AckResourceExhausted
        } else if accepted.accepted == 0 && rejected > 0 {
            // Every envelope was refused permanently. The sender advances, because nothing
            // here will ever be acceptable -- and `ACK_REJECTED` is what tells it to stop
            // and look rather than to retry.
            *acked_through = batch.through;
            pb::AckStatus::AckRejected
        } else {
            *acked_through = batch.through;
            pb::AckStatus::AckOk
        };
        Ok(pb::IngestAck {
            // **Meaningless unless `status == ACK_OK`** (the field's own documentation in
            // `collector.proto`). On either other status this is the last position that
            // *was* durably acknowledged on this stream, which is `0` when none has been --
            // so a sender must not move a cursor on anything but `ACK_OK`.
            through: *acked_through,
            // Once per stream, on the first ack (W7 **D-23**).
            known_through: if first {
                self.known_through(peer)
            } else {
                Vec::new()
            },
            status: status as i32,
            duplicates: accepted.duplicates,
            rejected,
        })
    }

    /// `Enroll` — the one operation that can add a peer (§5.3).
    ///
    /// Not behind layer 2, by construction: a node that is enrolling has no `peers` row
    /// yet, so requiring one would make the bootstrap impossible. What stands in its place
    /// is a separate gate that is **closed by default**: enrolment is served only while an
    /// operator has minted a token.
    pub async fn enrol(
        &self,
        source: IpAddr,
        request: &pb::EnrolRequest,
    ) -> Result<pb::EnrolGrant, Status> {
        if !self.admit_enrolment(source) {
            m::enrolment("rate_limited");
            info!(%source, "an enrolment attempt was rate-limited");
            return Err(Status::resource_exhausted(
                "enrolment attempts are limited to one every five seconds per source",
            ));
        }
        // Checked before the token is looked at, so that "enrolment is closed" and "that
        // token is wrong" are two answers an operator can tell apart (**D-33**).
        match self.peers.enrolment_open().await {
            Ok(true) => {}
            Ok(false) => {
                m::enrolment("closed");
                info!(
                    %source,
                    "an enrolment attempt arrived while enrolment is closed"
                );
                return Err(Status::failed_precondition(
                    "enrolment is closed: ask the operator of this collector to mint a \
                     token",
                ));
            }
            Err(e) => {
                m::enrolment("error");
                return Err(peer_write_status(&e, "checking whether enrolment is open"));
            }
        }
        let identity = request
            .identity
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("an enrolment carries a `NodeIdentity`"))?;
        let node = node_id(&identity.node_id)
            .ok_or_else(|| Status::invalid_argument("`node_id` is not the 16 bytes a NodeId is"))?;
        if request.spki.is_empty() {
            m::enrolment("no_material");
            return Err(Status::invalid_argument(
                "an enrolment carries the DER SubjectPublicKeyInfo of the keypair this \
                 node generated",
            ));
        }
        let name = identity.node_name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument(
                "an enrolment carries a `node_name`, which is what an operator sees",
            ));
        }
        let enrolment = Enrolment {
            token: request.token.clone().into(),
            node,
            name: name.to_string(),
            software: identity.software.clone(),
            spki_sha256: crate::handshake::sha256(&request.spki),
            stations: identity.stations.iter().filter_map(wire::station).collect(),
        };
        match self.peers.enrol(&enrolment).await {
            Ok(enrolled) => {
                m::enrolment("accepted");
                // Logged at INFO with the source and the outcome, as §5.3 requires -- and
                // with no token in it, because the token is a `SecretString` and this is
                // the log line the current management token is printed in cleartext on.
                info!(
                    %source,
                    peer = %enrolled.peer.name,
                    node = %node,
                    auth_kind = enrolled.peer.auth.kind(),
                    "a peer enrolled"
                );
                Ok(pb::EnrolGrant {
                    pinned_spki_sha256: enrolled
                        .pinned_spki_sha256
                        .map(|d| d.to_vec())
                        .unwrap_or_else(Vec::new),
                    collector_node_id: self.gate.node().as_bytes().to_vec(),
                    auth_kind: enrolled.peer.auth.kind().to_string(),
                })
            }
            Err(e) => {
                let outcome = match &e {
                    PeerWriteError::BadToken => "bad_token",
                    PeerWriteError::EnrolmentClosed => "closed",
                    PeerWriteError::NoMaterial => "no_material",
                    PeerWriteError::TooManyStations { .. } => "too_many_stations",
                    PeerWriteError::NameTaken { .. } => "name_taken",
                    PeerWriteError::CannotIssueCertificates => "cannot_issue",
                    PeerWriteError::Backend { .. } => "error",
                };
                m::enrolment(outcome);
                info!(%source, outcome, "an enrolment attempt was refused");
                Err(match &e {
                    PeerWriteError::EnrolmentClosed => Status::failed_precondition(
                        "enrolment is closed: ask the operator of this collector to mint \
                         a token",
                    ),
                    PeerWriteError::BadToken => {
                        Status::permission_denied("that enrolment token was not accepted")
                    }
                    other => peer_write_status(other, "enrolling"),
                })
            }
        }
    }

    /// §5.3's rate limit: one attempt per [`ENROL_RATE`] per source address.
    ///
    /// Per address rather than global, because a global limit lets one source lock every
    /// other operator out of enrolling — which is a denial of service with a one-line
    /// implementation.
    /// **A map with a cap, not a scan.** The first version kept a `Vec` and ran a `retain`
    /// plus a linear search on every attempt, so the cost of one request was proportional
    /// to the number of *distinct* sources seen in the window — which an attacker with a
    /// wide source range chooses, making the limiter quadratic in their own request rate.
    /// A hash lookup is constant, and the sweep is amortised: it runs only when the table
    /// reaches [`ENROL_SOURCES`], which also bounds the memory a spoofed-source flood can
    /// cost. If a sweep frees nothing — more than `ENROL_SOURCES` distinct sources inside
    /// one window — the attempt is refused rather than growing the table, because that
    /// shape is a flood and not an operator.
    fn admit_enrolment(&self, source: IpAddr) -> bool {
        let now = Instant::now();
        let mut seen = self
            .enrolments
            .lock()
            .expect("the enrolment limiter is not poisoned");
        if let Some(at) = seen.get(&source) {
            if now.duration_since(*at) < ENROL_RATE {
                return false;
            }
        }
        if seen.len() >= ENROL_SOURCES {
            seen.retain(|_, at| now.duration_since(*at) < ENROL_RATE);
            if seen.len() >= ENROL_SOURCES {
                warn!(
                    sources = seen.len(),
                    "more than {ENROL_SOURCES} distinct sources attempted enrolment inside \
                     one rate window, so attempts are refused until it drains"
                );
                return false;
            }
        }
        seen.insert(source, now);
        true
    }

    /// The component this is serving for, for a log field.
    pub fn receiver(&self) -> &ComponentId {
        &self.receiver
    }
}

/// `mw_ingest_envelopes_total{peer, origin}`, one increment per origin in the batch.
///
/// Counted per origin rather than per envelope because the counter is a *sum* and the
/// label set is what costs: a 256-envelope batch from one origin is one map lookup here and
/// 256 there. `accepted` is the number the spool actually appended, and it is attributed to
/// the origins present in proportion to nothing — with one origin, which is every leaf, it
/// is exact; with several it is the batch's own composition. Stated rather than hidden,
/// because `ingest_duplicates_total` is the series that tells an operator the difference.
fn count_by_origin(peer: &PeerName, candidates: &[Envelope], accepted: u32, duplicates: u32) {
    let mut by_origin: std::collections::BTreeMap<OriginId, u64> =
        std::collections::BTreeMap::new();
    for envelope in candidates {
        *by_origin.entry(envelope.provenance.origin()).or_insert(0) += 1;
    }
    let total: u64 = by_origin.values().sum();
    if total == 0 {
        return;
    }
    for (origin, count) in by_origin {
        let origin = origin.to_string();
        // Scaled by what the spool actually appended, so the counter is envelopes stored
        // rather than envelopes offered. Integer arithmetic, floor: with one origin -- every
        // leaf, and every relay forwarding one node's batch -- it is exact, and with
        // several the remainder is at most one per origin per batch.
        let stored = u64::from(accepted) * count / total;
        if stored > 0 {
            m::ingested(peer, &origin, stored);
        }
        let deduped = u64::from(duplicates) * count / total;
        if deduped > 0 {
            // §2.3's diamond: a non-zero count here is the dedup gate working, not a fault.
            m::duplicates(&origin, deduped);
        }
    }
}

/// 16 bytes as a `NodeId`.
fn node_id(bytes: &[u8]) -> Option<OriginId> {
    <[u8; 16]>::try_from(bytes).ok().map(OriginId::new)
}

/// A lower- or upper-case hex `NodeId` from [`NODE_ID_HEADER`].
///
/// Digit by digit rather than through `u8::from_str_radix`, which accepts a leading `+`:
/// `+1+1…` is 32 characters and parses, so the same node would have had two spellings.
/// Two spellings of one identity is how a revocation comes to miss.
fn node_id_from_hex(text: &str) -> Option<OriginId> {
    let text = text.trim();
    if text.len() != 32 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = char::from(bytes[i * 2]).to_digit(16)?;
        let lo = char::from(bytes[i * 2 + 1]).to_digit(16)?;
        *byte = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(OriginId::new(out))
}

/// `Authorization: Bearer <psk>`, as a [`secrecy::SecretString`] (**D-33**).
fn bearer(headers: &http::HeaderMap) -> Option<secrecy::SecretString> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let secret = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(secrecy::SecretString::from(secret.trim().to_string()))
}

/// A registry write failure, as the `Status` a satellite should act on.
fn peer_write_status(e: &PeerWriteError, what: &str) -> Status {
    match e.class() {
        Class::Ignorable => Status::invalid_argument(format!("{what} failed: {e}")),
        _ => {
            warn!(error = %DisplayChain(e), "{what} failed");
            Status::unavailable(format!("this node could not complete {what}; please retry"))
        }
    }
}

// ---------------------------------------------------------------------------------------
// The routes, and the slot the running component fills
// ---------------------------------------------------------------------------------------

/// The slot the running component installs its [`Handlers`] into.
///
/// `mw_api_legacy::RouteState`'s pattern, for its reason: the route is built before `plan()`
/// runs because the listener needs it, and the component is what makes it answer — so "the
/// service is mounted" and "the component is `Running`" are the same fact.
#[derive(Debug, Default)]
pub struct ServiceState {
    handlers: ArcSwapOption<Handlers>,
}

impl ServiceState {
    /// An empty slot, which is what the binary builds before `plan()` runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current handlers, or `None` while the component is not running.
    pub fn handlers(&self) -> Option<Arc<Handlers>> {
        self.handlers.load_full()
    }

    fn install(&self, handlers: Arc<Handlers>) {
        self.handlers.store(Some(handlers));
    }

    /// [`ServiceState::install`], for a test that drives the service without a supervisor.
    #[doc(hidden)]
    pub fn install_for_test(&self, handlers: Arc<Handlers>) {
        self.install(handlers);
    }

    fn clear(&self) {
        self.handlers.store(None);
    }
}

/// Every path this service claims, as **D-51**'s pure declaration.
///
/// **Driven by which `config.ingest` rows exist**, because the two listeners are configured
/// separately: §8.3's collector has a `grpc` row and a `rest` row, and the schema admits
/// either alone. Declaring all four paths whatever the rows said meant a `grpc`-only node
/// served `POST /v1/ingest` that nothing had asked for, and a `rest`-only node served
/// nothing at all while `mw_grpc::grpcrx` warned that `POST /v1/ingest` was still M6-4's to
/// write.
///
/// # What REST ingest can and cannot reach
///
/// `Scope::Any` puts `/v1/ingest` on the **main** socket as well as the gRPC one, which is
/// what §4.5 asks for — it is additive to the frozen surface. Admission is the part that
/// does not follow: the main socket is `with_no_client_auth` (it serves a website, and a
/// browser has no peer certificate), so a request arriving there carries no
/// `ConnectInfo<PeerCerts>` and `Handlers::presented` refuses it. **REST ingest is
/// therefore reachable on the ingest socket only.** That is stated here rather than
/// discovered: closing it needs client-certificate negotiation on the website's port, which
/// is a change to what §6 says that socket is for, and it is recorded as a proposed
/// amendment in this item's PR.
pub fn manifest(rows: &[IngestRow]) -> RouteManifest {
    let mut entries = Vec::with_capacity(4);
    if rows.iter().any(|row| row.listen == IngestRow::GRPC) {
        entries.push(RouteEntry::exact(wire::HELLO, Methods::POST));
        entries.push(RouteEntry::exact(wire::INGEST, Methods::POST));
        entries.push(RouteEntry::exact(wire::ENROL, Methods::POST));
    }
    if rows.iter().any(|row| row.listen == IngestRow::REST) {
        entries.push(RouteEntry::exact(wire::REST_INGEST, Methods::POST));
    }
    RouteManifest::new(Scope::Any, entries)
}

/// The component id, which is a constant and is still validated.
pub fn component_id() -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(ID)
}

/// The route table the one listener mounts.
///
/// `rows` decides which paths are on it, and [`manifest`] has why.
pub fn route_table(
    state: Arc<ServiceState>,
    rows: &[IngestRow],
) -> Result<mw_http::RouteTable, ComponentIdError> {
    let owner = component_id()?;
    let grpc = rows.iter().any(|row| row.listen == IngestRow::GRPC);
    let rest = rows.iter().any(|row| row.listen == IngestRow::REST);
    let hello = {
        let state = Arc::clone(&state);
        axum::routing::post(move |request: axum::extract::Request| {
            let state = Arc::clone(&state);
            async move { unary_hello(state, request).await }
        })
    };
    let ingest = {
        let state = Arc::clone(&state);
        axum::routing::post(move |request: axum::extract::Request| {
            let state = Arc::clone(&state);
            async move { streaming_ingest(state, request).await }
        })
    };
    let enrol = {
        let state = Arc::clone(&state);
        axum::routing::post(move |request: axum::extract::Request| {
            let state = Arc::clone(&state);
            async move { unary_enrol(state, request).await }
        })
    };
    let rest_ingest = {
        let state = Arc::clone(&state);
        axum::routing::post(move |request: axum::extract::Request| {
            let state = Arc::clone(&state);
            async move { crate::rest::ingest(state, request).await }
        })
    };
    let mut router = Router::new();
    if grpc {
        router = router
            .route(wire::HELLO, hello)
            .route(wire::INGEST, ingest)
            .route(wire::ENROL, enrol);
    }
    if rest {
        router = router.route(wire::REST_INGEST, rest_ingest);
    }
    Ok(mw_http::RouteTable::new(owner, manifest(rows), router))
}

/// What a request gets while the component is not running.
///
/// The route exists and the component is not `Running`, which is a temporary condition the
/// sender's own retry is the right answer to -- so `UNAVAILABLE` rather than
/// `UNIMPLEMENTED`, which would read as the wrong address.
///
/// A function returning the response rather than a `Result` whose `Err` is one: an
/// `axum::Response` is 128 bytes, so `Result<Arc<Handlers>, Response>` puts a response-sized
/// hole in every success path (`clippy::result_large_err`).
pub(crate) fn not_running() -> axum::response::Response {
    Status::unavailable("this node's ingest endpoint is not running; please retry")
        .into_http::<axum::body::Body>()
}

/// What a request's connection presented, from `ConnectInfo`.
///
/// `None` for a request that did not arrive over the ingest socket. See
/// [`Handlers::presented`].
pub(crate) fn connection(request: &axum::extract::Request) -> Option<&mw_http::PeerCerts> {
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<mw_http::PeerCerts>>()
        .map(|info| &info.0)
}

async fn unary_hello(
    state: Arc<ServiceState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let Some(handlers) = state.handlers() else {
        return not_running();
    };
    // **Layer 2, before the body is even decoded.** `Grpc::unary` would read the message
    // first; resolving here means an unknown peer costs one map lookup and no parse.
    let peer = match handlers.resolve(connection(&request), request.headers()) {
        Ok(peer) => peer,
        Err(status) => return status.into_http::<axum::body::Body>(),
    };
    let mut grpc = tonic::server::Grpc::new(wire::HelloCodec::default());
    grpc.unary(
        HelloService {
            handlers: Arc::clone(&handlers),
            peer,
        },
        request,
    )
    .await
    .map(axum::body::Body::new)
}

struct HelloService {
    handlers: Arc<Handlers>,
    peer: PeerEntry,
}

impl tonic::server::UnaryService<pb::NodeIdentity> for HelloService {
    type Response = pb::HelloAck;
    type Future = BoxFuture<'static, Result<Response<pb::HelloAck>, Status>>;

    fn call(&mut self, request: Request<pb::NodeIdentity>) -> Self::Future {
        let handlers = Arc::clone(&self.handlers);
        let peer = self.peer.clone();
        Box::pin(async move {
            handlers
                .hello(&peer, request.get_ref())
                .await
                .map(Response::new)
        })
    }
}

async fn unary_enrol(
    state: Arc<ServiceState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let Some(handlers) = state.handlers() else {
        return not_running();
    };
    // **No layer 2 here, and that is the point of a separate RPC**: a node that is
    // enrolling has no `peers` row, so resolving one would make the bootstrap impossible.
    // The gate in its place is `enrolment_open`, which is closed unless an operator minted
    // a token.
    //
    // **But it is confined to the ingest socket.** The router is shared (§6.1), so without
    // this the one RPC that can add a peer would also answer on the website's port -- where
    // there is no `ConnectInfo<PeerCerts>`, so §5.3's "1 attempt per 5 s **per source
    // address**" would collapse to a single global bucket (the exact denial of service
    // `admit_enrolment` is per-source to avoid) and §5.3's "every attempt is logged with
    // the source" would log `0.0.0.0` for all of them. §5.4 mounts the ingest endpoint on
    // the gRPC port; the bootstrap belongs with it.
    let Some(connection) = connection(&request) else {
        return Status::permission_denied(
            "enrolment is served on this node's ingest endpoint only",
        )
        .into_http::<axum::body::Body>();
    };
    let source = connection.remote().ip();
    let mut grpc = tonic::server::Grpc::new(wire::EnrolCodec::default());
    grpc.unary(
        EnrolService {
            handlers: Arc::clone(&handlers),
            source,
        },
        request,
    )
    .await
    .map(axum::body::Body::new)
}

struct EnrolService {
    handlers: Arc<Handlers>,
    source: IpAddr,
}

impl tonic::server::UnaryService<pb::EnrolRequest> for EnrolService {
    type Response = pb::EnrolGrant;
    type Future = BoxFuture<'static, Result<Response<pb::EnrolGrant>, Status>>;

    fn call(&mut self, request: Request<pb::EnrolRequest>) -> Self::Future {
        let handlers = Arc::clone(&self.handlers);
        let source = self.source;
        Box::pin(async move {
            handlers
                .enrol(source, request.get_ref())
                .await
                .map(Response::new)
        })
    }
}

async fn streaming_ingest(
    state: Arc<ServiceState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let Some(handlers) = state.handlers() else {
        return not_running();
    };
    // **Layer 2, and "no stream is created" is literal**: this returns before
    // `Grpc::streaming` is called, so no `IngestBatch` is ever read from an unknown peer.
    let peer = match handlers.resolve(connection(&request), request.headers()) {
        Ok(peer) => peer,
        Err(status) => return status.into_http::<axum::body::Body>(),
    };
    let mut grpc = tonic::server::Grpc::new(wire::IngestCodec::default());
    grpc.streaming(
        IngestService {
            handlers: Arc::clone(&handlers),
            peer,
        },
        request,
    )
    .await
    .map(axum::body::Body::new)
}

struct IngestService {
    handlers: Arc<Handlers>,
    peer: PeerEntry,
}

impl tonic::server::StreamingService<pb::IngestBatch> for IngestService {
    type Response = pb::IngestAck;
    type ResponseStream = AckStream;
    type Future = BoxFuture<'static, Result<Response<AckStream>, Status>>;

    fn call(&mut self, request: Request<tonic::Streaming<pb::IngestBatch>>) -> Self::Future {
        let handlers = Arc::clone(&self.handlers);
        let peer = self.peer.clone();
        Box::pin(async move {
            Ok(Response::new(AckStream::new(Session {
                inbound: request.into_inner(),
                handlers,
                peer,
                first: true,
                acked_through: 0,
            })))
        })
    }
}

/// One `Ingest` call: the inbound batches, and the state the acks are computed from.
struct Session {
    inbound: tonic::Streaming<pb::IngestBatch>,
    handlers: Arc<Handlers>,
    peer: PeerEntry,
    /// Whether the next ack is the first of this stream, which is the one that carries
    /// `known_through` (W7 **D-23**).
    first: bool,
    /// The highest sender position acknowledged so far, which is what
    /// `ACK_RESOURCE_EXHAUSTED` echoes unchanged.
    acked_through: u64,
}

impl Session {
    /// Read one batch and produce one ack. `None` is the end of the stream.
    ///
    /// Returns the session back on `Ok` and drops it on `Err`, which is what makes "an
    /// error ends the stream" a property of the type rather than of a flag.
    async fn step(mut self) -> Option<(Result<pb::IngestAck, Status>, Option<Self>)> {
        match self.inbound.message().await {
            // The satellite closed the stream: a clean shutdown or a drain that finished.
            Ok(None) => {
                debug!(peer = %self.peer.name, "a peer's ingest stream ended");
                None
            }
            Err(status) => {
                debug!(peer = %self.peer.name, %status, "a peer's ingest stream failed");
                Some((Err(status), None))
            }
            Ok(Some(batch)) => {
                let envelopes = batch.envelopes.len() as u64;
                let ack = self
                    .handlers
                    .ingest_batch(&self.peer, &batch, self.first, &mut self.acked_through)
                    .await;
                match ack {
                    Ok(ack) => {
                        // **Cleared only when the ack actually carried it.** A first batch
                        // that hits the ceiling, or one with a transient refusal in it,
                        // answers `ACK_RESOURCE_EXHAUSTED` with an empty `known_through`
                        // (that status does not compute one) -- so clearing the flag
                        // unconditionally would mean a stream whose first batch was refused
                        // never receives the vector at all, and §4.2's fast resume would be
                        // silently unavailable for exactly the connections that most need
                        // it: the ones reconnecting after an outage.
                        self.first = ack.known_through.is_empty();
                        debug!(
                            peer = %self.peer.name,
                            envelopes,
                            through = ack.through,
                            duplicates = ack.duplicates,
                            rejected = ack.rejected,
                            "acknowledged a batch as durably spooled"
                        );
                        Some((Ok(ack), Some(self)))
                    }
                    Err(status) => Some((Err(status), None)),
                }
            }
        }
    }
}

/// The ack stream, as a hand-written `Stream`.
///
/// `tokio_stream::wrappers::ReceiverStream` is the obvious implementation and is not
/// available: `tonic` depends on `tokio-stream` with `default-features = false`, so the
/// `sync` feature that module needs is not in the graph, and adding `tokio-stream` to
/// `[workspace.dependencies]` for one wrapper would be a manifest edit in place of ten
/// lines. The `Stream` trait itself comes through `tonic::codegen`, which is the same path
/// `tonic-build`'s own output uses.
pub struct AckStream {
    next: Option<Pin<Box<dyn Future<Output = StepOutput> + Send>>>,
    session: Option<Session>,
}

type StepOutput = Option<(Result<pb::IngestAck, Status>, Option<Session>)>;

impl fmt::Debug for AckStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AckStream")
            .field("open", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl AckStream {
    fn new(session: Session) -> Self {
        Self {
            next: None,
            session: Some(session),
        }
    }
}

impl tonic::codegen::tokio_stream::Stream for AckStream {
    type Item = Result<pb::IngestAck, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next.is_none() {
            let Some(session) = self.session.take() else {
                return Poll::Ready(None);
            };
            self.next = Some(Box::pin(session.step()));
        }
        let future = self
            .next
            .as_mut()
            .expect("a step future was just installed");
        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => {
                self.next = None;
                match output {
                    None => Poll::Ready(None),
                    Some((item, session)) => {
                        self.session = session;
                        Poll::Ready(Some(item))
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------------------

/// The marker this component publishes `HttpRoutes` as.
#[derive(Debug)]
struct Mounted;

impl RouteProvider for Mounted {}

/// The `Serve` body: install, publish, wait, revoke, clear.
///
/// No loop and no socket — the service is a route on the one listener (§1.4), so what this
/// component does for its whole life is hold the handlers installed.
pub struct Receiver {
    state: Arc<ServiceState>,
    handlers: Arc<Handlers>,
    routes: Publisher<Arc<dyn RouteProvider>>,
    peers: Arc<dyn PeerRegistry>,
    auth: PeerAuthKind,
    insecure: bool,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("auth", &self.auth)
            .field("handlers", &self.handlers)
            .finish_non_exhaustive()
    }
}

impl Serve for Receiver {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            // Installed before the capability is published, so a consumer that reacts to
            // the publication cannot observe a mounted-but-empty service.
            self.state.install(Arc::clone(&self.handlers));
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            let peers = self.peers.peers();
            self.handlers.seed_last_seen(&peers);
            for peer in &peers {
                // Zeroed here, where the label value is known to be a configured one.
                m::arm_peer(
                    &peer.name,
                    &std::iter::once(peer.node)
                        .chain(peer.origins.iter().copied())
                        .collect::<Vec<_>>(),
                );
                // Labelled on every peer, including the ones with no recorded version:
                // `unknown` is what makes W7 OQ-W7-6's removal condition observable rather
                // than a series that simply is not there (`crate::metrics`).
                self.handlers
                    .note_software(&peer.name, peer.software.as_deref().unwrap_or(""));
            }
            if peers.is_empty() {
                // §8.5: `[[ingest]]` present with no `[[peer]]` rows is **not** an error. A
                // collector awaiting enrolment is a legitimate state and should be visible.
                warn!(
                    "this node accepts peers and has no `peers` rows, so every connection \
                     will be refused until one is enrolled: `Enroll` is served only while \
                     an operator has minted a token"
                );
            }
            if self.insecure {
                warn!("{INSECURE_NOTE}");
            }
            info!(
                auth = self.auth.as_str(),
                peers = peers.len(),
                methods = %format!("{}, {}, {}", wire::HELLO, wire::INGEST, wire::ENROL),
                "the collector is mounted"
            );
            cx.cancelled().await;
            // Revoke before clearing, which is the ordering §3.6 asks of a provider.
            self.routes.revoke();
            self.state.clear();
            info!("the collector is unmounted");
            Ok(())
        })
    }
}

/// What this build says, once, about the opt-out that let it serve ingest in plaintext.
pub const INSECURE_NOTE: &str =
    "`config.ingest.allow_insecure = 1` is set, so this node serves `collector.v1.Collector` \
     over PLAINTEXT: the handshake authenticates nobody, a `psk` crosses the wire in the \
     clear, and anyone who can reach the gRPC port can present any node id they like. \
     Decision 35 requires this to be said on every start. Configure `listener.https_port` \
     and a website certificate so the ingest socket can complete a TLS handshake, and clear \
     `allow_insecure`.";

/// Why a `config.ingest` row does not produce a collector.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestPlanError {
    /// The component id is not one, which is a build error rather than a configuration one.
    #[error("`{ID}` is not a usable component id")]
    Id(#[from] ComponentIdError),

    /// `main` did not build the state slot.
    #[error(
        "the collector's state slot was not built, so `{ID}` has nowhere to install its \
         handlers. `crates/modernweather/src/boot.rs` builds it beside the listener."
    )]
    NoServiceState,

    /// `peer_auth` holds a value its own `CHECK` should have rejected.
    #[error(
        "`config.ingest.peer_auth = '{peer_auth}'` is not one this build knows: expected \
         `mtls-ca`, `mtls-pinned` or `psk`"
    )]
    UnknownPeerAuth {
        /// What the row held.
        peer_auth: String,
    },

    /// Plaintext ingest with no explicit opt-out, which **D-35** makes a plan error.
    ///
    /// **The condition has changed with this item and the error has not.** It used to fire
    /// on every `config.ingest` row, because the gRPC socket had no TLS branch at all; now
    /// it fires when the socket *cannot* complete a TLS handshake, which is when no
    /// `listener.https_port` and certificate are configured for the SNI resolver to answer
    /// from. A node with TLS no longer needs the opt-out, which is the whole point of
    /// landing layer 1.
    #[error(
        "`config.ingest` serves `collector.v1.Collector` over plaintext, because this node \
         has no TLS certificate for the ingest socket to present -- so a `peer_auth = \
         '{peer_auth}'` authenticates nobody and a PSK would cross the wire in the clear. \
         Configure `listener.https_port` and a website certificate, or set \
         `config.ingest.allow_insecure = 1` to accept that (decision 35 requires it to be \
         explicit), or remove the row"
    )]
    InsecureIngest {
        /// What the row asked for, so the message names the setting being ignored.
        peer_auth: String,
    },
}

/// One `config.ingest` row, in the shape this crate plans from.
#[derive(Debug, Clone, PartialEq, Eq, std::hash::Hash)]
pub struct IngestRow {
    /// `listen` — `grpc` or `rest`.
    pub listen: String,
    /// `peer_auth`.
    pub peer_auth: String,
    /// `ca_path`.
    pub ca_path: Option<String>,
    /// `allow_insecure`.
    pub allow_insecure: bool,
}

impl IngestRow {
    /// The `listen` value this component serves.
    pub const GRPC: &'static str = "grpc";
    /// The `listen` value `POST /v1/ingest` serves.
    pub const REST: &'static str = "rest";
}

/// What this crate needs from the configuration to plan `ingest:grpc`.
pub trait CollectorConfig {
    /// Every `config.ingest` row.
    ///
    /// **Its emptiness is meaningful**: a node with no row is not a collector, which is
    /// W7 **D-44**'s "no flag, no mode".
    fn ingest(&self) -> &[IngestRow];

    /// The write end of the reading pipeline, once the spool is open.
    fn ingress(&self) -> Option<&Arc<dyn Ingress>>;

    /// The spool itself, for `known_through` and for the uplink's own depth.
    fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>>;

    /// The published station registry.
    fn station_registry(&self) -> Option<&Arc<dyn mw_component::StationRegistry>>;

    /// The peer registry and the registration path.
    fn peer_registry(&self) -> Option<&Arc<dyn PeerRegistry>>;

    /// This node's own `NodeId`, from the `node` row.
    fn node_id(&self) -> Option<OriginId>;

    /// The state slot the binary built beside the listener.
    fn ingest_service(&self) -> Option<&Arc<ServiceState>>;

    /// Whether the ingest socket can complete a TLS handshake.
    ///
    /// Not "is there an `https_port`": the ingest socket presents a certificate out of the
    /// SNI resolver, so what matters is whether the resolver can answer at all. The binary
    /// decides, because it owns both.
    fn ingest_tls_available(&self) -> bool;
}

/// Plan `ingest:grpc`, or say why not.
///
/// **One condition**: a `config.ingest` row with `listen = 'grpc'`. A node with no such row
/// is a satellite and has no collector, which is W7 **D-44** and is not an error.
pub fn register<C>(cfg: &C, out: &mut Vec<ComponentSpec>) -> Result<(), IngestPlanError>
where
    C: CollectorConfig + ?Sized,
{
    // **Either row makes this node a collector.** The gRPC row brings
    // `collector.v1.Collector`; the REST row brings `POST /v1/ingest`. §8.3's collector has
    // both, and the schema admits either alone -- so planning on the gRPC row alone left a
    // `rest`-only node with no ingest surface and nothing saying so.
    let grpc = cfg
        .ingest()
        .iter()
        .find(|row| row.listen == IngestRow::GRPC);
    let rest = cfg
        .ingest()
        .iter()
        .find(|row| row.listen == IngestRow::REST);
    let Some(row) = grpc.or(rest) else {
        return Ok(());
    };
    if grpc.is_none() {
        // A node whose only ingest row is `rest`. It is served, and the limit is worth one
        // line at startup: admission needs a client certificate or a PSK over a socket this
        // node knows is the ingest one, and the main socket is neither -- see `manifest`.
        warn!(
            "`config.ingest` has a `rest` row and no `grpc` row, so this node serves \
             `POST /v1/ingest` and no `collector.v1.Collector`. That path is reachable on \
             the gRPC socket only: the main socket requests no client certificate, so a \
             request arriving there cannot be authenticated and is refused."
        );
    }
    let auth =
        PeerAuthKind::parse(&row.peer_auth).ok_or_else(|| IngestPlanError::UnknownPeerAuth {
            peer_auth: row.peer_auth.clone(),
        })?;
    // **Before anything is built**, because the point of a plan error is that the daemon
    // does not start (**D-35**).
    if !cfg.ingest_tls_available() && !row.allow_insecure {
        return Err(IngestPlanError::InsecureIngest {
            peer_auth: row.peer_auth.clone(),
        });
    }
    // Each of these is a root resource the binary built. A missing one is a node that is
    // not ingesting rather than a refused configuration, for the reason `mw_grpc::grpcrx`
    // states: refusing the whole configuration for a node that serves but does not ingest
    // is wrong.
    let (Some(ingress), Some(spool), Some(registry), Some(peers), Some(node)) = (
        cfg.ingress(),
        cfg.spool(),
        cfg.station_registry(),
        cfg.peer_registry(),
        cfg.node_id(),
    ) else {
        warn!(
            "a `config.ingest` row makes this node a collector and it has no spool, so no \
             collector is planned: there would be nothing to accept a peer's envelopes \
             into. The spool arrives with the `config.node` row the first start allocates."
        );
        return Ok(());
    };
    let state = Arc::clone(
        cfg.ingest_service()
            .ok_or(IngestPlanError::NoServiceState)?,
    );
    let id = component_id()?;
    let receiver = id.clone();
    let ingress = Arc::clone(ingress);
    let spool = Arc::clone(spool);
    let gate = Gate::new(Arc::clone(registry), node);
    let peers = Arc::clone(peers);
    let insecure = row.allow_insecure && !cfg.ingest_tls_available();
    // Every value the builder closes over, so that changing `peer_auth` restarts the
    // component.
    let digest = ConfigDigest::of(row);

    out.push(
        ComponentSpec::new(
            id,
            Trigger::Service,
            mw_component::Builder::serve(move |cx| {
                let state = Arc::clone(&state);
                let ingress = Arc::clone(&ingress);
                let spool = Arc::clone(&spool);
                let peers = Arc::clone(&peers);
                let gate = gate.clone();
                let receiver = receiver.clone();
                async move {
                    let routes = cx.publisher::<Arc<dyn RouteProvider>>()?;
                    Ok(Receiver {
                        state,
                        handlers: Arc::new(Handlers::new(
                            receiver,
                            Arc::clone(&peers),
                            ingress,
                            spool,
                            gate,
                            auth,
                        )),
                        routes,
                        peers,
                        auth,
                        insecure,
                    })
                }
            }),
        )
        .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
        .provides([Provision::routes(manifest(cfg.ingest()))])
        .identity(mw_component::IdentityShape {
            // Inbound, resolved from a registration, N nodes on one endpoint, and a stream.
            // `GrpcIn` is not a `Transport` variant -- the gRPC receiver is an `ingest` row
            // and never a station's local transport -- so the shape is declared rather than
            // derived.
            initiation: mw_component::Initiation::Inbound,
            source: mw_component::IdentitySource::FromRegistration,
            cardinality: mw_component::Cardinality::Many,
            continuity: mw_component::Continuity::Stream,
        })
        .config_digest(digest),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(listen: &str, allow_insecure: bool) -> IngestRow {
        IngestRow {
            listen: listen.to_string(),
            peer_auth: "mtls-ca".to_string(),
            ca_path: Some("/etc/modernweather/peers-ca.pem".to_string()),
            allow_insecure,
        }
    }

    #[derive(Debug, Default)]
    struct Cfg {
        ingest: Vec<IngestRow>,
        tls: bool,
        state: Option<Arc<ServiceState>>,
    }

    impl CollectorConfig for Cfg {
        fn ingest(&self) -> &[IngestRow] {
            &self.ingest
        }
        fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
            None
        }
        fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>> {
            None
        }
        fn station_registry(&self) -> Option<&Arc<dyn mw_component::StationRegistry>> {
            None
        }
        fn peer_registry(&self) -> Option<&Arc<dyn PeerRegistry>> {
            None
        }
        fn node_id(&self) -> Option<OriginId> {
            None
        }
        fn ingest_service(&self) -> Option<&Arc<ServiceState>> {
            self.state.as_ref()
        }
        fn ingest_tls_available(&self) -> bool {
            self.tls
        }
    }

    /// Handlers over the test doubles, for the two properties below.
    fn handlers() -> Handlers {
        let registry = crate::testing::station_registry(&[("satellite-a:backyard", 1)]);
        Handlers::new(
            ComponentId::new(crate::INGEST_ID).expect("a legal id"),
            crate::testing::FakePeers::ca("satellite-a", OriginId::new([1; 16]))
                as Arc<dyn PeerRegistry>,
            Arc::new(crate::testing::NullIngress) as Arc<dyn Ingress>,
            crate::testing::null_spool(),
            crate::Gate::new(registry, OriginId::new([9; 16])),
            PeerAuthKind::MtlsCa,
        )
    }

    /// **§7's silence gauge has to be able to show silence.**
    ///
    /// `peer_last_seen_seconds` is written to zero when a peer arrives, so without the
    /// sweep a peer that goes quiet leaves it at zero for ever and the one failure mode §7
    /// names — "silence is the failure mode a stream-based protocol hides" — is the one
    /// thing the metric cannot report. What is asserted is the number the sweep is about to
    /// write, because reading a gauge back needs a recorder this workspace does not depend
    /// on (**D-32**).
    #[test]
    fn a_peer_that_goes_quiet_has_a_rising_last_seen_age() {
        let handlers = handlers();
        let peer = PeerName::new("satellite-a");
        handlers.seen(&peer);
        let first = handlers.ages();
        assert_eq!(first.len(), 1, "one peer, one series");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = handlers.ages();
        assert!(
            second[0].1 > first[0].1,
            "the age has to rise while the peer is quiet: {:?} then {:?}",
            first[0].1,
            second[0].1
        );
        // And a batch resets it, which is the other half.
        handlers.seen(&peer);
        let third = handlers.ages();
        assert!(
            third[0].1 < second[0].1,
            "a peer that spoke again is not silent: {:?}",
            third[0].1
        );
        // The sweep itself, so the metric call is on a tested path.
        handlers.sweep_last_seen();
    }

    /// A configured peer that has **never** connected is a series, not an absence.
    ///
    /// An absent series cannot be alerted on without `absent()`, and a collector whose
    /// satellite has never called since the daemon started is exactly the state an operator
    /// wants to see. The age runs from the mount.
    #[test]
    fn a_peer_that_has_never_connected_is_still_reported() {
        let handlers = handlers();
        assert!(handlers.ages().is_empty(), "nothing is seeded yet");
        let peers = vec![PeerEntry {
            node: OriginId::new([1; 16]),
            name: PeerName::new("satellite-a"),
            auth: mw_component::PeerAuth::MtlsCa,
            relay: false,
            origins: std::collections::BTreeSet::new(),
            revoked_at: None,
            software: None,
        }];
        handlers.seed_last_seen(&peers);
        let ages = handlers.ages();
        assert_eq!(ages.len(), 1);
        assert_eq!(ages[0].0, PeerName::new("satellite-a"));
    }

    /// **D-44**: no flag, no mode. A node with no row plans nothing and that is not an
    /// error.
    #[test]
    fn a_node_with_no_ingest_row_is_not_a_collector() {
        let mut out = Vec::new();
        register(&Cfg::default(), &mut out).expect("a satellite plans no collector");
        assert!(out.is_empty());
    }

    /// A `rest`-only row **does** make this node a collector: it is what brings
    /// `POST /v1/ingest`. Planning only on the `grpc` row left such a node with no ingest
    /// surface at all and nothing saying so.
    #[test]
    fn a_rest_only_row_plans_the_component() {
        let mut out = Vec::new();
        register(
            &Cfg {
                ingest: vec![row("rest", true)],
                state: Some(Arc::new(ServiceState::new())),
                ..Cfg::default()
            },
            &mut out,
        )
        .expect("a rest row is an ingest row");
        // No spool in this `Cfg`, so it stops at the root-resource check -- the point is
        // that it got past the "is this node a collector" question.
        assert!(out.is_empty(), "no spool, so no component");
    }

    /// **D-35**, and the condition that changed: plaintext is a plan error unless the
    /// operator opted out explicitly.
    #[test]
    fn plaintext_ingest_without_the_opt_out_is_a_plan_error() {
        let mut out = Vec::new();
        let err = register(
            &Cfg {
                ingest: vec![row("grpc", false)],
                tls: false,
                ..Cfg::default()
            },
            &mut out,
        )
        .expect_err("a plaintext ingest socket authenticates nobody");
        assert!(
            matches!(err, IngestPlanError::InsecureIngest { .. }),
            "{err}"
        );
        let message = err.to_string();
        assert!(message.contains("allow_insecure"), "{message}");
        assert!(message.contains("https_port"), "{message}");
    }

    /// …and with TLS the opt-out is no longer needed, which is what landing layer 1 buys.
    #[test]
    fn with_tls_the_opt_out_is_not_required() {
        let mut out = Vec::new();
        // No spool, so this stops at the root-resource check rather than planning -- which
        // is the point: it got **past** the insecure check.
        register(
            &Cfg {
                ingest: vec![row("grpc", false)],
                tls: true,
                ..Cfg::default()
            },
            &mut out,
        )
        .expect("a TLS ingest socket needs no opt-out");
        assert!(out.is_empty(), "no spool, so no component");
    }

    #[test]
    fn a_peer_auth_this_build_does_not_know_is_refused() {
        let mut out = Vec::new();
        let err = register(
            &Cfg {
                ingest: vec![IngestRow {
                    peer_auth: "trust-me".to_string(),
                    ..row("grpc", true)
                }],
                ..Cfg::default()
            },
            &mut out,
        )
        .expect_err("an unknown auth kind cannot be honoured");
        assert!(
            matches!(err, IngestPlanError::UnknownPeerAuth { .. }),
            "{err}"
        );
    }

    /// **The rows decide the paths.** A `grpc`-only node must not serve a REST path
    /// nothing asked for, and a `rest`-only node must not serve the gRPC methods.
    #[test]
    fn the_route_manifest_follows_the_ingest_rows() {
        let paths = |rows: &[IngestRow]| -> Vec<String> {
            manifest(rows)
                .entries
                .iter()
                .map(|e| e.pattern.clone())
                .collect()
        };

        let both = paths(&[row("grpc", true), row("rest", true)]);
        for path in [wire::HELLO, wire::INGEST, wire::ENROL, wire::REST_INGEST] {
            assert!(both.contains(&path.to_string()), "{path}");
        }

        let grpc_only = paths(&[row("grpc", true)]);
        assert!(grpc_only.contains(&wire::HELLO.to_string()));
        assert!(
            !grpc_only.contains(&wire::REST_INGEST.to_string()),
            "a node with no `rest` row must not serve `POST /v1/ingest`"
        );

        let rest_only = paths(&[row("rest", true)]);
        assert_eq!(rest_only, vec![wire::REST_INGEST.to_string()]);

        assert!(paths(&[]).is_empty(), "a satellite claims no ingest path");
    }

    #[test]
    fn the_component_id_is_the_renamed_one() {
        // W7 **D-2**: `ingest:`, not `station:` -- a peer is not a station.
        assert_eq!(ID, "ingest:grpc");
        assert!(component_id().is_ok());
        assert_ne!(ID, mw_grpc_id(), "the Go compat shim keeps its own id");
    }

    /// Spelled out rather than imported: this crate may not depend on `mw-grpc` (same
    /// layer), and the point of the assertion is that the two ids differ.
    fn mw_grpc_id() -> &'static str {
        "station:grpcrx"
    }

    #[test]
    fn a_node_id_header_is_sixteen_bytes_of_hex_and_nothing_else() {
        assert_eq!(
            node_id_from_hex("0193f3c27a107b649c2e4f1b8a6d0e11"),
            Some(OriginId::new([
                0x01, 0x93, 0xf3, 0xc2, 0x7a, 0x10, 0x7b, 0x64, 0x9c, 0x2e, 0x4f, 0x1b, 0x8a, 0x6d,
                0x0e, 0x11
            ]))
        );
        assert_eq!(node_id_from_hex(""), None);
        assert_eq!(node_id_from_hex("0193f3c2"), None, "too short");
        assert_eq!(
            node_id_from_hex("0193f3c27a107b649c2e4f1b8a6d0e1100"),
            None,
            "too long"
        );
        assert_eq!(node_id_from_hex("zz93f3c27a107b649c2e4f1b8a6d0e11"), None);
        assert_eq!(
            node_id_from_hex("+1+1+1+1+1+1+1+1+1+1+1+1+1+1+1+1"),
            None,
            "`u8::from_str_radix` accepts a leading sign, so one node would have had two \
             spellings"
        );
    }

    #[test]
    fn a_bearer_secret_is_read_and_nothing_else_is() {
        use secrecy::ExposeSecret;
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer c0ffee"),
        );
        assert_eq!(
            bearer(&headers).map(|s| s.expose_secret().to_string()),
            Some("c0ffee".to_string())
        );
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert!(
            bearer(&headers).is_none(),
            "only a bearer token is a PSK, and `Basic` is a different credential"
        );
        assert!(bearer(&http::HeaderMap::new()).is_none());
    }

    /// §5.3's rate limit, which is per source so that one attacker cannot lock an operator
    /// out of enrolling.
    #[test]
    fn enrolment_is_rate_limited_per_source() {
        let handlers = Handlers {
            receiver: component_id().expect("a legal id"),
            peers: crate::testing::FakePeers::empty(),
            ingress: Arc::new(crate::testing::NullIngress),
            spool: crate::testing::null_spool(),
            gate: Gate::new(
                crate::testing::station_registry(&[]),
                OriginId::new([9; 16]),
            ),
            auth: PeerAuthKind::MtlsCa,
            enrolments: Mutex::new(std::collections::HashMap::new()),
            last_seen: Mutex::new(std::collections::BTreeMap::new()),
            software: Mutex::new(std::collections::BTreeMap::new()),
        };
        let a: IpAddr = "192.0.2.1".parse().expect("an address");
        let b: IpAddr = "192.0.2.2".parse().expect("an address");
        assert!(handlers.admit_enrolment(a));
        assert!(!handlers.admit_enrolment(a), "one attempt per window");
        assert!(
            handlers.admit_enrolment(b),
            "a second source is not limited by the first"
        );
    }
}
