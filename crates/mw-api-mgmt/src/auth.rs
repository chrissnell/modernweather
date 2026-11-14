//! **Authentication**: the credential, the session, the setup claim, and the policy around
//! all three.
//!
//! W5 §3 and implementation-plan.md **M7-2**. One administrator, no RBAC, no user table, no
//! roles, no permissions (**D-35**) -- which is why nothing here is parameterised by a user
//! id: `migrations/config/20261102230000_config_schema_initial.sql` writes that decision as
//! `CHECK (id = 1)` on `admin_credential`, and a second account is unrepresentable rather
//! than discouraged.
//!
//! # What is in this module and what is in the binary
//!
//! Everything here is **pure or declarative**: the Argon2id parameters, the policy, the
//! digest, the constant-time comparison, the encodings, and the [`AuthStore`] trait. The SQL
//! is the binary's, for `ConfigAdmin`'s reason exactly -- `ci/layers.toml` forbids this crate
//! from naming `mw-store`, so what travels is a trait the consumer declares and the binary
//! implements (docs/service-architecture.md §1.3 rule 1).
//!
//! That split is not only a layering obligation. `modernweather reset-password` (W5 §3.6)
//! has to produce a hash that this crate's login will verify, and it runs in the binary with
//! no HTTP anywhere near it -- so the hashing has to be callable from both, which means it
//! cannot live behind a handler.
//!
//! # The four things being replaced, and what replaces each
//!
//! | Go | Here |
//! |---|---|
//! | a UUID used as a bearer token, compared with `==` | Argon2id over a username and password, verified in constant time |
//! | the session cookie set to the token's own value | a 32-byte secret whose SHA-256 is stored, revocable one row at a time |
//! | the token logged in cleartext on every failed attempt | [`AUDIT`] events carrying `{outcome, username, source_ip, session_id}` and no credential material |
//! | `Access-Control-Allow-Origin: *` and a `0.0.0.0` default bind | [`crate::guard`]'s exact-match allowlist, default empty, and a loopback default |
//!
//! # Two hashes, on purpose
//!
//! **Argon2id** for the password, at m=64 MiB/t=3/p=4 (§3.2), because the input is
//! low-entropy human text and the target hardware is a Raspberry Pi -- 64 MiB for a
//! once-a-week login is affordable on the 512 MB machine W3 §5.7 sizes against and 256 MiB
//! is not. The full PHC string is stored, so the parameters travel with the hash and a login
//! can re-hash transparently when they fall below policy ([`Verified::needs_rehash`]).
//!
//! **SHA-256** for the session secret and the setup code, because those are 32 CSPRNG bytes
//! and there is nothing to brute-force. Hashing them at all is not ceremony: a read of the
//! configuration database -- a backup, a support dump, an `scp` -- must not hand over a live
//! session. Using Argon2id here would also be a performance defect rather than extra
//! caution, because [`AuthStore::resolve`] runs on **every** management request.

use std::time::Duration;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use mw_component::BoxFuture;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, FieldError};

/// The `tracing` target every authentication outcome is recorded on.
///
/// W3 §5.6 makes `mw::audit` the one target that is **never suppressed**, which is the
/// property that matters here: a failed login that a log-level change can hide is not an
/// audit trail. §3.5 fixes the fields as `{outcome, username, source_ip, session_id}` and
/// forbids credential material of any kind -- including, explicitly, the thing the Go
/// version logged (`"Bearer token mismatch: got %s, expected %s"`) into the very ring this
/// API displays.
pub const AUDIT: &str = "mw::audit";

/// Argon2id's memory cost, in KiB. 64 MiB (§3.2).
pub const ARGON2_M_COST_KIB: u32 = 65_536;
/// Argon2id's time cost: three passes (§3.2).
pub const ARGON2_T_COST: u32 = 3;
/// Argon2id's parallelism: four lanes (§3.2).
pub const ARGON2_P_COST: u32 = 4;

/// The shortest password accepted, in **characters** (§3.5).
///
/// Twelve, with **no composition rules**: NIST SP 800-63B deprecates mandatory
/// character-class requirements, which push people towards `Password1!` and buy nothing.
/// Counted in `char`s rather than bytes so that a twelve-character passphrase in a
/// non-Latin script is not refused for being short.
pub const POLICY_MIN_CHARS: usize = 12;

/// The longest password accepted, in **bytes** (§3.5).
///
/// Bytes rather than characters here, and that asymmetry is the point: this bound exists
/// only to stop an unbounded Argon2 input, and what Argon2 reads is bytes.
pub const POLICY_MAX_BYTES: usize = 256;

/// The session cookie's name over a plaintext socket.
pub const COOKIE_NAME: &str = "mw_session";

/// The session cookie's name over TLS.
///
/// The `__Host-` prefix (§3.3) is only legal with `Secure`, `Path=/` and no `Domain`, so it
/// cannot be the name on a plaintext socket -- a browser would refuse the `Set-Cookie`
/// outright and the operator would see a login that silently never takes. Two names rather
/// than one conditional attribute, because the prefix *is* the enforcement: it stops a
/// sibling subdomain on the same registrable domain from overwriting the cookie.
pub const COOKIE_NAME_SECURE: &str = "__Host-mw_session";

/// The header every state-changing request must carry (§3.3).
///
/// Lowercase because `http::HeaderName` comparison is, and this constant is used to look one
/// up rather than to render one.
pub const CSRF_HEADER: &str = "x-mw-request";

/// Attempts 1..=3 are free; the delay starts at the fourth (§3.5).
const FREE_ATTEMPTS: u32 = 3;

/// The ceiling on the progressive delay, in seconds (§3.5).
///
/// A ceiling rather than a lockout, and §3.5 states the reason as a decision rather than a
/// preference: a hard lockout on an account with no second administrator to unlock it is a
/// self-inflicted denial of service. Thirty seconds costs an attacker everything and the
/// operator one coffee sip.
const DELAY_CEILING_SECONDS: u64 = 30;

/// How long a quiet period clears the failure counter, in seconds (§3.5).
const DELAY_RESET_SECONDS: i64 = 15 * 60;

/// The embedded breached-password list -- **O-10 / OQ-W5-2**, answered `(b)`.
///
/// Sorted bytewise, lowercase, every entry at least [`POLICY_MIN_CHARS`] long;
/// `crates/mw-api-mgmt/data/README.md` has the provenance and the derivation, and the tests
/// below re-assert both invariants so a replacement list that breaks either fails the build
/// rather than returning `false` for ever.
///
/// A list rather than HaveIBeenPwned's API, which §3.5 rules out for a reason that is not
/// about cost: an outbound call from a tool that must work offline, leaking a hash prefix of
/// the operator's password.
const BREACHED: &str = include_str!("../data/breached-passwords.txt");

// ---------------------------------------------------------------------------------------
// The password
// ---------------------------------------------------------------------------------------

