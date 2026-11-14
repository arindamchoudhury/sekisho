//! Plan 2's acceptance tests: people sign in through Dex, their sessions go
//! through agentgateway like keys, and admins govern them. Ignored by
//! default; run with the stack up:
//!   cargo test -p sekisho-e2e -- --ignored --test-threads=1

use std::time::Duration;

use reqwest::StatusCode;
use sekisho_e2e::*;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

/// An address no earlier run used.
fn email(prefix: &str) -> String {
    format!("{}@example.com", unique(prefix))
}

/// A local account in [`GROUP`], signed in: (user ID, email, password,
/// tokens).
async fn member(prefix: &str) -> (Uuid, String, String, Tokens) {
    let address = email(prefix);
    let (id, password) = invite(&admin_token().await, &address, &[GROUP]).await;
    let tokens = sign_in(&address, &password).await;
    (id, address, password, tokens)
}

/// The admin's user ID.
async fn admin_id() -> Uuid {
    let (_, me) = admin("GET", "/api/v1/me", None).await;
    me["id"].as_str().unwrap().parse().unwrap()
}

/// A new environment for [`GROUP`], live at the gateway for `token`.
async fn env_for(prefix: &str, token: &str) -> String {
    seed().await;
    let env = unique(prefix);
    put_env(&env).await;
    wait_route(&env, token).await;
    env
}

