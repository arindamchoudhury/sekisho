//! Asking Dex about a person again: Sekisho replays their one Dex refresh
//! token, and through it Dex asks the identity provider.
//!
//! Dex keeps one refresh token per person and rotates it on every use, so
//! the replay happens under the token's row lock (two refreshes never show
//! Dex the same token) and on a task of its own: a client that hangs up
//! while Dex answers can't make Sekisho lose the token Dex rotated to.
//!
//! Only Dex's refusal ends sessions. Dex not answering, or failing on its
//! side, changes nothing.

use sekisho_store::{RefreshLock, SignIn, SignInOutcome, StoreError};
use tokio::time::timeout;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::AuthState;
use crate::dex::{DEX_TIMEOUT, DexError, DexIdentity};

/// Why a person couldn't be confirmed at Dex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Revalidation {
    /// Dex refused them, or Sekisho holds no Dex token for them: every
    /// session of theirs has ended, and their keys are invalid until they
    /// sign in again.
    #[error("Dex refused the person")]
    Refused,
    /// The user is disabled (or gone); nothing was asked of Dex.
    #[error("the user is disabled")]
    Disabled,
    /// Dex couldn't be reached or didn't answer within [`DEX_TIMEOUT`];
    /// nothing changed.
    #[error("Dex couldn't be reached")]
    Unreachable,
    /// Dex answered with a failure of its own (including, in Dex v2.46.1, a
    /// connector's refusal) or an answer that failed Sekisho's checks;
    /// nothing changed.
    #[error("Dex failed")]
    Unavailable,
    /// Sekisho's database failed.
    #[error("Sekisho's database failed")]
    Internal,
}

/// Who Dex says signed in, as the store records it.
pub(crate) fn sign_in_of(identity: &DexIdentity) -> SignIn {
    SignIn {
        subject: identity.subject.clone(),
        email: identity.email.clone(),
        name: identity.name.clone(),
        groups: identity.groups.clone(),
        connector_id: identity.connector_id.clone(),
        connector_user_id: identity.connector_user_id.clone(),
    }
}

fn internal(e: StoreError) -> Revalidation {
    error!(error = %e, "the store failed while re-checking a person at Dex");
    Revalidation::Internal
}

/// Re-checks `user` at Dex: replays their Dex token, stores the token Dex
/// rotated to and the identity it answered with. A refusal ends every
/// session of the user and marks them refused at the identity provider.
pub async fn revalidate(state: &AuthState, user: Uuid) -> Result<(), Revalidation> {
    on_task(state, user, None).await.map(drop)
}

/// [`revalidate`] for a refresh holding its session's row: the lock comes
/// back if the person is confirmed. If not, a refusal or a disabled user has
/// ended the session through the lock; otherwise the lock is dropped and
/// nothing changed.
pub(crate) async fn revalidate_session(
    state: &AuthState,
    lock: RefreshLock,
) -> Result<RefreshLock, Revalidation> {
    let user = lock.session().user_id;
    let back = on_task(state, user, Some(lock)).await?;
    back.ok_or(Revalidation::Internal)
}

/// Runs the check on a task of its own, which finishes even if the caller's
/// future is dropped. A session lock it hands back then is dropped with the
/// task's output, which rolls it back.
async fn on_task(
    state: &AuthState,
    user: Uuid,
    held: Option<RefreshLock>,
) -> Result<Option<RefreshLock>, Revalidation> {
    let state = state.clone();
    tokio::spawn(async move { conclude(&state, user, held).await })
        .await
        .unwrap_or_else(|e| {
            error!(%user, error = %e, "the re-check at Dex failed");
            Err(Revalidation::Internal)
        })
}

