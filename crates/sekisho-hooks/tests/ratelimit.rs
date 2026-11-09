use std::sync::Arc;

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

/// A descriptor as agentgateway sends it for a Sekisho route.
fn req(env: &str, token: &str, cost: u64) -> RateLimitRequest {
    RateLimitRequest {
        domain: "sekisho".into(),
        descriptors: vec![RateLimitDescriptor {
            entries: vec![
                Entry {
                    key: "env".into(),
                    value: env.into(),
                },
                Entry {
                    key: "credential".into(),
                    value: format!("Bearer {token}"),
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

#[tokio::test]
async fn charge_records_exact_cost() {
    let mut rig = rig().await;
    let (store, budgets, now) = (&rig.store, rig.budgets.clone(), NOW);
    let resp = rig
        .rls
        .should_rate_limit(req("data-team", &rig.token, 4500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    assert_eq!(
        store.status(&budgets, now).await.unwrap()[0].spent,
        MicroUsd(4500)
    );
    // Every applicable budget is charged: environment, key and person.
    assert_eq!(spent(&rig).await, [MicroUsd(4500); 3]);
    rig.rls
        .should_rate_limit(req("data-team", &rig.token, 500))
        .await
        .unwrap();
    assert_eq!(spent(&rig).await, [MicroUsd(5000); 3]);
}

#[tokio::test]
async fn precheck_with_zero_cost_charges_nothing() {
    let mut rig = rig().await;
    let resp = rig
        .rls
        .should_rate_limit(req("data-team", &rig.token, 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
    assert_eq!(spent(&rig).await, [MicroUsd(0); 3]);
}

#[tokio::test]
async fn request_level_hits_addend_is_the_fallback() {
    let mut rig = rig().await;
    let mut request = req("data-team", &rig.token, 0);
    request.descriptors[0].hits_addend = None;
    request.hits_addend = 1234;
    rig.rls.should_rate_limit(request).await.unwrap();
    assert_eq!(spent(&rig).await, [MicroUsd(1234); 3]);
}

#[tokio::test]
async fn always_ok_but_charges_only_known_keys() {
    let mut rig = rig().await;
    let (id, _) = sekisho_core::keys::parse_token(&rig.token).unwrap();
    let wrong_secret = format!("sek_{id}_{}", "A".repeat(43));
    let mut odd = vec![
        req("data-team", "garbage", 4500),
        req("data-team", &wrong_secret, 4500),
        req(
            "data-team",
            &format!("sek_aaaaaaaaaaaa_{}", "A".repeat(43)),
            4500,
        ),
    ];
    let mut no_credential = req("data-team", &rig.token, 4500);
    no_credential.descriptors[0]
        .entries
        .retain(|e| e.key != "credential");
    odd.push(no_credential);
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
    assert_eq!(spent(&rig).await, [MicroUsd(0); 3]);
}

#[tokio::test]
async fn a_revoked_key_is_still_charged_for_its_last_request() {
    let mut rig = rig().await;
    rig.store.revoke_key(&rig.key).await.unwrap();
    rig.rls
        .should_rate_limit(req("data-team", &rig.token, 4500))
        .await
        .unwrap();
    assert_eq!(spent(&rig).await, [MicroUsd(4500); 3]);
}

#[tokio::test]
async fn store_failure_still_answers_ok() {
    let mut rig = rig().await;
    rig.store.pool().close().await;
    let resp = rig
        .rls
        .should_rate_limit(req("data-team", &rig.token, 4500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.overall_code, Code::Ok as i32);
}
