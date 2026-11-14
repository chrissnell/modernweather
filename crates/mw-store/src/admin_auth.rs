//! The three administrative-authentication tables, as statements.
//!
//! `admin_credential`, `admin_session` and `admin_setup_token` — W5 §3, D-35, and
//! implementation-plan.md **M7-2**. Separate from [`crate::config_db`] and
//! [`crate::config_admin`] because what these rows are is different in kind: they are not
//! configuration. Nothing here is in `Config`, nothing here is in a plan, and no write below
//! bumps a generation or restarts a component — which is also why they are not a
//! `config_admin::Change`.
//!
//! # What this module is and is not
//!
//! **It is statements.** Every policy decision — the Argon2id parameters, the progressive
//! delay's schedule, what a session's secret is, how a setup code is encoded — is
//! `mw_api_mgmt::auth`'s, and `ci/layers.toml` forbids that crate from naming this one. So
//! what crosses is a hash, a digest and an integer; this module never sees a password.
//!
//! **Three functions are deliberately whole transactions**, because their atomicity is the
//! requirement rather than an optimisation:
//!
//! | | |
//! |---|---|
//! | [`claim`] | creates the account and deletes the setup token together, so a race between two claimants resolves to exactly one (§3.4, **M7-2**'s done-criterion) |
//! | [`change_password`] | replaces the hash and revokes every other session together, because "my laptop was stolen" is answered by both or neither (§3.3) |
//! | [`open_session`] | clears the failure counter, applies the transparent re-hash and inserts the row together, so a login cannot half-succeed (§3.2) |
//!
//! # Why the read on every request is also a write, and why it is two functions
//!
//! §3.3's idle expiry needs `last_seen_at`, so a session that is being used has to say so.
//! [`touch`] is that write, and it is **separate from [`resolve`]** because it must happen
//! only after the caller has compared the presented secret: a session id is public by design,
//! and a touch on the strength of the id alone would let anyone who has seen one hold a
//! session open for ever without being able to use it.
//!
//! It is also **debounced** — the staleness test is in the statement's own `WHERE` clause, so
//! it is one `UPDATE` that usually matches nothing rather than a write per request to an SD
//! card on a Raspberry Pi, for a column whose resolution requirement is measured in days.

use sqlx::sqlite::SqliteConnection;
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq as _;

/// `BEGIN IMMEDIATE`, for the three writes whose atomicity is a stated requirement.
///
/// SQLite's default `BEGIN` is **deferred**: it takes no lock until the first write, so two
/// concurrent claimants both read "unclaimed", and the loser then fails at its `INSERT` with
/// `SQLITE_BUSY_SNAPSHOT` in WAL mode -- the right outcome reached by an error the operator
/// sees as a `500`. `BEGIN IMMEDIATE` takes the write lock up front, so the two serialise at
/// the first statement and the loser reads the account the winner wrote and gets
/// [`Claimed::AlreadyClaimed`].
const IMMEDIATE: &str = "BEGIN IMMEDIATE";

/// How stale `last_seen_at` has to be before a request updates it, in seconds.
///
/// A minute. The column feeds §3.3's seven-day idle expiry and the session list's "last
/// used" line, so a minute of imprecision is invisible in both — and it turns a write per
/// request into a write per minute of activity.
const TOUCH_AFTER_SECONDS: i64 = 60;

/// Why an authentication read or write failed.
///
/// Typed per module and preserving its cause chain (**D-34**). One variant, because there is
/// only one thing that can go wrong down here: the statement did not run. A wrong password,
/// an expired session and an already-claimed instance are all *outcomes* rather than errors
/// and are in the return types below.
#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    #[error("the administrator credential store could not be {what}")]
    Query {
        /// Which operation, in words an operator can act on.
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

fn failed(what: &'static str) -> impl FnOnce(sqlx::Error) -> AuthStoreError {
    move |source| AuthStoreError::Query { what, source }
}

/// The stored credential, as the login path needs it.
///
/// Carries the PHC string. It is not returned by any endpoint and `GET /api/v1/config` has
/// no field for it at all (§3.2), which is why there is no `Credential`-style boolean
/// projection here: nothing outside the login path ever reads this row.
#[derive(Debug, Clone)]
pub struct Credential {
    /// The one administrator's username.
    pub username: String,
    /// The full PHC string, `$argon2id$v=19$m=...`.
    pub password_phc: String,
    /// Consecutive failures, for the progressive delay.
    pub failed_attempts: u32,
    /// When the last one was, in Unix seconds.
    pub last_failed_at: Option<i64>,
    /// The absolute session lifetime, in seconds.
    pub session_absolute_seconds: i64,
    /// The idle session lifetime, in seconds.
    pub session_idle_seconds: i64,
}

/// A session row.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// The public handle, 16 bytes.
    pub id: [u8; 16],
    /// When it was created, in Unix seconds.
    pub created_at: i64,
    /// When it was last used, in Unix seconds.
    pub last_seen_at: i64,
    /// When it expires, in Unix seconds.
    pub expires_at: i64,
    /// The browser that created it, if it said.
    pub user_agent: Option<String>,
    /// Where it was created from.
    pub created_ip: Option<String>,
}

