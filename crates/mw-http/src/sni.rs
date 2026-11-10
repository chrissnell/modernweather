//! Per-website TLS: the certificate store, the SNI resolver and the **DEV-6** miss chain
//! (§6.4).
//!
//! # What DEV-6 closes, and the one part of it rustls decides
//!
//! Two defects, both in `restserver/controller.go`:
//!
//! 1. **The wildcard match.** `strings.HasSuffix(hostname, domain)` after stripping `*.`
//!    matches any depth *and* matches `evilexample.com` for `*.example.com` -- a
//!    certificate handed out for a name it does not cover. [`crate::host::matches_one_label`]
//!    requires a `.` boundary and a single remaining label.
//! 2. **The miss.** `controller.go:826` iterates a Go map and returns the first certificate
//!    it finds, which is **non-deterministic between runs** and hands a client a
//!    certificate for a name it did not ask for. The chain here is exact → wildcard →
//!    the designated default → **refuse**.
//!
//! **The alert on a refusal is `access_denied`, not `unrecognized_name`, and that is
//! rustls's choice rather than this module's.** `docs/reconciliation.md` §3's DEV-6 entry
//! names `unrecognized_name`; a `ResolvesServerCert` that returns `None` makes rustls send
//! a fatal `AccessDenied` (`rustls-0.23.46/src/server/hs.rs:451`) and there is no hook to
//! choose the code. Choosing it would mean driving every handshake through
//! `LazyConfigAcceptor` and building a `ServerConfig` per connection, to change one byte of
//! an alert nothing branches on. What DEV-6 is *for* -- a diagnosable refusal instead of a
//! browser interstitial that blames the user -- is unaffected, and this is recorded here
//! rather than left as a diff.
//!
//! # The private key, and D-33
//!
//! The key file is read into a [`secrecy::SecretSlice`] and parsed from the exposed bytes,
//! so the PEM is zeroised when the load returns. What rustls keeps afterwards is a
//! `SigningKey`, whose own representation this crate does not control. So D-33 is honoured
//! for everything this code holds and no further claim is made for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use secrecy::{ExposeSecret, SecretSlice};

use crate::host::{matches_one_label, normalise};

/// One website's certificate paths, as the configuration holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertPaths {
    /// The hostname the certificate is keyed by. `*.example.com` is a wildcard entry; an
    /// empty hostname is a certificate for the default website and is reachable only
    /// through the fallback.
    pub hostname: String,
    /// `website.tls_cert_path`.
    pub cert: PathBuf,
    /// `website.tls_key_path`.
    pub key: PathBuf,
    /// `website.is_default_tls` -- the SNI fallback, tried before the default website's own
    /// certificate. At most one row may set it (a partial unique index says so).
    pub is_default: bool,
}

/// Why a certificate could not be loaded.
///
/// **Every variant is a build failure naming the path**, never a silent substitution of
/// some other site's certificate (§6.4). A daemon that starts with the wrong certificate is
/// worse than one that refuses to start: the first is discovered by a client, the second by
/// the operator who just changed the configuration.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    /// The file could not be read.
    #[error("cannot read {what} for `{hostname}` at {}", path.display())]
    Read {
        /// `"the certificate"` or `"the private key"`.
        what: &'static str,
        /// Which website's.
        hostname: String,
        /// The configured path.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The certificate file held no certificate.
    #[error("{} holds no PEM certificate for `{hostname}`", path.display())]
    NoCertificate {
        /// Which website's.
        hostname: String,
        /// The configured path.
        path: PathBuf,
    },

    /// The key file held no private key.
    #[error("{} holds no PEM private key for `{hostname}`", path.display())]
    NoPrivateKey {
        /// Which website's.
        hostname: String,
        /// The configured path.
        path: PathBuf,
    },

    /// The key is not one `ring` can sign with.
    #[error("the private key at {} for `{hostname}` is not usable", path.display())]
    UnusableKey {
        /// Which website's.
        hostname: String,
        /// The configured path.
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },
}

