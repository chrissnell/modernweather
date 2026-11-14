//! **Layer 1**: who may complete a TLS handshake on the ingest socket (W7 §5.4, **D-34**).
//!
//! > An unauthorised peer is refused **at the TLS layer, before any frame is decoded**.
//!
//! That sentence is the whole of this module, and it is the only one of the three admission
//! layers that can keep its promise literally: layers 2 and 3 run after a connection exists.
//! Today the Go receiver configures server TLS only
//! (`grpcreceiver/station.go:118-128`), so anyone who can reach the port is a peer.
//!
//! | `auth_kind` | Verifier | Where identity comes from |
//! |---|---|---|
//! | `mtls-ca` | `rustls`'s `WebPkiClientVerifier` over the peer CA | the SAN URI `urn:modernweather:node:<uuid>` (`crate::x509`) |
//! | `mtls-pinned` | [`PinnedSpki`], over the `peers` snapshot | the SPKI digest that matched |
//! | `psk` | none — client certificates are not requested | the secret in gRPC metadata, at layer 2 |
//!
//! **`mw-http` binds the socket and this crate decides who may be on it.** The split is
//! `ci/layers.toml`: only `mw-http` may name a listener, and only this crate may name a
//! peer. What crosses the seam is an `Arc<rustls::ServerConfig>` and nothing else.
//!
//! # Why the pinned verifier is a verifier and not a later check
//!
//! A pinned peer's certificate is signed by nobody this node trusts, so `rustls` has no
//! chain to validate and the obvious shortcut is to accept any certificate and compare the
//! digest once the connection is up. That would be layer **2**, not layer 1 — the frames
//! would already be arriving. [`PinnedSpki`] compares inside `verify_client_cert`, so an
//! unknown key is a fatal alert during the handshake and the stream is never created.
//!
//! The signature checks are **not** bypassed: `verify_tls12_signature` and
//! `verify_tls13_signature` delegate to the provider's own implementations, so the peer
//! still has to prove possession of the private key whose SPKI was pinned. A verifier that
//! returned `Ok` from those would accept anybody holding a copy of the certificate, which is
//! public material.

use std::fmt;
use std::sync::Arc;

use mw_component::{PeerEntry, PeerRegistry};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use tracing::{debug, warn};

use crate::x509;

/// ALPN for the ingest socket.
///
/// `h2` only, and `http/1.1` deliberately absent: the one thing served here is gRPC, which
/// is HTTP/2, and offering HTTP/1.1 would let a client negotiate a protocol on which every
/// request 505s. The main socket offers both because it serves a website.
const ALPN: [&[u8]; 1] = [b"h2"];

/// Why the ingest socket's TLS could not be configured.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// `config.ingest.ca_path` names a file that cannot be read.
    #[error("the peer CA at {path} could not be read")]
    CaUnreadable {
        /// The configured path.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The file is readable and contains no certificate.
    #[error(
        "the peer CA at {path} contains no PEM certificate, so no peer's chain could ever \
         validate against it"
    )]
    CaEmpty {
        /// The configured path.
        path: String,
    },

    /// A certificate in it is not one `webpki` will use as an anchor.
    #[error("the peer CA at {path} is not a usable trust anchor")]
    CaUnusable {
        /// The configured path.
        path: String,
        /// Why.
        #[source]
        source: rustls::Error,
    },

    /// `rustls` refused the configuration.
    #[error("the ingest socket's TLS configuration was refused")]
    Config {
        /// Why.
        #[source]
        source: rustls::Error,
    },

    /// `peer_auth = 'mtls-ca'` with no `ca_path` on any `config.ingest` row.
    ///
    /// The schema deliberately permits `ca_path` to be NULL — §8.3 sets it on the gRPC
    /// listener and omits it on the REST one — so "where the CA comes from when no listener
    /// names one" is this item's to answer, and the answer is that it comes from nowhere and
    /// that is a refusal. A `mtls-ca` socket with no anchor would reject every peer at the
    /// handshake with a message about certificates rather than about configuration.
    #[error(
        "`config.ingest.peer_auth = 'mtls-ca'` needs a peer CA and no `config.ingest` row \
         sets `ca_path`: set it on one of them, or use `peer_auth = 'mtls-pinned'`, which \
         needs no CA to run"
    )]
    NoCa,
}

/// How the ingest socket authenticates peers, as the `peer_auth` column spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAuthKind {
    /// Chain validation against the peer CA.
    MtlsCa,
    /// SPKI pinning against the `peers` snapshot.
    MtlsPinned,
    /// A pre-shared secret in gRPC metadata, over server-authenticated TLS.
    Psk,
}

