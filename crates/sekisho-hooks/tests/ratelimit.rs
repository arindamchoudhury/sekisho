use std::sync::Arc;
use std::time::Duration;

use sekisho_core::formats::ProviderKind;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::Usage;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::ratelimit::Charger;
use sekisho_hooks::tally::{ProviderUsage, Tallies};
use sekisho_proto::ratelimit::RateLimitDescriptor;
use sekisho_proto::ratelimit::RateLimitRequest;
use sekisho_proto::ratelimit::rate_limit_descriptor::Entry;
use sekisho_proto::ratelimit::rate_limit_response::Code;
use sekisho_proto::ratelimit::rate_limit_service_client::RateLimitServiceClient;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitServiceServer;
use sekisho_store::{Budget, BudgetScope, EnvironmentSpec, NewBudget, NewModel, NewProvider};
use sekisho_testkit::TestStore;
use time::OffsetDateTime;
use time::macros::datetime;
use tonic::transport::Channel;

const NOW: OffsetDateTime = datetime!(2026-11-20 12:00 UTC);

struct Rig {
    rls: RateLimitServiceClient<Channel>,
    store: TestStore,
    token: String,
    key: EnvKeyId,
    /// Environment, key and person budgets, all applicable to `token`.
    budgets: Vec<Budget>,
    /// The stream hook's tallies, shared with the charger.
    tallies: Tallies,
}

async fn rig() -> Rig {
    rig_waiting(Duration::from_millis(300)).await
}