/// **One hash at a time, process-wide.**
///
/// §3.2 justifies m=64 MiB with "login is a once-a-week operation for one person", and that
/// is an assumption this code should enforce rather than hope for: without it, N concurrent
/// requests to an unauthenticated endpoint -- `POST /api/v1/auth/login` on a claimed
/// instance, `POST /api/v1/auth/claim` on an unclaimed one -- allocate N x 64 MiB on the
/// 512 MB machine W3 §5.7 sizes against. With it the surface's peak cost is **64 MiB however
/// many callers arrive**, and the price is that a second simultaneous login waits for the
/// first, which for one operator is not a price at all.
///
/// A `std::sync::Mutex` rather than a `tokio::sync::Semaphore`, and the reason is worth
/// knowing before somebody "fixes" it: Argon2id is **synchronous**, so this guard is never
/// held across an `.await` -- and if a later change tried to hold one, the compiler would
/// refuse it rather than deadlocking at run time. It does block a runtime worker for the
/// duration of the hash, which it already did before this lock existed.
///
/// What this does **not** bound is CPU: a caller who can reach the socket can still spend
/// one core's worth of Argon2id. That is inherent to a password hash on a
/// pre-authentication endpoint, and the thing that makes it acceptable here is the surface's
/// reachability rather than this lock -- loopback until claimed (§3.4), and the progressive
/// delay afterwards (§3.5).
static HASHING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the hashing permit, ignoring a poisoned lock.
///
/// Poisoning would mean a previous hash panicked, which `argon2` does not do for
/// in-range parameters -- and refusing every subsequent login because of it would turn one
/// panic into a permanent lockout with no recovery path but a restart.
fn permit() -> std::sync::MutexGuard<'static, ()> {
    HASHING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Argon2id at the policy parameters.
///
/// Built per call rather than held in a `OnceLock`: `Argon2` is a handful of integers and no
/// allocation, and the cost of this function is nil beside the hash it is about to compute.
fn hasher() -> Argon2<'static> {
    let params = Params::new(ARGON2_M_COST_KIB, ARGON2_T_COST, ARGON2_P_COST, None)
        .expect("the policy parameters are in range for Argon2id");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password into the full PHC string the `admin_credential.password_phc` column takes.
///
/// `$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>` -- which is also what that column's
/// `CHECK (password_phc GLOB '$argon2id$?*')` admits, so a bare digest cannot be written
/// there later by something that decided PHC was overhead.
pub fn hash_password(password: &SecretString) -> Result<String, ApiError> {
    let _permit = permit();
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .hash_password(password.expose_secret().as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| ApiError::Internal {
            summary: "The password could not be stored.".to_string(),
            // The `argon2` error names a parameter, never the input: there is no path by
            // which this renders any part of the password.
            detail: Some(e.to_string()),
        })
}

/// The outcome of verifying a password against a stored PHC string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// Whether the password matched.
    pub ok: bool,
    /// Whether the stored parameters are **below** current policy, so the just-verified
    /// plaintext should be re-hashed and the row replaced (§3.2).
    ///
    /// Only ever read on `ok`: there is nothing to re-hash from on a failure. Below, not
    /// different -- a hash stored at *stronger* parameters than policy is left exactly
    /// alone, because downgrading a credential on the owner's behalf is not an upgrade.
    pub needs_rehash: bool,
}

impl Verified {
    /// A failure, which carries no re-hash question.
    const NO: Self = Self {
        ok: false,
        needs_rehash: false,
    };
}

/// Verify a password against a stored PHC string.
///
/// **Never returns an error.** A `password_phc` this build cannot parse is a mismatch, not a
/// 500: the alternative is an endpoint whose response distinguishes "wrong password" from
/// "the stored hash is corrupt", which is a fact about the install that an unauthenticated
/// caller has no business learning. The corruption is logged by the caller on [`AUDIT`]
/// either way, because that is where an operator will look.
pub fn verify_password(phc: &str, password: &SecretString) -> Verified {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return Verified::NO;
    };
    // Taken after the parse, so a stored hash this build cannot read is refused without
    // queueing behind anything.
    let _permit = permit();
    // Argon2's own verification is constant-time, which is why `subtle` appears below for
    // the session and setup secrets and not here.
    if hasher()
        .verify_password(password.expose_secret().as_bytes(), &parsed)
        .is_err()
    {
        return Verified::NO;
    }
    Verified {
        ok: true,
        needs_rehash: below_policy(&parsed),
    }
}

/// Whether a parsed PHC string's parameters are weaker than [`hasher`]'s.
///
/// A missing parameter counts as **below** policy rather than as equal to it: a PHC string
/// with no `m` is one this build did not write, and re-hashing it is the conservative answer.
fn below_policy(parsed: &PasswordHash<'_>) -> bool {
    let read = |name: &str| parsed.params.get_decimal(name);
    let (Some(m), Some(t), Some(p)) = (read("m"), read("t"), read("p")) else {
        return true;
    };
    m < ARGON2_M_COST_KIB || t < ARGON2_T_COST || p < ARGON2_P_COST
}

/// Whether a proposed password is on the breached list (**O-10**, `(b)`).
///
/// Lowercased before the search, so one stored entry covers `password1234`,
/// `Password1234` and `PASSWORD1234`. ASCII-only lowercasing, because every entry in the
/// list is ASCII: `str::to_lowercase` would also fold `İ` to two code points and make the
/// candidate longer than anything it could match.
///
/// Binary search **over the embedded bytes**, with no index built and nothing retained: the
/// alternative shapes are a `HashSet<&str>` or a `Vec<&str>` in a `OnceLock`, which are
/// ~500 KB and ~160 KB of heap held for the life of the process on a machine where the whole
/// point of §3.2's parameter choice was to fit in 512 MB. This is consulted twice in an
/// install's lifetime -- a claim and a password change -- so what matters is that it costs
/// nothing when it is not being used.
pub fn is_breached(password: &SecretString) -> bool {
    let candidate: String = password
        .expose_secret()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    contains_line(BREACHED, &candidate)
}

/// Whether a bytewise-sorted, newline-delimited `haystack` contains `needle` as a whole line.
///
/// The bisection is over byte offsets and each probe is widened to its enclosing line, which
/// is what makes it a search over lines without an array of them. `lo` is always a line
/// start; `hi` is always one past the end of the search window.
fn contains_line(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let (mut lo, mut hi) = (0usize, bytes.len());
    while lo < hi {
        // Back the midpoint up to the start of the line it landed in. `lo` is a line start,
        // so this cannot walk past it.
        let mut start = lo + (hi - lo) / 2;
        while start > lo && bytes[start - 1] != b'\n' {
            start -= 1;
        }
        let end = start
            + bytes[start..hi]
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(hi - start);
        let line = haystack[start..end].trim_end_matches('\r');
        match line.cmp(needle) {
            std::cmp::Ordering::Equal => return true,
            // The window below this line. `start` is a line start and `start < hi`, so this
            // shrinks the window on every iteration and cannot loop.
            std::cmp::Ordering::Greater => hi = start,
            // The window above it. `end + 1` is the next line's start, or past the end.
            std::cmp::Ordering::Less => lo = (end + 1).min(hi),
        }
    }
    false
}

