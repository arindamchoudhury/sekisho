//! The daily check at Dex for people who own active environment keys.
//!
//! Keys never refresh, so without it a person the identity provider dropped
//! would keep working keys. Each sweep claims the owners due a check (one
//! replica gets each), then re-checks them one at a time.

use sekisho_store::Result;
use time::OffsetDateTime;
use tracing::{error, info};
use uuid::Uuid;

use crate::AuthState;
use crate::revalidate::{Revalidation, revalidate};

/// How many owners one sweep claims at most.
pub const SWEEP_LIMIT: i64 = 100;

/// What one sweep did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Owners claimed for a check.
    pub claimed: usize,
    /// Dex confirmed them.
    pub passed: usize,
    /// Dex refused them: their sessions have ended and their keys are
    /// invalid.
    pub refused: usize,
    /// Checked again a day later: Dex failed on its side (as Dex v2.46.1
    /// answers a connector's refusal, which would fail again at once), or
    /// the person was disabled meanwhile.
    pub kept: usize,
    /// Given back for the next sweep: Dex couldn't be reached (the rest of
    /// the sweep was given back too), or the database failed.
    pub released: usize,
}

/// Claims the key owners due a check at `now` and re-checks each at Dex.
pub async fn check_key_owners(state: &AuthState, now: OffsetDateTime) -> Result<SweepReport> {
    let claimed = state.store.claim_idp_checks(now, SWEEP_LIMIT).await?;
    let mut report = SweepReport {
        claimed: claimed.len(),
        ..SweepReport::default()
    };
    let mut owners = claimed.into_iter();
    while let Some(user) = owners.next() {
        match revalidate(state, user).await {
            Ok(()) => report.passed += 1,
            Err(Revalidation::Refused) => report.refused += 1,
            Err(Revalidation::Unavailable | Revalidation::Disabled) => report.kept += 1,
            Err(Revalidation::Internal) => give_back(state, &user, &mut report).await,
            Err(Revalidation::Unreachable) => {
                // Dex is down: the others would wait out its timeout in turn.
                give_back(state, &user, &mut report).await;
                for rest in owners.by_ref() {
                    give_back(state, &rest, &mut report).await;
                }
            }
        }
    }
    if report.claimed > 0 {
        info!(?report, "checked key owners at Dex");
    }
    Ok(report)
}

async fn give_back(state: &AuthState, user: &Uuid, report: &mut SweepReport) {
    match state.store.release_idp_check(user).await {
        Ok(()) => report.released += 1,
        Err(e) => {
            error!(%user, error = %e, "couldn't give back an IdP check; it waits a day");
            report.kept += 1;
        }
    }
}