/// What a new session row carries.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The public handle.
    pub id: [u8; 16],
    /// SHA-256 of the secret. **The secret itself never reaches this crate.**
    pub token_hash: [u8; 32],
    /// When it was created, in Unix seconds — also its initial `last_seen_at`.
    pub created_at: i64,
    /// When it expires, in Unix seconds.
    pub expires_at: i64,
    /// The `User-Agent` of the request that created it.
    pub user_agent: Option<String>,
    /// The source address of the request that created it.
    pub created_ip: Option<String>,
}

/// What creating the administrator account carries.
#[derive(Debug, Clone)]
pub struct NewAccount {
    /// SHA-256 of the code the claimant typed.
    pub typed_code: [u8; 32],
    /// The username to create.
    pub username: String,
    /// The password, already hashed into a PHC string.
    pub password_phc: String,
    /// The session the claimant is signed in with on the way out.
    pub session: NewSession,
}

/// How a claim ended.
///
/// A three-way outcome rather than a `Result<(), E>` because none of the three is an error in
/// this layer's sense: the statements all ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// The account was created and the setup token is gone.
    Created,
    /// There is already an administrator.
    AlreadyClaimed,
    /// There is a setup token and the typed code is not it.
    WrongCode,
    /// There is no setup token outstanding.
    NoCodeOutstanding,
}

/// Whether an administrator account exists.
///
/// Generic over the executor for [`crate::config_db`]'s reason: the boot sequence asks the
/// pool and [`claim`] asks its own open transaction, and the second is what makes "the setup
/// code is invalidated in the same transaction that creates the account" true.
///
/// **`sqlx::Error` rather than [`AuthStoreError`]**, uniquely in this module, because it has
/// two callers in two error domains: `crate::config_db`'s `Config::load` reads it as part of
/// the configuration -- W5 §3.4 makes it an input to the management bind address -- and it
/// cannot name this module's error. One statement, two wrappings, rather than two copies of
/// the statement.
pub async fn is_claimed<'e, E>(conn: E) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query("SELECT EXISTS(SELECT 1 FROM admin_credential WHERE id = 1) AS claimed")
        .fetch_one(conn)
        .await?;
    Ok(row.get::<i64, _>("claimed") != 0)
}

/// The exact-match CORS allowlist (**A-W5-3**, §3.5).
///
/// Here rather than in [`crate::config_db`] because it belongs to the same decision the rest
/// of this module implements: it is the other half of replacing
/// `Access-Control-Allow-Origin: *`, and it is read by the same consumer at the same moment.
/// **Empty is the normal answer** — the table has no seeded row, so the default is
/// same-origin only.
pub async fn cors_origins<'e, E>(conn: E) -> Result<Vec<String>, AuthStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query("SELECT origin FROM management_cors_origin ORDER BY origin")
        .fetch_all(conn)
        .await
        .map_err(failed("read"))?;
    Ok(rows.iter().map(|r| r.get::<String, _>("origin")).collect())
}

/// The stored credential, or `None` on an unclaimed instance.
pub async fn credential<'e, E>(conn: E) -> Result<Option<Credential>, AuthStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT username, password_phc, failed_attempts, last_failed_at, \
         session_absolute_seconds, session_idle_seconds FROM admin_credential WHERE id = 1",
    )
    .fetch_optional(conn)
    .await
    .map_err(failed("read"))?;
    Ok(row.map(|r| Credential {
        username: r.get("username"),
        password_phc: r.get("password_phc"),
        // The column's `CHECK (failed_attempts >= 0)` is what makes this conversion safe; a
        // saturating cast rather than an error because a counter this build cannot represent
        // should throttle rather than refuse to answer.
        failed_attempts: u32::try_from(r.get::<i64, _>("failed_attempts")).unwrap_or(u32::MAX),
        last_failed_at: r.get("last_failed_at"),
        session_absolute_seconds: r.get("session_absolute_seconds"),
        session_idle_seconds: r.get("session_idle_seconds"),
    }))
}

/// Record a failed login: bump the counter and stamp the time (§3.5).
///
/// Per username rather than per IP, which is the whole table: there is one username, and the
/// counter is persisted so a restart is not a way to clear the delay.
pub async fn record_failure(pool: &SqlitePool) -> Result<(), AuthStoreError> {
    sqlx::query(
        "UPDATE admin_credential \
         SET failed_attempts = failed_attempts + 1, last_failed_at = unixepoch(), \
             updated_at = unixepoch() \
         WHERE id = 1",
    )
    .execute(pool)
    .await
    .map_err(failed("updated"))?;
    Ok(())
}

