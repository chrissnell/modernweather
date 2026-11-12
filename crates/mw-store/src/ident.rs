//! Minting the two identities this node allocates for itself: its `NodeId`, and an
//! enrolment token.
//!
//! **`NodeId` is a 128-bit UUIDv7 allocated once, by the node itself, at first start**
//! (W7 **D-5**). Not by a migration: a migration that invented one would make every fresh
//! install claim the same identity, which is why `migrations/config/` deliberately seeds no
//! `node` row and why an empty table means "not yet started" rather than "broken".
//!
//! # Why this is hand written
//!
//! `DESIGN.md` **D-32** fixes the dependency set and names neither a UUID crate nor a
//! random-number crate, and "a substitution is an amendment, not a manifest edit" — so the
//! two things needed here are written out, exactly as `mw_output::https` writes a request
//! out rather than adding an HTTP client. Both are small and both are checkable:
//!
//! - **The randomness is the kernel's**, read from `/dev/urandom`. Every release triple in
//!   `ci/release.toml` is Linux, so there is no second path to maintain, and a short read
//!   is an error rather than a buffer with zeros in the tail.
//! - **The layout is RFC 9562 §5.7**, and [`node_id`]'s tests assert the version and
//!   variant nibbles and the timestamp field against a known instant.
//!
//! UUIDv7 rather than v4 for the reason the schema's own comment gives: the first 48 bits
//! are a millisecond timestamp, so a node's identity sorts by when it was created, which is
//! what makes `peers` and `config.stations` readable in an operator's hands.
//!
//! An enrolment token is **not** a UUID. It is 32 raw bytes rendered as lower-case hex,
//! because its whole job is to be unguessable and typed once into another machine's command
//! line (W7 §5.3).

use std::io::Read as _;

use chrono::{DateTime, Utc};

/// Where the kernel's randomness comes from.
///
/// `/dev/urandom` rather than `getrandom(2)`: a syscall needs `libc` or an `unsafe` block,
/// and the workspace lint set warns on `unsafe_code`. After the system is seeded — which it
/// is long before a daemon starts — the two are the same CSPRNG.
const URANDOM: &str = "/dev/urandom";

/// Why an identity could not be minted.
#[derive(Debug, thiserror::Error)]
pub enum IdentError {
    /// The kernel's random device could not be read.
    ///
    /// Fatal rather than retried, and with no fallback: a `NodeId` or an enrolment token
    /// made from a weaker source is worse than one that was not made at all — the first is
    /// a collision or a guessable credential nobody will ever look at again, the second is
    /// an error message.
    #[error("{URANDOM} could not be read, so no identity can be minted")]
    Random {
        /// What the read said.
        #[source]
        source: std::io::Error,
    },
}

/// Fill `bytes` from the kernel's CSPRNG, or fail.
///
/// `read_exact`, so a short read is [`IdentError::Random`] and never a partially filled
/// buffer: the tail of a half-filled array is zeros, which is precisely the value that
/// makes a credential guessable.
pub(crate) fn fill(bytes: &mut [u8]) -> Result<(), IdentError> {
    let mut f = std::fs::File::open(URANDOM).map_err(|source| IdentError::Random { source })?;
    f.read_exact(bytes)
        .map_err(|source| IdentError::Random { source })
}

/// Mint this node's `NodeId`: a UUIDv7 over `now`.
///
/// RFC 9562 §5.7's layout, little of which is ours to choose:
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                 unix_ts_ms (48 bits, big endian)              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |      unix_ts_ms       |  ver  |        rand_a (12 bits)       |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |var|                     rand_b (62 bits)                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// A negative timestamp — a clock set before 1970 — is clamped to zero rather than
/// wrapping, which costs the sort order for one install and keeps the 74 random bits that
/// are what make the identity unique.
pub fn node_id(now: DateTime<Utc>) -> Result<[u8; 16], IdentError> {
    let millis = now.timestamp_millis().max(0) as u128;
    let mut out = [0u8; 16];
    out[..6].copy_from_slice(&millis.to_be_bytes()[10..]);
    fill(&mut out[6..])?;
    // Version 7 in the high nibble of octet 6, keeping the 12 bits of `rand_a` below it.
    out[6] = 0x70 | (out[6] & 0x0f);
    // Variant 10xx in the two high bits of octet 8.
    out[8] = 0x80 | (out[8] & 0x3f);
    Ok(out)
}