/// Calls until `env` answers `token` with `status`, for changes the hooks
/// learn of from a notification.
async fn until(env: &str, token: &str, status: StatusCode) -> Value {
    let start = Instant::now();
    loop {
        let (now, body) = chat(env, token, 1).await;
        if now == status {
            return body;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{env} still answers {now}: {body}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// A key of the person whose access token this is: (id, token).
async fn own_key(access: &str, env: &str) -> (String, String) {
    let (s, body) = by(
        access,
        "POST",
        "/api/v1/me/keys",
        Some(json!({ "environment": env, "name": unique("key") })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    (
        body["id"].as_str().unwrap().into(),
        body["token"].as_str().unwrap().into(),
    )
}

/// A 15-chunk stream, a chunk a second, from `env`'s OpenAI route.
struct Stream {
    response: reqwest::Response,
    text: String,
}

impl Stream {
    async fn open(env: &str, token: &str) -> Self {
        let response = chat_request(
            &http(),
            &format!("/env/{env}/openai"),
            Some(token),
            MODEL,
            15,
            1000,
            true,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        Self {
            response,
            text: String::new(),
        }
    }

    /// Reads until `n` content chunks have arrived.
    async fn read(&mut self, n: usize) {
        while self.text.matches("\"content\"").count() < n {
            let bytes = self.response.chunk().await.unwrap().expect("more chunks");
            self.text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }

    /// Reads to the end. The stream must end within 2 s of `cut`, never
    /// reaching `[DONE]`.
    async fn ends_after(mut self, cut: Instant) {
        let mut last = Instant::now();
        // An error or a clean end both end the loop.
        while let Ok(Some(bytes)) = self.response.chunk().await {
            last = Instant::now();
            assert!(
                last.duration_since(cut) <= Duration::from_secs(2),
                "a chunk arrived {:?} after the cut",
                last.duration_since(cut)
            );
            self.text.push_str(&String::from_utf8_lossy(&bytes));
        }
        let ended = last.max(Instant::now()).duration_since(cut);
        println!("stream ended {ended:?} after the cut");
        assert!(
            ended <= Duration::from_millis(2500),
            "stream ran {ended:?} past the cut"
        );
        assert!(!self.text.contains("[DONE]"), "the stream was not cut");
    }
}

#[tokio::test]
#[ignore]
async fn the_first_admin_is_set_up_once() {
    admin_token().await;
    let page = http().get(format!("{ADMIN}/setup")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
    assert!(
        page.text()
            .await
            .unwrap()
            .contains("Sekisho is set up; sign in.")
    );
    let late = email("late-admin");
    let again = http()
        .post(format!("{ADMIN}/setup"))
        .form(&[
            ("bootstrap_token", BOOTSTRAP),
            ("email", late.as_str()),
            ("name", "Late"),
            ("password", ADMIN_PASSWORD),
            ("confirm", ADMIN_PASSWORD),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
    let (status, users) = admin("GET", "/api/v1/users", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!users.to_string().contains(&late), "{users}");
    // The admin API takes sessions, not the bootstrap token.
    let (status, body) = by(BOOTSTRAP, "GET", "/api/v1/users", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body["message"].as_str().unwrap().contains("/setup"),
        "{body}"
    );
}

#[tokio::test]
#[ignore]
async fn a_local_account_signs_in_and_reaches_the_gateway() {
    // sign_in checks that the redirect carries iss = SEKISHO_PUBLIC_URL.
    let (_, _, _, tokens) = member("gateway").await;
    let env = env_for("session", &tokens.access).await;
    let (status, body) = chat(&env, &tokens.access, 1).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The provider got its own key, and no Sekisho token of any kind.
    let headers: serde_json::Map<String, Value> = http()
        .get(format!("{MOCK}/_last_headers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for value in headers.values() {
        let value = value.as_str().unwrap();
        for sekisho in ["eyJ", "sek_", tokens.refresh.as_str()] {
            assert!(
                !value.contains(sekisho),
                "{sekisho} reached the provider: {headers:?}"
            );
        }
    }
    assert!(
        headers
            .values()
            .any(|v| v.as_str().unwrap().contains(MOCK_PROVIDER_KEY)),
        "{headers:?}"
    );
}

#[tokio::test]
#[ignore]
async fn the_device_flow_signs_in() {
    let address = email("device");
    let (id, password) = invite(&admin_token().await, &address, &[GROUP]).await;
    let tokens = device_sign_in(&address, &password).await;
    let (status, me) = by(&tokens.access, "GET", "/api/v1/me", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!((&me["id"], &me["email"]), (&json!(id), &json!(address)));
    // A device's session refreshes like any other.
    refresh(&tokens).await.expect("a refresh");
}

#[tokio::test]
#[ignore]
async fn a_session_without_access_is_refused_with_no_access() {
    let (env, _, _) = fresh_env("closed").await;
    let address = email("outsider");
    let (_, password) = invite(&admin_token().await, &address, &[]).await;
    let tokens = sign_in(&address, &password).await;
    let (status, body) = chat(&env, &tokens.access, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "no_access", "{body}");
}

#[tokio::test]
#[ignore]
async fn logging_out_cuts_the_running_stream_under_2s() {
    let (_, _, _, tokens) = member("logout").await;
    let env = env_for("logout", &tokens.access).await;
    let mut stream = Stream::open(&env, &tokens.access).await;
    stream.read(3).await;
    revoke(&tokens).await;
    stream.ends_after(Instant::now()).await;
    let (status, body) = chat(&env, &tokens.access, 1).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["reason"], "session_ended", "{body}");
}

#[tokio::test]
#[ignore]
async fn a_session_is_charged_to_its_person() {
    let (id, _, _, tokens) = member("charged").await;
    let env = env_for("charged", &tokens.access).await;
    let (s, budget) = admin(
        "POST",
        "/api/v1/budgets",
        Some(json!({ "name": unique("person"), "scope": "person", "person": id, "limit": "1", "window": "month" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{budget}");
    let (status, body) = chat(&env, &tokens.access, 100).await; // 1000 in, 100 out
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_spent(budget["id"].as_str().unwrap(), "0.0045").await; // 4500 micro-USD
}

#[tokio::test]
#[ignore]
async fn a_key_turns_invalid_when_its_owner_leaves_the_group() {
    let (id, _, _, tokens) = member("leaver").await;
    let env = env_for("leaver", &tokens.access).await;
    let (_, key) = own_key(&tokens.access, &env).await;
    wait_route(&env, &key).await;
    assert_eq!(chat(&env, &key, 1).await.0, StatusCode::OK);

    leave(&admin_token().await, GROUP, id).await;
    let body = until(&env, &key, StatusCode::FORBIDDEN).await;
    assert_eq!(body["reason"], "invalid", "{body}");
    join(&admin_token().await, GROUP, id).await;
    until(&env, &key, StatusCode::OK).await;
}

#[tokio::test]
#[ignore]
async fn offboarding_cuts_streams_revokes_keys_and_hands_over() {
    let (id, address, password, tokens) = member("offboarded").await;
    seed().await;
    let env = unique("theirs");
    let (s, body) = admin(
        "PUT",
        &format!("/api/v1/environments/{env}"),
        Some(json!({ "models": [MODEL], "access_groups": [GROUP], "owner": id })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (key_id, key) = own_key(&tokens.access, &env).await;
    wait_route(&env, &tokens.access).await;
    wait_route(&env, &key).await;

    let mut session_stream = Stream::open(&env, &tokens.access).await;
    let mut key_stream = Stream::open(&env, &key).await;
    tokio::join!(session_stream.read(3), key_stream.read(3));
    let new_owner = admin_id().await;
    let (s, done) = admin(
        "POST",
        &format!("/api/v1/users/{id}/offboard"),
        Some(json!({ "new_owner": new_owner })),
    )
    .await;
    let cut = Instant::now();
    assert_eq!(s, StatusCode::OK, "{done}");
    tokio::join!(session_stream.ends_after(cut), key_stream.ends_after(cut));
    assert_eq!(done["revoked_keys"], json!([key_id]), "{done}");
    assert_eq!(done["handed_over"], json!([env]), "{done}");
    assert_eq!(done["dex_refresh_revoked"], true, "{done}");

    let (status, body) = chat(&env, &key, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "revoked", "{body}");
    let (_, environment) = admin("GET", &format!("/api/v1/environments/{env}"), None).await;
    assert_eq!(environment["owner"], json!(new_owner));
    let (status, body) = refresh(&tokens).await.expect_err("a refresh");
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
    );
    assert_eq!(
        try_sign_in(&address, &password)
            .await
            .expect_err("a sign-in"),
        "access_denied"
    );
}

#[tokio::test]
#[ignore]
async fn a_reset_password_ends_sessions_and_the_old_password_fails() {
    let (id, address, password, tokens) = member("reset").await;
    let (s, link) = admin("POST", &format!("/api/v1/local-accounts/{id}/reset"), None).await;
    assert_eq!(s, StatusCode::OK, "{link}");
    let (status, body) = refresh(&tokens).await.expect_err("a refresh");
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
    );
    let refused = try_sign_in(&address, &password)
        .await
        .expect_err("the old password");
    assert_eq!(refused, "Dex refused the password");
    let new_password = format!("{password} renewed");
    set_password(link["setup_url"].as_str().unwrap(), &new_password).await;
    sign_in(&address, &new_password).await;
}

/// Waits until Dex answers again.
async fn wait_for_dex() {
    let start = Instant::now();
    loop {
        let up = http()
            .get(format!("{DEX}/dex/.well-known/openid-configuration"))
            .send()
            .await
            .is_ok_and(|r| r.status() == StatusCode::OK);
        if up {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "Dex did not come back"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

/// Every refresh asks Dex. With Dex v2.46.1 a deleted password is Dex's own
/// error (D104), so what shows it is Dex being away: the refresh can't be
/// answered, and the session is still there once Dex is back.
#[tokio::test]
#[ignore]
async fn refresh_rechecks_dex() {
    let (id, _, _, tokens) = member("recheck").await;
    let tokens = refresh(&tokens).await.expect("a refresh");

    compose(&["stop", "dex"]);
    let away = refresh(&tokens).await;
    compose(&["start", "dex"]);
    let (status, body) = away.expect_err("a refresh without Dex");
    assert_eq!(
        (status, &body["error"]),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            &json!("temporarily_unavailable")
        )
    );
    wait_for_dex().await;
    let tokens = refresh(&tokens)
        .await
        .expect("the same refresh token, Dex back");

    let (s, body) = admin("DELETE", &format!("/api/v1/local-accounts/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = refresh(&tokens).await.expect_err("a refresh");
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
    );
}

#[tokio::test]
#[ignore]
async fn connectors_round_trip_through_dex() {
    let id = unique("gh");
    let secret = format!("{id}-client-secret");
    let (s, body) = admin(
        "PUT",
        &format!("/api/v1/connectors/{id}"),
        Some(json!({
            "type": "github", "name": "GitHub (e2e)",
            "config": { "clientID": "e2e", "clientSecret": secret, "redirectURI": format!("{DEX}/dex/callback") }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (_, listed) = admin("GET", "/api/v1/connectors", None).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .contains(&json!({ "id": id, "type": "github", "name": "GitHub (e2e)" })),
        "{listed}"
    );
    assert!(!listed.to_string().contains(&secret) && !body.to_string().contains(&secret));

    // Dex's sign-in page now offers it beside local accounts.
    let mut browser = Browser::new();
    let mut authorize = url::Url::parse(&format!("{ADMIN}/oauth/authorize")).unwrap();
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLI)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair(
            "code_challenge",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        )
        .append_pair("code_challenge_method", "S256");
    let mut response = browser.get_when_ready(&authorize).await;
    let mut here = authorize;
    while response.status().is_redirection() {
        let next = here
            .join(response.headers()["location"].to_str().unwrap())
            .unwrap();
        response = browser.get(next.as_str()).await;
        here = next;
    }
    let page = response.text().await.unwrap();
    assert!(here.as_str().starts_with(DEX), "{here}");
    assert!(page.contains(&format!("/auth/{id}")), "{page}");
    assert!(page.contains("/auth/local"), "{page}");

    let (s, _) = admin("DELETE", &format!("/api/v1/connectors/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, listed) = admin("GET", "/api/v1/connectors", None).await;
    assert!(!listed.to_string().contains(&id), "{listed}");
}

#[tokio::test]
#[ignore]
async fn session_hooks_add_under_5ms_median() {
    seed().await;
    let (_, _, _, tokens) = member("latency").await;
    wait_route("data-team", &tokens.access).await;
    let client = http();
    let time = async |path: &str, token: Option<&str>| {
        let started = Instant::now();
        let resp = chat_request(&client, path, token, MODEL, 1, 0, false)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        resp.bytes().await.unwrap();
        started.elapsed()
    };
    for _ in 0..5 {
        time("/bare/openai", None).await; // warm up connections
        time("/env/data-team/openai", Some(&tokens.access)).await;
    }
    let (mut bare, mut hooked) = (Vec::new(), Vec::new());
    for _ in 0..40 {
        bare.push(time("/bare/openai", None).await);
        hooked.push(time("/env/data-team/openai", Some(&tokens.access)).await);
    }
    let (bare, hooked) = (median(bare), median(hooked));
    println!("median without hooks {bare:?}, with hooks and a session token {hooked:?}");
    assert!(
        hooked.saturating_sub(bare) < Duration::from_millis(5),
        "hooks add {:?}",
        hooked.saturating_sub(bare)
    );
}
