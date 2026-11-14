//! Sekisho's sign-in routes in process, with a Dex in memory: a browser's
//! and a client's side of each request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_auth::AuthState;
use sekisho_auth::dex::DexIdentity;
use sekisho_core::keys::{KeyState, hash_secret, random_secret};
use sekisho_core::names::EnvName;
use sekisho_core::secret_box::SecretBox;
use sekisho_core::session_token::{PublicKey, SessionClaims, verify};
use sekisho_hooks::cache::AuthCache;
use sekisho_store::{EnvKey, EnvironmentSpec};
use sekisho_testkit::{FakeDex, TestStore};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

pub const ISSUER: &str = "https://sekisho.test";
pub const CLI: &str = "sekisho-cli";
pub const REDIRECT: &str = "http://127.0.0.1:43123/callback";
/// What `setup` hashes as `SEKISHO_BOOTSTRAP_TOKEN`.
pub const BOOTSTRAP: &str = "bootstrap-token";

pub struct T {
    pub app: Router,
    pub store: TestStore,
    pub sb: SecretBox,
    pub dex: Arc<FakeDex>,
    pub state: AuthState,
    pub now: Arc<Mutex<OffsetDateTime>>,
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    pub fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    pub fn location(&self) -> Url {
        Url::parse(
            self.header(header::LOCATION)
                .unwrap_or_else(|| panic!("no Location; status {}", self.status)),
        )
        .unwrap()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The cookies this answer sets, as a browser sends them back: a
    /// `Cookie` header value. Cookies it deletes are left out.
    pub fn cookies(&self) -> String {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .filter(|c| !c.contains("Max-Age=0"))
            .map(|c| c.split(';').next().unwrap().to_owned())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// An RFC 6749 error answer: its status and `error`.
    pub fn oauth_error(&self) -> (u16, String) {
        (
            self.status.as_u16(),
            self.json()["error"].as_str().unwrap_or("").to_owned(),
        )
    }
}

pub fn query(url: &Url) -> HashMap<String, String> {
    url.query_pairs().into_owned().collect()
}

/// A client's PKCE pair: the verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn person(subject: &str, email: &str, groups: &[&str]) -> DexIdentity {
    DexIdentity {
        subject: subject.to_owned(),
        email: email.to_owned(),
        email_verified: true,
        name: email.split('@').next().unwrap().to_owned(),
        groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        connector_id: "github".to_owned(),
        connector_user_id: subject.to_owned(),
        refresh_token: None,
    }
}

pub fn ada() -> DexIdentity {
    person("github-ada", "ada@example.com", &["eng"])
}

/// What the token endpoint gave.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

pub async fn setup() -> T {
    let (store, sb) = sekisho_testkit::store().await;
    // A whole second: PostgreSQL keeps microseconds, so limits fall exactly
    // where the tests put them.
    let start = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    store.rotate_signing_key(&sb, start, false).await.unwrap();
    let cache = AuthCache::load(&store, ISSUER.to_owned()).await.unwrap();
    let dex = FakeDex::new();
    let now = Arc::new(Mutex::new(start));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().unwrap())
    };
    let state = AuthState {
        store: (*store).clone(),
        secret_box: sb.clone(),
        dex: dex.clone(),
        cache,
        public_url: Url::parse(ISSUER).unwrap(),
        bootstrap_sha256: hash_secret(BOOTSTRAP),
        clock,
    };
    T {
        app: sekisho_auth::router(state.clone()),
        store,
        sb,
        dex,
        state,
        now,
    }
}

impl T {
    pub fn now(&self) -> OffsetDateTime {
        *self.now.lock().unwrap()
    }