impl mw_types::Classify for CertError {
    /// Fatal, for [`crate::HttpError`]'s reason: a path that is wrong is not made right by
    /// asking again, and the supervisor must not re-decide that at the call site.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

/// Every website's certificate, as one generation.
#[derive(Debug, Default)]
pub struct CertStore {
    exact: HashMap<Box<str>, Arc<CertifiedKey>>,
    wildcard: Vec<(Box<str>, Arc<CertifiedKey>)>,
    default: Option<Arc<CertifiedKey>>,
    generation: u64,
}

impl CertStore {
    /// Load every configured certificate, in configuration order.
    ///
    /// `fallback` is the default website's id-order position: when no row sets
    /// `is_default_tls`, the default website's own certificate is the fallback if it has
    /// one. Pass the same hostname the host table's default resolves to, or `None`.
    pub fn load(paths: &[CertPaths], fallback: Option<&str>) -> Result<Self, CertError> {
        let mut store = CertStore::default();
        let fallback = fallback.map(normalise);
        for entry in paths {
            let key = Arc::new(certified_key(entry)?);
            let hostname = normalise(&entry.hostname);
            if entry.is_default {
                // The designated fallback (`website.is_default_tls`), which wins over the
                // default website's own certificate.
                store.default = Some(Arc::clone(&key));
            } else if store.default.is_none() && fallback.as_deref() == Some(hostname.as_str()) {
                store.default = Some(Arc::clone(&key));
            }
            match hostname.strip_prefix("*.") {
                Some(suffix) => store.wildcard.push((suffix.into(), key)),
                None if hostname.is_empty() => {
                    // A certificate on the default-website row, which has no hostname to be
                    // keyed by. It is reachable through the fallback and nowhere else.
                    if store.default.is_none() {
                        store.default = Some(key);
                    }
                }
                None => {
                    store.exact.entry(hostname.into()).or_insert(key);
                }
            }
        }
        Ok(store)
    }

    /// Record which rebuild this is.
    #[must_use]
    pub fn generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// How many certificates are keyed by a name, wildcards included.
    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    /// Whether no certificate is configured at all -- the fallback included.
    pub fn is_empty(&self) -> bool {
        self.len() == 0 && self.default.is_none()
    }

    /// Whether a designated or inherited fallback exists.
    pub fn has_default(&self) -> bool {
        self.default.is_some()
    }

    /// **DEV-6**'s chain: exact → wildcard → designated default → `None`.
    ///
    /// `None` for a client sending no SNI at all is not a separate case: an IP-address
    /// request or an ancient client takes the same path and gets the designated default,
    /// because the empty name matches nothing and falls through.
    pub fn resolve(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let name = server_name.map(normalise).unwrap_or_default();
        if let Some(key) = self.exact.get(name.as_str()) {
            return Some(Arc::clone(key));
        }
        if let Some((_, key)) = self
            .wildcard
            .iter()
            .find(|(suffix, _)| matches_one_label(&name, suffix))
        {
            return Some(Arc::clone(key));
        }
        self.default.clone()
    }
}

fn certified_key(entry: &CertPaths) -> Result<CertifiedKey, CertError> {
    let certs = read_certs(&entry.cert, &entry.hostname)?;
    if certs.is_empty() {
        return Err(CertError::NoCertificate {
            hostname: entry.hostname.clone(),
            path: entry.cert.clone(),
        });
    }
    let key = read_key(&entry.key, &entry.hostname)?;
    let signing = any_supported_type(&key).map_err(|source| CertError::UnusableKey {
        hostname: entry.hostname.clone(),
        path: entry.key.clone(),
        source,
    })?;
    Ok(CertifiedKey::new(certs, signing))
}

fn read_certs(path: &Path, hostname: &str) -> Result<Vec<CertificateDer<'static>>, CertError> {
    let pem = std::fs::read(path).map_err(|source| CertError::Read {
        what: "the certificate",
        hostname: hostname.to_string(),
        path: path.to_path_buf(),
        source,
    })?;
    rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| CertError::Read {
            what: "the certificate",
            hostname: hostname.to_string(),
            path: path.to_path_buf(),
            source,
        })
}

