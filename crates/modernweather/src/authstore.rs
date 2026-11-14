//! `AuthStore` over the configuration database, and the first-run banner.
//!
//! Implemented here for `crate::admin`'s reason exactly: `mw_api_mgmt::auth` owns the policy
//! and may not name `mw-store`, `mw_store::admin_auth` owns the statements and may not name
//! `mw-api-mgmt`, and the binary is the one place both may be named
//! (docs/service-architecture.md §1.3 rule 1). Everything below is translation plus one
//! decision this layer really does own: **what a successful claim does to the socket**.
//!
//! # The claim is the one authentication write that changes the plan
//!
//! W5 §3.4 binds the management API to loopback while the instance is unclaimed and widens it
//! to the configured address once an administrator exists. `crate::config::Config::claimed`
//! is what `management_addr` consults, so the widening is a **re-plan**: the claim commits,
//! [`SqliteAuthStore`] asks the `ConfigAdmin` to plan the configuration again, and
//! `http:listener`'s digest -- which covers the management socket -- restarts it on the
//! configured address.
//!
//! That re-plan is deliberately the *last* thing the claim does and its failure is
//! deliberately not the claim's failure: the account exists either way, and an instance
//! that is claimed but still on loopback is reachable over the tunnel the operator is
//! already using. The alternative -- rolling the account back because a socket could not
//! move -- would make first-run fail for a reason that has nothing to do with first-run.

use std::sync::Arc;

use mw_api_mgmt::auth::{
    AuthStore, ClaimRefusal, NewAccount, NewSession, Presented, Session, StoredCredential,
};
use mw_api_mgmt::error::ApiError;
use mw_component::BoxFuture;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// `AuthStore` over the configuration database.
pub struct SqliteAuthStore {
    /// The configuration database. **D-41** makes the pool `main`'s.
    pool: SqlitePool,
    /// How a claim reaches the plan, so the management socket can widen (W5 §3.4).
    ///
    /// A `OnceLock` rather than a constructor argument because the two are circular: the
    /// router holds this store, the `ConfigAdmin` holds the router, and this holds the
    /// `ConfigAdmin`. Written once by `main` before anything can call in -- the same shape
    /// `crate::admin::Shared::router` already has, for the same reason.
    ///
    /// A `Weak`, not an `Arc`: the `ConfigAdmin` reaches this store through the router it
    /// owns, so a strong handle here would be a cycle that never drops.
    replan: std::sync::OnceLock<std::sync::Weak<crate::admin::SqliteConfigAdmin>>,
}

impl std::fmt::Debug for SqliteAuthStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteAuthStore").finish_non_exhaustive()
    }
}

impl SqliteAuthStore {
    /// Over the configuration pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            replan: std::sync::OnceLock::new(),
        }
    }

    /// Hand it the `ConfigAdmin` a claim re-plans through. Called once, by `main`.
    pub fn install_admin(&self, admin: &Arc<crate::admin::SqliteConfigAdmin>) {
        let _ = self.replan.set(Arc::downgrade(admin));
    }

    /// Widen the management socket after a claim, if there is still an admin to ask.
    ///
    /// Every outcome is logged and none is returned: see this module's documentation for why
    /// a socket that could not move is not a claim that did not happen.
    async fn widen(&self) {
        let Some(admin) = self.replan.get().and_then(std::sync::Weak::upgrade) else {
            // Either `main` has not wired it yet -- impossible, the router is built after --
            // or the daemon is shutting down, in which case the next start reads the claimed
            // instance and binds the configured address with no help from here.
            info!(
                "the administrator account exists; the management API binds the configured \
                 address at the next start"
            );
            return;
        };
        match admin.replan("the instance has been claimed").await {
            Ok(()) => {}
            Err(e) => warn!(
                err = %e,
                "the account was created but the management API could not be moved off \
                 loopback; it will bind the configured address at the next start"
            ),
        }
    }

    /// `mw_store`'s error, as this surface's.
    ///
    /// Every variant is a `500`: the statements below have no operator-facing failure mode.
    /// A wrong password, an expired session and an already-claimed instance are outcomes and
    /// are in the return types.
    fn failed(e: mw_store::AuthStoreError) -> ApiError {
        ApiError::Internal {
            summary: "This daemon's administrator account could not be read.".to_string(),
            detail: Some(mw_types::DisplayChain(&e).to_string()),
        }
    }
}