/// Record a successful login: clear the counter, re-hash if asked, insert the session.
///
/// One transaction. `rehash` is §3.2's transparent upgrade, carried as an already-computed
/// PHC string because the hashing is policy and policy is `mw_api_mgmt::auth`'s.
pub async fn open_session(
    pool: &SqlitePool,
    session: &NewSession,
    rehash: Option<&str>,
) -> Result<(), AuthStoreError> {
    let mut tx = pool.begin().await.map_err(failed("opened"))?;
    match rehash {
        Some(phc) => {
            sqlx::query(
                "UPDATE admin_credential \
                 SET failed_attempts = 0, last_failed_at = NULL, password_phc = ?1, \
                     updated_at = unixepoch() \
                 WHERE id = 1",
            )
            .bind(phc)
            .execute(&mut *tx)
            .await
            .map_err(failed("updated"))?;
        }
        None => {
            sqlx::query(
                "UPDATE admin_credential \
                 SET failed_attempts = 0, last_failed_at = NULL, updated_at = unixepoch() \
                 WHERE id = 1",
            )
            .execute(&mut *tx)
            .await
            .map_err(failed("updated"))?;
        }
    }
    insert_session(&mut tx, session).await?;
    tx.commit().await.map_err(failed("written"))?;
    Ok(())
}

/// Resolve a presented session, enforcing both expiries and touching `last_seen_at`.
///
/// **The hash comparison is not here.** This returns the stored hash beside the row and the
/// caller compares it in constant time with `subtle` (§3.5) — because a SQL `WHERE
/// token_hash = ?` is a comparison this crate cannot make constant-time, and moving the
/// policy down here would also put it on the wrong side of `ci/layers.toml`.
///
/// Idle expiry is applied in SQL rather than by the caller, because it is a property of the
/// row and the lifetime it is measured against is on another row: doing it here is one
/// statement, and doing it above would be two reads and a subtraction the caller could get
/// wrong in a different place from the absolute expiry.
pub async fn resolve(
    pool: &SqlitePool,
    id: &[u8; 16],
) -> Result<Option<(SessionRow, [u8; 32])>, AuthStoreError> {
    let row = sqlx::query(
        "SELECT s.id, s.token_hash, s.created_at, s.last_seen_at, s.expires_at, \
                s.user_agent, s.created_ip \
         FROM admin_session s, admin_credential c \
         WHERE s.id = ?1 AND c.id = 1 \
           AND s.expires_at > unixepoch() \
           AND s.last_seen_at + c.session_idle_seconds > unixepoch()",
    )
    .bind(&id[..])
    .fetch_optional(pool)
    .await
    .map_err(failed("read"))?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some((
        session_row(&row),
        fixed32(&row.get::<Vec<u8>, _>("token_hash")),
    )))
}

/// Refresh a session's `last_seen_at`, if it is stale enough to be worth a write.
///
/// **Called only after the caller has verified the presented secret**, and that ordering is
/// the whole reason this is a second function rather than the tail of [`resolve`]. A session
/// id is deliberately public — it is in the audit log, in `GET /api/v1/auth/sessions` and in
/// a `DELETE` path — so a touch applied on the strength of the id alone would let anyone who
/// has seen one refresh the idle timer with a garbage secret and keep a session alive past
/// W5 §3.3's seven-day idle expiry. Presenting the id is not presenting the session.
///
/// Debounced, per this module's documentation: the staleness test is in the `WHERE` clause,
/// so this is one statement that usually matches no rows rather than a read and a write. Both
/// sides of the comparison are the database's `unixepoch()`, which is also what the expiry
/// clauses in [`resolve`] and [`sessions`] compare against.
///
/// A failure is deliberately **not** fatal to the caller's request: the session is valid and
/// the operator's page should load. What is lost is a minute of idle-timer precision.
pub async fn touch(pool: &SqlitePool, id: &[u8; 16]) {
    let _ = sqlx::query(
        "UPDATE admin_session SET last_seen_at = unixepoch() \
         WHERE id = ?1 AND last_seen_at + ?2 <= unixepoch()",
    )
    .bind(&id[..])
    .bind(TOUCH_AFTER_SECONDS)
    .execute(pool)
    .await;
}

/// Every live session, newest first.
///
/// The same two expiry clauses as [`resolve`], because a list that showed a session the next
/// request would refuse would be a list of things to worry about that are not there.
pub async fn sessions(pool: &SqlitePool) -> Result<Vec<SessionRow>, AuthStoreError> {
    let rows = sqlx::query(
        "SELECT s.id, s.created_at, s.last_seen_at, s.expires_at, s.user_agent, s.created_ip \
         FROM admin_session s, admin_credential c \
         WHERE c.id = 1 \
           AND s.expires_at > unixepoch() \
           AND s.last_seen_at + c.session_idle_seconds > unixepoch() \
         ORDER BY s.created_at DESC, s.id",
    )
    .fetch_all(pool)
    .await
    .map_err(failed("read"))?;
    Ok(rows.iter().map(session_row).collect())
}