fn read_key(path: &Path, hostname: &str) -> Result<PrivateKeyDer<'static>, CertError> {
    // Read into a secret, parse from the exposed bytes, and let the secret zeroise on the
    // way out (**D-33**). The parse borrows, so the clone below is what outlives it.
    let pem: SecretSlice<u8> = std::fs::read(path)
        .map_err(|source| CertError::Read {
            what: "the private key",
            hostname: hostname.to_string(),
            path: path.to_path_buf(),
            source,
        })?
        .into();
    let parsed = rustls_pemfile::private_key(&mut pem.expose_secret()).map_err(|source| {
        CertError::Read {
            what: "the private key",
            hostname: hostname.to_string(),
            path: path.to_path_buf(),
            source,
        }
    })?;
    parsed.ok_or_else(|| CertError::NoPrivateKey {
        hostname: hostname.to_string(),
        path: path.to_path_buf(),
    })
}

/// The resolver rustls asks, over an [`ArcSwap`] so that a renewal is one pointer write.
///
/// **A certificate swap is not a listener restart**, which is the other half of §6.4: the
/// socket is untouched, in-flight handshakes keep the `Arc` they loaded, and the next
/// `ClientHello` sees the new store. The Go version rebuilds `tlsConfigs` as a plain map
/// (`controller.go:708`, `:734`) while `getCertificate` reads it.
#[derive(Debug)]
pub struct SniResolver {
    store: ArcSwap<CertStore>,
}

impl SniResolver {
    /// A resolver over one generation of the store.
    pub fn new(store: CertStore) -> Self {
        Self {
            store: ArcSwap::from_pointee(store),
        }
    }

    /// Install a new generation. One atomic write; nothing waits.
    pub fn swap(&self, store: CertStore) {
        self.store.store(Arc::new(store));
    }

    /// How many names resolve in the current generation.
    pub fn len(&self) -> usize {
        self.store.load().len()
    }