/// `mw_store`'s session row, as the API's.
fn session(row: &mw_store::SessionRow) -> Option<Session> {
    let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0);
    Some(Session {
        id: row.id,
        created_at: at(row.created_at)?,
        last_seen_at: at(row.last_seen_at)?,
        expires_at: at(row.expires_at)?,
        user_agent: row.user_agent.clone(),
        created_ip: row.created_ip.clone(),
    })
}

/// The API's new-session input, as the store's.
fn new_session(s: &NewSession) -> mw_store::admin_auth::NewSession {
    mw_store::admin_auth::NewSession {
        id: s.minted.id,
        token_hash: s.minted.token_hash(),
        created_at: s.created_at,
        expires_at: s.expires_at,
        user_agent: s.user_agent.clone(),
        created_ip: s.created_ip.clone(),
    }
}

impl AuthStore for SqliteAuthStore {
    fn claimed(&self) -> BoxFuture<'_, Result<bool, ApiError>> {
        Box::pin(async move {
            mw_store::admin_auth::is_claimed(&self.pool)
                .await
                .map_err(|e| ApiError::Internal {
                    summary: "This daemon's administrator account could not be read.".to_string(),
                    detail: Some(mw_types::DisplayChain(&e).to_string()),
                })
        })
    }

    fn credential(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, ApiError>> {
        Box::pin(async move {
            let row = mw_store::admin_auth::credential(&self.pool)
                .await
                .map_err(Self::failed)?;
            Ok(row.map(|c| StoredCredential {
                username: c.username,
                password_phc: c.password_phc,
                failed_attempts: c.failed_attempts,
                last_failed_at: c.last_failed_at,
                session_absolute_seconds: c.session_absolute_seconds,
                session_idle_seconds: c.session_idle_seconds,
            }))
        })
    }

    fn record_failure(&self) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move {
            mw_store::admin_auth::record_failure(&self.pool)
                .await
                .map_err(Self::failed)
        })
    }

    fn open_session(
        &self,
        session: NewSession,
        rehash: Option<String>,
    ) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move {
            mw_store::admin_auth::open_session(
                &self.pool,
                &new_session(&session),
                rehash.as_deref(),
            )
            .await
            .map_err(Self::failed)
        })
    }

    fn resolve(&self, presented: Presented) -> BoxFuture<'_, Result<Option<Session>, ApiError>> {
        Box::pin(async move {
            let Some((row, stored)) = mw_store::admin_auth::resolve(&self.pool, &presented.id)
                .await
                .map_err(Self::failed)?
            else {
                return Ok(None);
            };
            // **Constant-time, here rather than in SQL** (§3.5): the store returns the hash
            // beside the row precisely so this comparison can be one. An unknown id and a
            // wrong secret are the same answer, which is what the `401` says.
            if !mw_api_mgmt::auth::ct_eq(&stored, &presented.token_hash) {
                return Ok(None);
            }
            // **After the comparison, never before.** The id half of the cookie is public --
            // it is in the audit log and in the session list -- so a `last_seen_at` refreshed
            // on the strength of the id alone would let anyone who has seen one keep a
            // session alive past §3.3's idle expiry with a secret they do not have.
            mw_store::admin_auth::touch(&self.pool, &presented.id).await;
            Ok(session(&row))
        })
    }

    fn sessions(&self) -> BoxFuture<'_, Result<Vec<Session>, ApiError>> {
        Box::pin(async move {
            let rows = mw_store::admin_auth::sessions(&self.pool)
                .await
                .map_err(Self::failed)?;
            Ok(rows.iter().filter_map(session).collect())
        })
    }

    fn revoke(&self, id: [u8; 16]) -> BoxFuture<'_, Result<bool, ApiError>> {
        Box::pin(async move {
            mw_store::admin_auth::revoke(&self.pool, &id)
                .await
                .map_err(Self::failed)
        })
    }

    fn revoke_all(&self) -> BoxFuture<'_, Result<u64, ApiError>> {
        Box::pin(async move {
            mw_store::admin_auth::revoke_all(&self.pool)
                .await
                .map_err(Self::failed)
        })
    }

    fn claim(
        &self,
        account: NewAccount,
    ) -> BoxFuture<'_, Result<Result<(), ClaimRefusal>, ApiError>> {
        Box::pin(async move {
            let outcome = mw_store::admin_auth::claim(
                &self.pool,
                &mw_store::NewAccount {
                    typed_code: account.typed_code,
                    username: account.username,
                    password_phc: account.password_phc,
                    session: new_session(&account.session),
                },
            )
            .await
            .map_err(Self::failed)?;
            match outcome {
                mw_store::Claimed::Created => {
                    self.widen().await;
                    Ok(Ok(()))
                }
                mw_store::Claimed::AlreadyClaimed => Ok(Err(ClaimRefusal::AlreadyClaimed)),
                mw_store::Claimed::WrongCode => Ok(Err(ClaimRefusal::WrongCode)),
                mw_store::Claimed::NoCodeOutstanding => Ok(Err(ClaimRefusal::NoCodeOutstanding)),
            }
        })
    }

    fn change_password(
        &self,
        password_phc: String,
        keep: [u8; 16],
    ) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move {
            let revoked =
                mw_store::admin_auth::change_password(&self.pool, &password_phc, Some(&keep))
                    .await
                    .map_err(Self::failed)?;
            info!(revoked, "the password was changed");
            Ok(())
        })
    }
}