/// Everything wrong with a proposed password, as the fields §7.2 attaches them to.
///
/// Empty means acceptable. `field` is the request body's path, so
/// `POST /api/v1/auth/claim`'s `password` and `POST /api/v1/auth/password`'s `new_password`
/// both land beside the input the operator typed into.
pub fn password_problems(password: &SecretString, field: &str) -> Vec<FieldError> {
    let secret = password.expose_secret();
    let mut out = Vec::new();
    if secret.chars().count() < POLICY_MIN_CHARS {
        out.push(FieldError::new(
            field,
            format!("Use at least {POLICY_MIN_CHARS} characters. Length is the only rule — a phrase you can remember beats a short password with symbols in it."),
        ));
    }
    if secret.len() > POLICY_MAX_BYTES {
        out.push(FieldError::new(
            field,
            format!("That is longer than {POLICY_MAX_BYTES} bytes, which is the most this daemon will hash."),
        ));
    }
    out
}

/// The non-blocking consequences of a proposed password (§8.5's `warnings`).
///
/// One entry today, and it is **advice rather than a refusal** by decision: **O-10** answers
/// OQ-W5-2 with "ship a list, warn, do not block". A list is a sample of what has leaked, not
/// a definition of what is weak, so blocking on it would refuse a perfectly good passphrase
/// for being someone else's and give the operator no way past it.
pub fn password_warnings(password: &SecretString) -> Vec<String> {
    if is_breached(password) {
        vec![
            "That password appears on a list of passwords found in public breaches. It will \
             work, but a different one would be better."
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------------------
// Digests and comparison
// ---------------------------------------------------------------------------------------

/// SHA-256, as the two `token_hash` columns hold it.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Constant-time equality over two digests (§3.5, **D-33**).
///
/// The session token hash and the setup token hash are both compared through this. Argon2
/// verification is already constant-time, so the password is not.
pub fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.ct_eq(b).into()
}

/// Lowercase hex, for the two opaque values that travel in a cookie.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is one hex digit"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("a nibble is one hex digit"));
    }
    out
}

/// The inverse of [`to_hex`], into a fixed-size array.
fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = char::from(bytes[i * 2]).to_digit(16)?;
        let lo = char::from(bytes[i * 2 + 1]).to_digit(16)?;
        *slot = u8::try_from(hi * 16 + lo).expect("two nibbles are one byte");
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------

/// A freshly minted session: the public handle, and the secret that is never stored.
///
/// `id` is 16 random bytes and is **safe to log** -- it is what `DELETE
/// /api/v1/auth/sessions/{id}` names and what the audit line carries. `secret` is 32 random
/// bytes and only its SHA-256 reaches the database.
pub struct Minted {
    /// The public handle.
    pub id: [u8; 16],
    /// The secret, for the one `Set-Cookie` that will ever carry it.
    secret: [u8; 32],
}

impl std::fmt::Debug for Minted {
    /// The id and **not** the secret, which is the whole reason this is hand-written.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("id", &to_hex(&self.id))
            .finish_non_exhaustive()
    }
}

impl Default for Minted {
    /// [`Minted::new`]. Present because `clippy::new_without_default` asks for it; a
    /// "default" session is a fresh random one, which is the only thing this type can be.
    fn default() -> Self {
        Self::new()
    }
}

impl Minted {
    /// Mint one from the OS CSPRNG.
    pub fn new() -> Self {
        let mut id = [0u8; 16];
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut id);
        OsRng.fill_bytes(&mut secret);
        Self { id, secret }
    }

    /// What the database stores: SHA-256 of the secret.
    pub fn token_hash(&self) -> [u8; 32] {
        digest(&self.secret)
    }

    /// The cookie's value: `<id>.<secret>`, both hex (§3.3).
    ///
    /// Hex rather than base64 because this is the one encoding that needs no alphabet
    /// decision -- a cookie value's permitted character set excludes `=` padding and
    /// base64url's `-`/`_` are fine but mean picking a variant, and nothing here is ever
    /// retyped by a person. The setup code is the value that is, and it uses base32 for
    /// exactly that reason.
    pub fn cookie_value(&self) -> String {
        format!("{}.{}", to_hex(&self.id), to_hex(&self.secret))
    }
}

/// A session's public handle, as it travels on the wire.
///
/// Hex, 32 characters. Safe to log and safe to put in a URL -- it is `DELETE
/// /api/v1/auth/sessions/{id}`'s path segment -- because knowing it reveals nothing: the
/// secret is the other half of the cookie and only its SHA-256 is stored.
pub fn session_id(id: &[u8; 16]) -> String {
    to_hex(id)
}

/// The inverse of [`session_id`]. `None` is a path segment that is not one.
pub fn parse_session_id(rendered: &str) -> Option<[u8; 16]> {
    from_hex::<16>(rendered)
}

/// `admin_credential.session_absolute_seconds`' own default: fourteen days (§3.3).
///
/// Duplicated from the schema for the one caller that cannot read it off a row -- the setup
/// claim, which creates that row. The schema is the authority everywhere else, which is why
/// this is `pub` with the reason attached rather than used as the general default.
pub const DEFAULT_ABSOLUTE_SECONDS: u64 = 14 * 24 * 60 * 60;

/// What a presented cookie resolves to before the database is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presented {
    /// The session row to look for.
    pub id: [u8; 16],
    /// SHA-256 of the presented secret, to compare against the stored hash.
    pub token_hash: [u8; 32],
}

impl Presented {
    /// Parse a cookie value, hashing the secret half immediately.
    ///
    /// The plaintext secret does not leave this function, which is what keeps it out of
    /// every log line, `{:?}` and error body downstream of it.
    pub fn parse(value: &str) -> Option<Self> {
        let (id, secret) = value.split_once('.')?;
        Some(Self {
            id: from_hex::<16>(id)?,
            token_hash: digest(&from_hex::<32>(secret)?),
        })
    }
}

/// Find the session cookie in a `Cookie` header.
///
/// Both names are looked for regardless of whether this socket is TLS, and the `__Host-`
/// one wins: a daemon that has just had a certificate configured will be presented the
/// plaintext cookie a browser still holds, and refusing it would sign the operator out for
/// no reason. The reverse case -- a `__Host-` cookie arriving on a plaintext socket -- cannot
/// happen, because a browser will not send one.
pub fn cookie_in(header: &str) -> Option<&str> {
    let mut plain = None;
    for pair in header.split(';') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        match name.trim() {
            COOKIE_NAME_SECURE => return Some(value.trim()),
            COOKIE_NAME => plain = Some(value.trim()),
            _ => {}
        }
    }
    plain
}

/// The `Set-Cookie` value that establishes a session (§3.3).
///
/// `max_age` rather than an `Expires` date, because a clock skewed by an hour on a Pi with
/// no RTC is the common case and `Max-Age` is relative.
pub fn set_cookie(minted: &Minted, max_age: Duration, secure: bool) -> String {
    let name = if secure {
        COOKIE_NAME_SECURE
    } else {
        COOKIE_NAME
    };
    let mut out = format!(
        "{name}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        minted.cookie_value(),
        max_age.as_secs()
    );
    if secure {
        out.push_str("; Secure");
    }
    out
}