    /// Whether the current generation can answer no handshake at all.
    pub fn is_empty(&self) -> bool {
        self.store.load().is_empty()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.store.load().resolve(hello.server_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed key pair, generated once and checked in as a test fixture would be —
    /// except that generating one needs a certificate library this crate does not have, so
    /// the store is tested over hand-built `CertifiedKey`s instead and the *loader* is
    /// tested over its failure paths. `tests/tls.rs` drives a real handshake.
    fn store(exact: &[(&str, u8)], wildcard: &[(&str, u8)], default: Option<u8>) -> CertStore {
        CertStore {
            exact: exact
                .iter()
                .map(|(h, tag)| ((*h).into(), fake(*tag)))
                .collect(),
            wildcard: wildcard
                .iter()
                .map(|(h, tag)| ((*h).into(), fake(*tag)))
                .collect(),
            default: default.map(fake),
            generation: 0,
        }
    }

    /// A `CertifiedKey` whose identity is one byte of its certificate, so a test can say
    /// *which* certificate resolved without a real key pair.
    fn fake(tag: u8) -> Arc<CertifiedKey> {
        Arc::new(CertifiedKey::new(
            vec![CertificateDer::from(vec![tag])],
            Arc::new(NoSign),
        ))
    }

    fn tag(key: &Arc<CertifiedKey>) -> u8 {
        key.cert[0].as_ref()[0]
    }

    #[derive(Debug)]
    struct NoSign;

    impl rustls::sign::SigningKey for NoSign {
        fn choose_scheme(
            &self,
            _offered: &[rustls::SignatureScheme],
        ) -> Option<Box<dyn rustls::sign::Signer>> {
            None
        }

        fn algorithm(&self) -> rustls::SignatureAlgorithm {
            rustls::SignatureAlgorithm::ECDSA
        }
    }

    #[test]
    fn exact_beats_wildcard_beats_default() {
        let s = store(&[("a.example.com", 1)], &[("example.com", 2)], Some(3));
        assert_eq!(tag(&s.resolve(Some("a.example.com")).expect("exact")), 1);
        assert_eq!(tag(&s.resolve(Some("b.example.com")).expect("wildcard")), 2);
        assert_eq!(tag(&s.resolve(Some("other.test")).expect("default")), 3);
        assert_eq!(s.len(), 2);
        assert!(s.has_default());
    }

    /// **DEV-6**, in the two shapes the Go check gets wrong.
    #[test]
    fn a_wildcard_does_not_cover_another_domain_or_a_deeper_label() {
        let s = store(&[], &[("example.com", 2)], None);
        assert!(
            s.resolve(Some("evilexample.com")).is_none(),
            "the Go check hands out example.com's certificate for evilexample.com"
        );
        assert!(
            s.resolve(Some("a.b.example.com")).is_none(),
            "the Go check matches any depth"
        );
        assert_eq!(
            tag(&s.resolve(Some("a.example.com")).expect("one label")),
            2
        );
    }

    /// The miss, which is the whole of the second half of DEV-6: a refusal rather than an
    /// arbitrary certificate chosen by Go map iteration order.
    #[test]
    fn a_miss_with_no_default_refuses() {
        let s = store(&[("a.example.com", 1)], &[], None);
        assert!(s.resolve(Some("nobody.test")).is_none());
        assert!(
            s.resolve(None).is_none(),
            "and so does a client with no SNI"
        );
    }

    #[test]
    fn no_sni_takes_the_designated_default() {
        let s = store(&[("a.example.com", 1)], &[], Some(9));
        assert_eq!(tag(&s.resolve(None).expect("the default")), 9);
    }

    #[test]
    fn sni_is_matched_case_insensitively() {
        let s = store(&[("a.example.com", 1)], &[], None);
        assert_eq!(tag(&s.resolve(Some("A.Example.COM")).expect("exact")), 1);
    }

    #[test]
    fn an_empty_store_answers_nothing() {
        let s = store(&[], &[], None);
        assert!(s.is_empty());
        assert!(s.resolve(Some("anything")).is_none());
    }

    #[test]
    fn a_swap_is_observed_by_the_next_resolve() {
        let r = SniResolver::new(store(&[("a.test", 1)], &[], None));
        assert_eq!(r.len(), 1);
        r.swap(store(&[("a.test", 1), ("b.test", 2)], &[], None));
        assert_eq!(r.len(), 2);
        assert!(!r.is_empty());
    }

    #[test]
    fn a_missing_certificate_file_names_the_path() {
        let err = CertStore::load(
            &[CertPaths {
                hostname: "a.test".into(),
                cert: PathBuf::from("/nonexistent/mw-http/cert.pem"),
                key: PathBuf::from("/nonexistent/mw-http/key.pem"),
                is_default: false,
            }],
            None,
        )
        .expect_err("an unreadable path is a build failure, not a substitution");
        let text = mw_types::DisplayChain(&err).to_string();
        assert!(text.contains("/nonexistent/mw-http/cert.pem"), "{text}");
        assert!(text.contains("a.test"), "{text}");
        assert_eq!(mw_types::Classify::class(&err), mw_types::Class::Fatal);
    }

    #[test]
    fn a_certificate_file_with_no_pem_in_it_is_named_as_such() {
        let dir = std::env::temp_dir().join(format!("mw-http-sni-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, b"not a certificate\n").expect("write");
        std::fs::write(&key, b"not a key\n").expect("write");
        let err = CertStore::load(
            &[CertPaths {
                hostname: "a.test".into(),
                cert: cert.clone(),
                key,
                is_default: false,
            }],
            None,
        )
        .expect_err("an empty certificate file is not a certificate");
        assert!(
            matches!(err, CertError::NoCertificate { .. }),
            "{}",
            mw_types::DisplayChain(&err)
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
