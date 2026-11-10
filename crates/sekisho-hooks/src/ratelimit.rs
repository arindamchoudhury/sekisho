//! The Envoy rate-limit service, used only to record spend.
//!
//! agentgateway calls it twice per request: before (cost 0) and after (the
//! catalog cost in micro-USD, from `cost: int(llm.cost.total * 1000000.0)`).
//! It always answers OK; refusals happen in `extAuthz`, whose answers reach
//! the client with a message. Each descriptor carries the route's `env` and
//! the `key` id that `extAuthz` verified.

use std::sync::Arc;
use std::time::Duration;

use sekisho_core::keys::{EnvKeyId, verify_secret};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_proto::ratelimit::rate_limit_response::Code;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitService;
use sekisho_proto::ratelimit::{RateLimitDescriptor, RateLimitRequest, RateLimitResponse};
use sekisho_store::{Store, StoreError};
use time::OffsetDateTime;
use tonic::{Request, Response, Status};

use crate::cache::AuthCache;

/// The pauses before each new attempt at recording spend after a database
/// error, so a brief outage doesn't lose spend. A charge that committed but
/// whose answer was lost is counted twice; for a budget, over-counting is the
/// safer side.
const CHARGE_RETRIES: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(400)];

pub struct Charger {
    pub cache: Arc<AuthCache>,
    pub store: Store,
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    /// SHA-256 of the charge secret the rendered descriptors carry. Calls
    /// without it are not from this install's agentgateway.
    pub charge_secret_sha256: [u8; 32],
}

#[tonic::async_trait]
impl RateLimitService for Charger {
    async fn should_rate_limit(
        &self,
        request: Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, Status> {
        let request = request.into_inner();
        for descriptor in &request.descriptors {
            self.charge(descriptor, request.hits_addend).await;
        }
        Ok(Response::new(RateLimitResponse {
            overall_code: Code::Ok as i32,
            ..Default::default()
        }))
    }
}

impl Charger {
    /// Charges one descriptor's cost to every budget the key's request counts
    /// against. Problems are logged, never answered: the request has already
    /// been served.
    async fn charge(&self, descriptor: &RateLimitDescriptor, request_hits: u32) {
        let cost = descriptor.hits_addend.unwrap_or(u64::from(request_hits));
        if cost == 0 {
            return; // the pre-check before the request
        }
        let Ok(cost) = i64::try_from(cost) else {
            tracing::error!(cost, "spend not recorded: cost too large");
            return;
        };
        let entry = |key: &str| {
            descriptor
                .entries
                .iter()
                .find(|e| e.key == key)
                .map(|e| e.value.as_str())
        };
        // Anyone who can reach this port could otherwise drain any budget.
        if !entry("secret").is_some_and(|s| verify_secret(s, &self.charge_secret_sha256)) {
            tracing::warn!(
                cost,
                "spend not recorded: the descriptor lacks this install's charge secret"
            );
            return;
        }
        // The key id comes from Sekisho's own extAuthz answer (`extauthz.key`),
        // never from the client, so no secret is needed. Revoked or suspended
        // keys are still charged for what they used.
        let Some(key) = entry("key")
            .and_then(EnvKeyId::parse)
            .and_then(|id| self.cache.key(&id))
        else {
            tracing::warn!(cost, "spend not recorded: no known key in the descriptor");
            return;
        };
        let env = entry("env")
            .and_then(|e| EnvName::parse(e).ok())
            .unwrap_or_else(|| key.environment.clone());
        let budgets = match retried(|| self.store.applicable_budgets(&env, &key.id, &key.owner))
            .await
        {
            Ok(budgets) => budgets,
            Err(error) => {
                tracing::error!(key = %key.id, cost, %error, "spend not recorded: reading budgets failed");
                return;
            }
        };
        let now = (self.clock)(); // every attempt charges the request's window
        // One budget at a time: each is its own upsert, so retrying several
        // together would charge again the ones that already succeeded.
        for budget in &budgets {
            let charged = retried(|| {
                self.store
                    .charge(std::slice::from_ref(&budget.id), MicroUsd(cost), now)
            })
            .await;
            if let Err(error) = charged {
                tracing::error!(
                    key = %key.id, budget = %budget.name, cost, %error,
                    "spend not recorded for this budget: the database kept failing"
                );
            }
        }
    }
}

/// Runs `attempt` until it succeeds, pausing as [`CHARGE_RETRIES`] says after
/// each failure. Returns the last error when every attempt failed.
async fn retried<T, F, Fut>(mut attempt: F) -> Result<T, StoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, StoreError>>,
{
    let mut pauses = CHARGE_RETRIES.iter();
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => match pauses.next() {
                Some(pause) => {
                    tracing::warn!(%error, "database error while recording spend; trying again");
                    tokio::time::sleep(*pause).await;
                }
                None => return Err(error),
            },
        }
    }
}