impl PeerAuthKind {
    /// Parse the column, or `None` for a value its own `CHECK` should have rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mtls-ca" => Some(PeerAuthKind::MtlsCa),
            "mtls-pinned" => Some(PeerAuthKind::MtlsPinned),
            "psk" => Some(PeerAuthKind::Psk),
            _ => None,
        }
    }

    /// The column's spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            PeerAuthKind::MtlsCa => "mtls-ca",
            PeerAuthKind::MtlsPinned => "mtls-pinned",
            PeerAuthKind::Psk => "psk",
        }
    }

    /// Whether this kind asks the peer for a certificate.
    pub const fn wants_client_certificate(self) -> bool {
        matches!(self, PeerAuthKind::MtlsCa | PeerAuthKind::MtlsPinned)
    }
}

/// Build the ingest socket's `ServerConfig` — the certificate it presents and the verifier
/// it applies.
///
/// `resolver` is the **same** SNI resolver the main socket uses (W3 assumption 47): the
/// collector's server certificate is its website's, because that is the certificate an
/// operator already configures and renews. A node with no TLS port therefore has nothing to
/// present, which is why this returns the configuration and `ingest:grpc`'s plan check is
/// what refuses the combination.
pub fn server_config(
    kind: PeerAuthKind,
    ca_path: Option<&str>,
    resolver: Arc<mw_http::SniResolver>,
    peers: Arc<dyn PeerRegistry>,
) -> Result<ServerConfig, TlsError> {
    // **The provider is named, not defaulted**, for `mw_http::socket::bind`'s reason: a
    // future dependency enabling `aws-lc-rs` would otherwise turn this into a panic.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|source| TlsError::Config { source })?;
    let mut config = match kind {
        PeerAuthKind::MtlsCa => {
            let path = ca_path.ok_or(TlsError::NoCa)?;
            let verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots(path)?),
                Arc::clone(&provider),
            )
            .build()
            .map_err(|e| TlsError::CaUnusable {
                path: path.to_string(),
                // `VerifierBuilderError` is not a `rustls::Error`; the one reachable cause
                // is an empty anchor set, which `roots` has already refused, so this is
                // carried as the generic configuration refusal.
                source: rustls::Error::General(e.to_string()),
            })?;
            builder.with_client_cert_verifier(verifier)
        }
        PeerAuthKind::MtlsPinned => builder.with_client_cert_verifier(Arc::new(PinnedSpki {
            peers,
            provider: Arc::clone(&provider),
        })),
        // TLS is still required (**D-35**) and it is still server-authenticated; what is
        // not requested is a client certificate, because a PSK peer has none.
        PeerAuthKind::Psk => builder.with_no_client_auth(),
    }
    .with_cert_resolver(resolver);
    config.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// The trust anchors in a PEM file.
fn roots(path: &str) -> Result<rustls::RootCertStore, TlsError> {
    let pem = std::fs::read(path).map_err(|source| TlsError::CaUnreadable {
        path: path.to_string(),
        source,
    })?;
    let mut store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        let cert = cert.map_err(|source| TlsError::CaUnreadable {
            path: path.to_string(),
            source,
        })?;
        store.add(cert).map_err(|source| TlsError::CaUnusable {
            path: path.to_string(),
            source,
        })?;
    }
    if store.is_empty() {
        return Err(TlsError::CaEmpty {
            path: path.to_string(),
        });
    }
    Ok(store)
}

/// `auth_kind = 'mtls-pinned'`: the presented certificate's SPKI must be a peer's.
///
/// Reads the published snapshot on every handshake rather than a set captured at build, so
/// a peer enrolled a moment ago can connect without the component restarting — which is
/// what makes `Enroll` followed immediately by `Hello` work.
pub struct PinnedSpki {
    peers: Arc<dyn PeerRegistry>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl fmt::Debug for PinnedSpki {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedSpki").finish_non_exhaustive()
    }
}