/// Revoke one session. `false` means there was no such row.
pub async fn revoke(pool: &SqlitePool, id: &[u8; 16]) -> Result<bool, AuthStoreError> {
    let done = sqlx::query("DELETE FROM admin_session WHERE id = ?1")
        .bind(&id[..])
        .execute(pool)
        .await
        .map_err(failed("updated"))?;
    Ok(done.rows_affected() > 0)
}

/// Revoke every session — §3.3's "sign out everywhere", including the caller's own.
pub async fn revoke_all(pool: &SqlitePool) -> Result<u64, AuthStoreError> {
    let done = sqlx::query("DELETE FROM admin_session")
        .execute(pool)
        .await
        .map_err(failed("updated"))?;
    Ok(done.rows_affected())
}

/// Remove every expired session.
///
/// Called at startup rather than on a schedule: `admin_session_expires_at` makes it cheap,
/// the table has one row per browser the operator has ever signed in from, and a component
/// whose whole job is to delete a handful of rows once a fortnight would be a `Calendar`
/// entry on the Activity page for no operator-visible benefit.
pub async fn sweep_expired(pool: &SqlitePool) -> Result<u64, AuthStoreError> {
    let done = sqlx::query("DELETE FROM admin_session WHERE expires_at <= unixepoch()")
        .execute(pool)
        .await
        .map_err(failed("updated"))?;
    Ok(done.rows_affected())
}

/// Create the administrator account and invalidate the setup code **in one transaction**.
///
/// §3.4 point 4, and implementation-plan.md **M7-2**'s done-criterion: "the setup code is
/// invalidated in the same transaction that creates the account", so a race between two
/// claimants resolves to exactly one.
///
/// Two things make that true rather than likely. The transaction is [`IMMEDIATE`], so two
/// concurrent claimants serialise at the first statement rather than failing the loser at its
/// `INSERT`. And the account row's `CHECK (id = 1)` means a second `INSERT` is refused by the
/// schema regardless, so "exactly one administrator" does not rest on this function being
/// careful.
///
/// The code comparison is **constant-time** (§3.5, **D-33**) and happens inside the
/// transaction. A SQL `WHERE token_hash = ?` would have been neither: it is not
/// constant-time, and it would have folded "there is no outstanding code" and "the code is
/// wrong" into one empty result, which are different sentences to the operator.
pub async fn claim(pool: &SqlitePool, account: &NewAccount) -> Result<Claimed, AuthStoreError> {
    let mut tx = pool.begin_with(IMMEDIATE).await.map_err(failed("opened"))?;

    if is_claimed(&mut *tx).await.map_err(failed("read"))? {
        return Ok(Claimed::AlreadyClaimed);
    }
    let stored = sqlx::query("SELECT token_hash FROM admin_setup_token WHERE id = 1")
        .fetch_optional(&mut *tx)
        .await
        .map_err(failed("read"))?;
    let Some(stored) = stored else {
        return Ok(Claimed::NoCodeOutstanding);
    };
    // The comparison is the caller's, in constant time, for `resolve`'s reason. What is
    // transactional is that the row was read, matched and deleted without an interleaving
    // writer -- not where the `==` happens.
    let stored_hash = fixed32(&stored.get::<Vec<u8>, _>("token_hash"));
    if !bool::from(stored_hash.ct_eq(&account.typed_code)) {
        return Ok(Claimed::WrongCode);
    }

    sqlx::query("INSERT INTO admin_credential (id, username, password_phc) VALUES (1, ?1, ?2)")
        .bind(&account.username)
        .bind(&account.password_phc)
        .execute(&mut *tx)
        .await
        .map_err(failed("written"))?;
    sqlx::query("DELETE FROM admin_setup_token WHERE id = 1")
        .execute(&mut *tx)
        .await
        .map_err(failed("written"))?;
    insert_session(&mut tx, &account.session).await?;
    tx.commit().await.map_err(failed("written"))?;
    Ok(Claimed::Created)
}

/// Replace the password and revoke every session **except** `keep` (§3.3).
///
/// One transaction, because the two halves are one decision. `keep` is the session making the
/// request; [`reset_password`] is the same write with nothing kept.
pub async fn change_password(
    pool: &SqlitePool,
    password_phc: &str,
    keep: Option<&[u8; 16]>,
) -> Result<u64, AuthStoreError> {
    let mut tx = pool.begin_with(IMMEDIATE).await.map_err(failed("opened"))?;
    sqlx::query(
        "UPDATE admin_credential \
         SET password_phc = ?1, failed_attempts = 0, last_failed_at = NULL, \
             updated_at = unixepoch() \
         WHERE id = 1",
    )
    .bind(password_phc)
    .execute(&mut *tx)
    .await
    .map_err(failed("updated"))?;
    let revoked = match keep {
        Some(id) => {
            sqlx::query("DELETE FROM admin_session WHERE id <> ?1")
                .bind(&id[..])
                .execute(&mut *tx)
                .await
        }
        None => {
            sqlx::query("DELETE FROM admin_session")
                .execute(&mut *tx)
                .await
        }
    }
    .map_err(failed("updated"))?;
    tx.commit().await.map_err(failed("written"))?;
    Ok(revoked.rows_affected())
}

