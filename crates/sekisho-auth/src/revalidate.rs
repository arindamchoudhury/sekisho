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
//!
//! Locks are taken in one order, as everywhere: the person's row, then their
//! sessions, then their Dex token. A refresh holds its session's row while
//! Dex answers, so it stores what Dex said about the person only after
//! letting the session go.

use sekisho_store::{Confirmation, RefreshLock, SignIn, StoreError};
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
    let state = state.clone();
    on_task(user, async move {
        let (_, identity) = conclude(&state, user, None).await?;
        confirm(&state, user, &identity).await
    })
    .await
}

/// [`revalidate`] for a refresh holding its session's row. Dex's answer
/// comes back with the lock, to be stored with [`record`] once the lock is
/// committed: storing it locks the person's row, which disabling,
/// offboarding and refusals lock before their sessions. If the person isn't
/// confirmed, a refusal or a disabled user has ended the session through the
/// lock; otherwise the lock is dropped and nothing changed.
pub(crate) async fn revalidate_session(
    state: &AuthState,
    lock: RefreshLock,
) -> Result<(RefreshLock, DexIdentity), Revalidation> {
    let user = lock.session().user_id;
    let state = state.clone();
    let (held, identity) = on_task(
        user,
        async move { conclude(&state, user, Some(lock)).await },
    )
    .await?;
    Ok((held.ok_or(Revalidation::Internal)?, identity))
}

/// Stores the identity Dex confirmed for a refresh, on a task of its own so
/// that a client hanging up can't undo it. Call it only once the refresh's
/// session lock is committed.
pub(crate) async fn record(
    state: &AuthState,
    user: Uuid,
    identity: DexIdentity,
) -> Result<(), Revalidation> {
    let state = state.clone();
    on_task(user, async move { confirm(&state, user, &identity).await }).await
}

/// Runs `check` on a task of its own, which finishes even if the caller's
/// future is dropped. A session lock it hands back then is dropped with the
/// task's output, which rolls it back.
async fn on_task<T: Send + 'static>(
    user: Uuid,
    check: impl Future<Output = Result<T, Revalidation>> + Send + 'static,
) -> Result<T, Revalidation> {
    tokio::spawn(check).await.unwrap_or_else(|e| {
        error!(%user, error = %e, "the re-check at Dex failed");
        Err(Revalidation::Internal)
    })
}

/// Asks Dex, then records what a refusal or a disabled user means.
async fn conclude(
    state: &AuthState,
    user: Uuid,
    mut held: Option<RefreshLock>,
) -> Result<(Option<RefreshLock>, DexIdentity), Revalidation> {
    let refusal = match ask_dex(state, user).await {
        Ok(identity) => return Ok((held, identity)),
        Err(e @ (Revalidation::Refused | Revalidation::Disabled)) => e,
        Err(e) => return Err(e),
    };
    let reason = if refusal == Revalidation::Refused {
        "idp_refused"
    } else {
        "disabled"
    };
    let now = state.now();
    // The held session's row is ours: end it through the lock and let it go
    // before the user's other sessions are ended, which would wait for it.
    if let Some(mut lock) = held.take() {
        lock.end(reason, now).await.map_err(internal)?;
        lock.commit().await.map_err(internal)?;
    }
    if refusal == Revalidation::Refused {
        state
            .store
            .mark_idp_refused(user, now)
            .await
            .map_err(internal)?;
        warn!(%user, "Dex refused the person: their sessions have ended and their keys are invalid");
    }
    Err(refusal)
}

/// Stores the identity Dex answered with, unless the person was disabled or
/// refused meanwhile. Never while one of their sessions' rows is held.
async fn confirm(
    state: &AuthState,
    user: Uuid,
    identity: &DexIdentity,
) -> Result<(), Revalidation> {
    match state
        .store
        .confirm_identity(&user, &sign_in_of(identity), state.now())
        .await
    {
        Ok(Confirmation::Confirmed) => Ok(()),
        Ok(Confirmation::Disabled) => Err(Revalidation::Disabled),
        Ok(Confirmation::Refused) => {
            info!(%user, "the person was refused at the identity provider meanwhile");
            Err(Revalidation::Refused)
        }
        Ok(Confirmation::OtherSubject) => {
            error!(%user, "Dex answered a refresh with another subject");
            Err(Revalidation::Internal)
        }
        Err(e) => Err(internal(e)),
    }
}

/// Replays the user's Dex token under its lock, and stores the rotated
/// token (committed before anything else can fail).
async fn ask_dex(state: &AuthState, user: Uuid) -> Result<DexIdentity, Revalidation> {
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
        let mut answer = replay(state, &token).await;
        if let Err(e @ DexError::Unreachable(_)) = &answer {
            // Dex may have rotated the token before its answer was lost. Asked
            // again within its reuse interval, Dex hands back the token it
            // rotated to; later, it would refuse this one.
            warn!(%user, error = %e, "Dex's answer about the person didn't arrive; asking again");
            answer = replay(state, &token).await;
        }
        let identity = match answer {
            Ok(identity) => identity,
            Err(DexError::Refused(why)) if refused.is_none() => {
                // A sign-in elsewhere may have just replaced the token at Dex,
                // and be waiting on this lock to store the new one: look again
                // before concluding.
                info!(%user, %why, "Dex refused the person's refresh token; looking again");
                refused = Some(token);
                drop(lock);
                continue;
            }
            Err(DexError::Refused(why)) => {
                info!(%user, %why, "Dex refused the person's new refresh token too");
                return Err(Revalidation::Refused);
            }
            Err(e @ DexError::Unreachable(_)) => {
                warn!(%user, error = %e, "Dex couldn't be asked about the person");
                return Err(Revalidation::Unreachable);
            }
            Err(e @ (DexError::Unavailable(_) | DexError::Invalid(_))) => {
                warn!(%user, error = %e, "Dex failed while asked about the person");
                return Err(Revalidation::Unavailable);
            }
        };
        // Dex has rotated its token: keep the new one before anything else.
        if let Some(next) = &identity.refresh_token {
            lock.replace(next).await.map_err(internal)?;
        }
        lock.commit().await.map_err(internal)?;
        break identity;
    };
    Ok(identity)
}

/// One refresh at Dex, within [`DEX_TIMEOUT`]: no answer by then is
/// [`DexError::Unreachable`].
async fn replay(state: &AuthState, token: &str) -> Result<DexIdentity, DexError> {
    timeout(DEX_TIMEOUT, state.dex.refresh(token))
        .await
        .unwrap_or_else(|_| {
            Err(DexError::Unreachable(format!(
                "no answer within {DEX_TIMEOUT:?}"
            )))
        })
}
