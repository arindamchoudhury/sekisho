use std::sync::{Arc, LazyLock};
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_hooks::authz::{HooksState, router};
use sekisho_hooks::cache::AuthCache;
use sekisho_store::{BudgetScope, EnvironmentSpec, NewBudget, Store};
use sekisho_testkit::TestStore;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::{Instant, sleep};

/// The hook's clock. Keys may expire exactly now.
/// The hook's clock: a day after the real now (whole seconds), so a key can
/// be created to expire exactly at it. Fixed for the whole test run.
static NOW: LazyLock<OffsetDateTime> = LazyLock::new(|| {
    (OffsetDateTime::now_utc() + time::Duration::DAY)
        .replace_nanosecond(0)
        .unwrap()
});
const PATH: &str = "/env/data-team/openai/v1/chat/completions";

struct Hook {
    base: String,
    http: reqwest::Client,
    store: TestStore,
    cache: Arc<AuthCache>,
    /// alice's active key in data-team.
    token: String,
    key: EnvKeyId,
    /// A key for ml-team.
    other_env_token: String,
    /// A data-team key that expires at `NOW`.
    expiring_now_token: String,
}

struct Reply {
    status: u16,
    json: Value,
    headers: HeaderMap,
}

async fn env(store: &Store, name: &str) -> EnvName {
    let spec = EnvironmentSpec {
        name: EnvName::parse(name).unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: Vec::new(),
    };
    store.upsert_environment(spec).await.unwrap().spec.name
}

