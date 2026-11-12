//! The Envoy rate-limit service, used only to record spend.
//!
//! agentgateway calls it twice per request: when the request starts and
//! when the provider's usage arrives. Each call carries one descriptor per
//! [`Measure`]: input, cache read, cache write and output tokens and
//! agentgateway's catalog cost in micro-USD, each costed when the request
//! ends (all 0 on the first call), and the prompt count, sent on the first
//! call. Every descriptor has the same entries: the route's `env`, the `key`
//! id that `extAuthz` verified, the install's charge `secret`, the requested
//! `model`, and the `request` ID Sekisho's stream hook gave the request.
//!
//! agentgateway v1.7.0 sends no second call for a stream that ends before
//! the provider's usage (the client hung up, or a revoked key cut it). So
//! the first call starts a watch on the stream hook's tally: if the stream
//! ends and no report follows within [`TALLY_WAIT`], the request is charged
//! an estimate here, and a later report is refused.
//!
//! What a request costs (design 9.6):
//! 1. output reported, the model has admin prices: Sekisho prices the
//!    provider's counts;
//! 2. output reported, no admin prices: agentgateway's catalog cost;
//! 3. no output count and the answer's status not 2xx: nothing;
//! 4. no output count otherwise: an estimate, the prompt plus the answer
//!    text the stream hook tallied, at the admin's prices or else at
//!    Sekisho's copy of agentgateway's catalog.
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
use sekisho_store::{ModelPricing, Store, StoreError};
use time::OffsetDateTime;
use tonic::{Request, Response, Status};

use crate::cache::{AuthCache, CachedKey};
use crate::catalog::catalog_rates;
use crate::tally::{Recorded, STALLED, Start, Tallies, Tally};
use crate::tokens::answer_tokens;

/// The pauses before each new attempt at recording spend after a database
/// error, so a brief outage doesn't lose spend. A charge that committed but
/// whose answer was lost is counted twice; for a budget, over-counting is the
/// safer side.
const CHARGE_RETRIES: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(400)];

/// How long `sekisho serve` waits, after a stream ends, for agentgateway to
/// report its usage before charging an estimate.
pub const TALLY_WAIT: Duration = Duration::from_secs(5);

/// The measures that cost something when a request ends: all but the prompt
/// count of the request-side call.
const COSTED: [Measure; 5] = [
    Measure::Input,
    Measure::CacheRead,
    Measure::CacheWrite,
    Measure::Output,
    Measure::Catalog,
];

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
    /// How long to wait after a stream ends for agentgateway's report
    /// ([`TALLY_WAIT`] in `sekisho serve`).
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
        // a stream: each call is handled on its own task, where a slow
        // database, the retries or a watch can't cut a charge short and its
        // errors are always logged.
        let charger = self.clone();
        tokio::spawn(async move { charger.handle(&request).await });
        Ok(Response::new(RateLimitResponse {
            overall_code: Code::Ok as i32,
            ..Default::default()
        }))
    }
}

/// One call's report of a request.
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

    /// The counts without the output, which is missing or not yet known.
    fn usage(&self) -> Usage {
        Usage {
            input: self.count(Measure::Input),
            cache_read: self.count(Measure::CacheRead),
            cache_write: self.count(Measure::CacheWrite),
            output: 0,
        }
    }

    fn start(&self, usage: Usage, from_response: bool) -> Start {
        Start {
            key: self.key.id.clone(),
            env: self.env.clone(),
            model: self.model.clone(),
            usage,
            from_response,
        }
    }
}

impl Charger {
    /// Handles one call. Problems are logged, never answered: the request
    /// has been served or is under way.
    async fn handle(&self, request: &RateLimitRequest) {
        let Some(report) = self.report(request) else {
            return;
        };
        let cost = if COSTED.iter().all(|&m| report.count(m) == 0) {
            // The request-side call: only the prompt count.
            let prompt = Usage {
                input: report.count(Measure::Prompt),
                ..Usage::default()
            };
            if !self.record_start(&report, prompt, false) && !report.request.is_empty() {
                // Another Sekisho's stream hook has it, or it is gone: a
                // stream that ends early can't be estimated here.
                tracing::debug!(
                    key = %report.key.id, model = report.model,
                    "no tally here for a request's start"
                );
            }
            return;
        } else if let Some(&output) = report.counts.get(&Measure::Output)
            // A Claude stream that ended without `message_delta` is reported
            // with `message_start`'s count: estimate it like one without.
            && !self.tallies.stale(&report.request, output)
        {
            if !report.request.is_empty() && !self.tallies.reported(&report.request) {
                tracing::warn!(
                    key = %report.key.id, model = report.model,
                    "agentgateway reported a request already charged an estimate: not charged again"
                );
                return;
            }
            self.reported_cost(&report, output).await
        } else {
            // A response-side call without the output count (or with a stale
            // one): its counts replace the prompt count, and the stream's end
            // decides.
            let usage = report.usage();
            if self.record_start(&report, usage, true) {
                return;
            }
            tracing::warn!(
                key = %report.key.id, model = report.model,
                "no tally for a request without an output count: charging its prompt alone"
            );
            self.estimate(&report.key.id, &report.model, usage, None)
                .await
        };
        if let Some(cost) = cost
            && cost.0 > 0
        {
            self.charge_budgets(&report.key, &report.env, cost).await;
        }
    }