impl PinnedSpki {
    /// A verifier over one peer registry.
    pub fn new(peers: Arc<dyn PeerRegistry>) -> Self {
        Self {
            peers,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// Which peer this certificate is, by its SPKI digest.
    ///
    /// Exposed so that the decision is testable without a handshake, and so that layer 2
    /// can ask the same question of the same bytes.
    pub fn resolve(&self, cert: &CertificateDer<'_>) -> Option<PeerEntry> {
        let spki = x509::spki(cert)?;
        self.peers.peer_by_spki(&sha256(&spki))
    }
}

impl ClientCertVerifier for PinnedSpki {
    /// None, and that is the correct answer rather than an omission.
    ///
    /// `certificate_authorities` in a `CertificateRequest` tells the client which issuers
    /// the server will accept. A pinned deployment has no issuers: the peer holds exactly
    /// one certificate and sends it. An empty hint is also the only honest one — listing
    /// every pinned peer's subject would tell an unauthenticated client the names of the
    /// whole fleet.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    /// Required. A connection with no certificate cannot be pinned to anything, and a
    /// verifier that made it optional would admit one and leave layer 2 to notice.
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            // A chain under a pinned configuration is not an error worth refusing on its
            // own -- the pin is on the leaf -- but it means the peer is configured for a CA
            // this node is not running, which is worth one line.
            debug!(
                intermediates = intermediates.len(),
                "a pinned peer presented a chain; only the end-entity key is pinned"
            );
        }
        match self.resolve(end_entity) {
            Some(peer) if peer.is_revoked() => {
                // Refused here as well as at layer 2, because this is the layer that can
                // stop the stream from existing at all.
                warn!(
                    peer = %peer.name,
                    "a revoked peer attempted a handshake on the ingest socket"
                );
                Err(rustls::Error::General(
                    "this peer's enrolment has been revoked".to_string(),
                ))
            }
            Some(peer) => {
                debug!(peer = %peer.name, "a pinned peer completed client authentication");
                Ok(ClientCertVerified::assertion())
            }
            None => {
                // No peer name to label: at this point the certificate is an unknown key,
                // and `crate::metrics` is where the counted rejection goes.
                crate::metrics::handshake_refused();
                warn!(
                    "a certificate whose public key is not a pinned peer's was refused at \
                     the ingest socket's handshake"
                );
                Err(rustls::Error::General(
                    "this certificate's public key is not an enrolled peer's".to_string(),
                ))
            }
        }
    }

    /// Delegated, deliberately: possession of the private key is what the pin is **for**.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// SHA-256, from the provider the handshake itself uses.
///
/// `rustls`'s `ring` provider is already in the binary (**A-W8-3**), so the digest a pin is
/// computed with is the one TLS is computing with. Adding `sha2` for this would be a second
/// implementation of one function.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = sha256_provider().hash(bytes);
    let mut out = [0u8; 32];
    // SHA-256 is 32 bytes by definition; a shorter output would mean the provider returned
    // a different algorithm, which `sha256_provider` has already established it did not.
    out.copy_from_slice(&digest.as_ref()[..32]);
    out
}

/// The provider's SHA-256, found through its cipher suites.
///
/// `rustls::crypto::ring::hash::SHA256` is the obvious spelling and that module is
/// `pub(crate)`; what **is** public is `CipherSuiteCommon::hash_provider`, so the
/// implementation is reached through the TLS 1.3 suite that uses it. Resolved once rather
/// than per call, because the search is a linear scan over the suite table.
fn sha256_provider() -> &'static dyn rustls::crypto::hash::Hash {
    static PROVIDER: std::sync::OnceLock<&'static dyn rustls::crypto::hash::Hash> =
        std::sync::OnceLock::new();
    *PROVIDER.get_or_init(|| {
        rustls::crypto::ring::default_provider()
            .cipher_suites
            .iter()
            .filter_map(|suite| suite.tls13())
            .map(|suite| suite.common.hash_provider)
            .find(|hash| hash.algorithm() == rustls::crypto::hash::HashAlgorithm::SHA256)
            .expect("the ring provider offers TLS13_AES_128_GCM_SHA256, so it offers SHA-256")
    })
}