/// Make sure an unclaimed instance has a setup code outstanding, and print it once.
///
/// Called by the boot sequence after the migrations and before the listener binds. Returns
/// whether a code was issued, which is what decides whether the banner is printed.
///
/// **Only when there is none.** A restart does not re-issue: the code's whole purpose is to
/// be read off a console once and typed into a browser, and an instance that reissued on
/// every start would invalidate the code the operator is halfway through typing. An
/// instance whose console output has scrolled away has `modernweather reset-setup-token`
/// (§3.6), which is the same write asked for deliberately.
pub async fn ensure_setup_code(
    pool: &SqlitePool,
    cfg: &crate::config::Config,
) -> anyhow::Result<()> {
    if cfg.claimed {
        return Ok(());
    }
    if mw_store::admin_auth::has_setup_token(pool).await? {
        // There is one and this process does not know it -- only its hash is stored, which
        // is the point. Say so, and name the command that replaces it.
        warn!(
            "this daemon has no administrator account yet and a setup code was issued on an \
             earlier start; run `modernweather reset-setup-token --config <path>` to print a \
             new one"
        );
        return Ok(());
    }
    let code = mw_api_mgmt::SetupCode::issue();
    if !mw_store::admin_auth::issue_setup_token(pool, &code.token_hash()).await? {
        // Claimed between the two reads above. Nothing to print.
        return Ok(());
    }
    // §3.4 point 5: the issue is audited, **not its value**.
    info!(
        target: mw_api_mgmt::auth::AUDIT,
        outcome = "ok",
        "a one-time setup code was issued"
    );
    print_banner(&code, cfg);
    Ok(())
}

/// Issue a fresh setup code and print it — `modernweather reset-setup-token` (§3.6).
///
/// Returns `false` when the instance is already claimed, which is not a failure: the
/// operator wanted `reset-password`, and the caller says so.
///
/// The configuration is read here rather than taken as an argument, because this runs
/// *instead of* the daemon: there is no boot sequence to have loaded it, and the banner's
/// port and scheme have to be the ones the daemon will actually use.
pub async fn reissue_setup_code(pool: &SqlitePool) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let cfg = crate::config::Config::load(&mut conn).await?;
    drop(conn);
    let code = mw_api_mgmt::SetupCode::issue();
    if !mw_store::admin_auth::issue_setup_token(pool, &code.token_hash()).await? {
        return Ok(false);
    }
    print_banner(&code, &cfg);
    Ok(true)
}

