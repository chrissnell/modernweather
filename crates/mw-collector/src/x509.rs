//! Two reads over a DER certificate: its `SubjectPublicKeyInfo`, and its `subjectAltName`
//! URIs.
//!
//! # Why this is hand written, and what it is not
//!
//! W7 §5.4 needs exactly two facts out of a client certificate:
//!
//! | `auth_kind` | What is read | What it decides |
//! |---|---|---|
//! | `mtls-pinned` | the SPKI's DER bytes, to be hashed | whether this key is a `peers` row at all |
//! | `mtls-ca` | the SAN URI `urn:modernweather:node:<uuid>` | which peer a chain-validated certificate is |
//!
//! `DESIGN.md` **D-32** fixes the dependency set and names no X.509 parser; `rustls` hands
//! over a [`CertificateDer`] and exposes neither field. So the two walks are written out,
//! exactly as `mw_output::https` writes an HTTP request out — and `clippy.toml`'s own
//! reasoning applies: the alternative is a crate for two functions.
//!
//! **This is not a validator and must never become one.** It parses nothing it does not
//! need, it verifies no signature, it checks no expiry, and it makes no trust decision. The
//! chain is already valid by the time either function runs:
//!
//! - under `mtls-ca`, `rustls`'s `WebPkiClientVerifier` has validated the chain against the
//!   peer CA before the connection exists, so the SAN is read out of a certificate an
//!   issuer vouched for;
//! - under `mtls-pinned` there is no chain to validate and the SPKI digest **is** the
//!   decision, so a wrong parse is a digest that matches nothing and refuses the peer.
//!
//! Both failure directions are therefore closed: a misparse cannot admit a peer, it can
//! only fail to recognise one. That is the property that makes 150 lines of DER walking an
//! acceptable thing to own, and it is why every helper below returns `Option` rather than
//! guessing.
//!
//! # The shape being walked (RFC 5280 §4.1)
//!
//! ```text
//! Certificate              ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
//! TBSCertificate           ::= SEQUENCE {
//!     version         [0] EXPLICIT INTEGER DEFAULT v1,   -- context tag 0xa0, optional
//!     serialNumber        INTEGER,
//!     signature           SEQUENCE,
//!     issuer              SEQUENCE,
//!     validity            SEQUENCE,
//!     subject             SEQUENCE,
//!     subjectPublicKeyInfo SEQUENCE,                     -- what `spki` returns, verbatim
//!     ...
//!     extensions      [3] EXPLICIT Extensions OPTIONAL   -- context tag 0xa3
//! }
//! ```

use mw_types::OriginId;
use rustls::pki_types::CertificateDer;
use tracing::warn;

/// The URN scheme a peer's `NodeId` travels in, per W7 §5.4's `mtls-ca` row.
pub const NODE_URN_PREFIX: &str = "urn:modernweather:node:";

/// DER identifier octets, named rather than inlined.
const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
/// `[0] EXPLICIT` — `TBSCertificate.version`.
const CONTEXT_0: u8 = 0xa0;
/// `[3] EXPLICIT` — `TBSCertificate.extensions`.
const CONTEXT_3: u8 = 0xa3;
/// `GeneralName.uniformResourceIdentifier`, which is `[6] IMPLICIT IA5String`.
const GENERAL_NAME_URI: u8 = 0x86;
/// `id-ce-subjectAltName`, `2.5.29.17`, as its DER content octets.
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];

/// One parsed TLV: its tag, its content, and the bytes after it.
#[derive(Debug, Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
    rest: &'a [u8],
}

/// Read one DER TLV from the front of `input`.
///
/// Definite-length form only. Indefinite length (`0x80`) is BER and not DER, and a
/// certificate carrying it is one no TLS implementation accepted in the first place, so it
/// reads as `None` rather than as a second parsing mode.
fn tlv(input: &[u8]) -> Option<Tlv<'_>> {
    let (&tag, after_tag) = input.split_first()?;
    let (&first, after_len) = after_tag.split_first()?;
    let (len, body) = if first < 0x80 {
        (usize::from(first), after_len)
    } else {
        let count = usize::from(first & 0x7f);
        // `0x80` is indefinite length, and a length over eight octets is longer than any
        // address space this runs on.
        if count == 0 || count > 8 || after_len.len() < count {
            return None;
        }
        let mut len = 0usize;
        for byte in &after_len[..count] {
            len = len.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        (len, &after_len[count..])
    };
    if body.len() < len {
        return None;
    }
    Some(Tlv {
        tag,
        value: &body[..len],
        rest: &body[len..],
    })
}

