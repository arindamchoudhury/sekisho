//! Sekisho's OAuth server, in process, with a Dex in memory.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_auth::AuthState;
use sekisho_auth::dex::{DexError, DexIdentity};
use sekisho_auth::sweep::{SweepReport, check_key_owners};
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

const ISSUER: &str = "https://sekisho.test";
const CLI: &str = "sekisho-cli";
const REDIRECT: &str = "http://127.0.0.1:43123/callback";

struct T {
    app: Router,
    store: TestStore,
    sb: SecretBox,
    dex: Arc<FakeDex>,
    state: AuthState,
    now: Arc<Mutex<OffsetDateTime>>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    fn location(&self) -> Url {
        Url::parse(
            self.header(header::LOCATION)
                .unwrap_or_else(|| panic!("no Location; status {}", self.status)),
        )
        .unwrap()
    }

    /// An RFC 6749 error answer: its status and `error`.
    fn oauth_error(&self) -> (u16, String) {
        (
            self.status.as_u16(),
            self.json()["error"].as_str().unwrap_or("").to_owned(),
        )
    }
}

fn query(url: &Url) -> HashMap<String, String> {
    url.query_pairs().into_owned().collect()
}

/// A client's PKCE pair: the verifier and its S256 challenge.
fn pkce() -> (String, String) {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn person(subject: &str, email: &str, groups: &[&str]) -> DexIdentity {
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

fn ada() -> DexIdentity {
    person("github-ada", "ada@example.com", &["eng"])
}

/// What the token endpoint gave.
#[derive(Clone, Debug)]
struct Tokens {
    access: String,
    refresh: String,
}

async fn setup() -> T {
    let (store, sb) = sekisho_testkit::store().await;
    let start = OffsetDateTime::now_utc();
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
        bootstrap_sha256: hash_secret("bootstrap-token"),
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
    fn now(&self) -> OffsetDateTime {
        *self.now.lock().unwrap()
    }

    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }

    async fn send(&self, request: Request<Body>) -> Reply {
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

    async fn get(&self, uri: &str) -> Reply {
        self.send(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    fn form_request(uri: &str, form: &[(&str, &str)]) -> Request<Body> {
        let body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    }

    async fn post_form(&self, uri: &str, form: &[(&str, &str)]) -> Reply {
        self.send(Self::form_request(uri, form)).await
    }

    /// `/oauth/authorize` with these parameters.
    async fn authorize(&self, params: &[(&str, &str)]) -> Reply {
        let mut url = Url::parse("http://in-process/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs(params);
        let uri = format!("/oauth/authorize?{}", url.query().unwrap());
        self.get(&uri).await
    }

    /// A good authorisation request with this challenge.
    async fn authorize_ok(&self, challenge: &str) -> Reply {
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
    async fn browse(&self, who: DexIdentity) -> (Reply, String) {
        let (verifier, challenge) = pkce();
        let to_dex = self.authorize_ok(&challenge).await;
        assert_eq!(to_dex.status, StatusCode::FOUND, "authorize");
        let dex_url = to_dex.location();
        assert!(
            dex_url.as_str().starts_with("http://dex.test/dex/auth?"),
            "{dex_url}"
        );
        let state = query(&dex_url)["state"].clone();
        let dex_code = format!("dex-code-{}", random_secret());
        self.dex.add_code(&dex_code, who);
        let mut back = Url::parse("http://in-process/oauth/callback").unwrap();
        back.query_pairs_mut()
            .append_pair("code", &dex_code)
            .append_pair("state", &state);
        let reply = self
            .get(&format!("/oauth/callback?{}", back.query().unwrap()))
            .await;
        (reply, verifier)
    }

    /// A code for `who`, through the whole browser trip, and its verifier.
    async fn code(&self, who: DexIdentity) -> (String, String) {
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

    async fn redeem(&self, code: &str, verifier: &str) -> Reply {
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
    async fn session(&self, who: DexIdentity) -> Tokens {
        let (code, verifier) = self.code(who).await;
        let reply = self.redeem(&code, &verifier).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        tokens(&reply)
    }

    fn refresh_request(refresh: &str) -> Request<Body> {
        Self::form_request(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLI),
                ("refresh_token", refresh),
            ],
        )
    }

    async fn refresh(&self, refresh: &str) -> Reply {
        self.send(Self::refresh_request(refresh)).await
    }

    /// Refreshes, which must work, and returns the new tokens.
    async fn refreshed(&self, refresh: &str) -> Tokens {
        let reply = self.refresh(refresh).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        tokens(&reply)
    }

    /// The access token's claims, checked with the published keys.
    async fn claims(&self, access: &str) -> SessionClaims {
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

    async fn user_of(&self, access: &str) -> Uuid {
        self.claims(access).await.sub.parse().unwrap()
    }

    /// The Dex token Sekisho holds for `user`.
    async fn stored_dex_token(&self, user: Uuid) -> Option<String> {
        let lock = self.store.lock_dex_token(&self.sb, user).await.unwrap();
        lock.token().map(str::to_owned)
    }

    /// An environment for group `eng`, and a key in it for `owner`.
    async fn key_for(&self, owner: Uuid, env: &str) -> EnvKey {
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

    async fn key_state(&self, key: &EnvKey) -> KeyState {
        let (key, _) = self.store.find_key(&key.id).await.unwrap().unwrap();
        key.state(self.now())
    }
}

fn tokens(reply: &Reply) -> Tokens {
    let body = reply.json();
    Tokens {
        access: body["access_token"].as_str().unwrap().to_owned(),
        refresh: body["refresh_token"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test]
async fn metadata_names_every_endpoint_and_iss_support() {
    let t = setup().await;
    let reply = t.get("/.well-known/oauth-authorization-server").await;
    assert_eq!(reply.status, StatusCode::OK);
    let m = reply.json();
    assert_eq!(m["issuer"], ISSUER);
    assert_eq!(
        m["authorization_endpoint"],
        format!("{ISSUER}/oauth/authorize")
    );
    assert_eq!(m["token_endpoint"], format!("{ISSUER}/oauth/token"));
    assert_eq!(
        m["device_authorization_endpoint"],
        format!("{ISSUER}/oauth/device_authorization")
    );
    assert_eq!(m["revocation_endpoint"], format!("{ISSUER}/oauth/revoke"));
    assert_eq!(m["jwks_uri"], format!("{ISSUER}/oauth/jwks"));
    assert_eq!(m["response_types_supported"], serde_json::json!(["code"]));
    assert_eq!(
        m["grant_types_supported"],
        serde_json::json!([
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ])
    );
    assert_eq!(
        m["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    assert_eq!(
        m["token_endpoint_auth_methods_supported"],
        serde_json::json!(["none"])
    );
    assert_eq!(m["authorization_response_iss_parameter_supported"], true);
}

#[tokio::test]
async fn jwks_publishes_the_signing_keys() {
    let t = setup().await;
    let reply = t.get("/oauth/jwks").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header(header::CONTENT_TYPE),
        Some("application/jwk-set+json")
    );
    let keys = reply.json()["keys"].as_array().unwrap().clone();
    assert_eq!(keys.len(), 1);
    assert_eq!(
        (keys[0]["kty"].as_str(), keys[0]["crv"].as_str()),
        (Some("OKP"), Some("Ed25519"))
    );

    // An issued token verifies with the published key.
    let s = t.session(ada()).await;
    let claims = t.claims(&s.access).await;
    assert_eq!(claims.client_id, CLI);
    assert_eq!(claims.aud, "sekisho");
}

#[tokio::test]
async fn authorize_never_redirects_to_an_unregistered_uri() {
    let t = setup().await;
    let (_, challenge) = pkce();
    for uri in [
        "http://localhost:8000/cb",
        "https://127.0.0.1/cb",
        "http://127.0.0.1.evil.com/cb",
        "http://user@127.0.0.1:9/cb",
        "http://127.0.0.1:9/cb#x",
        "http://10.0.0.1:9/cb",
        "http://127.1:9/cb",
        "http://127.0.0.1/cb",
        "http://127.0.0.1:9/cb?x=1",
        "",
    ] {
        let reply = t
            .authorize(&[
                ("response_type", "code"),
                ("client_id", CLI),
                ("redirect_uri", uri),
                ("state", "s"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(reply.header(header::LOCATION).is_none(), "{uri}");
        assert!(
            reply
                .header(header::CONTENT_TYPE)
                .is_some_and(|c| c.starts_with("text/html")),
            "{uri}"
        );
    }
    // A loopback redirect on IPv6 is fine.
    let ok = t
        .authorize(&[
            ("response_type", "code"),
            ("client_id", CLI),
            ("redirect_uri", "http://[::1]:9/cb"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
        .await;
    assert_eq!(ok.status, StatusCode::FOUND);

    // An unknown client, or none.
    for client in ["other-client", ""] {
        let reply = t
            .authorize(&[
                ("response_type", "code"),
                ("client_id", client),
                ("redirect_uri", REDIRECT),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{client:?}");
        assert!(reply.header(header::LOCATION).is_none(), "{client:?}");
    }
}

#[tokio::test]
async fn authorize_errors_redirect_with_state_and_iss() {
    let t = setup().await;
    let (_, challenge) = pkce();
    type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a str);
    let cases: [Case; 4] = [
        (
            "no PKCE",
            vec![("response_type", "code")],
            "invalid_request",
        ),
        (
            "plain",
            vec![
                ("response_type", "code"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "plain"),
            ],
            "invalid_request",
        ),
        (
            "no method",
            vec![("response_type", "code"), ("code_challenge", &challenge)],
            "invalid_request",
        ),
        (
            "token",
            vec![
                ("response_type", "token"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
            "unsupported_response_type",
        ),
    ];
    for (what, mut params, error) in cases {
        params.extend([
            ("client_id", CLI),
            ("redirect_uri", REDIRECT),
            ("state", "client-state"),
        ]);
        let reply = t.authorize(&params).await;
        assert_eq!(reply.status, StatusCode::FOUND, "{what}");
        let to = reply.location();
        assert!(
            to.as_str().starts_with(&format!("{REDIRECT}?")),
            "{what}: {to}"
        );
        let q = query(&to);
        assert_eq!(q["error"], error, "{what}");
        assert_eq!(q["state"], "client-state", "{what}");
        assert_eq!(q["iss"], ISSUER, "{what}");
        assert!(!q.contains_key("code"), "{what}");
    }
}

#[tokio::test]
async fn the_code_flow_issues_a_session() {
    let t = setup().await;
    let (code, verifier) = t.code(ada()).await;
    let reply = t.redeem(&code, &verifier).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    assert_eq!(reply.header(header::CACHE_CONTROL), Some("no-store"));
    let body = reply.json();
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 3600);
    let s = tokens(&reply);
    assert!(!s.refresh.is_empty());

    let claims = t.claims(&s.access).await;
    let user: Uuid = claims.sub.parse().unwrap();
    let stored = t.store.user(&user).await.unwrap();
    assert_eq!(stored.dex_subject.as_deref(), Some("github-ada"));
    assert_eq!(stored.idp_groups, vec!["eng".to_owned()]);
    assert_eq!(claims.exp - claims.iat, 3600);
    // Dex's refresh token is kept, for later refreshes.
    assert_eq!(
        t.stored_dex_token(user).await,
        t.dex.live_token("github-ada")
    );
    assert!(t.dex.live_token("github-ada").is_some());
}

#[tokio::test]
async fn a_code_works_once() {
    let t = setup().await;
    let invalid_grant = (400, "invalid_grant".to_owned());

    // Used twice.
    let (code, verifier) = t.code(ada()).await;
    assert_eq!(t.redeem(&code, &verifier).await.status, StatusCode::OK);
    assert_eq!(
        t.redeem(&code, &verifier).await.oauth_error(),
        invalid_grant
    );

    // A wrong verifier uses the code up.
    let (code, verifier) = t.code(ada()).await;
    let (_, other) = pkce();
    assert_eq!(t.redeem(&code, &other).await.oauth_error(), invalid_grant);
    assert_eq!(
        t.redeem(&code, &verifier).await.oauth_error(),
        invalid_grant
    );

    // Another redirect URI.
    let (code, verifier) = t.code(ada()).await;
    let reply = t
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLI),
                ("code", &code),
                ("redirect_uri", "http://127.0.0.1:43124/callback"),
                ("code_verifier", &verifier),
            ],
        )
        .await;
    assert_eq!(reply.oauth_error(), invalid_grant);

    // Another client: none but `sekisho-cli` is registered, so it is unknown,
    // and the code stays for its own client.
    let (code, verifier) = t.code(ada()).await;
    let reply = t
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", "other-client"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", &verifier),
            ],
        )
        .await;
    assert_eq!(reply.oauth_error(), (401, "invalid_client".to_owned()));
    assert_eq!(t.redeem(&code, &verifier).await.status, StatusCode::OK);

    // After 60 s.
    let (code, verifier) = t.code(ada()).await;
    t.advance(Duration::seconds(61));
    assert_eq!(
        t.redeem(&code, &verifier).await.oauth_error(),
        invalid_grant
    );
}

#[tokio::test]
async fn a_disabled_user_is_refused_at_the_callback() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    t.store.set_disabled(&user, true, t.now()).await.unwrap();

    let (reply, _) = t.browse(ada()).await;
    assert_eq!(reply.status, StatusCode::FOUND);
    let to = reply.location();
    assert!(to.as_str().starts_with(&format!("{REDIRECT}?")), "{to}");
    let q = query(&to);
    assert_eq!(q["error"], "access_denied");
    assert_eq!(q["state"], "client-state");
    assert_eq!(q["iss"], ISSUER);
    assert!(!q.contains_key("code"));
}

#[tokio::test]
async fn a_sign_in_without_a_dex_refresh_token_is_refused() {
    let t = setup().await;
    t.dex.withhold_refresh_tokens(true);
    let (reply, _) = t.browse(ada()).await;
    let q = query(&reply.location());
    assert_eq!(q["error"], "access_denied");
    assert_eq!(q["iss"], ISSUER);
    assert!(!q.contains_key("code"));
}

#[tokio::test]
async fn refresh_rechecks_dex_and_rotates() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    let before = t.stored_dex_token(user).await.unwrap();

    // The identity provider moved Ada to another group.
    t.dex
        .set_identity(person("github-ada", "ada@example.com", &["eng", "ops"]));
    let next = t.refreshed(&s.refresh).await;
    assert_ne!(next.refresh, s.refresh);
    assert_eq!(t.dex.refreshes_shown(), vec![before.clone()]);
    let after = t.stored_dex_token(user).await.unwrap();
    assert_ne!(after, before);
    assert_eq!(Some(after), t.dex.live_token("github-ada"));
    assert_eq!(
        t.store.user(&user).await.unwrap().idp_groups,
        vec!["eng".to_owned(), "ops".to_owned()]
    );
    let claims = t.claims(&next.access).await;
    assert_eq!(claims.sid, t.claims(&s.access).await.sid);
    assert_eq!(claims.sub, user.to_string());

    // The old token, presented again at once, is only refused.
    let again = t.refresh(&s.refresh).await;
    assert_eq!(again.oauth_error(), (400, "invalid_grant".to_owned()));
    assert_eq!(
        again.json()["error_description"],
        "refresh token rotation in progress"
    );
    t.refreshed(&next.refresh).await;
}

#[tokio::test]
async fn a_dex_refusal_ends_every_session_of_the_user() {
    let t = setup().await;
    let first = t.session(ada()).await;
    let second = t.session(ada()).await;
    let user = t.user_of(&first.access).await;
    let key = t.key_for(user, "team").await;
    assert_eq!(t.key_state(&key).await, KeyState::Active);

    t.dex.revoke("github-ada");
    let reply = t.refresh(&first.refresh).await;
    assert_eq!(reply.oauth_error(), (400, "invalid_grant".to_owned()));
    let reply = t.refresh(&second.refresh).await;
    assert_eq!(reply.oauth_error(), (400, "invalid_grant".to_owned()));
    assert_eq!(t.key_state(&key).await, KeyState::Invalid);
    let stored = t.store.user(&user).await.unwrap();
    assert!(stored.idp_refused_at.is_some());
    assert_eq!(t.stored_dex_token(user).await, None);
    // Dex was asked once; the second session had already ended.
    assert_eq!(t.dex.refreshes_shown().len(), 1);

    // Signing in again brings the key back.
    t.session(ada()).await;
    assert_eq!(t.key_state(&key).await, KeyState::Active);
}

#[tokio::test]
async fn an_unreachable_dex_keeps_the_session() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    let key = t.key_for(user, "team").await;
    for failure in [
        DexError::Unreachable("token endpoint: connection refused".into()),
        DexError::Unavailable("HTTP 500 from the token endpoint: invalid_request".into()),
    ] {
        t.dex.fail_refresh("github-ada", Some(failure.clone()));
        let reply = t.refresh(&s.refresh).await;
        assert_eq!(
            reply.oauth_error(),
            (503, "temporarily_unavailable".to_owned()),
            "{failure:?}"
        );
        assert_eq!(reply.header(header::CACHE_CONTROL), Some("no-store"));
    }
    assert_eq!(t.key_state(&key).await, KeyState::Active);
    assert!(t.store.user(&user).await.unwrap().idp_refused_at.is_none());

    // The same refresh token works once Dex is back.
    t.dex.fail_refresh("github-ada", None);
    t.refreshed(&s.refresh).await;
}

#[tokio::test]
async fn concurrent_refreshes_share_one_dex_token() {
    let t = setup().await;
    let mut a = t.session(ada()).await;
    let mut b = t.session(ada()).await;
    t.dex.delay_refresh(StdDuration::from_millis(20));
    for round in 0..10 {
        let (ra, rb) = tokio::join!(t.refresh(&a.refresh), t.refresh(&b.refresh));
        assert_eq!(ra.status, StatusCode::OK, "round {round}: {:?}", ra.json());
        assert_eq!(rb.status, StatusCode::OK, "round {round}: {:?}", rb.json());
        (a, b) = (tokens(&ra), tokens(&rb));
    }
    let shown = t.dex.refreshes_shown();
    assert_eq!(shown.len(), 20);
    let distinct: HashSet<&String> = shown.iter().collect();
    assert_eq!(distinct.len(), 20, "Dex saw a token twice: {shown:?}");
}

#[tokio::test]
async fn a_client_hanging_up_mid_refresh_keeps_the_rotated_dex_token() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    let before = t.stored_dex_token(user).await.unwrap();
    t.dex.delay_refresh(StdDuration::from_millis(500));

    let hung_up = tokio::time::timeout(
        StdDuration::from_millis(100),
        t.app.clone().oneshot(T::refresh_request(&s.refresh)),
    )
    .await;
    assert!(hung_up.is_err(), "the refresh should still be waiting");
    tokio::time::sleep(StdDuration::from_millis(900)).await;

    let rotated = t.dex.live_token("github-ada").unwrap();
    assert_ne!(rotated, before);
    assert_eq!(t.stored_dex_token(user).await, Some(rotated));
    // The client never got a new refresh token, so its old one still works.
    t.dex.delay_refresh(StdDuration::ZERO);
    t.refreshed(&s.refresh).await;
    let shown = t.dex.refreshes_shown();
    let distinct: HashSet<&String> = shown.iter().collect();
    assert_eq!(distinct.len(), shown.len(), "{shown:?}");
}

#[tokio::test]
async fn a_browser_leaving_the_callback_keeps_dex_s_new_token() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    t.dex.delay_sign_in(StdDuration::from_millis(500));

    // Ada signs in again and closes the tab while Dex answers: Dex has
    // already replaced the token Sekisho holds.
    let left = tokio::time::timeout(StdDuration::from_millis(100), t.browse(ada())).await;
    assert!(left.is_err(), "the callback should still be waiting");
    tokio::time::sleep(StdDuration::from_millis(900)).await;

    let live = t.dex.live_token("github-ada");
    assert!(live.is_some());
    assert_eq!(t.stored_dex_token(user).await, live);
    // So the first session still refreshes.
    t.refreshed(&s.refresh).await;
}

#[tokio::test]
async fn a_sign_in_during_a_refresh_ends_no_session() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let user = t.user_of(&s.access).await;
    t.dex.delay_refresh(StdDuration::from_millis(400));

    // Ada signs in on another machine while this session refreshes: Dex
    // replaces the refresh token Sekisho is replaying.
    let refresh = {
        let app = t.app.clone();
        let request = T::refresh_request(&s.refresh);
        tokio::spawn(async move { app.oneshot(request).await.unwrap().status() })
    };
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    let other = t.session(ada()).await;
    assert_eq!(refresh.await.unwrap(), StatusCode::OK);

    t.dex.delay_refresh(StdDuration::ZERO);
    assert!(t.store.user(&user).await.unwrap().idp_refused_at.is_none());
    t.refreshed(&other.refresh).await;
}

#[tokio::test]
async fn revoking_a_refresh_token_ends_its_session() {
    let t = setup().await;
    let s = t.session(ada()).await;
    let reply = t
        .post_form(
            "/oauth/revoke",
            &[("token", &s.refresh), ("client_id", CLI)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        t.refresh(&s.refresh).await.oauth_error(),
        (400, "invalid_grant".to_owned())
    );

    // An access token ends its session too.
    let s = t.session(ada()).await;
    let reply = t
        .post_form(
            "/oauth/revoke",
            &[
                ("token", &s.access),
                ("token_type_hint", "access_token"),
                ("client_id", CLI),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        t.refresh(&s.refresh).await.oauth_error(),
        (400, "invalid_grant".to_owned())
    );

    // Unknown tokens are fine.
    for token in ["not-a-token", "eyJhbGciOiJub25lIn0.e30.x"] {
        let reply = t
            .post_form("/oauth/revoke", &[("token", token), ("client_id", CLI)])
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{token}");
    }
}

#[tokio::test]
async fn the_sweep_makes_a_dropped_owners_keys_invalid() {
    let t = setup().await;
    let bob = person("github-bob", "bob@example.com", &["eng"]);
    let carol = person("github-carol", "carol@example.com", &["eng"]);
    let ada_session = t.session(ada()).await;
    let ada_id = t.user_of(&ada_session.access).await;
    let bob_id = t.user_of(&t.session(bob).await.access).await;
    let carol_id = t.user_of(&t.session(carol).await.access).await;
    let ada_key = t.key_for(ada_id, "team-a").await;
    t.key_for(bob_id, "team-b").await;
    let carol_key = t.key_for(carol_id, "team-c").await;

    // Dex refuses Ada's token; Bob's check fails on Dex's side (HTTP 500,
    // as Dex v2.46.1 answers a connector's refusal); Carol passes.
    t.dex.revoke("github-ada");
    t.dex.fail_refresh(
        "github-bob",
        Some(DexError::Unavailable(
            "HTTP 500 from the token endpoint".into(),
        )),
    );
    let report = check_key_owners(&t.state, t.now()).await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            claimed: 3,
            passed: 1,
            refused: 1,
            kept: 1,
            released: 0
        }
    );
    assert_eq!(t.key_state(&ada_key).await, KeyState::Invalid);
    assert_eq!(t.key_state(&carol_key).await, KeyState::Active);
    assert_eq!(
        t.refresh(&ada_session.refresh).await.oauth_error(),
        (400, "invalid_grant".to_owned())
    );

    // Nobody is due again today: Bob keeps his claim after Dex's 500.
    t.advance(Duration::minutes(10));
    let report = check_key_owners(&t.state, t.now()).await.unwrap();
    assert_eq!(report.claimed, 0, "{report:?}");

    // A day later Dex can't be reached: the claims go back, and the sweep
    // stops at the first unreachable answer.
    t.advance(Duration::hours(25));
    t.dex.fail_refresh(
        "github-bob",
        Some(DexError::Unreachable(
            "token endpoint: connection refused".into(),
        )),
    );
    t.dex.fail_refresh(
        "github-carol",
        Some(DexError::Unreachable(
            "token endpoint: connection refused".into(),
        )),
    );
    let asked = t.dex.refreshes_shown().len();
    let report = check_key_owners(&t.state, t.now()).await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            claimed: 2,
            passed: 0,
            refused: 0,
            kept: 0,
            released: 2
        }
    );
    assert_eq!(t.dex.refreshes_shown().len(), asked + 1);

    // The next sweep tries them again, and Dex is back.
    t.dex.fail_refresh("github-bob", None);
    t.dex.fail_refresh("github-carol", None);
    t.advance(Duration::minutes(10));
    let report = check_key_owners(&t.state, t.now()).await.unwrap();
    assert_eq!((report.claimed, report.passed), (2, 2), "{report:?}");
    assert_eq!(t.key_state(&carol_key).await, KeyState::Active);
}