/// Lower-case hex, for a log field and for the pin an operator configures.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{cert, FakePeers};
    use mw_types::OriginId;

    #[test]
    fn the_column_s_three_values_parse_and_nothing_else_does() {
        for (text, kind) in [
            ("mtls-ca", PeerAuthKind::MtlsCa),
            ("mtls-pinned", PeerAuthKind::MtlsPinned),
            ("psk", PeerAuthKind::Psk),
        ] {
            assert_eq!(PeerAuthKind::parse(text), Some(kind));
            assert_eq!(kind.as_str(), text);
        }
        assert_eq!(PeerAuthKind::parse("none"), None);
        assert_eq!(PeerAuthKind::parse(""), None);
    }

    #[test]
    fn only_the_two_mtls_kinds_ask_for_a_certificate() {
        assert!(PeerAuthKind::MtlsCa.wants_client_certificate());
        assert!(PeerAuthKind::MtlsPinned.wants_client_certificate());
        assert!(!PeerAuthKind::Psk.wants_client_certificate());
    }

    #[test]
    fn a_pinned_certificate_resolves_to_its_peer_and_an_unpinned_one_to_nobody() {
        let pinned = cert("pinned-cert.pem");
        let digest = sha256(&x509::spki(&pinned).expect("an SPKI"));
        let peers = FakePeers::pinned("relay-west", OriginId::new([9; 16]), digest);
        let verifier = PinnedSpki::new(peers);
        assert_eq!(
            verifier
                .resolve(&pinned)
                .expect("the pinned key resolves")
                .name
                .as_str(),
            "relay-west"
        );
        assert!(
            verifier.resolve(&cert("peer-a-cert.pem")).is_none(),
            "a certificate the same CA signed is not a pin"
        );
    }

    /// The refusal is at `verify_client_cert`, which is what makes it layer 1 rather than a
    /// check after the stream exists.
    #[test]
    fn an_unpinned_certificate_is_refused_by_the_verifier_itself() {
        let pinned = cert("pinned-cert.pem");
        let digest = sha256(&x509::spki(&pinned).expect("an SPKI"));
        let verifier = PinnedSpki::new(FakePeers::pinned(
            "relay-west",
            OriginId::new([9; 16]),
            digest,
        ));
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_762_732_800));
        assert!(verifier.verify_client_cert(&pinned, &[], now).is_ok());
        let err = verifier
            .verify_client_cert(&cert("peer-a-cert.pem"), &[], now)
            .expect_err("an unknown key cannot complete a handshake");
        assert!(format!("{err}").contains("not an enrolled peer"), "{err}");
    }

    #[test]
    fn a_revoked_peer_cannot_complete_a_handshake() {
        let pinned = cert("pinned-cert.pem");
        let digest = sha256(&x509::spki(&pinned).expect("an SPKI"));
        let peers = FakePeers::pinned("relay-west", OriginId::new([9; 16]), digest).revoked();
        let verifier = PinnedSpki::new(peers);
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_762_732_800));
        let err = verifier
            .verify_client_cert(&pinned, &[], now)
            .expect_err("revocation is honoured at the handshake too");
        assert!(format!("{err}").contains("revoked"), "{err}");
    }

    /// Client authentication is mandatory under pinning: there is nothing to compare
    /// against on a connection with no certificate.
    #[test]
    fn pinned_client_authentication_is_mandatory_and_hints_nothing() {
        let verifier = PinnedSpki::new(FakePeers::empty());
        assert!(verifier.client_auth_mandatory());
        assert!(
            verifier.root_hint_subjects().is_empty(),
            "hinting every pinned peer's subject would name the fleet to an \
             unauthenticated client"
        );
        assert!(
            !verifier.supported_verify_schemes().is_empty(),
            "the provider's schemes are offered, so possession of the key is still proved"
        );
    }

    #[test]
    fn mtls_ca_with_no_ca_path_is_refused_rather_than_accepting_everyone() {
        let err = server_config(
            PeerAuthKind::MtlsCa,
            None,
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .expect_err("a CA socket with no anchor cannot authenticate anybody");
        assert!(matches!(err, TlsError::NoCa), "{err}");
    }

    #[test]
    fn an_unreadable_or_empty_ca_is_named() {
        let err = server_config(
            PeerAuthKind::MtlsCa,
            Some("/nonexistent/peers-ca.pem"),
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .expect_err("a missing CA file is a configuration error");
        assert!(matches!(err, TlsError::CaUnreadable { .. }), "{err}");

        let empty = std::env::temp_dir().join("mw-collector-empty-ca.pem");
        std::fs::write(&empty, b"# no certificates here\n").expect("a writable temp dir");
        let err = server_config(
            PeerAuthKind::MtlsCa,
            Some(&empty.to_string_lossy()),
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .expect_err("a CA file with no certificate in it is a configuration error");
        assert!(matches!(err, TlsError::CaEmpty { .. }), "{err}");
        let _ = std::fs::remove_file(&empty);
    }

    /// The socket offers `h2` and not `http/1.1`: gRPC is HTTP/2, and a client that
    /// negotiated HTTP/1.1 here would 505 on every request.
    #[test]
    fn the_ingest_socket_offers_h2_only() {
        let config = server_config(
            PeerAuthKind::Psk,
            None,
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .expect("a psk socket needs no CA");
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec()]);
    }

    #[test]
    fn a_pinned_socket_needs_no_ca() {
        assert!(server_config(
            PeerAuthKind::MtlsPinned,
            None,
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .is_ok());
    }

    #[test]
    fn the_ca_path_is_used_when_one_is_given() {
        let path = crate::testing::data("ca-cert.pem");
        assert!(server_config(
            PeerAuthKind::MtlsCa,
            Some(&path.to_string_lossy()),
            Arc::new(mw_http::SniResolver::new(mw_http::CertStore::default())),
            FakePeers::empty(),
        )
        .is_ok());
    }
}
