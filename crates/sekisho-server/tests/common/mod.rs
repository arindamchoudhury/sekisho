//! Sekisho's server on a real listener: a throwaway store, a Dex in memory,
//! and session tokens for the people the tests sign in.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions};
use sekisho_hooks::cache::AuthCache;
use sekisho_server::app::{AppState, Settings, register_with_dex, router, start_publisher};
use sekisho_server::catalog::CatalogSettings;
use sekisho_testkit::people::{person, session_token};
use sekisho_testkit::{FakeDex, TestStore};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

pub const BOOTSTRAP: &str = "bootstrap-token-0123456789";
pub const GATEWAY: &str = "gateway-token-0123456789";
pub const ISSUER: &str = "https://sekisho.test";
/// The Sekisho group `SEKISHO_ADMIN_GROUPS` names in these tests. It also
/// names GitHub's group of that name, `github/sekisho-admins` (people sign in
/// through GitHub here).
pub const ADMIN_GROUP: &str = "sekisho-admins";

pub struct TestApp {
    pub base: String,
    pub http: reqwest::Client,
    pub store: TestStore,
    pub sb: SecretBox,
    pub dex: Arc<FakeDex>,
    pub state: AppState,
    /// An admin in the group `eng`, which `put_env` gives environments: the
    /// owner of the tests' environments, and the caller of `admin`.
    pub owner: Uuid,
    /// The owner's session token.
    pub token: String,
}

pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Value,
    pub text: String,
}

pub fn settings() -> Settings {
    Settings {
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
        public_url: Url::parse(ISSUER).unwrap(),
        gateway_url: Url::parse(ISSUER).unwrap(),
        admin_groups: vec![ADMIN_GROUP.to_owned(), format!("github/{ADMIN_GROUP}")],
        // The tests' Git hosts, registries and agents speak plain HTTP.
        catalog: CatalogSettings {
            allow_plain_http: true,
            ..CatalogSettings::default()
        },
    }
}

/// The server, registered with a Dex that works.
pub async fn app() -> TestApp {
    let app = app_with(FakeDex::new()).await;
    app.eventually(Duration::from_secs(5), "registered with Dex", || {
        app.state.dex_ready()
    })
    .await;
    app
}

/// The server with this Dex; it registers with Dex in the background,
/// trying again every 100 ms.
pub async fn app_with(dex: Arc<FakeDex>) -> TestApp {
    let (store, sb) = sekisho_testkit::store().await;
    let owner = person(&store, "admin@example.com", &["eng"]).await;
    store.set_admin(&owner, true).await.unwrap();
    let (token, _) = session_token(&store, &sb, ISSUER, owner).await;
    let cache = AuthCache::load(&store, ISSUER.to_owned()).await.unwrap();
    cache.spawn_refresh((*store).clone());
    let state = AppState::new((*store).clone(), sb.clone(), settings(), cache, dex.clone());
    start_publisher(&state).await.unwrap();
    register_with_dex(&state, Duration::from_millis(100));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = state.clone();
    tokio::spawn(async move { axum::serve(listener, router(served)).await.unwrap() });
    TestApp {
        base: format!("http://{addr}"),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        store,
        sb,
        dex,
        state,
        owner,
        token,
    }
}

impl TestApp {
    pub async fn call(
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
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        Reply {
            status,
            headers,
            body,
            text,
        }
    }

    /// A call by the owner, an admin.
    pub async fn admin(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.call(method, path, Some(&self.token), body).await
    }

    /// A call by whoever holds `token`.
    pub async fn by(&self, token: &str, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.call(method, path, Some(token), body).await
    }

    /// Signs `email` in through GitHub with these groups: their user ID and
    /// a session token.
    pub async fn sign_in(&self, email: &str, groups: &[&str]) -> (Uuid, String) {
        let user = person(&self.store, email, groups).await;
        let (token, _) = session_token(&self.store, &self.sb, ISSUER, user).await;
        (user, token)
    }

    /// Waits up to `within` for `done`, checking every 20 ms.
    pub async fn eventually(&self, within: Duration, what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < within,
                "{what} not seen within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Calls until the status is `status`, for changes the hooks' cache
    /// learns of from a notification.
    pub async fn until_status(&self, token: &str, path: &str, status: u16) -> Reply {
        let start = Instant::now();
        loop {
            let r = self.by(token, "GET", path, None).await;
            if r.status == status || start.elapsed() > Duration::from_secs(3) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The newest published version, or 0. With `after`, long-polls past it.
    pub async fn version_after(&self, after: i64) -> Option<i64> {
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

    pub async fn latest_version(&self) -> i64 {
        self.version_after(0).await.unwrap_or(0)
    }

    /// The latest version once publishing has settled: earlier writes render
    /// in the background, so wait until a poll past the latest times out.
    pub async fn settled_version(&self) -> i64 {
        let mut latest = self.latest_version().await;
        while let Some(newer) = self.version_after(latest).await {
            latest = newer;
        }
        latest
    }

    pub async fn provider_and_model(&self) {
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

    /// An environment for the group `eng`, owned by the owner.
    pub async fn put_env(&self, name: &str, models: &[&str]) -> Reply {
        self.admin(
            "PUT",
            &format!("/api/v1/environments/{name}"),
            Some(json!({ "models": models, "access_groups": ["github/eng"], "owner": self.owner })),
        )
        .await
    }

    /// A key for the owner in `environment`, made by the owner: the key's
    /// answer, with its token.
    pub async fn owners_key(&self, environment: &str, name: &str) -> Reply {
        let r = self
            .admin(
                "POST",
                "/api/v1/me/keys",
                Some(json!({ "environment": environment, "name": name })),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text);
        r
    }
}
