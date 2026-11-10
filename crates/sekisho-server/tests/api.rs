use std::time::{Duration, Instant};

use sekisho_core::money::MicroUsd;
use sekisho_gateway::{HookTargets, RenderOptions};
use sekisho_server::app::{AppState, Settings, router, start_publisher};
use sekisho_testkit::TestStore;
use serde_json::{Value, json};
use uuid::Uuid;

const BOOTSTRAP: &str = "bootstrap-token-0123456789";
const GATEWAY: &str = "gateway-token-0123456789";

struct TestApp {
    base: String,
    http: reqwest::Client,
    store: TestStore,
}

struct Reply {
    status: u16,
    body: Value,
    text: String,
}

async fn app() -> TestApp {
    let (store, secret_box) = sekisho_testkit::store().await;
    let settings = Settings {
        bootstrap_token: BOOTSTRAP.into(),
        gateway_token: GATEWAY.into(),
        hooks: HookTargets {
            authz_http: "sekisho:9080".into(),
            grpc: "sekisho:9081".into(),
            charge_secret: "test-charge-secret".into(),
        },
        render: RenderOptions::default(),
        long_poll: Duration::from_secs(1),
        republish: Duration::from_millis(300),
    };
    let state = AppState::new(store.clone(), secret_box, settings);
    start_publisher(&state).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    TestApp {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        store,
    }
}

impl TestApp {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut req = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        Reply { status, body, text }
    }

    async fn admin(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.call(method, path, Some(BOOTSTRAP), body).await
    }

    /// The newest published version, or 0. With `after`, long-polls past it.
    async fn version_after(&self, after: i64) -> Option<i64> {
        let r = self
            .call(
                "GET",
                &format!("/internal/gateway/config?after={after}"),
                Some(GATEWAY),
                None,
            )
            .await;
        match r.status {
            200 => Some(r.body["version"].as_i64().unwrap()),
            204 => None,
            other => panic!("unexpected {other}: {}", r.text),
        }
    }

    async fn latest_version(&self) -> i64 {
        self.version_after(0).await.unwrap_or(0)
    }

    /// The latest version once publishing has settled: earlier writes render
    /// in the background, so wait until a poll past the latest times out.
    async fn settled_version(&self) -> i64 {
        let mut latest = self.latest_version().await;
        while let Some(newer) = self.version_after(latest).await {
            latest = newer;
        }
        latest
    }

    async fn provider_and_model(&self) {
        let p = self
            .admin("POST", "/api/v1/providers", Some(json!({
                "name": "mock", "kind": "openai_compatible", "base_url": "http://mock:8000/v1", "api_key": "mock-secret-key",
                // `mock` doesn't resolve.
                "allow_private_network": true
            })))
            .await;
        assert_eq!(p.status, 201, "{}", p.text);
        let m = self
            .admin("POST", "/api/v1/models", Some(json!({
                "name": "mock-model", "provider": "mock", "input_price": "3", "output_price": "15"
            })))
            .await;
        assert_eq!(m.status, 201, "{}", m.text);
    }

    async fn put_env(&self, name: &str, models: &[&str]) -> Reply {
        self.admin(
            "PUT",
            &format!("/api/v1/environments/{name}"),
            Some(json!({ "models": models })),
        )
        .await
    }
}

#[tokio::test]
async fn admin_routes_need_bootstrap_token() {
    let app = app().await;
    for token in [None, Some("wrong-token-0123456789"), Some(GATEWAY)] {
        let r = app.call("GET", "/api/v1/environments", token, None).await;
        assert_eq!(r.status, 401, "{token:?}");
        assert_eq!(r.body["error"], "unauthorized");
    }
    let raw = app
        .http
        .get(format!("{}/api/v1/environments", app.base))
        .header("authorization", BOOTSTRAP)
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 401); // not Bearer
    let ok = app.admin("GET", "/api/v1/environments", None).await;
    assert_eq!((ok.status, ok.body.clone()), (200, json!([])));
}

