//! The Envoy rate-limit service, used only to record spend.
//!
//! agentgateway calls it twice per request: before (every cost 0) and when
//! the request ends. The second call carries one descriptor per [`Measure`]:
//! input, cache read, cache write and output tokens, and agentgateway's
//! catalog cost in micro-USD. Every descriptor has the same entries: the
//! route's `env`, the `key` id that `extAuthz` verified, the install's charge
//! `secret`, the requested `model`, and the `request` ID Sekisho's stream
//! hook gave the request. A measure agentgateway couldn't evaluate is left
//! out, so a missing output count means the stream ended before the
//! provider reported its usage.
//!
//! What a request costs (design 9.6):
//! 1. output reported, the model has admin prices: Sekisho prices the
//!    provider's counts;
//! 2. output reported, no admin prices: agentgateway's catalog cost;
//! 3. output missing and the answer's status not 2xx: nothing;
//! 4. output missing otherwise: an estimate, the prompt plus the answer text
//!    the stream hook tallied, at the admin's prices or else at Sekisho's
//!    copy of agentgateway's catalog.
//!
//! This is the only place that charges. It always answers OK; refusals
//! happen in `extAuthz`, whose answers reach the client with a message.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sekisho_core::keys::{EnvKeyId, verify_secret};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName};
use sekisho_core::pricing::{Measure, Usage};
use sekisho_proto::ratelimit::rate_limit_response::Code;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitService;
use sekisho_proto::ratelimit::{RateLimitDescriptor, RateLimitRequest, RateLimitResponse};
use sekisho_store::{Store, StoreError};
use time::OffsetDateTime;
use tonic::{Request, Response, Status};

use crate::cache::{AuthCache, CachedKey};
use crate::catalog::catalog_rates;
use crate::tally::{Tallies, Tally};
use crate::tokens::answer_tokens;

/// The pauses before each new attempt at recording spend after a database
/// error, so a brief outage doesn't lose spend. A charge that committed but
/// whose answer was lost is counted twice; for a budget, over-counting is the
/// safer side.
const CHARGE_RETRIES: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(400)];

/// How long `sekisho serve` waits for the stream hook's tally of a stream
/// that ended early. After that, the prompt alone is charged.
pub const TALLY_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Charger {
    pub cache: Arc<AuthCache>,
    pub store: Store,
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    /// SHA-256 of the charge secret the rendered descriptors carry. Calls
    /// without it are not from this install's agentgateway.
    pub charge_secret_sha256: [u8; 32],
    /// What the stream hook relayed of each request, shared with it.
    pub tallies: Tallies,
    /// How long to wait for a tally ([`TALLY_WAIT`] in `sekisho serve`).
    pub tally_wait: Duration,
}

#[tonic::async_trait]
impl RateLimitService for Charger {
    async fn should_rate_limit(
        &self,
        request: Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, Status> {
        let request = request.into_inner();
        // agentgateway gives this call 10 s and then drops it, with whatever
        // it was waiting for. So the answer never waits for the database or
        // a tally: spend is recorded on its own task, where a slow database,
        // the retries or the tally wait can't cut a charge short and its
        // errors are always logged.
        let costs_something = request
            .descriptors
            .iter()
            .any(|d| d.hits_addend.unwrap_or(u64::from(request.hits_addend)) != 0);
        if costs_something {
            let charger = self.clone();
            tokio::spawn(async move { charger.charge(&request).await });
        }
        Ok(Response::new(RateLimitResponse {
            overall_code: Code::Ok as i32,
            ..Default::default()
        }))
    }
}

/// One finished request, as one call's descriptors report it.
struct Report {
    key: CachedKey,
    env: EnvName,
    /// The model the client asked for (`llm.requestModel`).
    model: String,
    /// The stream hook's ID for the request; empty without one.
    request: String,
    /// Each measure agentgateway could evaluate.
    counts: HashMap<Measure, u64>,
}

impl Report {
    fn count(&self, measure: Measure) -> u64 {
        self.counts.get(&measure).copied().unwrap_or(0)
    }
}

impl Charger {
    /// Charges one finished request to every budget its key's request counts
    /// against. Problems are logged, never answered: the request has already
    /// been served.
    async fn charge(&self, request: &RateLimitRequest) {
        let Some(report) = self.report(request) else {
            return;
        };
        let Some(cost) = self.cost(&report).await else {
            return;
        };
        if cost.0 > 0 {
            self.charge_budgets(&report.key, &report.env, cost).await;
        }
    }