/// Sixteen random bytes, for an Argon2id salt.
///
/// Separate from [`enrolment_token`] because a salt is not a secret and is not rendered:
/// it goes straight into a PHC string's base64 field.
pub fn salt_bytes() -> Result<[u8; 16], IdentError> {
    let mut bytes = [0u8; 16];
    fill(&mut bytes)?;
    Ok(bytes)
}

/// Mint an enrolment token: 32 random bytes as 64 lower-case hex characters.
///
/// Hex rather than base64 because it is typed on a command line and pasted into a second
/// terminal: there is no case to get wrong, no `+`/`/` to quote, and no `=` for a shell to
/// eat. 32 bytes because the token is single-use and short-lived but is nevertheless the
/// one credential that can add a peer.
pub fn enrolment_token() -> Result<String, IdentError> {
    let mut bytes = [0u8; 32];
    fill(&mut bytes)?;
    Ok(hex(&bytes))
}

/// Lower-case hex, no separators.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Parse lower- or upper-case hex back into bytes, for a pinned SPKI read out of
/// `peers.auth_material`.
///
/// `None` on an odd length or a non-hex character, which a caller turns into "this row's
/// material is not a digest" rather than a panic: the column is text and a `CHECK` cannot
/// say "32 bytes of hex".
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.strip_prefix("sha256:").unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        out.push(u8::try_from(hi * 16 + lo).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_id_carries_version_7_and_the_rfc_variant() {
        let id = node_id(Utc::now()).expect("the kernel's random device is readable");
        assert_eq!(id[6] >> 4, 0x7, "version 7 in the high nibble of octet 6");
        assert_eq!(id[8] >> 6, 0b10, "variant 10xx in octet 8");
    }

    #[test]
    fn the_first_six_octets_are_the_millisecond_timestamp() {
        // 2026-11-10T00:00:00Z, so the expectation is a constant rather than a
        // re-derivation of the thing being tested.
        let at = DateTime::from_timestamp_millis(1_762_732_800_000).expect("an instant");
        let id = node_id(at).expect("readable");
        let mut ts = [0u8; 8];
        ts[2..].copy_from_slice(&id[..6]);
        assert_eq!(u64::from_be_bytes(ts), 1_762_732_800_000);
    }

    /// A clock before the epoch costs the sort order and nothing else. Without the clamp
    /// the cast would wrap and the timestamp field would read as the far future, which is
    /// worse: it is wrong *and* it sorts.
    #[test]
    fn a_pre_epoch_clock_clamps_rather_than_wrapping() {
        let at = DateTime::from_timestamp_millis(-1).expect("an instant");
        let id = node_id(at).expect("readable");
        assert_eq!(&id[..6], &[0, 0, 0, 0, 0, 0]);
        assert_eq!(id[6] >> 4, 0x7);
    }

    #[test]
    fn two_node_ids_minted_in_the_same_millisecond_differ() {
        let at = DateTime::from_timestamp_millis(1_762_732_800_000).expect("an instant");
        let a = node_id(at).expect("readable");
        let b = node_id(at).expect("readable");
        assert_ne!(a, b, "74 random bits, not a counter");
    }

    #[test]
    fn a_token_is_sixty_four_hex_characters() {
        let token = enrolment_token().expect("readable");
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(token.chars().all(|c| !c.is_ascii_uppercase()));
        assert_ne!(token, enrolment_token().expect("readable"));
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..32u8).collect();
        let text = hex(&bytes);
        assert_eq!(text.len(), 64);
        assert_eq!(unhex(&text).as_deref(), Some(bytes.as_slice()));
        // The `sha256:` prefix §8.4 writes on a pinned SPKI is accepted and stripped.
        assert_eq!(
            unhex(&format!("sha256:{text}")).as_deref(),
            Some(bytes.as_slice())
        );
    }

    #[test]
    fn unhex_refuses_what_is_not_a_digest() {
        assert_eq!(unhex("abc"), None, "odd length");
        assert_eq!(unhex("zz"), None, "not hex");
        assert_eq!(unhex(""), Some(Vec::new()), "empty is empty, not an error");
    }
}