/// The `Set-Cookie` value that clears a session.
///
/// Both names are cleared by [`crate::routes`] rather than only the one this socket would
/// set, so that signing out after a certificate was added does not leave the plaintext
/// cookie behind in the browser.
pub fn clear_cookie(secure: bool) -> String {
    let name = if secure {
        COOKIE_NAME_SECURE
    } else {
        COOKIE_NAME
    };
    let mut out = format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        out.push_str("; Secure");
    }
    out
}

// ---------------------------------------------------------------------------------------
// The setup code
// ---------------------------------------------------------------------------------------

/// Crockford's base32 alphabet: no `I`, `L`, `O` or `U`.
///
/// §3.4 asks for "base32-encoded without ambiguous characters -- this gets read off a
/// console and retyped", and this is the published answer to that requirement rather than a
/// local invention: `1`/`I`/`l`, `0`/`O` and `V`/`U` are the pairs a person confuses, and
/// [`canonical`] folds the first two back rather than rejecting them.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A setup code, minted once and printed once.
///
/// Holds the **display** form. The stored hash is [`SetupCode::token_hash`], and the display
/// form is dropped with this value the moment the banner has been written -- there is no
/// field anywhere that keeps it, which is what makes "shown once" a property of the code
/// rather than a promise in a log line.
pub struct SetupCode(String);

impl std::fmt::Debug for SetupCode {
    /// **Not** the code. The one place it is rendered is the banner, by the one caller that
    /// is supposed to render it; a `{:?}` in a log line is the exact leak §3.5's "nothing
    /// secret in logs" rule is about, and the ring this would reach is the one the
    /// management UI displays.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupCode(<not shown>)")
    }
}

impl SetupCode {
    /// Mint one from 32 bytes of OS CSPRNG output.
    ///
    /// Thirty-two bytes is `migrations/config/...initial.sql`'s own description of this
    /// column's input and §3.4's normative sentence; base32 turns them into **52
    /// characters**, grouped in thirteen fours. W5 §3.4's worked example shows eight groups,
    /// which is 160 bits and inconsistent with the sentence above it -- see
    /// `docs/reconciliation.md` **A-M7-2-3**. The sentence wins, because the two places that
    /// state a byte count agree with each other.
    pub fn issue() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(group(&encode_base32(&bytes)))
    }

    /// The display form, for the banner and for nothing else.
    pub fn display(&self) -> &str {
        &self.0
    }

    /// What the database stores: SHA-256 of the canonical form.
    pub fn token_hash(&self) -> [u8; 32] {
        digest(canonical(&self.0).as_bytes())
    }
}

/// The hash to compare a typed code against.
///
/// Canonicalised first, so hyphens, lowercase, and an `l` typed for a `1` all resolve to the
/// code that was printed. **The hash is of the canonical character string, not of the
/// decoded bytes**, which is why there is no base32 *decoder* here: a decoder would be the
/// only part of this that could get the padding wrong, and nothing needs the bytes back.
pub fn typed_setup_code(typed: &str) -> [u8; 32] {
    digest(canonical(typed).as_bytes())
}

/// Base32-encode with Crockford's alphabet, five bits at a time, most significant first.
fn encode_base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut acc: u16 = 0;
    let mut bits: u32 = 0;
    for byte in bytes {
        acc = (acc << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::from((acc >> bits) & 0x1f);
            out.push(char::from(CROCKFORD[index]));
        }
    }
    if bits > 0 {
        // The last group is left-aligned and zero-padded, which is the conventional
        // direction and the only one that keeps a prefix of the output a prefix of the
        // input's bits.
        let index = usize::from((acc << (5 - bits)) & 0x1f);
        out.push(char::from(CROCKFORD[index]));
    }
    out
}

/// Hyphenate in groups of four, for retyping.
fn group(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / 4);
    for (i, c) in code.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// The canonical form of a typed code: uppercase, no separators, ambiguous letters folded.
///
/// `I` and `l` fold to `1` and `O` to `0`, per Crockford. `U` is **not** folded to `V`: it is
/// excluded from the alphabet rather than confused with anything, so a `U` is a typo the
/// operator should see fail rather than a character this should guess at.
fn canonical(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The progressive delay
// ---------------------------------------------------------------------------------------

/// How much longer this account is refusing login attempts, in seconds (§3.5).
///
/// `None` means "try now". Attempts 1..=3 are free; after that the wait is
/// `min(2^(n-3), 30)` seconds measured from the last failure, and a quiet 15 minutes clears
/// the counter.
///
/// **Per username, not per IP**, which §3.5 states as a decision: there is one username, and
/// a per-IP counter is evaded by changing source address. The counter is persisted in
/// `admin_credential`, so a restart is not a way to clear it either.
///
/// Pure, over an explicit `now`, because this is the one piece of the login path worth
/// testing without a clock -- and because the caller has to apply it **before** Argon2
/// verification, so that the delay cannot be used as a timing oracle for whether the
/// password was right.
pub fn retry_after(failed_attempts: u32, last_failed_at: Option<i64>, now: i64) -> Option<u64> {
    let last = last_failed_at?;
    if failed_attempts <= FREE_ATTEMPTS {
        return None;
    }
    let quiet = now.saturating_sub(last);
    if quiet >= DELAY_RESET_SECONDS {
        return None;
    }
    // `min(2^(n-3), 30)` exactly, with `n` the attempt count and `FREE_ATTEMPTS` the 3: the
    // fourth failure waits **two** seconds, not one. `checked_shl` rather than `pow` because
    // a counter large enough to overflow the shift should land on the ceiling rather than
    // panic.
    let wait = 1u64
        .checked_shl(failed_attempts - FREE_ATTEMPTS)
        .unwrap_or(DELAY_CEILING_SECONDS)
        .min(DELAY_CEILING_SECONDS);
    // `quiet` is below the reset window and therefore non-negative.
    let elapsed = u64::try_from(quiet.max(0)).unwrap_or(u64::MAX);
    wait.checked_sub(elapsed).filter(|left| *left > 0)
}

// ---------------------------------------------------------------------------------------
// What the store has to be able to do
// ---------------------------------------------------------------------------------------

/// The stored credential, as the login path reads it.
///
/// Carries the PHC string, which is not a secret in the sense the `Credential` view's
/// omission rule is about -- but it never leaves this process: no endpoint returns it and
/// `GET /api/v1/config` has no field for it at all (§3.2).
#[derive(Debug, Clone)]
pub struct StoredCredential {
    /// The one administrator's username.
    pub username: String,
    /// The full PHC string.
    pub password_phc: String,
    /// How many consecutive failures, for [`retry_after`].
    pub failed_attempts: u32,
    /// When the last one was, in Unix seconds.
    pub last_failed_at: Option<i64>,
    /// The absolute session lifetime, in seconds.
    pub session_absolute_seconds: i64,
    /// The idle session lifetime, in seconds.
    pub session_idle_seconds: i64,
}

/// A live session, as [`AuthStore::resolve`] returns it.
#[derive(Debug, Clone)]
pub struct Session {
    /// The public handle.
    pub id: [u8; 16],
    /// When it was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it was last used.
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    /// When it stops being valid, whichever of the two lifetimes comes first.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// The browser that created it, if it said.
    pub user_agent: Option<String>,
    /// Where it was created from.
    pub created_ip: Option<String>,
}

/// What a new session row needs.
///
/// `created_at` is supplied rather than left to the column's `DEFAULT (unixepoch())`, which
/// is the small decision that lets the response carry the row: the handler renders the
/// session it just created from these three values instead of reading it back, so there is
/// no second query and no window in which the response describes a row that was written a
/// second later.
#[derive(Debug)]
pub struct NewSession {
    /// The minted secret, whose hash is stored.
    pub minted: Minted,
    /// When it was created, in Unix seconds. Also its initial `last_seen_at`.
    pub created_at: i64,
    /// Its absolute expiry, in Unix seconds.
    pub expires_at: i64,
    /// The `User-Agent` of the request that created it.
    pub user_agent: Option<String>,
    /// The source address of the request that created it.
    pub created_ip: Option<String>,
}

/// What creating the administrator account needs, as one transaction's worth of input.
///
/// One struct rather than four arguments because the whole point is that these commit or
/// roll back **together**: §3.4 requires the setup code to be invalidated in the same
/// transaction that creates the account, "so a race between two claimants resolves to
/// exactly one", and that is implementation-plan.md **M7-2**'s done-criterion.
#[derive(Debug)]
pub struct NewAccount {
    /// SHA-256 of the code the claimant typed, to compare against the stored one.
    pub typed_code: [u8; 32],
    /// The username to create.
    pub username: String,
    /// The password, already hashed.
    pub password_phc: String,
    /// The session the claimant is signed in with on the way out.
    pub session: NewSession,
}

impl NewSession {
    /// The row this will become, for the response that reports it.
    ///
    /// An `Option` only because `DateTime::from_timestamp` is: a Unix second outside
    /// chrono's range cannot come from `Utc::now()`, and a handler that cannot render what
    /// it stored is a `500` rather than a guess.
    pub fn as_session(&self) -> Option<Session> {
        let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0);
        Some(Session {
            id: self.minted.id,
            created_at: at(self.created_at)?,
            last_seen_at: at(self.created_at)?,
            expires_at: at(self.expires_at)?,
            user_agent: self.user_agent.clone(),
            created_ip: self.created_ip.clone(),
        })
    }
}