#[tokio::test]
async fn gateway_config_needs_gateway_token() {
    let app = app().await;
    for token in [None, Some(BOOTSTRAP)] {
        let r = app
            .call("GET", "/internal/gateway/config", token, None)
            .await;
        assert_eq!(
            (r.status, &r.body["error"]),
            (401, &json!("unauthorized")),
            "{token:?}"
        );
    }
    // The publisher wrote a first config at start: no environments yet, still valid.
    let r = app
        .call(
            "GET",
            "/internal/gateway/config?after=0",
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        sekisho_gateway::validate(r.body["yaml"].as_str().unwrap()),
        Ok(())
    );
    let bad = app
        .call(
            "GET",
            "/internal/gateway/config?after=soon",
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(
        (bad.status, &bad.body["error"]),
        (400, &json!("bad_request"))
    );
}

#[tokio::test]
async fn provider_key_never_returned() {
    let app = app().await;
    let created = app
        .admin(
            "POST",
            "/api/v1/providers",
            Some(
                json!({ "name": "anthropic", "kind": "anthropic", "api_key": "sk-ant-secret-123" }),
            ),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.text);
    assert_eq!(
        (
            &created.body["name"],
            &created.body["kind"],
            &created.body["base_url"]
        ),
        (&json!("anthropic"), &json!("anthropic"), &Value::Null)
    );
    let listed = app.admin("GET", "/api/v1/providers", None).await;
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    for text in [&created.text, &listed.text] {
        assert!(
            !text.contains("api_key") && !text.contains("sk-ant-secret-123"),
            "{text}"
        );
    }
}

/// Rotating a provider's key republishes the gateway config with the new
/// key; the key is never echoed back.
#[tokio::test]
async fn provider_key_rotation_reaches_the_gateway() {
    let app = app().await;
    app.provider_and_model().await;
    assert_eq!(app.put_env("data-team", &["mock-model"]).await.status, 200);
    let settled = app.settled_version().await;

    let r = app
        .admin(
            "PATCH",
            "/api/v1/providers/mock",
            Some(json!({ "api_key": "rotated-key-456" })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        (&r.body["name"], &r.body["base_url"]),
        (&json!("mock"), &json!("http://mock:8000/v1"))
    );
    assert!(
        !r.text.contains("api_key") && !r.text.contains("rotated-key-456"),
        "{}",
        r.text
    );
    let c = app
        .call(
            "GET",
            &format!("/internal/gateway/config?after={settled}"),
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(c.status, 200, "no new version after the rotation");
    let yaml = c.body["yaml"].as_str().unwrap();
    assert!(yaml.contains("rotated-key-456") && !yaml.contains("mock-secret-key"));

    // A compatible provider keeps its URL; the kind and name never change.
    let r = app
        .admin(
            "PATCH",
            "/api/v1/providers/mock",
            Some(json!({ "base_url": null })),
        )
        .await;
    assert_eq!(
        (r.status, &r.body["fields"][0]["field"]),
        (422, &json!("base_url")),
        "{}",
        r.text
    );
    let r = app
        .admin(
            "PATCH",
            "/api/v1/providers/mock",
            Some(json!({ "kind": "openai" })),
        )
        .await;
    assert_eq!(
        (r.status, &r.body["error"]),
        (400, &json!("bad_request")),
        "{}",
        r.text
    );
    let r = app
        .admin(
            "PATCH",
            "/api/v1/providers/nope",
            Some(json!({ "api_key": "k" })),
        )
        .await;
    assert_eq!(r.status, 404, "{}", r.text);
}

#[tokio::test]
async fn validation_errors_are_422_with_fields() {
    let app = app().await;
    let r = app.put_env("data-team", &["gpt-5"]).await;
    assert_eq!(r.status, 422, "{}", r.text);
    assert_eq!(r.body["error"], "validation");
    assert_eq!(r.body["fields"][0]["field"], "models[0]");
    assert!(r.body["message"].is_string());

    let kind = app
        .admin(
            "POST",
            "/api/v1/providers",
            Some(json!({ "name": "x", "kind": "openAI", "api_key": "k" })),
        )
        .await;
    assert_eq!(
        (kind.status, &kind.body["fields"][0]["field"]),
        (422, &json!("kind"))
    );
    let name = app
        .admin(
            "POST",
            "/api/v1/providers",
            Some(json!({ "name": "Bad_Name", "kind": "openai", "api_key": "k" })),
        )
        .await;
    assert_eq!(
        (name.status, &name.body["fields"][0]["field"]),
        (422, &json!("name"))
    );
    let version = app
        .admin(
            "PUT",
            "/api/v1/environments/data-team",
            Some(json!({ "api_version": "sekisho/v2" })),
        )
        .await;
    assert_eq!(
        (version.status, &version.body["fields"][0]["field"]),
        (422, &json!("api_version"))
    );

    let malformed = app
        .http
        .post(format!("{}/api/v1/providers", app.base))
        .bearer_auth(BOOTSTRAP)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status().as_u16(), 400);
    let body: Value = malformed.json().await.unwrap();
    assert_eq!(body["error"], "bad_request");
    // A name that cannot exist is simply not found.
    let r = app
        .admin("GET", "/api/v1/environments/Not_A_Name", None)
        .await;
    assert_eq!((r.status, &r.body["error"]), (404, &json!("not_found")));
}

#[tokio::test]
async fn providers_models_and_environments_round_trip() {
    let app = app().await;
    app.provider_and_model().await;
    let dup = app
        .admin(
            "POST",
            "/api/v1/providers",
            Some(json!({ "name": "mock", "kind": "openai", "api_key": "k" })),
        )
        .await;
    assert_eq!((dup.status, &dup.body["error"]), (409, &json!("conflict")));

    let models = app.admin("GET", "/api/v1/models", None).await;
    assert_eq!(
        models.body,
        json!([{ "id": models.body[0]["id"], "name": "mock-model", "provider": "mock", "provider_kind": "openai_compatible", "input_price": "3", "output_price": "15" }])
    );

    let put = app
        .admin("PUT", "/api/v1/environments/data-team", Some(json!({
            "api_version": "sekisho/v1alpha1", "description": "Data team", "notice": "Be kind.", "access_groups": ["data"], "models": ["mock-model"]
        })))
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    let got = app
        .admin("GET", "/api/v1/environments/data-team", None)
        .await;
    assert_eq!(got.body, put.body);
    assert_eq!(
        (
            &got.body["api_version"],
            &got.body["name"],
            &got.body["models"],
            &got.body["access_groups"]
        ),
        (
            &json!("sekisho/v1alpha1"),
            &json!("data-team"),
            &json!(["mock-model"]),
            &json!(["data"])
        )
    );
    assert!(got.body["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        app.admin("GET", "/api/v1/environments", None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let in_use = app.admin("DELETE", "/api/v1/models/mock-model", None).await;
    assert_eq!(
        (in_use.status, &in_use.body["error"]),
        (409, &json!("in_use"))
    );
    assert!(
        in_use.body["message"]
            .as_str()
            .unwrap()
            .contains("data-team")
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/environments/data-team", None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/models/mock-model", None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/providers/mock", None)
            .await
            .status,
        204
    );
    for path in [
        "/api/v1/providers/mock",
        "/api/v1/models/mock-model",
        "/api/v1/environments/data-team",
    ] {
        let r = app.admin("DELETE", path, None).await;
        assert_eq!(
            (r.status, &r.body["error"]),
            (404, &json!("not_found")),
            "{path}"
        );
    }
}

#[tokio::test]
async fn key_token_shown_once() {
    let app = app().await;
    assert_eq!(app.put_env("data-team", &[]).await.status, 200);
    let created = app
        .admin(
            "POST",
            "/api/v1/environments/data-team/keys",
            Some(json!({ "owner": "alice@example.com", "name": "ci" })),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.text);
    let token = created.body["token"].as_str().unwrap().to_owned();
    let id = created.body["id"].as_str().unwrap().to_owned();
    assert!(token.starts_with(&format!("sek_{id}_")));
    assert_eq!(created.body["state"], "active");

    let listed = app
        .admin("GET", "/api/v1/environments/data-team/keys", None)
        .await;
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    assert!(listed.body[0].get("token").is_none());
    assert!(!listed.text.contains(&token));

    assert_eq!(
        app.admin("POST", &format!("/api/v1/keys/{id}/suspend"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("GET", "/api/v1/environments/data-team/keys", None)
            .await
            .body[0]["state"],
        "suspended"
    );
    assert_eq!(
        app.admin("POST", &format!("/api/v1/keys/{id}/resume"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("POST", &format!("/api/v1/keys/{id}/revoke"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("GET", "/api/v1/environments/data-team/keys", None)
            .await
            .body[0]["state"],
        "revoked"
    );
    let resume = app
        .admin("POST", &format!("/api/v1/keys/{id}/resume"), None)
        .await;
    assert_eq!(
        (resume.status, &resume.body["error"]),
        (409, &json!("conflict"))
    );
    assert_eq!(
        app.admin("POST", "/api/v1/keys/aaaaaaaaaaaa/revoke", None)
            .await
            .status,
        404
    );
    assert_eq!(
        app.admin("POST", &format!("/api/v1/keys/{id}/explode"), None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn budgets_and_status() {
    let app = app().await;
    assert_eq!(app.put_env("data-team", &[]).await.status, 200);
    let created = app
        .admin("POST", "/api/v1/budgets", Some(json!({
            "name": "team", "scope": "environment", "environment": "data-team", "limit": "0.01", "window": "month", "calendar": true
        })))
        .await;
    assert_eq!(created.status, 201, "{}", created.text);
    assert_eq!(
        (
            &created.body["scope"],
            &created.body["environment"],
            &created.body["limit"],
            &created.body["window"],
            &created.body["calendar"]
        ),
        (
            &json!("environment"),
            &json!("data-team"),
            &json!("0.01"),
            &json!("month"),
            &json!(true)
        )
    );
    let id: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        app.admin("GET", "/api/v1/budgets", None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let status = app
        .admin("GET", &format!("/api/v1/budgets/{id}/status"), None)
        .await;
    assert_eq!(
        (&status.body["spent"], &status.body["exhausted"]),
        (&json!("0"), &json!(false))
    );
    assert!(status.body["resets_at"].as_str().unwrap().ends_with('Z'));
    app.store
        .charge(&[id], MicroUsd(13_500), time::OffsetDateTime::now_utc())
        .await
        .unwrap();
    let status = app
        .admin("GET", &format!("/api/v1/budgets/{id}/status"), None)
        .await;
    assert_eq!(
        (
            &status.body["spent"],
            &status.body["exhausted"],
            &status.body["budget"]["name"]
        ),
        (&json!("0.0135"), &json!(true), &json!("team"))
    );

    for (body, field) in [
        (
            json!({ "name": "x", "scope": "key", "limit": "1", "window": "day", "calendar": true }),
            "key",
        ),
        (
            json!({ "name": "x", "scope": "person", "person": "alice", "limit": "lots", "window": "day", "calendar": true }),
            "limit",
        ),
        (
            json!({ "name": "x", "scope": "team", "limit": "1", "window": "day", "calendar": true }),
            "scope",
        ),
        (
            json!({ "name": "x", "scope": "person", "person": "alice", "limit": "1", "window": "year", "calendar": true }),
            "window",
        ),
    ] {
        let r = app.admin("POST", "/api/v1/budgets", Some(body)).await;
        assert_eq!(
            (r.status, &r.body["fields"][0]["field"]),
            (422, &json!(field)),
            "{}",
            r.text
        );
    }
    assert_eq!(
        app.admin("DELETE", &format!("/api/v1/budgets/{id}"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("GET", &format!("/api/v1/budgets/{id}/status"), None)
            .await
            .status,
        404
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/budgets/not-a-uuid", None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn config_version_bumps_on_env_change_only() {
    let app = app().await;
    app.provider_and_model().await;
    let v1 = app.settled_version().await;
    assert_eq!(app.put_env("data-team", &["mock-model"]).await.status, 200);
    // Long-poll past v1: the publisher renders in the background.
    assert!(
        app.version_after(v1)
            .await
            .expect("a new version within the poll")
            > v1
    );
    let v2 = app.settled_version().await;
    assert_eq!(
        app.admin(
            "POST",
            "/api/v1/environments/data-team/keys",
            Some(json!({ "owner": "alice@example.com", "name": "ci" }))
        )
        .await
        .status,
        201
    );
    assert_eq!(app.version_after(v2).await, None); // a key is not config
    assert_eq!(app.latest_version().await, v2);
}

#[tokio::test]
async fn long_poll_returns_204_when_unchanged() {
    let app = app().await;
    let latest = app.latest_version().await;
    let started = Instant::now();
    let r = app
        .call(
            "GET",
            &format!("/internal/gateway/config?after={latest}"),
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(5),
        "{waited:?}"
    );
}

#[tokio::test]
async fn long_poll_wakes_when_config_changes() {
    let app = app().await;
    app.provider_and_model().await;
    let latest = app.settled_version().await;
    let url = format!("{}/internal/gateway/config?after={latest}", app.base);
    let http = app.http.clone();
    let poll = tokio::spawn(async move {
        let started = Instant::now();
        let resp = http.get(url).bearer_auth(GATEWAY).send().await.unwrap();
        (
            resp.status().as_u16(),
            resp.json::<Value>().await.ok(),
            started.elapsed(),
        )
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(app.put_env("data-team", &["mock-model"]).await.status, 200);
    let (status, body, waited) = poll.await.unwrap();
    assert_eq!(status, 200);
    assert!(
        waited < Duration::from_secs(1),
        "woke after {waited:?}, not at the 1 s timeout"
    );
    let body = body.unwrap();
    assert!(body["version"].as_i64().unwrap() > latest);
    let yaml = body["yaml"].as_str().unwrap();
    assert!(yaml.contains("pathPrefix: /env/data-team/openai"), "{yaml}");
    assert!(yaml.contains("key: mock-secret-key"));
    assert_eq!(sekisho_gateway::validate(yaml), Ok(()));
}

/// A change whose notification was lost (or whose publish failed) still
/// reaches the gateway: the publisher also re-renders on a timer.
#[tokio::test]
async fn missed_changes_are_published_by_the_timer() {
    let app = app().await;
    app.provider_and_model().await;
    let settled = app.settled_version().await;
    // Written straight to the database: no notification is sent.
    let env: uuid::Uuid =
        sqlx::query_scalar("insert into environments (name) values ('quiet-team') returning id")
            .fetch_one(app.store.pool())
            .await
            .unwrap();
    sqlx::query("insert into environment_models (environment_id, model_id, position) select $1, id, 0 from models where name = 'mock-model'")
        .bind(env)
        .execute(app.store.pool())
        .await
        .unwrap();
    let r = app
        .call(
            "GET",
            &format!("/internal/gateway/config?after={settled}"),
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "no new version within the long poll");
    assert!(
        r.body["yaml"]
            .as_str()
            .unwrap()
            .contains("pathPrefix: /env/quiet-team/openai")
    );
}

/// An agent that applied a version this database never had (it was
/// re-created, so versions started again) gets the newest config at once
/// instead of waiting for a version above its own.
#[tokio::test]
async fn an_agent_ahead_of_the_database_gets_the_newest() {
    let app = app().await;
    app.provider_and_model().await;
    let settled = app.settled_version().await;
    let started = Instant::now();
    let r = app
        .call(
            "GET",
            &format!("/internal/gateway/config?after={}", settled + 100),
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.body["version"], settled);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "it long-polled"
    );
}

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const CLIENT: &str = "66666666-7777-8888-9999-000000000000";

impl TestApp {
    async fn post_provider(&self, body: Value) -> Reply {
        self.admin("POST", "/api/v1/providers", Some(body)).await
    }

    async fn patch_provider(&self, name: &str, body: Value) -> Reply {
        self.admin("PATCH", &format!("/api/v1/providers/{name}"), Some(body))
            .await
    }
}

/// The answer's status and its first field error.
fn first_field(r: &Reply) -> (u16, &Value) {
    (r.status, &r.body["fields"][0]["field"])
}

#[tokio::test]
async fn bedrock_and_azure_providers_round_trip() {
    let app = app().await;
    let bedrock = app
        .post_provider(json!({
            "name": "bedrock-claude", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "aws_access_key_id": "AKIAEXAMPLE0000001", "aws_secret_access_key": "aws-secret"
        }))
        .await;
    assert_eq!(bedrock.status, 201, "{}", bedrock.text);
    assert_eq!(
        (
            &bedrock.body["kind"],
            &bedrock.body["format"],
            &bedrock.body["credential"],
            &bedrock.body["region"],
            &bedrock.body["allow_private_network"],
            &bedrock.body["base_url"],
        ),
        (
            &json!("bedrock"),
            &json!("anthropic"),
            &json!("aws_access_keys"),
            &json!("us-east-1"),
            &json!(false),
            &Value::Null,
        )
    );
    assert!(bedrock.body.get("endpoint").is_none(), "{}", bedrock.text);

    let gpt = app
        .post_provider(json!({
            "name": "azure-gpt", "kind": "azure", "format": "openai",
            "endpoint": "https://my-resource.openai.azure.com/", "cloud_identity": true
        }))
        .await;
    assert_eq!(gpt.status, 201, "{}", gpt.text);
    assert_eq!(
        (
            &gpt.body["format"],
            &gpt.body["credential"],
            &gpt.body["endpoint"],
        ),
        (
            &json!("openai"),
            &json!("cloud_identity"),
            &json!("https://my-resource.openai.azure.com"), // canonical
        )
    );
    assert!(gpt.body.get("region").is_none(), "{}", gpt.text);

    let claude = app
        .post_provider(json!({
            "name": "azure-claude", "kind": "azure", "format": "anthropic",
            "endpoint": "https://my-foundry.services.ai.azure.com/api/projects/team",
            "azure_tenant_id": TENANT, "azure_client_id": CLIENT, "azure_client_secret": "sp-secret"
        }))
        .await;
    assert_eq!(claude.status, 201, "{}", claude.text);
    assert_eq!(
        (
            &claude.body["format"],
            &claude.body["credential"],
            &claude.body["endpoint"],
        ),
        (
            &json!("anthropic"),
            &json!("azure_client_secret"),
            &json!("https://my-foundry.services.ai.azure.com/api/projects/team"),
        )
    );

    // A single-format kind may name its format, or leave it out.
    let anthropic = app
        .post_provider(json!({
            "name": "anthropic", "kind": "anthropic", "format": "anthropic", "api_key": "sk-ant"
        }))
        .await;
    assert_eq!(anthropic.status, 201, "{}", anthropic.text);
    assert_eq!(
        (&anthropic.body["format"], &anthropic.body["credential"]),
        (&json!("anthropic"), &json!("key"))
    );

    let listed = app.admin("GET", "/api/v1/providers", None).await;
    assert_eq!(
        listed.body,
        json!([anthropic.body, claude.body, gpt.body, bedrock.body]), // sorted by name
        "{}",
        listed.text
    );
}

#[tokio::test]
async fn provider_credentials_and_formats_are_validated() {
    let app = app().await;
    for (body, field) in [
        (
            json!({ "name": "b", "kind": "bedrock", "region": "us-east-1", "api_key": "k" }),
            "format",
        ),
        (
            json!({ "name": "a", "kind": "azure", "endpoint": "https://r1.openai.azure.com", "api_key": "k" }),
            "format",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "gemini", "region": "us-east-1", "api_key": "k" }),
            "format",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "Anthropic", "region": "us-east-1", "api_key": "k" }),
            "format",
        ),
        // Foundry takes Claude API keys only in a header agentgateway can't send.
        (
            json!({ "name": "a", "kind": "azure", "format": "anthropic",
                    "endpoint": "https://my-foundry.services.ai.azure.com/api/projects/team", "api_key": "k" }),
            "credential",
        ),
        (json!({ "name": "a", "kind": "anthropic" }), "credential"),
        (
            json!({ "name": "a", "kind": "anthropic", "api_key": "k", "cloud_identity": true }),
            "credential",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
                    "api_key": "k", "aws_access_key_id": "AKIAEXAMPLE0000001", "aws_secret_access_key": "s" }),
            "credential",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
                    "aws_access_key_id": "AKIAEXAMPLE0000001" }),
            "aws_secret_access_key",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
                    "aws_session_token": "t" }),
            "aws_access_key_id",
        ),
        (
            json!({ "name": "a", "kind": "azure", "format": "openai",
                    "endpoint": "https://r1.openai.azure.com", "azure_tenant_id": TENANT }),
            "azure_client_id",
        ),
        (
            json!({ "name": "a", "kind": "azure", "format": "openai",
                    "endpoint": "https://evil.com/r1.openai.azure.com", "api_key": "k" }),
            "endpoint",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "anthropic", "api_key": "k" }),
            "region",
        ),
        (
            json!({ "name": "b", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
                    "endpoint": "https://r1.openai.azure.com", "api_key": "k" }),
            "endpoint",
        ),
        (
            json!({ "name": "a", "kind": "anthropic", "region": "us-east-1", "api_key": "k" }),
            "region",
        ),
    ] {
        let r = app.post_provider(body.clone()).await;
        assert_eq!(first_field(&r), (422, &json!(field)), "{body}: {}", r.text);
    }

    // One answer lists every field error the request has.
    let r = app
        .post_provider(json!({ "name": "Bad_Name", "kind": "bedrock", "region": "us-east-1" }))
        .await;
    let fields: Vec<&Value> = r.body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| &f["field"])
        .collect();
    assert_eq!(
        (r.status, fields),
        (
            422,
            vec![&json!("name"), &json!("format"), &json!("credential")]
        ),
        "{}",
        r.text
    );
    assert_eq!(
        app.admin("GET", "/api/v1/providers", None).await.body,
        json!([])
    );
}

#[tokio::test]
async fn provider_secrets_never_returned() {
    let app = app().await;
    let aws = json!({
        "aws_access_key_id": "AKIAEXAMPLE0000002", "aws_secret_access_key": "s3cr3t-value",
        "aws_session_token": "t0ken-value"
    });
    let sp = json!({
        "azure_tenant_id": TENANT, "azure_client_id": CLIENT, "azure_client_secret": "sp-s3cr3t"
    });
    let with = |base: Value, extra: &Value| {
        let mut body = base;
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    let replies = [
        app.post_provider(with(
            json!({ "name": "bedrock", "kind": "bedrock", "format": "anthropic", "region": "us-east-1" }),
            &aws,
        ))
        .await,
        app.patch_provider("bedrock", with(json!({ "region": "eu-west-1" }), &aws))
            .await,
        app.post_provider(with(
            json!({ "name": "azure", "kind": "azure", "format": "openai",
                    "endpoint": "https://my-resource.openai.azure.com" }),
            &sp,
        ))
        .await,
        app.patch_provider(
            "azure",
            with(
                json!({ "endpoint": "https://other-resource.openai.azure.com" }),
                &sp,
            ),
        )
        .await,
        app.admin("GET", "/api/v1/providers", None).await,
    ];
    for r in &replies {
        assert!(r.status == 200 || r.status == 201, "{}", r.text);
        for secret in [
            "AKIAEXAMPLE0000002",
            "s3cr3t-value",
            "t0ken-value",
            "sp-s3cr3t",
            CLIENT,
            "api_key",
        ] {
            assert!(!r.text.contains(secret), "{secret} in {}", r.text);
        }
    }
    assert_eq!(replies[1].body["region"], "eu-west-1");
    assert_eq!(
        replies[3].body["endpoint"],
        "https://other-resource.openai.azure.com"
    );
}

#[tokio::test]
async fn moving_a_provider_needs_its_secret_again() {
    let app = app().await;
    app.provider_and_model().await;
    let r = app
        .patch_provider("mock", json!({ "base_url": "http://203.0.113.10/v1" }))
        .await;
    assert_eq!(first_field(&r), (422, &json!("credential")), "{}", r.text);
    let r = app
        .patch_provider(
            "mock",
            json!({ "base_url": "http://203.0.113.10/v1", "api_key": "moved-key" }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.body["base_url"], "http://203.0.113.10/v1");

    let created = app
        .post_provider(json!({
            "name": "bedrock", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "aws_access_key_id": "AKIAEXAMPLE0000001", "aws_secret_access_key": "aws-secret"
        }))
        .await;
    assert_eq!(created.status, 201, "{}", created.text);
    let r = app
        .patch_provider("bedrock", json!({ "region": "eu-west-1" }))
        .await;
    assert_eq!(first_field(&r), (422, &json!("credential")), "{}", r.text);
    // Turning on private networks moves nothing.
    let r = app
        .patch_provider("bedrock", json!({ "allow_private_network": true }))
        .await;
    assert_eq!(
        (r.status, &r.body["allow_private_network"]),
        (200, &json!(true)),
        "{}",
        r.text
    );

    // The gateway's cloud identity only goes to AWS hosts.
    let r = app
        .post_provider(json!({
            "name": "bedrock-cloud", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "cloud_identity": true, "base_url": "https://proxy.example.com"
        }))
        .await;
    assert_eq!(first_field(&r), (422, &json!("base_url")), "{}", r.text);

    // A provider's name, kind and format never change.
    for field in [
        json!({ "kind": "azure" }),
        json!({ "format": "openai" }),
        json!({ "name": "other" }),
    ] {
        let r = app.patch_provider("bedrock", field.clone()).await;
        assert_eq!(
            (r.status, &r.body["error"]),
            (400, &json!("bad_request")),
            "{field}: {}",
            r.text
        );
    }
}

#[tokio::test]
async fn private_addresses_need_the_setting() {
    let app = app().await;
    let compat = |name: &str, url: &str| json!({ "name": name, "kind": "openai_compatible", "base_url": url, "api_key": "k" });
    let r = app
        .post_provider(compat("local", "http://127.0.0.1:9/v1"))
        .await;
    assert_eq!(first_field(&r), (422, &json!("base_url")), "{}", r.text);
    assert!(
        r.body["fields"][0]["message"]
            .as_str()
            .unwrap()
            .contains("allow_private_network"),
        "{}",
        r.text
    );
    let mut allowed = compat("local", "http://127.0.0.1:9/v1");
    allowed["allow_private_network"] = json!(true);
    let r = app.post_provider(allowed).await;
    assert_eq!(
        (r.status, &r.body["allow_private_network"]),
        (201, &json!(true)),
        "{}",
        r.text
    );
    let mut metadata = compat("metadata", "http://169.254.169.254");
    metadata["allow_private_network"] = json!(true);
    let r = app.post_provider(metadata).await;
    assert_eq!(first_field(&r), (422, &json!("base_url")), "{}", r.text);

    // Turning the setting off checks the stored URL again.
    let r = app
        .patch_provider("local", json!({ "allow_private_network": false }))
        .await;
    assert_eq!(first_field(&r), (422, &json!("base_url")), "{}", r.text);
}

/// Switching a provider's credential to the gateway's cloud identity drops
/// its key from the next published config.
#[tokio::test]
async fn switching_to_cloud_identity_republishes_without_the_key() {
    let app = app().await;
    let r = app
        .post_provider(json!({
            "name": "bedrock", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "api_key": "bedrock-key-123"
        }))
        .await;
    assert_eq!(r.status, 201, "{}", r.text);
    let r = app
        .admin(
            "POST",
            "/api/v1/models",
            Some(json!({
                "name": "us.anthropic.claude-mock-v1:0", "provider": "bedrock",
                "input_price": "3", "output_price": "15"
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(
        app.put_env("cloud", &["us.anthropic.claude-mock-v1:0"])
            .await
            .status,
        200
    );
    let settled = app.settled_version().await;
    let before = app
        .call("GET", "/internal/gateway/config", Some(GATEWAY), None)
        .await;
    assert!(
        before.body["yaml"]
            .as_str()
            .unwrap()
            .contains("bedrock-key-123")
    );

    let r = app
        .patch_provider("bedrock", json!({ "cloud_identity": true }))
        .await;
    assert_eq!(
        (r.status, &r.body["credential"]),
        (200, &json!("cloud_identity")),
        "{}",
        r.text
    );
    let after = app
        .call(
            "GET",
            &format!("/internal/gateway/config?after={settled}"),
            Some(GATEWAY),
            None,
        )
        .await;
    assert_eq!(after.status, 200, "no new version after the switch");
    assert!(
        !after.body["yaml"]
            .as_str()
            .unwrap()
            .contains("bedrock-key-123")
    );
}