/// Read one TLV and require its tag.
fn expect(input: &[u8], tag: u8) -> Option<Tlv<'_>> {
    tlv(input).filter(|t| t.tag == tag)
}

/// `Certificate.tbsCertificate`'s content octets.
fn tbs(cert: &[u8]) -> Option<&[u8]> {
    let outer = expect(cert, SEQUENCE)?;
    Some(expect(outer.value, SEQUENCE)?.value)
}

/// The DER `SubjectPublicKeyInfo`, **including** its own `SEQUENCE` header.
///
/// The header is included because that is what RFC 5280 defines the SPKI to be and what
/// every other tool hashes: `openssl x509 -pubkey | openssl pkey -pubin -outform der |
/// sha256sum` produces a digest over these exact bytes, which is what makes a pin an
/// operator can compute themselves.
///
/// The field is positional — seventh in `TBSCertificate`, or sixth when the optional
/// `version` is absent — so the walk is "skip five `SEQUENCE`/`INTEGER` fields after the
/// optional version, then take the next one".
pub fn spki(cert: &CertificateDer<'_>) -> Option<Vec<u8>> {
    let mut rest = tbs(cert.as_ref())?;
    // `version [0] EXPLICIT`, present on every v3 certificate and absent on a v1 one.
    if let Some(version) = tlv(rest).filter(|t| t.tag == CONTEXT_0) {
        rest = version.rest;
    }
    // serialNumber, signature, issuer, validity, subject.
    let serial = expect(rest, INTEGER)?;
    let signature = expect(serial.rest, SEQUENCE)?;
    let issuer = expect(signature.rest, SEQUENCE)?;
    let validity = expect(issuer.rest, SEQUENCE)?;
    let subject = expect(validity.rest, SEQUENCE)?;
    // subjectPublicKeyInfo, re-serialised with its header so that the bytes are the ones
    // RFC 5280 names.
    let spki = expect(subject.rest, SEQUENCE)?;
    let consumed = subject.rest.len() - spki.rest.len();
    Some(subject.rest[..consumed].to_vec())
}

/// Every `uniformResourceIdentifier` in the certificate's `subjectAltName`, in order.
///
/// An empty vector is the ordinary answer for a certificate that names no URI, including
/// one with no `subjectAltName` at all and one whose SAN carries only DNS names.
pub fn san_uris(cert: &CertificateDer<'_>) -> Vec<String> {
    let Some(extensions) = extensions(cert.as_ref()) else {
        return Vec::new();
    };
    let mut rest = extensions;
    while let Some(extension) = tlv(rest) {
        rest = extension.rest;
        if extension.tag != SEQUENCE {
            continue;
        }
        // Extension ::= SEQUENCE { extnID OID, critical BOOLEAN DEFAULT FALSE,
        //                          extnValue OCTET STRING }
        let Some(id) = expect(extension.value, OID) else {
            continue;
        };
        if id.value != OID_SAN {
            continue;
        }
        // `critical` is optional, so the OCTET STRING is found by scanning rather than by
        // position. There is exactly one in an `Extension`.
        let mut tail = id.rest;
        let wrapped = loop {
            match tlv(tail) {
                Some(t) if t.tag == OCTET_STRING => break Some(t.value),
                Some(t) => tail = t.rest,
                None => break None,
            }
        };
        let Some(wrapped) = wrapped else { continue };
        let Some(names) = expect(wrapped, SEQUENCE) else {
            continue;
        };
        let mut out = Vec::new();
        let mut names = names.value;
        while let Some(name) = tlv(names) {
            names = name.rest;
            if name.tag == GENERAL_NAME_URI {
                // IA5String: ASCII by definition, so a non-ASCII byte means this is not the
                // field it claims to be and the name is skipped rather than lossily
                // decoded.
                if let Ok(uri) = std::str::from_utf8(name.value) {
                    if uri.is_ascii() {
                        out.push(uri.to_string());
                    }
                }
            }
        }
        return out;
    }
    Vec::new()
}

/// `TBSCertificate.extensions`' content octets.
fn extensions(cert: &[u8]) -> Option<&[u8]> {
    let mut rest = tbs(cert)?;
    // Scan rather than count: `extensions` is the last of three optional `[n] EXPLICIT`
    // fields and the two before it (`issuerUniqueID`, `subjectUniqueID`) are `[1]` and
    // `[2]`, so a positional walk would have to enumerate combinations that differ between
    // certificate versions. The tag is unambiguous at this level.
    while let Some(field) = tlv(rest) {
        rest = field.rest;
        if field.tag == CONTEXT_3 {
            return Some(expect(field.value, SEQUENCE)?.value);
        }
    }
    None
}

