use std::sync::Arc;
use std::time::Duration;

use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::ratelimit::Charger;
use sekisho_proto::ratelimit::RateLimitDescriptor;
use sekisho_proto::ratelimit::RateLimitRequest;
use sekisho_proto::ratelimit::rate_limit_descriptor::Entry;
use sekisho_proto::ratelimit::rate_limit_response::Code;
use sekisho_proto::ratelimit::rate_limit_service_client::RateLimitServiceClient;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitServiceServer;
use sekisho_store::{Budget, BudgetScope, EnvironmentSpec, NewBudget};
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
}

async fn rig() -> Rig {
    let (store, _) = sekisho_testkit::store().await;
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
    let charger = Charger {
        cache,
        store: store.clone(),
        clock: Arc::new(|| NOW),
        charge_secret_sha256: sekisho_core::keys::hash_secret(CHARGE_SECRET),
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
    }
}

/// The install's charge secret, as the rendered descriptors carry it.
const CHARGE_SECRET: &str = "test-charge-secret";

/// A descriptor as agentgateway sends it for a Sekisho route: the route's
/// environment, the key id Sekisho's extAuthz hook verified
/// (`extauthz.key`) and the charge secret.
fn req(env: &str, key: &str, cost: u64) -> RateLimitRequest {
    RateLimitRequest {
        domain: "sekisho".into(),
        descriptors: vec![RateLimitDescriptor {
            entries: vec![
                Entry {
                    key: "env".into(),
                    value: env.into(),
                },
                Entry {
                    key: "key".into(),
                    value: key.into(),
                },
                Entry {
                    key: "secret".into(),
                    value: CHARGE_SECRET.into(),
                },
            ],
            hits_addend: Some(cost),
            ..Default::default()
        }],
        hits_addend: 0,
    }
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

/// The spend once a charge that should change nothing has had time to run.
async fn settled(rig: &Rig) -> Vec<MicroUsd> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    spent(rig).await
}

#[tokio::test]
async fn charge_records_exact_cost() {
    let mut rig = rig().await;
    let (store, budgets, now) = (&rig.store, rig.budgets.clone(), NOW);
    let resp = rig
        .rls
        .should_rate_limit(req("data-team", rig.key.as_str(), 4500))
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
        .should_rate_limit(req("data-team", rig.key.as_str(), 500))
        .await
        .unwrap();
    spent_becomes(&rig, MicroUsd(5000)).await;
}

#[tokio::test]
async fn precheck_with_zero_cost_charges_nothing() {
    let mut rig = rig().await;
    let resp = rig
        .rls
        .should_rate_limit(req("data-team", rig.key.as_str(), 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    assert_eq!(settled(&rig).await, [MicroUsd(0); 3]);
}

#[tokio::test]
async fn request_level_hits_addend_is_the_fallback() {
    let mut rig = rig().await;
    let mut request = req("data-team", rig.key.as_str(), 0);
    request.descriptors[0].hits_addend = None;
    request.hits_addend = 1234;
    rig.rls.should_rate_limit(request).await.unwrap();
    spent_becomes(&rig, MicroUsd(1234)).await;
}

#[tokio::test]
async fn always_ok_but_charges_only_known_keys() {
    let mut rig = rig().await;
    let mut odd = vec![
        req("data-team", "garbage", 4500),
        req("data-team", "aaaaaaaaaaaa", 4500), // well-formed, unknown
    ];
    let mut no_key = req("data-team", rig.key.as_str(), 4500);
    no_key.descriptors[0].entries.retain(|e| e.key != "key");
    odd.push(no_key);
    // Without the charge secret, or with a wrong one: not from our agentgateway.
    let mut no_secret = req("data-team", rig.key.as_str(), 4500);
    no_secret.descriptors[0]
        .entries
        .retain(|e| e.key != "secret");
    odd.push(no_secret);
    let mut wrong_secret = req("data-team", rig.key.as_str(), 4500);
    wrong_secret.descriptors[0].entries[2].value = "guessed-secret".into();
    odd.push(wrong_secret);
    // The old descriptor shape: a bearer token is not accepted as a key id.
    let mut credential = req("data-team", rig.key.as_str(), 4500);
    credential.descriptors[0].entries[1] = Entry {
        key: "credential".into(),
        value: format!("Bearer {}", rig.token),
    };
    odd.push(credential);
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
        .should_rate_limit(req("data-team", rig.key.as_str(), 4500))
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
        .should_rate_limit(req("data-team", rig.key.as_str(), 4500))
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
        .should_rate_limit(req("data-team", rig.key.as_str(), 4500))
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
        rig.rls
            .should_rate_limit(req("data-team", rig.key.as_str(), 4500)),
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
        .should_rate_limit(req("data-team", rig.key.as_str(), 4500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
}