/// A rig whose charger waits `tally_wait`, after a stream ends, for
/// agentgateway's report before it estimates. Its models:
/// `claude-priced` (Anthropic, 3 / 15 USD per million tokens), `gpt-priced`
/// (OpenAI, 3 / 15) and `claude-fable-5` (Anthropic, no admin price; 10 /
/// 50 in the catalog).
async fn rig_waiting(tally_wait: Duration) -> Rig {
    let (store, secret_box) = sekisho_testkit::store().await;
    for (provider, kind) in [
        ("anthropic", ProviderKind::Anthropic),
        ("openai", ProviderKind::OpenAi),
    ] {
        let name = ProviderName::parse(provider).unwrap();
        store
            .create_provider(&secret_box, NewProvider::keyed(name, kind, None, "k"))
            .await
            .unwrap();
    }
    for (model, provider, prices) in [
        ("claude-priced", "anthropic", Some((3, 15))),
        ("gpt-priced", "openai", Some((3, 15))),
        ("claude-fable-5", "anthropic", None),
    ] {
        let usd = |n: i64| MicroUsd(n * 1_000_000);
        store
            .create_model(NewModel {
                name: ModelName::parse(model).unwrap(),
                provider: ProviderName::parse(provider).unwrap(),
                input_price: prices.map(|(input, _)| usd(input)),
                output_price: prices.map(|(_, output)| usd(output)),
                cache_read_price: None,
                cache_write_price: None,
            })
            .await
            .unwrap();
    }
    let spec = EnvironmentSpec {
        name: EnvName::parse("data-team").unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: Vec::new(),
    };
    let env = store.upsert_environment(spec).await.unwrap().spec.name;
    let (key, token) = store
        .create_key(&env, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let mut budgets = Vec::new();
    for (name, scope) in [
        ("team", BudgetScope::Environment(env.clone())),
        ("ci-key", BudgetScope::Key(key.id.clone())),
        (
            "alice",
            BudgetScope::Person {
                person: "alice@example.com".into(),
                environment: None,
            },
        ),
    ] {
        let budget = NewBudget {
            name: name.into(),
            scope,
            limit: MicroUsd::from_usd_str("1").unwrap(),
            window: BudgetWindow {
                kind: WindowKind::Month,
                calendar: true,
            },
        };
        budgets.push(store.create_budget(budget).await.unwrap());
    }

    let cache = AuthCache::load(&store).await.unwrap();
    cache.spawn_refresh(store.clone());
    let tallies = Tallies::new();
    let charger = Charger {
        cache,
        store: store.clone(),
        clock: Arc::new(|| NOW),
        charge_secret_sha256: sekisho_core::keys::hash_secret(CHARGE_SECRET),
        tallies: tallies.clone(),
        tally_wait,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(RateLimitServiceServer::new(charger))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let rls = RateLimitServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    Rig {
        rls,
        store,
        token,
        key: key.id,
        budgets,
        tallies,
    }
}

/// The install's charge secret, as the rendered descriptors carry it.
const CHARGE_SECRET: &str = "test-charge-secret";

/// One call as agentgateway sends it when a request on a Sekisho route
/// ends: a descriptor per measure it could evaluate, each with the route's
/// environment, the key id Sekisho's extAuthz hook verified
/// (`extauthz.key`), the charge secret, the requested model and the stream
/// hook's request ID.
fn req(key: &str, model: &str, request: &str, measures: &[(&str, u64)]) -> RateLimitRequest {
    let descriptor = |measure: &str, count: u64| RateLimitDescriptor {
        entries: [
            ("env", "data-team"),
            ("key", key),
            ("secret", CHARGE_SECRET),
            ("model", model),
            ("request", request),
            ("measure", measure),
        ]
        .into_iter()
        .map(|(key, value)| Entry {
            key: key.into(),
            value: value.into(),
        })
        .collect(),
        hits_addend: Some(count),
        ..Default::default()
    };
    RateLimitRequest {
        domain: "sekisho".into(),
        descriptors: measures
            .iter()
            .map(|&(measure, count)| descriptor(measure, count))
            .collect(),
        hits_addend: 0,
    }
}

/// A finished request on a model without an admin price, which is charged
/// agentgateway's catalog cost.
fn catalog(key: &str, cost: u64) -> RateLimitRequest {
    req(
        key,
        "claude-fable-5",
        "",
        &[("input", 1_000), ("output", 10), ("catalog", cost)],
    )
}

/// Changes every descriptor of `request`.
fn each(
    mut request: RateLimitRequest,
    change: impl Fn(&mut RateLimitDescriptor),
) -> RateLimitRequest {
    request.descriptors.iter_mut().for_each(change);
    request
}

/// agentgateway's request-side call: every costed measure 0, and the prompt
/// count on the descriptor without a cost.
fn start(key: &str, model: &str, request: &str, prompt: u64) -> RateLimitRequest {
    req(
        key,
        model,
        request,
        &[
            ("input", 0),
            ("cache_read", 0),
            ("cache_write", 0),
            ("output", 0),
            ("catalog", 0),
            ("prompt", prompt),
        ],
    )
}

/// A tally as the stream hook leaves it: status and answer tokens, not yet
/// finished.
fn tally(rig: &Rig, status: u16, answer_o200k: u64) -> String {
    let id = rig.tallies.open();
    rig.tallies.set_status(&id, status);
    rig.tallies.add(&id, answer_o200k);
    id
}

async fn spent(rig: &Rig) -> Vec<MicroUsd> {
    let now = NOW;
    let statuses = rig.store.status(&rig.budgets, now).await.unwrap();
    statuses.into_iter().map(|s| s.spent).collect()
}

/// Spend is recorded on its own task, after the answer: waits until the
/// three budgets read `want` each.
async fn spent_becomes(rig: &Rig, want: MicroUsd) {
    let start = tokio::time::Instant::now();
    loop {
        let now = spent(rig).await;
        if now == [want; 3] {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "spent {now:?}, expected {want:?} each"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The spend once a charge that should change nothing has had time to run:
/// well past the rig's 300 ms grace after a stream ends, so an estimate the
/// code wrongly charges is seen.
async fn settled(rig: &Rig) -> Vec<MicroUsd> {
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    spent(rig).await
}

/// A finished request without an admin price: agentgateway's catalog cost.
#[tokio::test]
async fn charge_records_exact_cost() {
    let mut rig = rig().await;
    let (store, budgets, now) = (&rig.store, rig.budgets.clone(), NOW);
    let resp = rig
        .rls
        .should_rate_limit(catalog(rig.key.as_str(), 4500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    // Every applicable budget is charged: environment, key and person.
    spent_becomes(&rig, MicroUsd(4500)).await;
    assert_eq!(
        store.status(&budgets, now).await.unwrap()[0].spent,
        MicroUsd(4500)
    );
    rig.rls
        .should_rate_limit(catalog(rig.key.as_str(), 500))
        .await
        .unwrap();
    spent_becomes(&rig, MicroUsd(5000)).await;
}

#[tokio::test]
async fn precheck_with_zero_cost_charges_nothing() {
    let mut rig = rig().await;
    let resp = rig
        .rls
        .should_rate_limit(req(
            rig.key.as_str(),
            "gpt-priced",
            "",
            &[
                ("input", 0),
                ("cache_read", 0),
                ("cache_write", 0),
                ("output", 0),
                ("catalog", 0),
            ],
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

#[tokio::test]
async fn request_level_hits_addend_is_the_fallback() {
    let mut rig = rig().await;
    let mut request = each(catalog(rig.key.as_str(), 0), |d| d.hits_addend = None);
    request.hits_addend = 1234;
    rig.rls.should_rate_limit(request).await.unwrap();
    spent_becomes(&rig, MicroUsd(1234)).await;
}

#[tokio::test]
async fn always_ok_but_charges_only_known_keys() {
    let mut rig = rig().await;
    let key = rig.key.as_str();
    let mut odd = vec![
        catalog("garbage", 4500),
        catalog("aaaaaaaaaaaa", 4500), // well-formed, unknown
        each(catalog(key, 4500), |d| d.entries.retain(|e| e.key != "key")),
        // Without the charge secret, or with a wrong one: not from our agentgateway.
        each(catalog(key, 4500), |d| {
            d.entries.retain(|e| e.key != "secret")
        }),
        each(catalog(key, 4500), |d| {
            d.entries[2].value = "guessed-secret".into()
        }),
        // One descriptor without it voids the whole call.
        {
            let mut request = catalog(key, 4500);
            request.descriptors[2].entries[2].value = "guessed-secret".into();
            request
        },
        // Plan 1's descriptor shape: a bearer token is not accepted as a key id.
        each(catalog(key, 4500), |d| {
            d.entries[1] = Entry {
                key: "credential".into(),
                value: format!("Bearer {}", rig.token),
            }
        }),
    ];
    odd.push(RateLimitRequest {
        domain: "sekisho".into(),
        descriptors: vec![],
        hits_addend: 4500,
    });
    for request in odd {
        let resp = rig
            .rls
            .should_rate_limit(request)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.overall_code, Code::Ok as i32);
    }
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

#[tokio::test]
async fn a_revoked_key_is_still_charged_for_its_last_request() {
    let mut rig = rig().await;
    rig.store.revoke_key(&rig.key).await.unwrap();
    rig.rls
        .should_rate_limit(catalog(rig.key.as_str(), 4500))
        .await
        .unwrap();
    spent_becomes(&rig, MicroUsd(4500)).await;
}

/// A brief database failure while charging must not lose the spend: the
/// charge is tried again. Here the spend table is missing for 50 ms.
#[tokio::test]
async fn a_brief_database_failure_does_not_lose_spend() {
    let mut rig = rig().await;
    sqlx::query("alter table spend rename to spend_away")
        .execute(rig.store.pool())
        .await
        .unwrap();
    let pool = rig.store.pool().clone();
    let restore = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        sqlx::query("alter table spend_away rename to spend")
            .execute(&pool)
            .await
            .unwrap();
    });
    let resp = rig
        .rls
        .should_rate_limit(catalog(rig.key.as_str(), 4500))
        .await
        .unwrap()
        .into_inner();
    restore.await.unwrap();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    spent_becomes(&rig, MicroUsd(4500)).await;
}

/// Each budget's spend is its own upsert, so a retry after one budget fails
/// must not charge the others again. A trigger fails the first write to the
/// budget with the highest id, which the store charges last.
#[tokio::test]
async fn a_retry_never_charges_a_budget_twice() {
    let mut rig = rig().await;
    let last = rig.budgets.iter().map(|b| b.id).max().unwrap();
    for sql in [
        "create sequence spend_failures".to_owned(),
        format!(
            "create function fail_once() returns trigger language plpgsql as $$ begin              if new.budget_id = '{last}' and nextval('spend_failures') = 1 then              raise exception 'injected failure'; end if; return new; end $$"
        ),
        "create trigger fail_once before insert or update on spend          for each row execute function fail_once()"
            .to_owned(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(rig.store.pool())
            .await
            .unwrap();
    }
    rig.rls
        .should_rate_limit(catalog(rig.key.as_str(), 4500))
        .await
        .unwrap();
    spent_becomes(&rig, MicroUsd(4500)).await;
    // The retries are over within a second; none charged a budget twice.
    assert_eq!(
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
            spent(&rig).await
        },
        [MicroUsd(4500); 3]
    );
}

/// agentgateway gives this call 10 s, then drops it (and the work it was
/// waiting for), so the answer must not wait for the database: spend is
/// recorded on its own task, after the answer. Here a lock on `spend` holds
/// every charge until the test releases it.
#[tokio::test]
async fn the_answer_never_waits_for_the_database() {
    let mut rig = rig().await;
    let mut held = rig.store.pool().begin().await.unwrap();
    sqlx::query("lock table spend in access exclusive mode")
        .execute(&mut *held)
        .await
        .unwrap();
    let answer = tokio::time::timeout(
        Duration::from_secs(2),
        rig.rls.should_rate_limit(catalog(rig.key.as_str(), 4500)),
    )
    .await
    .expect("an answer while the database is held")
    .unwrap()
    .into_inner();
    assert_eq!(answer.overall_code, Code::Ok as i32);
    held.rollback().await.unwrap();
    spent_becomes(&rig, MicroUsd(4500)).await;
}

#[tokio::test]
async fn store_failure_still_answers_ok() {
    let mut rig = rig().await;
    rig.store.pool().close().await;
    let resp = rig
        .rls
        .should_rate_limit(catalog(rig.key.as_str(), 4500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
}

#[tokio::test]
async fn a_complete_request_with_an_admin_price_is_priced_by_sekisho() {
    let mut rig = rig().await;
    let measures = [
        ("input", 10_000),
        ("cache_read", 8_000),
        ("cache_write", 1_000),
        ("output", 500),
        ("catalog", 999_999), // agentgateway's own price is ignored
    ];
    rig.rls
        .should_rate_limit(req(rig.key.as_str(), "claude-priced", "", &measures))
        .await
        .unwrap();
    // 1,000 uncached × 3 + 8,000 × 0.3 + 1,000 × 3.75 + 500 × 15 USD per
    // million (Anthropic's cache ratios).
    spent_becomes(&rig, MicroUsd(16_650)).await;
    assert_eq!(settled(&rig).await, [MicroUsd(16_650); 3]);
}

#[tokio::test]
async fn a_complete_request_without_an_admin_price_is_charged_the_catalog_cost() {
    let mut rig = rig().await;
    rig.rls
        .should_rate_limit(catalog(rig.key.as_str(), 4_500))
        .await
        .unwrap();
    spent_becomes(&rig, MicroUsd(4_500)).await;
    assert_eq!(settled(&rig).await, [MicroUsd(4_500); 3]);
}

#[tokio::test]
async fn an_unpriced_request_is_not_charged() {
    let mut rig = rig().await;
    let key = rig.key.as_str();
    // Finished, with neither an admin price nor a catalog cost.
    for model in ["claude-fable-5", "not-a-sekisho-model"] {
        rig.rls
            .should_rate_limit(req(key, model, "", &[("input", 1_000), ("output", 10)]))
            .await
            .unwrap();
    }
    // Ended early on a model with no price anywhere.
    let id = tally(&rig, 200, 10);
    rig.rls
        .should_rate_limit(start(key, "not-a-sekisho-model", &id, 1_000))
        .await
        .unwrap();
    rig.tallies.finish(&id);
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

/// agentgateway reports only the start of a stream that ends early: Sekisho
/// estimates it once the stream hook's tally ends and no report followed.
#[tokio::test]
async fn a_stream_that_ends_early_is_charged_an_estimate() {
    let mut rig = rig().await;
    let id = tally(&rig, 200, 40);
    rig.rls
        .should_rate_limit(start(rig.key.as_str(), "gpt-priced", &id, 1_000))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        spent(&rig).await,
        [MicroUsd(0); 3],
        "waits for the stream's end"
    );
    rig.tallies.finish(&id);
    // 1,000 × 3 + 40 × 15 USD per million.
    spent_becomes(&rig, MicroUsd(3_600)).await;
    assert_eq!(settled(&rig).await, [MicroUsd(3_600); 3]);
}

/// A stream that ends early on `model`, with a 1,000-token prompt and
/// `answer_o200k` tokens tallied. Returns its request ID.
async fn ended_early(rig: &mut Rig, model: &str, status: u16, answer_o200k: u64) -> String {
    let id = tally(rig, status, answer_o200k);
    rig.rls
        .should_rate_limit(start(rig.key.as_str(), model, &id, 1_000))
        .await
        .unwrap();
    rig.tallies.finish(&id);
    id
}

#[tokio::test]
async fn a_claude_estimate_scales_the_answer() {
    let mut rig = rig().await;
    ended_early(&mut rig, "claude-priced", 200, 100).await;
    // 1,000 × 3 + 130 × 15: a Claude model without a version counts 1.3
    // times o200k.
    spent_becomes(&rig, MicroUsd(4_950)).await;
}

#[tokio::test]
async fn an_estimate_without_an_admin_price_uses_the_catalog_copy() {
    let mut rig = rig().await;
    ended_early(&mut rig, "claude-fable-5", 200, 10).await;
    // 1,000 × 10 + 16 × 50 at the catalog's prices: Claude 5 counts 1.6
    // times o200k.
    spent_becomes(&rig, MicroUsd(10_800)).await;
}

#[tokio::test]
async fn a_stream_cut_before_any_text_is_charged_its_prompt() {
    let mut rig = rig().await;
    ended_early(&mut rig, "gpt-priced", 200, 0).await;
    spent_becomes(&rig, MicroUsd(3_000)).await;
    assert_eq!(settled(&rig).await, [MicroUsd(3_000); 3]);
}

#[tokio::test]
async fn an_error_answer_is_not_charged() {
    let mut rig = rig().await;
    for status in [429, 500] {
        ended_early(&mut rig, "gpt-priced", status, 5).await;
    }
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

/// A response-side call without the output count for a request whose tally
/// isn't here (another Sekisho's stream hook, or none) charges the prompt at
/// once: there is no stream to wait for.
#[tokio::test]
async fn a_missing_tally_charges_the_prompt() {
    let mut rig = rig_waiting(Duration::from_secs(5)).await;
    let key = rig.key.as_str();
    let begun = tokio::time::Instant::now();
    for request in ["0123456789abcdef0123456789abcdef", ""] {
        rig.rls
            .should_rate_limit(req(key, "gpt-priced", request, &[("input", 1_000)]))
            .await
            .unwrap();
    }
    spent_becomes(&rig, MicroUsd(6_000)).await;
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "not after the wait"
    );
    // agentgateway's request-side call for such a request charges nothing.
    rig.rls
        .should_rate_limit(start(
            key,
            "gpt-priced",
            "0123456789abcdef0123456789abcdef",
            1_000,
        ))
        .await
        .unwrap();
    assert_eq!(settled(&rig).await, [MicroUsd(6_000); 3]);
}

#[tokio::test]
async fn a_complete_request_drops_its_tally() {
    let mut rig = rig().await;
    let key = rig.key.as_str();
    let id = tally(&rig, 200, 999);
    rig.rls
        .should_rate_limit(start(key, "gpt-priced", &id, 1_000))
        .await
        .unwrap();
    rig.rls
        .should_rate_limit(req(
            key,
            "gpt-priced",
            &id,
            &[("input", 1_000), ("output", 10)],
        ))
        .await
        .unwrap();
    // The provider's counts, not the tally: 1,000 × 3 + 10 × 15.
    spent_becomes(&rig, MicroUsd(3_150)).await;
    rig.tallies.finish(&id); // the stream ends after its report
    assert_eq!(rig.tallies.wait(&id, Duration::ZERO).await, None);
    assert_eq!(settled(&rig).await, [MicroUsd(3_150); 3]);
}

#[tokio::test]
async fn a_late_report_is_not_charged_twice() {
    let mut rig = rig().await;
    let id = ended_early(&mut rig, "gpt-priced", 200, 40).await;
    spent_becomes(&rig, MicroUsd(3_600)).await;
    // agentgateway's report arrives after Sekisho estimated the request.
    rig.rls
        .should_rate_limit(req(
            rig.key.as_str(),
            "gpt-priced",
            &id,
            &[("input", 1_000), ("output", 40)],
        ))
        .await
        .unwrap();
    assert_eq!(settled(&rig).await, [MicroUsd(3_600); 3]);
}

#[tokio::test]
async fn a_report_without_output_is_estimated_once_the_stream_ends() {
    let mut rig = rig().await;
    let key = rig.key.as_str();
    let id = tally(&rig, 200, 40);
    rig.rls
        .should_rate_limit(start(key, "gpt-priced", &id, 900))
        .await
        .unwrap();
    // A response-side call without the output count: agentgateway's input
    // count replaces the prompt count.
    rig.rls
        .should_rate_limit(req(key, "gpt-priced", &id, &[("input", 1_000)]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        spent(&rig).await,
        [MicroUsd(0); 3],
        "waits for the stream's end"
    );
    rig.tallies.finish(&id);
    // 1,000 × 3 + 40 × 15, once.
    spent_becomes(&rig, MicroUsd(3_600)).await;
    assert_eq!(settled(&rig).await, [MicroUsd(3_600); 3]);
}

/// agentgateway answers with its own error when it can't reach the provider,
/// and ext_proc never sees that answer: no status, nothing reached the
/// client, and the provider billed nothing.
#[tokio::test]
async fn an_answer_that_never_started_is_not_charged() {
    let mut rig = rig().await;
    let id = rig.tallies.open(); // no answer headers
    rig.rls
        .should_rate_limit(start(rig.key.as_str(), "gpt-priced", &id, 1_000))
        .await
        .unwrap();
    rig.tallies.finish(&id);
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

/// The provider's own usage events in the stream win over agentgateway's
/// prompt count: cached prompts are priced as cached.
#[tokio::test]
async fn an_estimate_uses_the_counts_the_provider_streamed() {
    let mut rig = rig().await;
    let key = rig.key.as_str();
    let prompt = Some(Usage {
        input: 100_000,
        cache_read: 95_000,
        cache_write: 0,
        output: 0,
    });
    // Cut before the end: the prompt's counts, and the tallied answer.
    let id = tally(&rig, 200, 40);
    rig.tallies.set_usage(
        &id,
        ProviderUsage {
            prompt,
            output: None,
        },
    );
    rig.rls
        .should_rate_limit(start(key, "claude-priced", &id, 100_000))
        .await
        .unwrap();
    rig.tallies.finish(&id);
    // 5,000 × 3 + 95,000 × 0.3 + 52 × 15 (40 tallied, ×1.3 for Claude).
    spent_becomes(&rig, MicroUsd(44_280)).await;
    // The provider's final count got through: it replaces the tally.
    let id = tally(&rig, 200, 40);
    rig.tallies.set_usage(
        &id,
        ProviderUsage {
            prompt,
            output: Some(60),
        },
    );
    rig.rls
        .should_rate_limit(start(key, "claude-priced", &id, 100_000))
        .await
        .unwrap();
    rig.tallies.finish(&id);
    // + 5,000 × 3 + 95,000 × 0.3 + 60 × 15.
    spent_becomes(&rig, MicroUsd(44_280 + 44_400)).await;
}