/// Asks Dex, then records what a refusal or a disabled user means.
async fn conclude(
    state: &AuthState,
    user: Uuid,
    mut held: Option<RefreshLock>,
) -> Result<Option<RefreshLock>, Revalidation> {
    let outcome = ask_dex(state, user).await;
    let reason = match outcome {
        Ok(()) => return Ok(held),
        Err(Revalidation::Refused) => "idp_refused",
        Err(Revalidation::Disabled) => "disabled",
        Err(e) => return Err(e),
    };
    let now = state.now();
    // The held session's row is ours: end it through the lock and let it go
    // before the user's other sessions are ended, which would wait for it.
    if let Some(mut lock) = held.take() {
        lock.end(reason, now).await.map_err(internal)?;
        lock.commit().await.map_err(internal)?;
    }
    if outcome == Err(Revalidation::Refused) {
        state
            .store
            .mark_idp_refused(user, now)
            .await
            .map_err(internal)?;
        warn!(%user, "Dex refused the person: their sessions have ended and their keys are invalid");
    }
    outcome.map(|()| None)
}

/// Replays the user's Dex token under its lock. Stores the rotated token
/// (committed before anything else can fail) and the refreshed identity.
async fn ask_dex(state: &AuthState, user: Uuid) -> Result<(), Revalidation> {
    let account = match state.store.user(&user).await {
        Ok(account) => account,
        Err(StoreError::NotFound) => return Err(Revalidation::Disabled),
        Err(e) => return Err(internal(e)),
    };
    if account.disabled_at.is_some() {
        return Err(Revalidation::Disabled);
    }
    // The token Dex refused, if it did.
    let mut refused: Option<String> = None;
    let identity = loop {
        let mut lock = state
            .store
            .lock_dex_token(&state.secret_box, user)
            .await
            .map_err(internal)?;
        let Some(token) = lock.token().map(str::to_owned) else {
            info!(%user, "Sekisho holds no Dex token for the person");
            return Err(Revalidation::Refused);
        };
        if refused.as_deref() == Some(token.as_str()) {
            return Err(Revalidation::Refused);
        }
        let identity = match timeout(DEX_TIMEOUT, state.dex.refresh(&token)).await {
            Ok(Ok(identity)) => identity,
            Ok(Err(DexError::Refused(why))) if refused.is_none() => {
                // A sign-in elsewhere may have just replaced the token at Dex,
                // and be waiting on this lock to store the new one: look again
                // before concluding.
                info!(%user, %why, "Dex refused the person's refresh token; looking again");
                refused = Some(token);
                drop(lock);
                continue;
            }
            Ok(Err(DexError::Refused(why))) => {
                info!(%user, %why, "Dex refused the person's new refresh token too");
                return Err(Revalidation::Refused);
            }
            Ok(Err(e @ DexError::Unreachable(_))) => {
                warn!(%user, error = %e, "Dex couldn't be asked about the person");
                return Err(Revalidation::Unreachable);
            }
            Ok(Err(e @ (DexError::Unavailable(_) | DexError::Invalid(_)))) => {
                warn!(%user, error = %e, "Dex failed while asked about the person");
                return Err(Revalidation::Unavailable);
            }
            Err(_) => {
                warn!(%user, "Dex didn't answer about the person within {DEX_TIMEOUT:?}");
                return Err(Revalidation::Unreachable);
            }
        };
        // Dex has rotated its token: keep the new one before anything else.
        if let Some(next) = &identity.refresh_token {
            lock.replace(next).await.map_err(internal)?;
        }
        lock.commit().await.map_err(internal)?;
        break identity;
    };
    if account.dex_subject.as_deref() != Some(identity.subject.as_str()) {
        error!(%user, "Dex answered a refresh with another subject");
        return Err(Revalidation::Internal);
    }
    match state
        .store
        .sign_in(&sign_in_of(&identity), state.now())
        .await
    {
        Ok(SignInOutcome::User(signed_in)) if signed_in.id == user => Ok(()),
        Ok(SignInOutcome::Disabled) => Err(Revalidation::Disabled),
        Ok(SignInOutcome::User(other)) => {
            error!(%user, other = %other.id, "a refreshed identity matched another user");
            Err(Revalidation::Internal)
        }
        Ok(SignInOutcome::UnknownLocalAccount) => {
            error!(%user, "a refreshed identity is a local account Sekisho doesn't know");
            Err(Revalidation::Internal)
        }
        Err(e) => Err(internal(e)),
    }
}