/// §3.4's console banner.
///
/// **To stderr only, and never to the log ring.** W5 §3.5 makes the setup code the single
/// deliberate exception to "nothing secret in logs", and the exception is this banner --
/// which is why it goes through `mw_obs::stderr::write_line` rather than through a `tracing`
/// macro. The ring is what `GET /api/v1/logs` displays, and a code that reached it would be
/// readable by anyone who could reach the surface it exists to protect. That is **W5 D-27**
/// and it is enforced by this function being the only place the code is rendered:
/// `SetupCode`'s own `Debug` prints `<not shown>`.
///
/// `ci/layers.toml`'s `no-stdout-logging` rule is what keeps that true from the other
/// direction: writing to the console directly from this file would be a gate failure, and
/// the one file in the workspace that may name the process's standard error is
/// `crates/mw-obs/src/stderr.rs`. (That rule's scan is a substring match over whole files
/// and does not know what a comment is, which is why neither of the two macro names it
/// forbids appears anywhere in this one.)
fn print_banner(code: &mw_api_mgmt::SetupCode, cfg: &crate::config::Config) {
    let host = crate::hostinfo::hostname();
    // **Read from the configuration, not hardcoded.** `8081` is the column's default and not
    // its value: an operator who moved the management port, or configured a certificate for
    // it, would otherwise be given a URL and an `ssh -L` line that cannot work -- on the one
    // screen where they have nothing else to go on.
    let port = cfg.listener.management_port;
    let scheme = if cfg.management_cert().is_some() {
        "https"
    } else {
        "http"
    };
    // Where it will actually bind, which is the same function the listener is built from --
    // so the banner cannot disagree with the socket. An address this build cannot parse is
    // reported by the boot sequence a moment later with the field name on it; here it only
    // decides which of the two paragraphs below to print, and loopback is the safe guess.
    let reachable = cfg
        .management_addr()
        .map(|addr| !addr.ip().is_loopback())
        .unwrap_or(false);
    let rule = "═".repeat(74);
    let mut lines = vec![
        rule.clone(),
        " ModernWeather is not set up yet.".to_string(),
        String::new(),
    ];
    if reachable {
        lines.push(format!(
            " Open  {scheme}://{host}:{port}/  and enter this one-time setup code:"
        ));
    } else {
        lines.push(format!(
            " Open  {scheme}://localhost:{port}/  on this machine and enter this one-time"
        ));
        lines.push(" setup code:".to_string());
    }
    lines.extend([
        String::new(),
        format!("     {}", code.display()),
        String::new(),
        " This code is shown once and is valid until an account is created.".to_string(),
        " Anyone with this code can create the administrator account.".to_string(),
        String::new(),
    ]);
    if reachable {
        lines.extend([
            " The management API is reachable from your network while this daemon has no"
                .to_string(),
            " administrator account, because --setup-listen was asked for. Create the account"
                .to_string(),
            " now: until you do, anyone with this code can.".to_string(),
        ]);
    } else {
        lines.extend([
            format!(
                " The management API is on {} until that account exists, so from another",
                cfg.management_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_else(|_| "127.0.0.1".to_string())
            ),
            " machine, tunnel to it first:".to_string(),
            String::new(),
            format!("     ssh -L {port}:localhost:{port} {host}"),
            String::new(),
            " For a headless install with no SSH, start the daemon with".to_string(),
            " --setup-listen <address> instead. That widens the window this code exists to"
                .to_string(),
            " close, so it logs a warning and should be used deliberately.".to_string(),
        ]);
    }
    lines.push(rule);
    mw_obs::console_banner(&lines);
}
