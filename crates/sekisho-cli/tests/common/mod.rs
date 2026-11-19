//! Sekisho's server on a loopback listener, with a Dex in memory, and a
//! browser's side of each sign-in: what a person does while the CLI waits.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use sekisho_auth::dex::DexIdentity;
use sekisho_core::keys::random_secret;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions};
use sekisho_hooks::cache::AuthCache;
use sekisho_server::app::{AppState, Settings, register_with_dex, router};
use sekisho_server::catalog::CatalogSettings;
use sekisho_testkit::{FakeDex, TestStore};
use url::Url;

pub struct Harness {
    /// `http://127.0.0.1:<port>`, also Sekisho's public URL here.
    pub base: String,
    pub store: TestStore,
    pub sb: SecretBox,
    pub dex: Arc<FakeDex>,
    pub state: AppState,
    /// The person's browser: no redirects followed, cookies by hand.
    pub browser: reqwest::Client,
}

/// A browser answer.
pub struct Page {
    pub status: u16,
    pub location: Option<String>,
    pub cookies: String,
    pub text: String,
}

pub fn ada() -> DexIdentity {
    DexIdentity {
        subject: "github-ada".to_owned(),
        email: "ada@example.com".to_owned(),
        email_verified: true,
        name: "ada".to_owned(),
        groups: vec!["eng".to_owned()],
        connector_id: "github".to_owned(),
        connector_user_id: "github-ada".to_owned(),
        refresh_token: None,
    }
}

/// The server, registered with Dex and answering on a loopback port.
pub async fn server() -> Harness {
    let (store, sb) = sekisho_testkit::store().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let settings = Settings {
        bootstrap_token: "bootstrap-token-0123456789".into(),
        gateway_token: "gateway-token-0123456789".into(),
        hooks: HookTargets {
            authz_http: "sekisho:9080".into(),
            grpc: "sekisho:9081".into(),
            charge_secret: "test-charge-secret".into(),
        },
        render: RenderOptions::default(),
        long_poll: Duration::from_secs(1),
        republish: Duration::from_millis(300),
        public_url: Url::parse(&base).unwrap(),
        admin_groups: vec!["github/sekisho-admins".to_owned()],
        catalog: CatalogSettings::default(),
    };
    store
        .rotate_signing_key(&sb, time::OffsetDateTime::now_utc(), false)
        .await
        .unwrap();
    let cache = AuthCache::load(&store, base.clone()).await.unwrap();
    cache.spawn_refresh((*store).clone());
    let dex = FakeDex::new();
    let state = AppState::new((*store).clone(), sb.clone(), settings, cache, dex.clone());
    register_with_dex(&state, Duration::from_millis(50));
    let served = state.clone();
    tokio::spawn(async move { axum::serve(listener, router(served)).await.unwrap() });
    let start = Instant::now();
    while !state.dex_ready() {
        assert!(start.elapsed() < Duration::from_secs(5), "Dex never ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Harness {
        base,
        store,
        sb,
        dex,
        state,
        browser: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    }
}

fn cookies_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .filter(|c| !c.contains("Max-Age=0"))
        .map(|c| c.split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn query(url: &str) -> HashMap<String, String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

impl Harness {
    async fn page(&self, request: reqwest::RequestBuilder, cookies: &str) -> Page {
        let request = if cookies.is_empty() {
            request
        } else {
            request.header(COOKIE, cookies)
        };
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let cookies = cookies_of(&response);
        let text = response.text().await.unwrap();
        Page {
            status,
            location,
            cookies,
            text,
        }
    }

    /// The browser opens `url`, with `cookies`.
    pub async fn open(&self, url: &str, cookies: &str) -> Page {
        self.page(self.browser.get(url), cookies).await
    }

    /// The browser posts a form to one of Sekisho's paths.
    pub async fn post(&self, path: &str, form: &[(&str, &str)], cookies: &str) -> Page {
        self.page(
            self.browser.post(format!("{}{path}", self.base)).form(form),
            cookies,
        )
        .await
    }

    /// The person signs in to Dex as `who` at `dex_url`, and the browser
    /// comes back to Sekisho's callback with `cookies`.
    pub async fn back_from_dex(&self, dex_url: &str, who: DexIdentity, cookies: &str) -> Page {
        let state = query(dex_url)["state"].clone();
        let code = format!("dex-code-{}", random_secret());
        self.dex.add_code(&code, who);
        let mut back = Url::parse(&format!("{}/oauth/callback", self.base)).unwrap();
        back.query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &state);
        self.open(back.as_str(), cookies).await
    }

    /// The person follows the CLI's sign-in link as `who`; the answer that
    /// sends the browser back to the CLI.
    pub async fn browse(&self, authorize_url: &str, who: DexIdentity) -> Page {
        let to_dex = self.open(authorize_url, "").await;
        assert_eq!(to_dex.status, 302, "authorize: {}", to_dex.text);
        let dex_url = to_dex.location.clone().unwrap();
        let back = self.back_from_dex(&dex_url, who, &to_dex.cookies).await;
        assert_eq!(back.status, 302, "callback: {}", back.text);
        back
    }

    /// The person types `user_code` on the device page and approves it as
    /// `who`.
    pub async fn approve_device(&self, user_code: &str, who: DexIdentity) {
        let page = self
            .post("/oauth/device", &[("user_code", user_code)], "")
            .await;
        assert_eq!(page.status, 200, "{}", page.text);
        let marker = r#"name="csrf" value=""#;
        let at = page.text.find(marker).expect("a csrf field") + marker.len();
        let csrf = page.text[at..at + page.text[at..].find('"').unwrap()].to_owned();
        let to_dex = self
            .post(
                "/oauth/device/confirm",
                &[
                    ("user_code", user_code),
                    ("decision", "continue"),
                    ("csrf", &csrf),
                ],
                &page.cookies,
            )
            .await;
        assert_eq!(to_dex.status, 302, "{}", to_dex.text);
        let done = self
            .back_from_dex(to_dex.location.as_deref().unwrap(), who, &to_dex.cookies)
            .await;
        assert_eq!(done.status, 200, "{}", done.text);
    }

    /// `/oauth/token` with a refresh token, as any client could send it: its
    /// status and `error`.
    pub async fn refresh_status(&self, refresh: &str) -> (u16, String) {
        let response = self
            .browser
            .post(format!("{}/oauth/token", self.base))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", "sekisho-cli"),
                ("refresh_token", refresh),
            ])
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        (
            status,
            body["error"].as_str().unwrap_or_default().to_owned(),
        )
    }

    /// `GET /api/v1/me` with a session token: its status.
    pub async fn me_status(&self, token: &str) -> u16 {
        self.browser
            .get(format!("{}/api/v1/me", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}
