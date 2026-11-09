//! The Envoy rate-limit service, used only to record spend.
//!
//! agentgateway calls it twice per request: before (cost 0) and after (the
//! catalog cost in micro-USD, from `cost: int(llm.cost.total * 1000000.0)`).
//! It always answers OK; refusals happen in `extAuthz`, whose answers reach
//! the client with a message.

use std::sync::Arc;

use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_proto::ratelimit::rate_limit_response::Code;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitService;
use sekisho_proto::ratelimit::{RateLimitDescriptor, RateLimitRequest, RateLimitResponse};
use sekisho_store::Store;
use time::OffsetDateTime;
use tonic::{Request, Response, Status};

use crate::cache::AuthCache;
use crate::credential::{Credential, resolve};

pub struct Charger {
    pub cache: Arc<AuthCache>,
    pub store: Store,
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
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
        // Revoked or suspended keys are still charged for what they used.
        let Credential::Known(key) = resolve(&self.cache, entry("credential")) else {
            tracing::warn!(cost, "spend not recorded: no known key in the descriptor");
            return;
        };
        let env = entry("env")
            .and_then(|e| EnvName::parse(e).ok())
            .unwrap_or_else(|| key.environment.clone());
        let budgets = match self
            .store
            .applicable_budgets(&env, &key.id, &key.owner)
            .await
        {
            Ok(budgets) => budgets,
            Err(error) => {
                tracing::error!(key = %key.id, cost, %error, "spend not recorded: reading budgets failed");
                return;
            }
        };
        let ids: Vec<_> = budgets.iter().map(|b| b.id).collect();
        if let Err(error) = self
            .store
            .charge(&ids, MicroUsd(cost), (self.clock)())
            .await
        {
            tracing::error!(key = %key.id, cost, %error, "spend not recorded: charging failed");
        }
    }
}