/// `modernweather reset-password` (§3.6): replace the password, rename if asked, and revoke
/// **every** session.
///
/// Every session, with nothing kept, because the command is run on the host by someone who
/// is not holding a session — and because the case it exists for is being locked out, which
/// is usually accompanied by not knowing what else is signed in.
///
/// `username` is `None` to leave the name alone. Renaming is part of this command rather
/// than an endpoint because a username change is indistinguishable from a lockout from the
/// outside: if the operator has forgotten which it was, the API cannot tell them.
pub async fn reset_password(
    pool: &SqlitePool,
    password_phc: &str,
    username: Option<&str>,
) -> Result<u64, AuthStoreError> {
    if let Some(username) = username {
        sqlx::query(
            "UPDATE admin_credential SET username = ?1, updated_at = unixepoch() WHERE id = 1",
        )
        .bind(username)
        .execute(pool)
        .await
        .map_err(failed("updated"))?;
    }
    change_password(pool, password_phc, None).await
}

/// Install a setup code, replacing any outstanding one.
///
/// `INSERT OR REPLACE` rather than a delete and an insert: the table has one row by
/// `CHECK (id = 1)` and re-issuing is one statement. This is how both first start and
/// `modernweather reset-setup-token` write it, so there is one definition of what an
/// outstanding code is.
///
/// **Refuses on a claimed instance**, and returns `false` to say so. Issuing a setup code for
/// an instance that already has an administrator would be a credential-reset path that
/// bypasses §3.6's trust boundary — except that it would not even be that, because
/// [`claim`] refuses a claimed instance; it would just be a confusing row. The caller that
/// wants to change a forgotten password has `reset-password`.
pub async fn issue_setup_token(
    pool: &SqlitePool,
    token_hash: &[u8; 32],
) -> Result<bool, AuthStoreError> {
    let mut tx = pool.begin_with(IMMEDIATE).await.map_err(failed("opened"))?;
    if is_claimed(&mut *tx).await.map_err(failed("read"))? {
        return Ok(false);
    }
    sqlx::query("INSERT OR REPLACE INTO admin_setup_token (id, token_hash) VALUES (1, ?1)")
        .bind(&token_hash[..])
        .execute(&mut *tx)
        .await
        .map_err(failed("written"))?;
    tx.commit().await.map_err(failed("written"))?;
    Ok(true)
}

/// Whether a setup code is outstanding.
pub async fn has_setup_token(pool: &SqlitePool) -> Result<bool, AuthStoreError> {
    let row = sqlx::query("SELECT EXISTS(SELECT 1 FROM admin_setup_token WHERE id = 1) AS yes")
        .fetch_one(pool)
        .await
        .map_err(failed("read"))?;
    Ok(row.get::<i64, _>("yes") != 0)
}

/// The one `INSERT` both login and the claim make.
async fn insert_session(
    conn: &mut SqliteConnection,
    session: &NewSession,
) -> Result<(), AuthStoreError> {
    sqlx::query(
        "INSERT INTO admin_session \
             (id, token_hash, created_at, last_seen_at, expires_at, user_agent, created_ip) \
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
    )
    .bind(&session.id[..])
    .bind(&session.token_hash[..])
    .bind(session.created_at)
    .bind(session.expires_at)
    .bind(session.user_agent.as_deref())
    .bind(session.created_ip.as_deref())
    .execute(&mut *conn)
    .await
    .map_err(failed("written"))?;
    Ok(())
}

/// One session row, from either of the two queries that select it.
fn session_row(row: &sqlx::sqlite::SqliteRow) -> SessionRow {
    SessionRow {
        id: fixed16(&row.get::<Vec<u8>, _>("id")),
        created_at: row.get("created_at"),
        last_seen_at: row.get("last_seen_at"),
        expires_at: row.get("expires_at"),
        user_agent: row.get("user_agent"),
        created_ip: row.get("created_ip"),
    }
}