    pub fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }

    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, uri: &str) -> Reply {
        self.send(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    /// A browser's `GET`, with its cookies.
    pub async fn get_with(&self, uri: &str, cookies: &str) -> Reply {
        let mut request = Request::get(uri);
        if !cookies.is_empty() {
            request = request.header(header::COOKIE, cookies);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    /// A browser's form `POST`, with its cookies.
    pub async fn post_form_with(&self, uri: &str, form: &[(&str, &str)], cookies: &str) -> Reply {
        let mut request = Self::form_request(uri, form);
        if !cookies.is_empty() {
            request
                .headers_mut()
                .insert(header::COOKIE, cookies.parse().unwrap());
        }
        self.send(request).await
    }

    pub fn form_request(uri: &str, form: &[(&str, &str)]) -> Request<Body> {
        let body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    }

    pub async fn post_form(&self, uri: &str, form: &[(&str, &str)]) -> Reply {
        self.send(Self::form_request(uri, form)).await
    }

    /// `/oauth/authorize` with these parameters.
    pub async fn authorize(&self, params: &[(&str, &str)]) -> Reply {
        let mut url = Url::parse("http://in-process/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs(params);
        let uri = format!("/oauth/authorize?{}", url.query().unwrap());
        self.get(&uri).await
    }

    /// A good authorisation request with this challenge.
    pub async fn authorize_ok(&self, challenge: &str) -> Reply {
        self.authorize(&[
            ("response_type", "code"),
            ("client_id", CLI),
            ("redirect_uri", REDIRECT),
            ("state", "client-state"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
        .await
    }

    /// The browser's trip: authorise, sign in to Dex as `who`, and come back
    /// to the callback. Returns the callback's answer and the client's
    /// verifier.
    pub async fn browse(&self, who: DexIdentity) -> (Reply, String) {
        let (verifier, challenge) = pkce();
        let to_dex = self.authorize_ok(&challenge).await;
        assert_eq!(to_dex.status, StatusCode::FOUND, "authorize");
        let dex_url = to_dex.location();
        assert!(
            dex_url.as_str().starts_with("http://dex.test/dex/auth?"),
            "{dex_url}"
        );
        let reply = self.back_from_dex(&dex_url, who, &to_dex.cookies()).await;
        (reply, verifier)
    }

    /// Signs in to Dex as `who` at `dex_url`, in a browser holding
    /// `cookies`, and comes back to the callback.
    pub async fn back_from_dex(&self, dex_url: &Url, who: DexIdentity, cookies: &str) -> Reply {
        let state = query(dex_url)["state"].clone();
        let dex_code = format!("dex-code-{}", random_secret());
        self.dex.add_code(&dex_code, who);
        let mut back = Url::parse("http://in-process/oauth/callback").unwrap();
        back.query_pairs_mut()
            .append_pair("code", &dex_code)
            .append_pair("state", &state);
        self.get_with(
            &format!("/oauth/callback?{}", back.query().unwrap()),
            cookies,
        )
        .await
    }

    /// A code for `who`, through the whole browser trip, and its verifier.
    pub async fn code(&self, who: DexIdentity) -> (String, String) {
        let (reply, verifier) = self.browse(who).await;
        assert_eq!(reply.status, StatusCode::FOUND, "callback");
        let to_client = reply.location();
        assert!(
            to_client.as_str().starts_with(&format!("{REDIRECT}?")),
            "{to_client}"
        );
        let q = query(&to_client);
        assert_eq!(q["state"], "client-state");
        assert_eq!(q["iss"], ISSUER);
        (q["code"].clone(), verifier)
    }

    pub async fn redeem(&self, code: &str, verifier: &str) -> Reply {
        self.post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLI),
                ("code", code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", verifier),
            ],
        )
        .await
    }

    /// A signed-in session for `who`.
    pub async fn session(&self, who: DexIdentity) -> Tokens {
        let (code, verifier) = self.code(who).await;
        let reply = self.redeem(&code, &verifier).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        tokens(&reply)
    }

    pub fn refresh_request(refresh: &str) -> Request<Body> {
        Self::form_request(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLI),
                ("refresh_token", refresh),
            ],
        )
    }

    pub async fn refresh(&self, refresh: &str) -> Reply {
        self.send(Self::refresh_request(refresh)).await
    }

    /// Refreshes, which must work, and returns the new tokens.
    pub async fn refreshed(&self, refresh: &str) -> Tokens {
        let reply = self.refresh(refresh).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        tokens(&reply)
    }

    /// The access token's claims, checked with the published keys.
    pub async fn claims(&self, access: &str) -> SessionClaims {
        let keys = self.get("/oauth/jwks").await.json();
        let keys: Vec<PublicKey> = keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                let x = URL_SAFE_NO_PAD.decode(k["x"].as_str().unwrap()).unwrap();
                PublicKey {
                    kid: k["kid"].as_str().unwrap().to_owned(),
                    x: x.try_into().unwrap(),
                }
            })
            .collect();
        verify(
            access,
            |kid| keys.iter().find(|k| k.kid == kid).cloned(),
            ISSUER,
            self.now(),
        )
        .unwrap()
    }

    pub async fn user_of(&self, access: &str) -> Uuid {
        self.claims(access).await.sub.parse().unwrap()
    }

    /// The Dex token Sekisho holds for `user`.
    pub async fn stored_dex_token(&self, user: Uuid) -> Option<String> {
        let lock = self.store.lock_dex_token(&self.sb, user).await.unwrap();
        lock.token().map(str::to_owned)
    }

    /// An environment for group `eng`, and a key in it for `owner`.
    pub async fn key_for(&self, owner: Uuid, env: &str) -> EnvKey {
        let spec = EnvironmentSpec {
            name: EnvName::parse(env).unwrap(),
            description: String::new(),
            notice: String::new(),
            access_groups: vec!["eng".into()],
            models: Vec::new(),
            owner: None,
        };
        let name = self
            .store
            .upsert_environment(spec, owner)
            .await
            .unwrap()
            .spec
            .name;
        self.store
            .create_key(&name, owner, "laptop", None)
            .await
            .unwrap()
            .0
    }

    pub async fn key_state(&self, key: &EnvKey) -> KeyState {
        let (key, _) = self.store.find_key(&key.id).await.unwrap().unwrap();
        key.state(self.now())
    }
}

pub fn tokens(reply: &Reply) -> Tokens {
    let body = reply.json();
    Tokens {
        access: body["access_token"].as_str().unwrap().to_owned(),
        refresh: body["refresh_token"].as_str().unwrap().to_owned(),
    }
}

/// An HTML page with the security headers every page carries, and no
/// script.
pub fn assert_page(reply: &Reply, what: &str) {
    assert!(
        reply
            .header(header::CONTENT_TYPE)
            .is_some_and(|c| c.starts_with("text/html")),
        "{what}: {:?}",
        reply.headers
    );
    assert_eq!(
        reply.header(header::CONTENT_SECURITY_POLICY),
        Some("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
        "{what}"
    );
    assert_eq!(
        reply.header(header::REFERRER_POLICY),
        Some("no-referrer"),
        "{what}"
    );
    assert_eq!(
        reply.header(header::CACHE_CONTROL),
        Some("no-store"),
        "{what}"
    );
    assert!(!reply.text().contains("<script"), "{what}");
}