/// Why a claim was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRefusal {
    /// There is already an administrator. The instance is claimed.
    AlreadyClaimed,
    /// The typed code did not match the issued one.
    WrongCode,
    /// There is no setup code to match -- the instance has none outstanding.
    NoCodeOutstanding,
}

impl ClaimRefusal {
    /// A stable word for the audit line. Not operator-facing: the sentence the operator
    /// reads is `crate::routes`', because §0.1's vocabulary rule applies to the screen and
    /// a log field is not one.
    pub const fn as_str(self) -> &'static str {
        match self {
            ClaimRefusal::AlreadyClaimed => "already_claimed",
            ClaimRefusal::WrongCode => "wrong_code",
            ClaimRefusal::NoCodeOutstanding => "no_code_outstanding",
        }
    }
}

/// The stored credential and the session administration the management API needs.
///
/// Declared here and implemented in the binary, for [`crate::admin::ConfigAdmin`]'s reason:
/// every method below is SQL over the configuration database, `ci/layers.toml` forbids this
/// crate from naming `mw-store`, and the binary is the one place the query and the policy may
/// meet (docs/service-architecture.md §1.3 rule 1).
///
/// **Every method is a whole decision, not a row operation.** `open_session` records the
/// failure reset, the transparent re-hash and the new row in one transaction rather than
/// offering three writes a caller could get half right; `claim` and `change_password` are
/// single transactions because their atomicity is the requirement.
pub trait AuthStore: Send + Sync + 'static {
    /// Whether an administrator account exists.
    ///
    /// The one question the unauthenticated surface answers, because it is what the UI needs
    /// in order to show a setup screen rather than a login box (§1.1), and what decides
    /// whether the management socket is on loopback (§3.4).
    fn claimed(&self) -> BoxFuture<'_, Result<bool, ApiError>>;

    /// The stored credential, or `None` on an unclaimed instance.
    fn credential(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, ApiError>>;

    /// Record a failed login: bump `failed_attempts` and stamp `last_failed_at`.
    fn record_failure(&self) -> BoxFuture<'_, Result<(), ApiError>>;

    /// Record a successful login: clear the counter, replace the hash when `rehash` is
    /// `Some`, and insert the session -- in one transaction.
    fn open_session(
        &self,
        session: NewSession,
        rehash: Option<String>,
    ) -> BoxFuture<'_, Result<(), ApiError>>;

    /// Resolve a presented cookie to a live session.
    ///
    /// Compares the stored hash in constant time, enforces both expiries, and touches
    /// `last_seen_at`. `None` covers every reason a cookie is not a session -- no such row,
    /// wrong secret, expired, idle too long -- deliberately as one answer: the caller's
    /// response is `401` in all four cases and a richer return would only invite it to say
    /// which.
    fn resolve(&self, presented: Presented) -> BoxFuture<'_, Result<Option<Session>, ApiError>>;

    /// Every live session, newest first.
    fn sessions(&self) -> BoxFuture<'_, Result<Vec<Session>, ApiError>>;

    /// Revoke one session. `false` means there was no such session to revoke.
    fn revoke(&self, id: [u8; 16]) -> BoxFuture<'_, Result<bool, ApiError>>;

    /// Revoke every session, including the caller's own -- "sign out everywhere" (§3.3).
    fn revoke_all(&self) -> BoxFuture<'_, Result<u64, ApiError>>;

    /// Create the administrator account and invalidate the setup code **in one transaction**.
    fn claim(
        &self,
        account: NewAccount,
    ) -> BoxFuture<'_, Result<Result<(), ClaimRefusal>, ApiError>>;

    /// Replace the password, and revoke every session **except** `keep` (§3.3).
    ///
    /// One transaction, because the two halves are one decision: "my laptop was stolen" is
    /// answered by a password change that signs the thief out, and a change that committed
    /// without the revocation would be the opposite of what the operator asked for.
    fn change_password(
        &self,
        password_phc: String,
        keep: [u8; 16],
    ) -> BoxFuture<'_, Result<(), ApiError>>;
}