/// A 16-byte array from a column whose `CHECK` already requires that length.
fn fixed16(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = bytes.len().min(16);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// A 32-byte array from a column whose `CHECK` already requires that length.
///
/// Truncating rather than erroring, for one reason worth stating: the only caller compares
/// the result in constant time against a real digest, so a short or long column value
/// produces a mismatch — which is the correct outcome for a row this build did not write.
fn fixed32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = bytes.len().min(32);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A scratch directory per test, removed on drop — `crate::config_db::tests`' own, for
    /// the same reason: `tempfile` would be a dev-dependency for twenty lines.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-admin-auth-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        fn db(&self) -> PathBuf {
            self.0.join("config.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn pool(scratch: &Scratch) -> SqlitePool {
        crate::config_db::open_and_migrate(&scratch.db())
            .await
            .expect("the configuration schema applies from empty")
    }

    /// A PHC string the `password_phc` column's `CHECK` accepts. Not produced by Argon2id
    /// here: this module never hashes, and what these tests are about is the statements.
    const PHC: &str = "$argon2id$v=19$m=65536,t=3,p=4$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA";

    /// A session created now and expiring in a day.
    ///
    /// **Now**, not a literal instant: `resolve` and `sessions` enforce the idle expiry as
    /// `last_seen_at + session_idle_seconds > unixepoch()`, so a fixture stamped in 2023
    /// would be idle-expired and every assertion below would be about an empty table.
    fn new_session(id: u8) -> NewSession {
        let now = chrono::Utc::now().timestamp();
        NewSession {
            id: [id; 16],
            token_hash: [id; 32],
            created_at: now,
            expires_at: now + 86_400,
            user_agent: Some("a browser".into()),
            created_ip: Some("127.0.0.1".into()),
        }
    }

    fn account(code: [u8; 32], username: &str, session: u8) -> NewAccount {
        NewAccount {
            typed_code: code,
            username: username.to_string(),
            password_phc: PHC.to_string(),
            session: new_session(session),
        }
    }

    /// **implementation-plan.md M7-2's done-criterion**: the setup code is invalidated in the
    /// same transaction that creates the account.
    ///
    /// Asserted as a *race* rather than as a sequence, because the sequence is trivially
    /// true of any implementation and the race is what the requirement is about: §3.4 says
    /// "so a race between two claimants resolves to exactly one". Two claims with the same
    /// valid code are issued concurrently against one database; exactly one is
    /// `Created`, the loser is `AlreadyClaimed` rather than an error, the account exists
    /// once, and the token is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_claimants_with_the_same_code_resolve_to_exactly_one() {
        let scratch = Scratch::new("race");
        let pool = pool(&scratch).await;
        let code = [7u8; 32];
        assert!(issue_setup_token(&pool, &code)
            .await
            .expect("the token is issuable"));

        let (first, second) = (account(code, "first", 1), account(code, "second", 2));
        let (a, b) = tokio::join!(claim(&pool, &first), claim(&pool, &second));
        let a = a.expect("the first claim ran");
        let b = b.expect("the second claim ran");

        let mut outcomes = [a, b];
        outcomes.sort_by_key(|o| format!("{o:?}"));
        assert_eq!(
            outcomes,
            [Claimed::AlreadyClaimed, Claimed::Created],
            "exactly one claimant won"
        );

        // One account, and the one the winner named.
        let credential = credential(&pool)
            .await
            .expect("the credential is readable")
            .expect("an account exists");
        assert!(
            ["first", "second"].contains(&credential.username.as_str()),
            "{}",
            credential.username
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_credential")
                .fetch_one(&pool)
                .await
                .expect("countable"),
            1
        );
        // The token is gone with the account, so a third claimant has nothing to present.
        assert!(!has_setup_token(&pool).await.expect("readable"));
        // And only the winner's session was written: the loser's `INSERT` rolled back with
        // its transaction, which is the other half of "in the same transaction".
        assert_eq!(sessions(&pool).await.expect("readable").len(), 1);

        // A third attempt, now that there is an account, is refused for the right reason.
        assert_eq!(
            claim(&pool, &account(code, "third", 3)).await.expect("ran"),
            Claimed::AlreadyClaimed
        );
    }

    /// The two refusals that are not "already claimed", and the fact that **neither spends
    /// the code**: a mistyped code must leave the operator able to try again.
    #[tokio::test]
    async fn a_wrong_code_and_a_missing_one_are_different_answers_and_neither_spends_it() {
        let scratch = Scratch::new("refusals");
        let pool = pool(&scratch).await;

        assert_eq!(
            claim(&pool, &account([1u8; 32], "nobody", 1))
                .await
                .expect("ran"),
            Claimed::NoCodeOutstanding
        );

        let code = [9u8; 32];
        assert!(issue_setup_token(&pool, &code).await.expect("issuable"));
        assert_eq!(
            claim(&pool, &account([8u8; 32], "nobody", 1))
                .await
                .expect("ran"),
            Claimed::WrongCode
        );
        assert!(
            has_setup_token(&pool).await.expect("readable"),
            "a wrong code must not consume the right one"
        );
        assert!(
            credential(&pool).await.expect("readable").is_none(),
            "a refused claim created an account"
        );
        // And the real code still works.
        assert_eq!(
            claim(&pool, &account(code, "chris", 1)).await.expect("ran"),
            Claimed::Created
        );
    }

    /// Re-issuing replaces the outstanding code rather than adding a second one, and is
    /// refused once an account exists.
    #[tokio::test]
    async fn a_reissued_code_replaces_the_old_one_and_is_refused_after_a_claim() {
        let scratch = Scratch::new("reissue");
        let pool = pool(&scratch).await;
        assert!(issue_setup_token(&pool, &[1u8; 32])
            .await
            .expect("issuable"));
        assert!(issue_setup_token(&pool, &[2u8; 32])
            .await
            .expect("issuable"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_setup_token")
                .fetch_one(&pool)
                .await
                .expect("countable"),
            1,
            "the table holds one row by CHECK (id = 1)"
        );
        // The first code is dead, the second works.
        assert_eq!(
            claim(&pool, &account([1u8; 32], "chris", 1))
                .await
                .expect("ran"),
            Claimed::WrongCode
        );
        assert_eq!(
            claim(&pool, &account([2u8; 32], "chris", 1))
                .await
                .expect("ran"),
            Claimed::Created
        );
        assert!(
            !issue_setup_token(&pool, &[3u8; 32]).await.expect("ran"),
            "a claimed instance has no use for a setup code"
        );
    }

    /// **M7-2's other done-criterion**: changing the password revokes every other session and
    /// keeps the current one. And `reset_password` keeps none, because its caller is not
    /// holding a session.
    #[tokio::test]
    async fn changing_the_password_keeps_one_session_and_resetting_it_keeps_none() {
        let scratch = Scratch::new("revocation");
        let pool = pool(&scratch).await;
        assert!(issue_setup_token(&pool, &[1u8; 32])
            .await
            .expect("issuable"));
        assert_eq!(
            claim(&pool, &account([1u8; 32], "chris", 1))
                .await
                .expect("ran"),
            Claimed::Created
        );
        for id in [2u8, 3, 4] {
            open_session(&pool, &new_session(id), None)
                .await
                .expect("the session is insertable");
        }
        assert_eq!(sessions(&pool).await.expect("readable").len(), 4);

        let revoked = change_password(&pool, PHC, Some(&[3u8; 16]))
            .await
            .expect("the password is changeable");
        assert_eq!(revoked, 3);
        let left = sessions(&pool).await.expect("readable");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, [3u8; 16], "the current session survived");

        let revoked = reset_password(&pool, PHC, Some("someone-else"))
            .await
            .expect("the password is resettable");
        assert_eq!(revoked, 1, "reset keeps nothing");
        assert!(sessions(&pool).await.expect("readable").is_empty());
        assert_eq!(
            credential(&pool)
                .await
                .expect("readable")
                .expect("an account")
                .username,
            "someone-else",
            "--username renames while resetting"
        );
    }

    /// Resolution, both expiries, and the debounced touch.
    ///
    /// The absolute expiry and the idle expiry are separate clauses over separate columns,
    /// and the one that is easy to get wrong is the idle one: it is measured against
    /// `admin_credential.session_idle_seconds`, which is on **another row**.
    #[tokio::test]
    async fn a_session_resolves_until_either_expiry_passes() {
        let scratch = Scratch::new("expiry");
        let pool = pool(&scratch).await;
        assert!(issue_setup_token(&pool, &[1u8; 32])
            .await
            .expect("issuable"));
        assert_eq!(
            claim(&pool, &account([1u8; 32], "chris", 1))
                .await
                .expect("ran"),
            Claimed::Created
        );

        // The claim's own session, created in the past and far from its absolute expiry.
        let now = chrono::Utc::now().timestamp();
        sqlx::query("UPDATE admin_session SET created_at = ?1, last_seen_at = ?1, expires_at = ?2")
            .bind(now - 3_600)
            .bind(now + 86_400)
            .execute(&pool)
            .await
            .expect("updatable");
        let (row, stored) = resolve(&pool, &[1u8; 16])
            .await
            .expect("readable")
            .expect("the session resolves");
        assert_eq!(
            stored, [1u8; 32],
            "the stored hash comes back for the caller"
        );
        assert_eq!(row.id, [1u8; 16]);

        // **`resolve` alone does not write.** The id is public, so the refresh is the
        // caller's to make after it has verified the secret -- reading twice must leave the
        // stamp exactly where it was.
        let unchanged = resolve(&pool, &[1u8; 16])
            .await
            .expect("readable")
            .expect("still resolves")
            .0;
        assert_eq!(
            unchanged.last_seen_at, row.last_seen_at,
            "resolve refreshed the idle timer on the strength of the id alone"
        );

        // An hour stale is past the debounce, so the touch takes.
        touch(&pool, &[1u8; 16]).await;
        let touched = resolve(&pool, &[1u8; 16])
            .await
            .expect("readable")
            .expect("still resolves")
            .0;
        assert!(
            touched.last_seen_at > row.last_seen_at,
            "last_seen_at was not touched: {} -> {}",
            row.last_seen_at,
            touched.last_seen_at
        );
        // And now it is fresh, so a second touch is debounced away.
        touch(&pool, &[1u8; 16]).await;
        let again = resolve(&pool, &[1u8; 16])
            .await
            .expect("readable")
            .expect("still resolves")
            .0;
        assert_eq!(again.last_seen_at, touched.last_seen_at);
        // A touch on an id that is not a session writes nothing and does not fail.
        touch(&pool, &[99u8; 16]).await;

        // Past the ABSOLUTE expiry: gone, and gone from the list too.
        sqlx::query("UPDATE admin_session SET expires_at = ?1")
            .bind(now - 1)
            .execute(&pool)
            .await
            .expect("updatable");
        assert!(resolve(&pool, &[1u8; 16])
            .await
            .expect("readable")
            .is_none());
        assert!(sessions(&pool).await.expect("readable").is_empty());
        assert_eq!(sweep_expired(&pool).await.expect("sweeps"), 1);

        // Past the IDLE expiry, with the absolute one still far away.
        open_session(&pool, &new_session(5), None)
            .await
            .expect("insertable");
        sqlx::query("UPDATE admin_session SET last_seen_at = ?1, expires_at = ?2")
            .bind(now - 604_801)
            .bind(now + 86_400)
            .execute(&pool)
            .await
            .expect("updatable");
        assert!(
            resolve(&pool, &[5u8; 16])
                .await
                .expect("readable")
                .is_none(),
            "a session idle longer than session_idle_seconds still resolved"
        );
        // Not expired by the absolute clause, so the sweep leaves it: the idle rule is
        // enforced at read time and the sweep is housekeeping over the other column.
        assert_eq!(sweep_expired(&pool).await.expect("sweeps"), 0);

        // An unknown id is `None` rather than an error.
        assert!(resolve(&pool, &[99u8; 16])
            .await
            .expect("readable")
            .is_none());
    }

    /// The failure counter, and the two writes that clear it.
    #[tokio::test]
    async fn the_failure_counter_is_persisted_and_cleared_by_a_success() {
        let scratch = Scratch::new("failures");
        let pool = pool(&scratch).await;
        assert!(issue_setup_token(&pool, &[1u8; 32])
            .await
            .expect("issuable"));
        assert_eq!(
            claim(&pool, &account([1u8; 32], "chris", 1))
                .await
                .expect("ran"),
            Claimed::Created
        );
        for _ in 0..5 {
            record_failure(&pool).await.expect("recordable");
        }
        let c = credential(&pool).await.expect("readable").expect("account");
        assert_eq!(c.failed_attempts, 5);
        assert!(c.last_failed_at.is_some());

        // A successful login clears it, and carries §3.2's transparent re-hash in the same
        // transaction as the new session.
        let upgraded = "$argon2id$v=19$m=65536,t=3,p=4$bmV3c2FsdG5ldw$bmV3aGFzaG5ld2hhc2hu";
        open_session(&pool, &new_session(2), Some(upgraded))
            .await
            .expect("insertable");
        let c = credential(&pool).await.expect("readable").expect("account");
        assert_eq!(c.failed_attempts, 0);
        assert_eq!(c.last_failed_at, None);
        assert_eq!(c.password_phc, upgraded, "the re-hash was written");
    }

    /// The CORS allowlist, whose **default is no rows** — which is W5 §3.5's "default empty,
    /// meaning same-origin only" expressed as the absence of data rather than a sentinel.
    #[tokio::test]
    async fn the_cors_allowlist_is_empty_by_default_and_ordered_when_it_is_not() {
        let scratch = Scratch::new("cors");
        let pool = pool(&scratch).await;
        assert!(cors_origins(&pool).await.expect("readable").is_empty());
        for origin in ["https://z.test", "https://a.test"] {
            sqlx::query("INSERT INTO management_cors_origin (origin) VALUES (?1)")
                .bind(origin)
                .execute(&pool)
                .await
                .expect("insertable");
        }
        assert_eq!(
            cors_origins(&pool).await.expect("readable"),
            vec!["https://a.test".to_string(), "https://z.test".to_string()]
        );
        // The column's `CHECK`s are the validator (**D-8**), not this module: a wildcard, a
        // path and a scheme-less entry are all refused by the schema.
        for bad in [
            "*",
            "https://a.test/x",
            "a.test",
            "https://a.test, https://b.test",
        ] {
            assert!(
                sqlx::query("INSERT INTO management_cors_origin (origin) VALUES (?1)")
                    .bind(bad)
                    .execute(&pool)
                    .await
                    .is_err(),
                "`{bad}` was accepted"
            );
        }
    }
}