/// The `NodeId` a chain-validated certificate names, from its SAN URI.
///
/// `urn:modernweather:node:<uuid>`, with the UUID in the hyphenated textual form every
/// tool that mints a certificate writes. The comparison of the scheme is exact and
/// case-sensitive: a URN scheme is case-insensitive per RFC 8141, and accepting
/// `URN:MODERNWEATHER:` would mean two spellings of one identity — so the stricter reading
/// is taken and the operator's `-addext` has to match the constant.
///
/// `None` when no SAN URI parses, which under `mtls-ca` is a certificate the peer CA signed
/// for something that is not a node. The connection is then refused by layer 2 for having
/// no resolvable identity, which is the correct answer and not a fallback.
pub fn node_from_san(cert: &CertificateDer<'_>) -> Option<OriginId> {
    let mut named: Vec<[u8; 16]> = san_uris(cert)
        .iter()
        .filter_map(|uri| uri.strip_prefix(NODE_URN_PREFIX))
        .filter_map(parse_uuid)
        .collect();
    named.dedup();
    // **Exactly one, or none.** A certificate carrying two node URNs is ambiguous about
    // which node it is, and resolving it to whichever came first would make the identity a
    // function of the issuer's field order. Duplicates of the *same* id are not ambiguous
    // and are collapsed above.
    match named.as_slice() {
        [one] => Some(OriginId::new(*one)),
        [] => None,
        many => {
            warn!(
                urns = many.len(),
                "a certificate's subjectAltName names more than one node, so it names \
                 none: which node it is would otherwise depend on the issuer's field order"
            );
            None
        }
    }
}