/// An in-memory [`AuthStore`], for this crate's own router tests.
///
/// Not `#[cfg(test)]`-only by accident: the real store is SQL in the binary, and what the
/// handlers in [`crate::routes`] need asserting -- the status codes, the cookie, the CSRF
/// refusal, the `401` on every other route -- is a property of the handler set rather than
/// of SQLite. The transactional guarantees are the opposite: "the setup code is invalidated
/// in the same transaction that creates the account" is only true of the real store, so it
/// is asserted in `crates/modernweather/tests/auth.rs` against the real binary and
/// deliberately **not** faked here, because a fake that holds a mutex would pass that test
/// while proving nothing.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::*;

    /// The whole store in one lock.
    #[derive(Debug, Default)]
    pub(crate) struct Store {
        inner: Mutex<State>,
    }

    #[derive(Debug, Default)]
    struct State {
        credential: Option<StoredCredential>,
        setup: Option<[u8; 32]>,
        sessions: Vec<(Session, [u8; 32])>,
    }

    impl Store {
        /// An instance with no administrator and one outstanding setup code.
        pub(crate) fn unclaimed() -> Self {
            Self::default()
        }

        /// An instance with an administrator whose password is `password`, and the setup
        /// code whose hash is `setup`.
        pub(crate) fn claimed(username: &str, password: &str, setup: Option<[u8; 32]>) -> Self {
            Self {
                inner: Mutex::new(State {
                    credential: Some(StoredCredential {
                        username: username.to_string(),
                        password_phc: hash_password(&SecretString::from(password))
                            .expect("the fixture hashes"),
                        failed_attempts: 0,
                        last_failed_at: None,
                        session_absolute_seconds: 1_209_600,
                        session_idle_seconds: 604_800,
                    }),
                    setup,
                    sessions: Vec::new(),
                }),
            }
        }

        /// Put an outstanding setup code on an instance.
        pub(crate) fn with_setup(self, hash: [u8; 32]) -> Self {
            self.lock().setup = Some(hash);
            self
        }

        /// Pre-load the failure counter, for the progressive-delay test.
        ///
        /// Taken after construction rather than during it, because constructing the fixture
        /// runs Argon2id at policy parameters -- hundreds of milliseconds, and seconds under
        /// a parallel test run -- and a `last_failed_at` stamped before that has already
        /// begun counting down.
        pub(crate) fn set_failures(&self, attempts: u32, at: i64) {
            if let Some(c) = self.lock().credential.as_mut() {
                c.failed_attempts = attempts;
                c.last_failed_at = Some(at);
            }
        }

        /// How many sessions are live.
        pub(crate) fn session_count(&self) -> usize {
            self.lock().sessions.len()
        }

        /// The stored PHC string, to assert a re-hash or a change took.
        pub(crate) fn phc(&self) -> Option<String> {
            self.lock()
                .credential
                .as_ref()
                .map(|c| c.password_phc.clone())
        }

        /// Overwrite the stored hash, for the transparent-upgrade test.
        pub(crate) fn set_phc(&self, phc: String) {
            if let Some(c) = self.lock().credential.as_mut() {
                c.password_phc = phc;
            }
        }

        /// Whether an administrator exists, without going through the trait.
        pub(crate) fn claimed_now(&self) -> bool {
            self.lock().credential.is_some()
        }

        /// How many consecutive failures are recorded.
        pub(crate) fn failures(&self) -> u32 {
            self.lock()
                .credential
                .as_ref()
                .map_or(0, |c| c.failed_attempts)
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    impl AuthStore for Store {
        fn claimed(&self) -> BoxFuture<'_, Result<bool, ApiError>> {
            Box::pin(async move { Ok(self.lock().credential.is_some()) })
        }

        fn credential(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, ApiError>> {
            Box::pin(async move { Ok(self.lock().credential.clone()) })
        }

        fn record_failure(&self) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                if let Some(c) = self.lock().credential.as_mut() {
                    c.failed_attempts += 1;
                    c.last_failed_at = Some(now().timestamp());
                }
                Ok(())
            })
        }

        fn open_session(
            &self,
            session: NewSession,
            rehash: Option<String>,
        ) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                let mut state = self.lock();
                if let Some(c) = state.credential.as_mut() {
                    c.failed_attempts = 0;
                    c.last_failed_at = None;
                    if let Some(phc) = rehash {
                        c.password_phc = phc;
                    }
                }
                state.sessions.push(row(&session));
                Ok(())
            })
        }

        fn resolve(
            &self,
            presented: Presented,
        ) -> BoxFuture<'_, Result<Option<Session>, ApiError>> {
            Box::pin(async move {
                let state = self.lock();
                Ok(state
                    .sessions
                    .iter()
                    .find(|(s, hash)| s.id == presented.id && ct_eq(hash, &presented.token_hash))
                    .filter(|(s, _)| s.expires_at > now())
                    .map(|(s, _)| s.clone()))
            })
        }

        fn sessions(&self) -> BoxFuture<'_, Result<Vec<Session>, ApiError>> {
            Box::pin(async move {
                Ok(self
                    .lock()
                    .sessions
                    .iter()
                    .map(|(s, _)| s.clone())
                    .collect())
            })
        }

        fn revoke(&self, id: [u8; 16]) -> BoxFuture<'_, Result<bool, ApiError>> {
            Box::pin(async move {
                let mut state = self.lock();
                let before = state.sessions.len();
                state.sessions.retain(|(s, _)| s.id != id);
                Ok(state.sessions.len() != before)
            })
        }

        fn revoke_all(&self) -> BoxFuture<'_, Result<u64, ApiError>> {
            Box::pin(async move {
                let mut state = self.lock();
                let n = state.sessions.len();
                state.sessions.clear();
                Ok(n as u64)
            })
        }

        fn claim(
            &self,
            account: NewAccount,
        ) -> BoxFuture<'_, Result<Result<(), ClaimRefusal>, ApiError>> {
            Box::pin(async move {
                let mut state = self.lock();
                if state.credential.is_some() {
                    return Ok(Err(ClaimRefusal::AlreadyClaimed));
                }
                let Some(stored) = state.setup else {
                    return Ok(Err(ClaimRefusal::NoCodeOutstanding));
                };
                if !ct_eq(&stored, &account.typed_code) {
                    return Ok(Err(ClaimRefusal::WrongCode));
                }
                state.credential = Some(StoredCredential {
                    username: account.username,
                    password_phc: account.password_phc,
                    failed_attempts: 0,
                    last_failed_at: None,
                    session_absolute_seconds: 1_209_600,
                    session_idle_seconds: 604_800,
                });
                state.setup = None;
                state.sessions.push(row(&account.session));
                Ok(Ok(()))
            })
        }

        fn change_password(
            &self,
            password_phc: String,
            keep: [u8; 16],
        ) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                let mut state = self.lock();
                if let Some(c) = state.credential.as_mut() {
                    c.password_phc = password_phc;
                }
                state.sessions.retain(|(s, _)| s.id == keep);
                Ok(())
            })
        }
    }

    fn row(session: &NewSession) -> (Session, [u8; 32]) {
        (
            session
                .as_session()
                .expect("the fixture's instants are in range"),
            session.minted.token_hash(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded list's two invariants, asserted rather than assumed: the lookup is a
    /// binary search, so an unsorted file would return `false` for most of its own entries,
    /// and an entry below the length minimum could never be reached because §3.5 refuses the
    /// password first. Both are properties of the **data**, which is why they are checked
    /// here rather than documented in the file.
    #[test]
    fn the_breached_list_is_sorted_and_every_entry_is_reachable() {
        let lines: Vec<&str> = BREACHED.lines().collect();
        assert_eq!(lines.len(), 10_000, "the list is the size O-10 specified");
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted, "the list is sorted bytewise");
        for entry in &lines {
            assert!(
                entry.chars().count() >= POLICY_MIN_CHARS,
                "`{entry}` is shorter than the minimum, so it can never be reached"
            );
            assert!(
                entry.len() <= POLICY_MAX_BYTES,
                "`{entry}` is longer than the maximum, so it can never be reached"
            );
            assert_eq!(
                *entry,
                entry.to_ascii_lowercase(),
                "`{entry}` is not lowercase, so the folded lookup cannot match it"
            );
        }
    }

    /// The bisection itself, against a list small enough to enumerate: every entry is found,
    /// nothing else is, and the three shapes that would hang or mis-answer a hand-written
    /// binary search -- the first line, the last line, and a one-line file -- are all here.
    #[test]
    fn the_line_search_finds_every_line_and_only_lines() {
        let list = "aaa\nbbb\nccc\nddd\neee\n";
        for entry in ["aaa", "bbb", "ccc", "ddd", "eee"] {
            assert!(contains_line(list, entry), "{entry} not found");
        }
        for miss in ["", "a", "aa", "aaaa", "bb", "fff", "zzz", "aaa\nbbb"] {
            assert!(!contains_line(list, miss), "`{miss}` found");
        }
        assert!(contains_line("only\n", "only"));
        assert!(!contains_line("only\n", "other"));
        assert!(!contains_line("", "anything"));
        // No trailing newline is still a line.
        assert!(contains_line("aaa\nbbb", "bbb"));
        // The real list, at both ends, which is where an off-by-one lands.
        let lines: Vec<&str> = BREACHED.lines().collect();
        for entry in [
            lines[0],
            lines[1],
            lines[lines.len() / 2],
            lines[lines.len() - 1],
        ] {
            assert!(contains_line(BREACHED, entry), "{entry} not found");
        }
    }

    /// A hit, a miss, and the case folding that makes one stored entry cover three typings.
    #[test]
    fn the_breached_lookup_folds_case_and_warns_without_blocking() {
        let hit = SecretString::from("qwerty123456");
        assert!(is_breached(&hit));
        assert!(is_breached(&SecretString::from("QWERTY123456")));
        assert!(is_breached(&SecretString::from("Qwerty123456")));
        assert!(!is_breached(&SecretString::from(
            "correct horse battery staple"
        )));

        // The whole of **O-10**'s answer: a hit is a warning and not a refusal.
        assert!(password_problems(&hit, "password").is_empty());
        assert_eq!(password_warnings(&hit).len(), 1);
        assert!(password_warnings(&SecretString::from("correct horse battery staple")).is_empty());
    }

    /// Length is the only rule, and it is counted the two different ways the two bounds need.
    #[test]
    fn the_policy_is_length_and_nothing_else() {
        // Eleven characters, no symbols: refused for being short.
        assert_eq!(
            password_problems(&SecretString::from("elevenchars"), "password").len(),
            1
        );
        // Twelve, still no symbols: accepted. A composition rule would have refused this and
        // accepted `Password1!`, which is the inversion NIST SP 800-63B deprecates.
        assert!(password_problems(&SecretString::from("twelvecharss"), "password").is_empty());
        // Twelve CHARACTERS of a multi-byte script is twelve characters.
        assert!(
            password_problems(&SecretString::from("ひらがなのパスワードです"), "password")
                .is_empty()
        );
        // The upper bound is bytes, because what Argon2 reads is bytes.
        let long = SecretString::from("a".repeat(POLICY_MAX_BYTES + 1));
        assert_eq!(password_problems(&long, "password").len(), 1);
    }

    /// A round trip at policy parameters, and the two things a verification has to get
    /// right: the wrong password fails, and a hash already at policy is **not** re-hashed.
    #[test]
    fn a_password_round_trips_and_a_current_hash_is_not_rehashed() {
        let password = SecretString::from("correct horse battery staple");
        let phc = hash_password(&password).expect("the policy parameters hash");
        assert!(
            phc.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"),
            "the PHC string carries the policy parameters: {phc}"
        );
        let verified = verify_password(&phc, &password);
        assert!(verified.ok);
        assert!(!verified.needs_rehash);
        assert!(!verify_password(&phc, &SecretString::from("something else")).ok);
    }

    /// Hashing is serialised process-wide, so the surface's peak memory is one Argon2id
    /// context however many callers arrive.
    ///
    /// Asserted as "they do not run at the same time" rather than by measuring memory: eight
    /// threads hash at once and the maximum observed overlap is one, which is the property
    /// the bound follows from.
    #[test]
    fn hashing_is_serialised_so_the_peak_cost_is_one_context() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static INSIDE: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    // The guard is taken inside `hash_password`, so the observation has to be
                    // of the lock itself: holding it and watching the counter is what shows
                    // that two callers cannot be inside at once.
                    let _permit = permit();
                    let now = INSIDE.fetch_add(1, Ordering::SeqCst) + 1;
                    PEAK.fetch_max(now, Ordering::SeqCst);
                    std::thread::yield_now();
                    INSIDE.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(PEAK.load(Ordering::SeqCst), 1);

        // And a poisoned lock does not lock the operator out for ever.
        //
        // Scoped and **explicitly joined**: `clippy.toml` forbids `std::thread::spawn`
        // outright ("a blocking worker belongs to `mw_supervisor::task` like any other
        // task"), and joining the handle here is also what keeps the deliberate panic inside
        // this assertion instead of propagating out of the scope.
        let poisoned = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _permit = permit();
                    panic!("a hash panicked");
                })
                .join()
        });
        assert!(poisoned.is_err());
        let password = SecretString::from("correct horse battery staple");
        assert!(hash_password(&password).is_ok(), "the lock stayed usable");
    }

    /// A hash stored below policy asks to be re-hashed; one stored above it is left alone.
    ///
    /// The second half is the one worth asserting: "below current policy" is §3.2's wording
    /// and the obvious implementation -- `!=` -- would quietly downgrade an operator who had
    /// raised the parameters by hand.
    #[test]
    fn a_weaker_stored_hash_asks_to_be_rehashed_and_a_stronger_one_does_not() {
        let password = SecretString::from("correct horse battery staple");
        let weaker = Params::new(19_456, 2, 1, None).expect("in range");
        let stronger = Params::new(131_072, 4, 4, None).expect("in range");
        for (params, expected) in [(weaker, true), (stronger, false)] {
            let salt = SaltString::generate(&mut OsRng);
            let phc = Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password(password.expose_secret().as_bytes(), &salt)
                .expect("hashes")
                .to_string();
            let verified = verify_password(&phc, &password);
            assert!(verified.ok, "{phc}");
            assert_eq!(verified.needs_rehash, expected, "{phc}");
        }
    }

    /// A `password_phc` this build cannot parse is a mismatch rather than a 500, because the
    /// alternative tells an unauthenticated caller something about the install.
    #[test]
    fn an_unparseable_stored_hash_is_a_mismatch() {
        let verified = verify_password("not a PHC string", &SecretString::from("anything at all"));
        assert!(!verified.ok);
        assert!(!verified.needs_rehash);
    }

    /// The cookie round trip, and the property the whole structure exists for: what the
    /// database holds is a hash, and the hash of the presented secret is what matches it.
    #[test]
    fn a_minted_session_is_recognised_by_its_hash_and_not_by_its_secret() {
        let minted = Minted::new();
        let cookie = minted.cookie_value();
        let presented = Presented::parse(&cookie).expect("the cookie value parses");
        assert_eq!(presented.id, minted.id);
        assert!(ct_eq(&presented.token_hash, &minted.token_hash()));

        // The stored hash is not the secret, so the cookie cannot be reconstructed from the
        // database -- which is the whole of §3.3's backup/support-dump argument.
        assert!(!cookie.contains(&to_hex(&minted.token_hash())));

        // Two mints do not collide, and one does not verify against the other.
        let other = Minted::new();
        assert_ne!(minted.id, other.id);
        assert!(!ct_eq(&presented.token_hash, &other.token_hash()));
    }

    /// Garbage in the cookie is `None` rather than a panic, at every boundary the parse has.
    #[test]
    fn a_malformed_cookie_is_not_a_session() {
        for value in [
            "",
            "nodot",
            ".",
            "0123456789abcdef0123456789abcdef.short",
            "zzzz.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            // Sixteen bytes of id and thirty-one of secret: the right shape, wrong length.
            "0123456789abcdef0123456789abcdef.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
        ] {
            assert!(Presented::parse(value).is_none(), "`{value}` parsed");
        }
    }

    /// The `__Host-` cookie wins when both are presented, and the plaintext one is still
    /// accepted on its own -- which is what stops a newly-configured certificate signing the
    /// operator out.
    #[test]
    fn the_host_prefixed_cookie_wins_and_the_plain_one_still_works() {
        assert_eq!(cookie_in("mw_session=plain"), Some("plain"));
        assert_eq!(cookie_in("__Host-mw_session=secure"), Some("secure"));
        assert_eq!(
            cookie_in("other=1; mw_session=plain; __Host-mw_session=secure"),
            Some("secure")
        );
        assert_eq!(cookie_in("other=1; nothing-here"), None);
    }

    /// Every attribute §3.3 names, and the one that is conditional.
    #[test]
    fn the_cookie_carries_every_attribute_section_3_3_names() {
        let minted = Minted::new();
        let plain = set_cookie(&minted, Duration::from_secs(1_209_600), false);
        assert!(plain.starts_with("mw_session="), "{plain}");
        for attribute in ["Path=/", "HttpOnly", "SameSite=Strict", "Max-Age=1209600"] {
            assert!(plain.contains(attribute), "{plain} is missing {attribute}");
        }
        assert!(!plain.contains("Secure"), "{plain}");

        let secure = set_cookie(&minted, Duration::from_secs(600), true);
        assert!(secure.starts_with("__Host-mw_session="), "{secure}");
        assert!(secure.contains("; Secure"), "{secure}");

        // Clearing uses the same name and attributes, which a browser requires in order to
        // match the cookie it is being asked to drop.
        assert!(clear_cookie(true).starts_with("__Host-mw_session=;"));
        assert!(clear_cookie(false).starts_with("mw_session=;"));
    }

    /// The setup code's shape, its entropy, and the retyping the alphabet exists for.
    #[test]
    fn a_setup_code_is_retypable_and_recognised_through_every_fold() {
        let code = SetupCode::issue();
        let display = code.display().to_string();
        // Thirty-two bytes is 256 bits, which is 52 base32 characters in 13 groups of four.
        assert_eq!(display.len(), 52 + 12, "{display}");
        assert_eq!(display.matches('-').count(), 12, "{display}");
        for c in display.chars().filter(|c| *c != '-') {
            assert!(
                CROCKFORD.contains(&u8::try_from(c).expect("ASCII")),
                "`{c}` is not in the alphabet"
            );
        }
        // No ambiguous character is ever emitted, which is the whole reason for the alphabet.
        for c in ['I', 'L', 'O', 'U'] {
            assert!(!display.contains(c), "{display} contains {c}");
        }

        // The code as printed, as retyped without hyphens, in lower case, and with the three
        // confusions a person makes, all hash to the same thing.
        let expected = code.token_hash();
        for typed in [
            display.clone(),
            display.replace('-', ""),
            display.to_lowercase(),
            display.replace('1', "l").replace('0', "O"),
            format!("  {display}  "),
        ] {
            assert!(
                ct_eq(&typed_setup_code(&typed), &expected),
                "`{typed}` did not match"
            );
        }
        // And a different code does not.
        assert!(!ct_eq(&SetupCode::issue().token_hash(), &expected));
    }

    /// `U` is excluded rather than folded, so typing one fails visibly.
    #[test]
    fn a_u_is_a_typo_rather_than_a_guess() {
        assert_ne!(canonical("VVVV"), canonical("UUUU"));
    }

    /// The code is not in its own `Debug`, which is the one way a value that is "printed
    /// once by design" reaches the ring the UI displays.
    #[test]
    fn a_setup_code_does_not_print_itself() {
        let code = SetupCode::issue();
        let rendered = format!("{code:?}");
        assert!(!rendered.contains(code.display().split('-').next().expect("a group")));
    }

    /// A minted session does not print its secret either.
    #[test]
    fn a_minted_session_does_not_print_its_secret() {
        let minted = Minted::new();
        let rendered = format!("{minted:?}");
        assert!(rendered.contains(&to_hex(&minted.id)), "{rendered}");
        assert!(!rendered.contains(&minted.cookie_value()), "{rendered}");
    }

    /// §3.5's schedule, at every boundary it has.
    #[test]
    fn the_progressive_delay_is_free_three_times_then_doubles_to_a_ceiling() {
        // Nothing has failed yet.
        assert_eq!(retry_after(0, None, 1_000), None);
        // The first three are free, which is what keeps a mistyped password from being a
        // punishment.
        for n in 1..=3 {
            assert_eq!(retry_after(n, Some(1_000), 1_000), None, "attempt {n}");
        }
        // Then `min(2^(n-3), 30)`: 2, 4, 8, 16, and a ceiling that does not grow.
        for (n, wait) in [(4, 2), (5, 4), (6, 8), (7, 16), (8, 30), (9, 30), (20, 30)] {
            assert_eq!(
                retry_after(n, Some(1_000), 1_000),
                Some(wait),
                "attempt {n}"
            );
        }
        // The wait counts down from the last failure rather than from the request.
        assert_eq!(retry_after(7, Some(1_000), 1_010), Some(6));
        assert_eq!(retry_after(7, Some(1_000), 1_016), None);
        // Fifteen minutes of quiet clears it however deep the counter is.
        assert_eq!(retry_after(40, Some(1_000), 1_000 + 15 * 60), None);
        // A shift wide enough to overflow a u64 lands on the ceiling rather than panicking.
        assert_eq!(retry_after(u32::MAX, Some(1_000), 1_000), Some(30));
    }

    /// A clock that went backwards is not a negative wait.
    #[test]
    fn a_backwards_clock_does_not_underflow_the_delay() {
        assert_eq!(retry_after(7, Some(2_000), 1_000), Some(16));
    }

    #[test]
    fn hex_round_trips_and_rejects_everything_else() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(from_hex::<4>("000fa5ff"), Some([0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(from_hex::<4>("000FA5FF"), Some([0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(from_hex::<4>("000fa5f"), None);
        assert_eq!(from_hex::<4>("000fa5fg"), None);
        // Multi-byte input whose `len()` happens to be right is still not hex.
        assert_eq!(from_hex::<2>("日本"), None);
    }
}