    /// Reads one call. `None` (logged) when it isn't from this install's
    /// agentgateway or names no known key.
    fn report(&self, request: &RateLimitRequest) -> Option<Report> {
        let mut counts = HashMap::new();
        for descriptor in &request.descriptors {
            // Anyone who can reach this port could otherwise drain any budget.
            if !entry(descriptor, "secret")
                .is_some_and(|s| verify_secret(s, &self.charge_secret_sha256))
            {
                tracing::warn!(
                    "spend not recorded: a descriptor lacks this install's charge secret"
                );
                return None;
            }
            let count = descriptor
                .hits_addend
                .unwrap_or(u64::from(request.hits_addend));
            match entry(descriptor, "measure").and_then(Measure::parse) {
                Some(measure) => {
                    counts.insert(measure, count);
                }
                None => tracing::warn!(
                    measure = entry(descriptor, "measure"),
                    "a spend descriptor without a known measure is ignored"
                ),
            }
        }
        // Every descriptor of a call carries the same request entries.
        let first = request.descriptors.first()?;
        // The key id comes from Sekisho's own extAuthz answer (`extauthz.key`),
        // never from the client, so no secret is needed. Revoked or suspended
        // keys are still charged for what they used.
        let Some(key) = entry(first, "key")
            .and_then(EnvKeyId::parse)
            .and_then(|id| self.cache.key(&id))
        else {
            tracing::warn!("spend not recorded: no known key in the descriptors");
            return None;
        };
        let env = entry(first, "env")
            .and_then(|e| EnvName::parse(e).ok())
            .unwrap_or_else(|| key.environment.clone());
        Some(Report {
            key,
            env,
            model: entry(first, "model").unwrap_or_default().to_owned(),
            request: entry(first, "request").unwrap_or_default().to_owned(),
            counts,
        })
    }

    /// What the request costs, or `None` (logged where it matters) when
    /// nothing is charged.
    async fn cost(&self, report: &Report) -> Option<MicroUsd> {
        let (key, model) = (&report.key.id, report.model.as_str());
        let pricing = match ModelName::parse(model) {
            Ok(name) => match retried(|| self.store.model_pricing(&name)).await {
                Ok(pricing) => pricing,
                Err(error) => {
                    tracing::error!(%key, model, %error, "spend not recorded: reading the model's prices failed");
                    return None;
                }
            },
            Err(_) => None, // not a name Sekisho could have
        };
        let admin = pricing
            .as_ref()
            .and_then(|p| Some(p.prices?.rates(p.format)));
        let mut usage = Usage {
            input: report.count(Measure::Input),
            cache_read: report.count(Measure::CacheRead),
            cache_write: report.count(Measure::CacheWrite),
            output: 0,
        };

        if let Some(&output) = report.counts.get(&Measure::Output) {
            // The provider reported its usage: its counts win, the tally isn't needed.
            if !report.request.is_empty() {
                self.tallies.discard(&report.request);
            }
            usage.output = output;
            return match (admin, report.counts.get(&Measure::Catalog)) {
                (Some(rates), _) => Some(rates.cost(&usage)),
                (None, Some(&catalog)) => match i64::try_from(catalog) {
                    Ok(cost) => Some(MicroUsd(cost)),
                    Err(_) => {
                        tracing::error!(%key, model, catalog, "spend not recorded: cost too large");
                        None
                    }
                },
                (None, None) => {
                    tracing::warn!(%key, model, "spend not recorded: no price for this model");
                    None
                }
            };
        }

        // The stream ended before the provider reported its usage.
        match self.tally(&report.request).await {
            Some(Tally {
                status: Some(status),
                ..
            }) if !(200..300).contains(&status) => {
                // A provider error isn't billed.
                tracing::debug!(%key, model, status, "an error answer ended early: nothing charged");
                return None;
            }
            Some(tally) => usage.output = answer_tokens(tally.answer_o200k, model),
            None => tracing::warn!(
                %key, model,
                "no tally for a stream that ended early: charging its prompt alone"
            ),
        }
        let rates = admin.or_else(|| {
            let p = pricing.as_ref()?;
            catalog_rates(p.kind, p.format, model)
        });
        let Some(rates) = rates else {
            tracing::warn!(%key, model, "spend not recorded: no price for this model");
            return None;
        };
        let cost = rates.cost(&usage);
        tracing::info!(
            %key, model, input = usage.input, output = usage.output, micro_usd = cost.0,
            "estimated a stream that ended before the provider reported its usage"
        );
        Some(cost)
    }

    /// The stream hook's finished tally for `request`. One that doesn't
    /// finish within the wait is dropped, so it can't be read later.
    async fn tally(&self, request: &str) -> Option<Tally> {
        if request.is_empty() {
            return None;
        }
        let tally = self.tallies.wait(request, self.tally_wait).await;
        if tally.is_none() {
            self.tallies.discard(request);
        }
        tally
    }

    /// Adds `cost` to every budget `key`'s requests in `env` count against.
    async fn charge_budgets(&self, key: &CachedKey, env: &EnvName, cost: MicroUsd) {
        let budgets = match retried(|| self.store.applicable_budgets(env, &key.id, &key.owner))
            .await
        {
            Ok(budgets) => budgets,
            Err(error) => {
                tracing::error!(key = %key.id, cost = cost.0, %error, "spend not recorded: reading budgets failed");
                return;
            }
        };
        let now = (self.clock)(); // every attempt charges the request's window
        // One budget at a time: each is its own upsert, so retrying several
        // together would charge again the ones that already succeeded.
        for budget in &budgets {
            let charged = retried(|| {
                self.store
                    .charge(std::slice::from_ref(&budget.id), cost, now)
            })
            .await;
            if let Err(error) = charged {
                tracing::error!(
                    key = %key.id, budget = %budget.name, cost = cost.0, %error,
                    "spend not recorded for this budget: the database kept failing"
                );
            }
        }
    }
}

/// The value of the descriptor's entry `key`.
fn entry<'a>(descriptor: &'a RateLimitDescriptor, key: &str) -> Option<&'a str> {
    descriptor
        .entries
        .iter()
        .find(|e| e.key == key)
        .map(|e| e.value.as_str())
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