/// A UUID in the canonical `8-4-4-4-12` form, or bare 32 hex characters, as 16 bytes.
///
/// **The shape is checked, not just the digits.** An earlier version stripped `-` from
/// anywhere and accepted any 32 hex characters left over, so
/// `-0-1-9-3F3C27A107B649C2E4F1B8A6D0E11-` and the canonical spelling were one identity —
/// which contradicts the reason this function refuses a trailing tail in the first place.
/// Two spellings of one identity is how a revocation comes to miss.
fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let hex: String = if text.contains('-') {
        let mut groups = text.split('-');
        let mut joined = String::with_capacity(32);
        for width in GROUPS {
            let group = groups.next()?;
            if group.len() != width {
                return None;
            }
            joined.push_str(group);
        }
        if groups.next().is_some() {
            return None;
        }
        joined
    } else {
        text.to_string()
    };
    if hex.len() != 32 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = char::from(bytes[i * 2]).to_digit(16)?;
        let lo = char::from(bytes[i * 2 + 1]).to_digit(16)?;
        *byte = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `subjectPublicKey`'s tag, used only to assert that what [`spki`] returned is an
    /// `SubjectPublicKeyInfo` without naming a key algorithm.
    const BIT_STRING: u8 = 0x03;

    use crate::testing::cert;

    #[test]
    fn a_truncated_length_reads_as_nothing_rather_than_panicking() {
        assert!(tlv(&[]).is_none());
        assert!(tlv(&[0x30]).is_none());
        assert!(tlv(&[0x30, 0x05, 0x01]).is_none(), "body shorter than len");
        assert!(tlv(&[0x30, 0x80]).is_none(), "indefinite length is BER");
        assert!(
            tlv(&[0x30, 0x89, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none(),
            "a nine-octet length is longer than any address space"
        );
    }

    #[test]
    fn a_long_form_length_is_read() {
        let mut der = vec![OCTET_STRING, 0x82, 0x01, 0x00];
        der.extend(std::iter::repeat_n(0xaa, 256));
        let t = tlv(&der).expect("a 256-byte octet string");
        assert_eq!(t.value.len(), 256);
        assert!(t.rest.is_empty());
    }

    #[test]
    fn rubbish_is_not_a_certificate() {
        let der = CertificateDer::from(vec![0u8; 0]);
        assert!(spki(&der).is_none());
        assert!(san_uris(&der).is_empty());
        let der = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(spki(&der).is_none());
        assert!(san_uris(&der).is_empty());
    }

    /// The pinned path's whole decision. Asserted against `openssl`'s own digest of the
    /// same certificate's public key, which is in `tests/data/peer-a-spki-sha256.txt`.
    #[test]
    fn the_spki_is_the_bytes_openssl_hashes() {
        let der = cert("peer-a-cert.pem");
        let spki = spki(&der).expect("a v3 certificate's SPKI");
        // An SPKI is a SEQUENCE of { AlgorithmIdentifier, BIT STRING }, so these two
        // assertions say "this is an SPKI" without naming a key type.
        assert_eq!(spki[0], SEQUENCE);
        let inner = tlv(&spki).expect("the SPKI sequence");
        let algorithm = expect(inner.value, SEQUENCE).expect("an AlgorithmIdentifier");
        assert_eq!(
            expect(algorithm.rest, BIT_STRING).map(|t| t.tag),
            Some(BIT_STRING),
            "the subjectPublicKey follows its algorithm"
        );
        let expected = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("data")
                .join("peer-a-spki-sha256.txt"),
        )
        .expect("the checked-in digest is readable");
        assert_eq!(
            crate::handshake::hex(&crate::handshake::sha256(&spki)),
            expected.trim(),
            "the digest must be the one `openssl dgst` produces, or an operator cannot \
             compute the pin they are asked to configure"
        );
    }

    #[test]
    fn the_san_uri_carries_the_node_id() {
        let der = cert("peer-a-cert.pem");
        let uris = san_uris(&der);
        assert_eq!(
            uris,
            vec!["urn:modernweather:node:0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11".to_string()],
            "the SAN is read out whole"
        );
        assert_eq!(
            node_from_san(&der),
            Some(OriginId::new([
                0x01, 0x93, 0xf3, 0xc2, 0x7a, 0x10, 0x7b, 0x64, 0x9c, 0x2e, 0x4f, 0x1b, 0x8a, 0x6d,
                0x0e, 0x11
            ]))
        );
    }

    /// A certificate with a DNS SAN and no URI is the common case for a *server*
    /// certificate, and must not resolve to a node.
    #[test]
    fn a_certificate_with_no_san_uri_names_no_node() {
        let der = cert("peer-dns-only-cert.pem");
        assert!(
            san_uris(&der).is_empty(),
            "a DNS name is not a uniformResourceIdentifier"
        );
        assert_eq!(node_from_san(&der), None);
        assert!(
            spki(&der).is_some(),
            "the SPKI is still readable, so such a certificate can still be pinned"
        );
    }

    #[test]
    fn a_uuid_parses_hyphenated_or_bare_and_nothing_else() {
        let expected = [
            0x01, 0x93, 0xf3, 0xc2, 0x7a, 0x10, 0x7b, 0x64, 0x9c, 0x2e, 0x4f, 0x1b, 0x8a, 0x6d,
            0x0e, 0x11,
        ];
        assert_eq!(
            parse_uuid("0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11"),
            Some(expected)
        );
        assert_eq!(
            parse_uuid("0193f3c27a107b649c2e4f1b8a6d0e11"),
            Some(expected)
        );
        assert_eq!(parse_uuid(""), None);
        assert_eq!(parse_uuid("0193f3c2-7a10"), None, "too short");
        assert_eq!(
            parse_uuid("0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e1100"),
            None,
            "a tail must not be ignored: two strings would be one identity"
        );
        assert_eq!(parse_uuid("zzzzzzzz-7a10-7b64-9c2e-4f1b8a6d0e11"), None);
        // **The group widths are checked, not just the digits.** Stripping `-` from
        // anywhere made every one of these the canonical id.
        assert_eq!(
            parse_uuid("-0-1-9-3f3c27a107b649c2e4f1b8a6d0e11-"),
            None,
            "hyphens are not whitespace: a UUID has five groups of fixed width"
        );
        assert_eq!(parse_uuid("0193f3c27a10-7b64-9c2e-4f1b-8a6d0e11"), None);
        assert_eq!(
            parse_uuid("0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11-"),
            None,
            "a sixth group is not a UUID"
        );
    }

    /// The scheme is matched exactly, so an operator's `-addext` has to say what the
    /// constant says.
    #[test]
    fn the_urn_scheme_is_matched_exactly() {
        assert_eq!(NODE_URN_PREFIX, "urn:modernweather:node:");
        assert!("URN:MODERNWEATHER:NODE:0193f3c27a107b649c2e4f1b8a6d0e11"
            .strip_prefix(NODE_URN_PREFIX)
            .is_none());
    }
}