    /// Records a request's start on its tally, and on the first record
    /// watches the stream. `false` when there is no tally for it.
    fn record_start(&self, report: &Report, usage: Usage, from_response: bool) -> bool {
        if report.request.is_empty() {
            return false;
        }
        match self
            .tallies
            .start(&report.request, report.start(usage, from_response))
        {
            Recorded::First => {
                let (charger, id) = (self.clone(), report.request.clone());
                tokio::spawn(async move { charger.charge_unreported(&id).await });
                true
            }
            Recorded::Again | Recorded::Estimated => true,
            Recorded::Unknown => false,
        }
    }

    /// Charges an estimate for a request whose stream ended with no report
    /// from agentgateway.
    async fn charge_unreported(&self, id: &str) {
        let Some((start, tally)) = self.tallies.unreported(id, self.tally_wait, STALLED).await
        else {
            return; // reported
        };
        let Some(key) = self.cache.key(&start.key) else {
            tracing::warn!(key = %start.key, "spend not recorded: the key is no longer known");
            return;
        };
        match tally.status {
            // agentgateway's own error (an unreachable provider): ext_proc
            // never sees it, nothing reached the client, nothing was billed.
            None => {
                tracing::debug!(key = %key.id, model = start.model, "no answer reached the client: nothing charged");
                return;
            }
            // A provider error isn't billed.
            Some(status) if !(200..300).contains(&status) => {
                tracing::debug!(key = %key.id, model = start.model, status, "an error answer: nothing charged");
                return;
            }
            Some(_) => {}
        }
        if let Some(cost) = self
            .estimate(&key.id, &start.model, start.usage, Some(tally))
            .await
            && cost.0 > 0
        {
            self.charge_budgets(&key, &start.env, cost).await;
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

    /// The model's admin prices and provider. `Err` (logged) when the
    /// database kept failing; `Ok(None)` for a model Sekisho doesn't have.
    async fn pricing(&self, key: &EnvKeyId, model: &str) -> Result<Option<ModelPricing>, ()> {
        let Ok(name) = ModelName::parse(model) else {
            return Ok(None); // not a name Sekisho could have
        };
        retried(|| self.store.model_pricing(&name))
            .await
            .map_err(|error| {
                tracing::error!(%key, model, %error, "spend not recorded: reading the model's prices failed");
            })
    }

    /// What a request agentgateway reported costs: the admin's prices, else
    /// agentgateway's catalog cost.
    async fn reported_cost(&self, report: &Report, output: u64) -> Option<MicroUsd> {
        let (key, model) = (&report.key.id, report.model.as_str());
        let pricing = self.pricing(key, model).await.ok()?;
        let admin = pricing
            .as_ref()
            .and_then(|p| Some(p.prices?.rates(p.format)));
        let usage = Usage {
            output,
            ..report.usage()
        };
        match (admin, report.counts.get(&Measure::Catalog)) {
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
        }
    }

    /// The estimate for a request without an output count: `usage` plus the
    /// tallied answer, scaled for Claude, at the admin's prices or else
    /// Sekisho's copy of agentgateway's catalog. The provider's own counts
    /// from the stream win where they got through.
    async fn estimate(
        &self,
        key: &EnvKeyId,
        model: &str,
        mut usage: Usage,
        tally: Option<Tally>,
    ) -> Option<MicroUsd> {
        let pricing = self.pricing(key, model).await.ok()?;
        usage.output = 0;
        if let Some(tally) = tally {
            if let Some(prompt) = tally.usage.prompt {
                (usage.input, usage.cache_read, usage.cache_write) =
                    (prompt.input, prompt.cache_read, prompt.cache_write);
            }
            usage.output = tally
                .usage
                .output
                .unwrap_or_else(|| answer_tokens(tally.answer_o200k, model));
        }
        let rates = pricing.as_ref().and_then(|p| match p.prices {
            Some(prices) => Some(prices.rates(p.format)),
            None => catalog_rates(p.kind, p.format, model),
        });
        let Some(rates) = rates else {
            tracing::warn!(%key, model, "spend not recorded: no price for this model");
            return None;
        };
        let cost = rates.cost(&usage);
        tracing::info!(
            %key, model, input = usage.input, cache_read = usage.cache_read,
            cache_write = usage.cache_write, output = usage.output, micro_usd = cost.0,
            "estimated a request agentgateway never reported"
        );
        Some(cost)
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