async fn hook() -> Hook {
    let (store, _) = sekisho_testkit::store().await;
    let data = env(&store, "data-team").await;
    let ml = env(&store, "ml-team").await;
    let (key, token) = store
        .create_key(&data, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (_, other_env_token) = store
        .create_key(&ml, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (_, expiring_now_token) = store
        .create_key(&data, "bob@example.com", "short", Some(*NOW))
        .await
        .unwrap();

    let cache = AuthCache::load(&store).await.unwrap();
    cache.spawn_refresh(store.clone());
    let state = HooksState {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::new(|| *NOW),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Hook {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        store,
        cache,
        token,
        key: key.id,
        other_env_token,
        expiring_now_token,
    }
}

/// The environment of the route a well-formed `/env/<name>/…` path matches.
fn route_env(path: &str) -> Option<&str> {
    path.strip_prefix("/env/")?.split('/').next()
}

/// Calls the hook as agentgateway's route for the path does: the client's
/// method and path, an empty body, the Authorization header, and the
/// route's `x-sekisho-env`.
async fn call_with(app: &Hook, method: &str, path: &str, authorization: &[HeaderValue]) -> Reply {
    let env: Vec<HeaderValue> = route_env(path)
        .map(|e| HeaderValue::from_str(e).unwrap())
        .into_iter()
        .collect();
    call_route(app, method, path, &env, authorization).await
}

/// Calls the hook with explicit `x-sekisho-env` values.
async fn call_route(
    app: &Hook,
    method: &str,
    path: &str,
    env: &[HeaderValue],
    authorization: &[HeaderValue],
) -> Reply {
    let mut headers = HeaderMap::new();
    for value in authorization {
        headers.append(AUTHORIZATION, value.clone());
    }
    for value in env {
        headers.append("x-sekisho-env", value.clone());
    }
    let resp = app
        .http
        .request(method.parse().unwrap(), format!("{}{path}", app.base))
        .headers(headers)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap();
    Reply {
        status,
        json: serde_json::from_str(&text).unwrap_or(Value::Null),
        headers,
    }
}

async fn call(app: &Hook, path: &str, header: Option<&str>) -> Reply {
    let values: Vec<HeaderValue> = header
        .map(|h| HeaderValue::from_str(h).unwrap())
        .into_iter()
        .collect();
    call_with(app, "POST", path, &values).await
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{what} not cached within 1 s"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn authz_allows_active_key() {
    let app = hook().await;
    let r = call(&app, PATH, Some(&bearer(&app.token))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["x-sekisho-key"], app.key.as_str());
    assert_eq!(r.headers["x-sekisho-person"], "alice@example.com");
    assert_eq!(r.headers["x-sekisho-env"], "data-team");
    // Any method and any path under the environment, and a lowercase scheme.
    for (method, path) in [
        ("GET", "/env/data-team/openai/v1/models"),
        ("POST", "/env/data-team"),
        ("PUT", "/env/data-team/x?y=1"),
    ] {
        let value = HeaderValue::from_str(&format!("bearer {}", app.token)).unwrap();
        assert_eq!(
            call_with(&app, method, path, &[value]).await.status,
            200,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn authz_rejects_malformed_and_foreign_credentials() {
    let app = hook().await;
    let other_env_token = bearer(&app.other_env_token);
    let expiring_now_token = bearer(&app.expiring_now_token);
    for (hdr, status) in [
        (None, 401),
        (Some("Basic abc"), 401),
        (Some("Bearer garbage"), 401),
        (Some(other_env_token.as_str()), 403),
        (Some(expiring_now_token.as_str()), 403),
    ] {
        let r = call(&app, PATH, hdr).await;
        assert_eq!(r.status, status, "{hdr:?}");
        assert!(r.json["error"].is_string());
    }
    assert_eq!(
        call(&app, PATH, Some(&other_env_token)).await.json["reason"],
        "wrong_environment"
    );
    assert_eq!(
        call(&app, PATH, Some(&expiring_now_token)).await.json["reason"],
        "expired"
    );

    // More odd headers: always a JSON 401 or 403, never 200 or 500.
    let (id, _) = sekisho_core::keys::parse_token(&app.token).unwrap();
    let wrong_secret = format!("Bearer sek_{id}_{}", "A".repeat(43));
    let unknown_key = format!("Bearer sek_aaaaaaaaaaaa_{}", "A".repeat(43));
    let two = [
        HeaderValue::from_str(&bearer(&app.token)).unwrap(),
        HeaderValue::from_str(&bearer(&app.token)).unwrap(),
    ];
    let cases: Vec<(Vec<HeaderValue>, u16, Option<&str>)> = vec![
        (vec![HeaderValue::from_static("Bearer")], 401, None),
        (vec![HeaderValue::from_static("Bearer ")], 401, None),
        (vec![HeaderValue::from_str(&app.token).unwrap()], 401, None), // no scheme
        (
            vec![HeaderValue::from_bytes(b"Bearer sek_\xff").unwrap()],
            401,
            None,
        ),
        (two.to_vec(), 401, None),
        (
            vec![HeaderValue::from_str(&wrong_secret).unwrap()],
            403,
            Some("unknown_key"),
        ),
        (
            vec![HeaderValue::from_str(&unknown_key).unwrap()],
            403,
            Some("unknown_key"),
        ),
    ];
    for (values, status, reason) in cases {
        let r = call_with(&app, "POST", PATH, &values).await;
        assert_eq!(r.status, status, "{values:?}");
        assert!(r.json["error"].is_string(), "{values:?}");
        if let Some(reason) = reason {
            assert_eq!(r.json["reason"], reason, "{values:?}");
        }
    }
    // The right key on a route of no or another environment.
    for path in [
        "/v1/chat/completions",
        "/env/ml-team/openai/v1/chat/completions",
        "/env/Not_An_Env/openai",
        "/env/ghost-team/openai",
    ] {
        let r = call(&app, path, Some(&bearer(&app.token))).await;
        assert_eq!(
            (r.status, &r.json["reason"]),
            (403, &Value::from("wrong_environment")),
            "{path}"
        );
    }
}

#[tokio::test]
async fn suspended_and_revoked_keys_are_refused() {
    let app = hook().await;
    let token = bearer(&app.token);
    app.store.set_suspended(&app.key, true).await.unwrap();
    wait_for("suspension", || app.cache.key(&app.key).unwrap().suspended).await;
    let r = call(&app, PATH, Some(&token)).await;
    assert_eq!(
        (r.status, &r.json["reason"]),
        (403, &Value::from("suspended"))
    );
    app.store.set_suspended(&app.key, false).await.unwrap();
    wait_for("resumption", || !app.cache.key(&app.key).unwrap().suspended).await;
    assert_eq!(call(&app, PATH, Some(&token)).await.status, 200);
}

#[tokio::test]
async fn revoked_key_is_403_reason_revoked() {
    let app = hook().await;
    app.store.revoke_key(&app.key).await.unwrap();
    wait_for("revocation", || app.cache.key(&app.key).unwrap().revoked).await;
    let r = call(&app, PATH, Some(&bearer(&app.token))).await;
    assert_eq!(r.status, 403);
    assert_eq!(
        (&r.json["error"], &r.json["reason"]),
        (&Value::from("forbidden"), &Value::from("revoked"))
    );
}

async fn budget(store: &Store, name: &str, scope: BudgetScope, usd: &str) -> uuid::Uuid {
    store
        .create_budget(NewBudget {
            name: name.into(),
            scope,
            limit: MicroUsd::from_usd_str(usd).unwrap(),
            window: BudgetWindow {
                kind: WindowKind::Month,
                calendar: true,
            },
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn budget_exhausted_is_429_with_message() {
    let app = hook().await;
    let path = PATH;
    let token = bearer(&app.token);
    let data = EnvName::parse("data-team").unwrap();
    // environment budget $0.01, pre-charged 13500 micro-USD
    let team = budget(
        &app.store,
        "team",
        BudgetScope::Environment(data.clone()),
        "0.01",
    )
    .await;
    assert_eq!(call(&app, path, Some(&token)).await.status, 200); // not used up yet
    app.store
        .charge(&[team], MicroUsd(13_500), *NOW)
        .await
        .unwrap();
    let r = call(&app, path, Some(&token)).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json["error"], "budget_exhausted");
    // The calendar month after NOW starts at UTC midnight on the 1st.
    let month = BudgetWindow {
        kind: WindowKind::Month,
        calendar: true,
    };
    let resets_at = month.current(*NOW, *NOW).1.format(&Rfc3339).unwrap();
    assert!(resets_at.ends_with("-01T00:00:00Z"), "{resets_at}");
    assert_eq!(
        (&r.json["budget"], &r.json["resets_at"]),
        (&Value::from("team"), &Value::from(resets_at.as_str()))
    );
    assert!(
        r.json["message"]
            .as_str()
            .unwrap()
            .contains(&format!("until {resets_at}"))
    );
    assert_eq!(
        r.json["message"],
        format!("Budget 'team' is used up until {resets_at}.")
    );
}

#[tokio::test]
async fn only_applicable_budgets_refuse() {
    let app = hook().await;
    let token = bearer(&app.token);
    let ml = EnvName::parse("ml-team").unwrap();
    // Used-up budgets of someone else and of another environment do not apply.
    let other_person = budget(
        &app.store,
        "bob",
        BudgetScope::Person {
            person: "bob@example.com".into(),
            environment: None,
        },
        "0",
    )
    .await;
    let other_env = budget(&app.store, "ml", BudgetScope::Environment(ml.clone()), "0").await;
    let alice_in_ml = budget(
        &app.store,
        "alice-ml",
        BudgetScope::Person {
            person: "alice@example.com".into(),
            environment: Some(ml),
        },
        "0",
    )
    .await;
    let _ = (other_person, other_env, alice_in_ml);
    assert_eq!(call(&app, PATH, Some(&token)).await.status, 200);
    // alice's own budget across environments does.
    budget(
        &app.store,
        "alice",
        BudgetScope::Person {
            person: "alice@example.com".into(),
            environment: None,
        },
        "0",
    )
    .await;
    let r = call(&app, PATH, Some(&token)).await;
    assert_eq!((r.status, &r.json["budget"]), (429, &Value::from("alice")));
    // So does a budget on the key itself.
    let app2 = hook().await;
    budget(
        &app2.store,
        "ci-key",
        BudgetScope::Key(app2.key.clone()),
        "0",
    )
    .await;
    let r = call(&app2, PATH, Some(&bearer(&app2.token))).await;
    assert_eq!((r.status, &r.json["budget"]), (429, &Value::from("ci-key")));
}

#[tokio::test]
async fn budget_check_failure_fails_closed() {
    let app = hook().await;
    app.store.pool().close().await;
    let r = call(&app, PATH, Some(&bearer(&app.token))).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json["error"], "unavailable");
}

/// agentgateway may match routes on a normalised path while forwarding the
/// original one, so the hook trusts the environment the route adds
/// (`x-sekisho-env`), never the path.
#[tokio::test]
async fn environment_comes_from_the_route_not_the_path() {
    let app = hook().await;
    let key = [HeaderValue::from_str(&bearer(&app.token)).unwrap()];
    let data = [HeaderValue::from_static("data-team")];
    let ml = [HeaderValue::from_static("ml-team")];
    // data-team's route, whatever the path says.
    let r = call_route(
        &app,
        "POST",
        "/env/ml-team/../data-team/openai/v1/chat/completions",
        &data,
        &key,
    )
    .await;
    assert_eq!(r.status, 200);
    // ml-team's route with a path that claims data-team.
    let r = call_route(&app, "POST", PATH, &ml, &key).await;
    assert_eq!(
        (r.status, &r.json["reason"]),
        (403, &Value::from("wrong_environment"))
    );
    // No route environment, or two of them: refused.
    for env in [
        &[][..],
        &[data[0].clone(), ml[0].clone()][..],
        &[data[0].clone(), data[0].clone()][..],
    ] {
        let r = call_route(&app, "POST", PATH, env, &key).await;
        assert_eq!(
            (r.status, &r.json["reason"]),
            (403, &Value::from("wrong_environment")),
            "{env:?}"
        );
    }
}
