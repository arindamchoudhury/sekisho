//! The RFC 8628 device flow, in process, with a Dex in memory.

mod common;

use axum::http::{StatusCode, header};
use common::app::*;
use sekisho_auth::dex::DexIdentity;
use serde_json::Value;
use time::Duration;
use uuid::Uuid;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

struct Started {
    device_code: String,
    user_code: String,
    body: Value,
}

/// The CLI asks for a device code.
async fn start(t: &T) -> Started {
    let reply = t
        .post_form("/oauth/device_authorization", &[("client_id", CLI)])
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    assert_eq!(reply.header(header::CACHE_CONTROL), Some("no-store"));
    let body = reply.json();
    Started {
        device_code: body["device_code"].as_str().unwrap().to_owned(),
        user_code: body["user_code"].as_str().unwrap().to_owned(),
        body,
    }
}

/// The CLI polls the token endpoint.
async fn poll(t: &T, device_code: &str) -> Reply {
    t.post_form(
        "/oauth/token",
        &[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLI),
            ("device_code", device_code),
        ],
    )
    .await
}

fn error(reply: &Reply) -> (u16, String) {
    reply.oauth_error()
}

fn oauth(status: u16, error: &str) -> (u16, String) {
    (status, error.to_owned())
}

/// A person types `user_code` on the device page.
async fn confirmation(t: &T, user_code: &str) -> Reply {
    t.post_form("/oauth/device", &[("user_code", user_code)])
        .await
}

/// The hidden field the confirmation page's form carries against
/// cross-site posts.
fn csrf_of(page: &Reply) -> String {
    let text = page.text();
    let at = text
        .find(r#"name="csrf" value=""#)
        .unwrap_or_else(|| panic!("no csrf field: {text}"));
    let rest = &text[at + r#"name="csrf" value=""#.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

/// The person presses a button on the confirmation `page`, in the browser
/// that got it.
async fn decide(t: &T, page: &Reply, user_code: &str, decision: &str) -> Reply {
    t.post_form_with(
        "/oauth/device/confirm",
        &[
            ("user_code", user_code),
            ("decision", decision),
            ("csrf", &csrf_of(page)),
        ],
        &page.cookies(),
    )
    .await
}

/// The person approves the device as `who`, through Dex; the callback's
/// page.
async fn approve(t: &T, user_code: &str, who: DexIdentity) -> Reply {
    let page = confirmation(t, user_code).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let to_dex = decide(t, &page, user_code, "continue").await;
    assert_eq!(to_dex.status, StatusCode::FOUND, "{}", to_dex.text());
    t.back_from_dex(&to_dex.location(), who, &to_dex.cookies())
        .await
}

#[tokio::test]
async fn the_device_flow_issues_a_session() {
    let t = setup().await;
    let s = start(&t).await;
    assert_eq!(s.body["verification_uri"], format!("{ISSUER}/oauth/device"));
    assert_eq!(
        s.body["verification_uri_complete"],
        format!("{ISSUER}/oauth/device?user_code={}", s.user_code)
    );
    assert_eq!(
        (s.body["expires_in"].as_u64(), s.body["interval"].as_u64()),
        (Some(600), Some(5))
    );
    let code = s.user_code.as_bytes();
    assert_eq!(code.len(), 9, "{}", s.user_code);
    assert_eq!(code[4], b'-');
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );

    // The person opens the link the terminal showed: the code is filled in.
    let form = t
        .get(&format!("/oauth/device?user_code={}", s.user_code))
        .await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.text().contains(&s.user_code), "{}", form.text());
    let page = confirmation(&t, &s.user_code).await;
    assert_eq!(page.status, StatusCode::OK);
    let text = page.text();
    assert!(text.contains("Sekisho CLI"), "{text}");
    assert!(
        text.contains("Continue only if you started this sign-in yourself"),
        "{text}"
    );
    assert!(text.contains(&s.user_code), "{text}");

    let to_dex = decide(&t, &page, &s.user_code, "continue").await;
    assert_eq!(to_dex.status, StatusCode::FOUND);
    assert!(
        to_dex
            .location()
            .as_str()
            .starts_with("http://dex.test/dex/auth?")
    );
    let done = t
        .back_from_dex(&to_dex.location(), ada(), &to_dex.cookies())
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert!(
        done.text().contains("Signed in. Return to your terminal."),
        "{}",
        done.text()
    );

    t.advance(Duration::seconds(5));
    let reply = poll(&t, &s.device_code).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    assert_eq!(reply.header(header::CACHE_CONTROL), Some("no-store"));
    assert_eq!(reply.json()["token_type"], "Bearer");
    let tokens = tokens(&reply);
    let claims = t.claims(&tokens.access).await;
    assert_eq!(claims.client_id, CLI);
    let user: Uuid = claims.sub.parse().unwrap();
    assert_eq!(
        t.store.user(&user).await.unwrap().dex_subject.as_deref(),
        Some("github-ada")
    );
    // The session refreshes like any other.
    t.refreshed(&tokens.refresh).await;
}

#[tokio::test]
async fn only_clients_with_the_device_grant_start_it() {
    let t = setup().await;
    for client in ["other-client", ""] {
        let reply = t
            .post_form("/oauth/device_authorization", &[("client_id", client)])
            .await;
        assert_eq!(error(&reply), oauth(401, "invalid_client"), "{client:?}");
    }
}

#[tokio::test]
async fn polling_too_fast_is_slow_down() {
    let t = setup().await;
    let s = start(&t).await;
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "slow_down")
    );
    // The interval grew to 10 s.
    t.advance(Duration::seconds(10));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "slow_down")
    );
}

#[tokio::test]
async fn an_unknown_or_expired_user_code_shows_an_error_page() {
    let t = setup().await;
    for typed in ["BCDF-GHJK", "hello", ""] {
        let reply = confirmation(&t, typed).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{typed:?}");
        assert_page(&reply, typed);
        assert!(!reply.text().contains("name=\"csrf\""), "{typed:?}");
    }

    // A code that expired before it was typed.
    let s = start(&t).await;
    t.advance(Duration::seconds(601));
    let reply = confirmation(&t, &s.user_code).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    // A code that expired on the confirmation page.
    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    assert_eq!(page.status, StatusCode::OK);
    t.advance(Duration::seconds(601));
    let reply = decide(&t, &page, &s.user_code, "continue").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.header(header::LOCATION).is_none());
}

#[tokio::test]
async fn a_denied_device_gets_access_denied() {
    let t = setup().await;
    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    let reply = decide(&t, &page, &s.user_code, "deny").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.text().contains("cancelled"), "{}", reply.text());
    assert!(reply.header(header::LOCATION).is_none());
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "access_denied")
    );
    // The code can't be approved afterwards.
    assert_eq!(
        confirmation(&t, &s.user_code).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_disabled_user_cannot_approve_a_device() {
    let t = setup().await;
    let user = t.user_of(&t.session(ada()).await.access).await;
    t.store.set_disabled(&user, true, t.now()).await.unwrap();

    let s = start(&t).await;
    let done = approve(&t, &s.user_code, ada()).await;
    assert_eq!(done.status, StatusCode::FORBIDDEN, "{}", done.text());
    assert_page(&done, "refused");
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "access_denied")
    );
}

#[tokio::test]
async fn device_codes_expire_after_10_minutes() {
    let t = setup().await;
    let s = start(&t).await;
    t.advance(Duration::seconds(599));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );
    t.advance(Duration::seconds(1));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "expired_token")
    );
    assert_eq!(
        confirmation(&t, &s.user_code).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn user_codes_are_case_and_dash_insensitive() {
    let t = setup().await;
    let s = start(&t).await;
    let plain: String = s.user_code.chars().filter(|c| *c != '-').collect();
    let lower = plain.to_lowercase();
    let spaced = format!("{} {}", &lower[..4], &lower[4..]);
    for typed in [s.user_code.to_lowercase(), plain, lower.clone(), spaced] {
        let page = confirmation(&t, &typed).await;
        assert_eq!(page.status, StatusCode::OK, "{typed}");
        assert!(page.text().contains(&s.user_code), "{typed}");
    }
    let done = approve(&t, &lower, ada()).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    t.advance(Duration::seconds(5));
    assert_eq!(poll(&t, &s.device_code).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_device_code_issues_tokens_once() {
    let t = setup().await;
    let s = start(&t).await;
    approve(&t, &s.user_code, ada()).await;
    t.advance(Duration::seconds(5));
    assert_eq!(poll(&t, &s.device_code).await.status, StatusCode::OK);
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "invalid_grant")
    );
    // Nor can it be approved again.
    assert_eq!(
        confirmation(&t, &s.user_code).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn dex_s_access_denied_denies_the_device() {
    let t = setup().await;
    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    let to_dex = decide(&t, &page, &s.user_code, "continue").await;
    // The person cancels at Dex (or the identity provider refuses).
    let dex_state = query(&to_dex.location())["state"].clone();
    let back = t
        .get_with(
            &format!("/oauth/callback?error=access_denied&state={dex_state}"),
            &to_dex.cookies(),
        )
        .await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.text());
    assert!(back.text().contains("cancelled"), "{}", back.text());
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "access_denied")
    );
}

#[tokio::test]
async fn other_dex_errors_keep_the_device_pending() {
    let t = setup().await;
    let s = start(&t).await;
    // Dex failing is no refusal: the code can be entered again.
    for (dex_says, status) in [
        (Some("server_error"), StatusCode::BAD_GATEWAY),
        (
            Some("temporarily_unavailable"),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (Some("invalid_scope"), StatusCode::BAD_GATEWAY),
        (None, StatusCode::BAD_GATEWAY),
    ] {
        let page = confirmation(&t, &s.user_code).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "{dex_says:?}: the code still works"
        );
        let to_dex = decide(&t, &page, &s.user_code, "continue").await;
        let state = query(&to_dex.location())["state"].clone();
        let mut back = url::Url::parse("http://in-process/oauth/callback").unwrap();
        back.query_pairs_mut().append_pair("state", &state);
        if let Some(error) = dex_says {
            back.query_pairs_mut().append_pair("error", error);
        }
        let reply = t
            .get_with(
                &format!("/oauth/callback?{}", back.query().unwrap()),
                &to_dex.cookies(),
            )
            .await;
        assert_eq!(reply.status, status, "{dex_says:?}: {}", reply.text());
        assert_page(&reply, "Dex failed");
        assert!(reply.text().contains("again"), "{}", reply.text());
        t.advance(Duration::seconds(5));
        assert_eq!(
            error(&poll(&t, &s.device_code).await),
            oauth(400, "authorization_pending"),
            "{dex_says:?}"
        );
    }
    // Then the person signs in.
    approve(&t, &s.user_code, ada()).await;
    t.advance(Duration::seconds(5));
    assert_eq!(poll(&t, &s.device_code).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_browser_leaving_the_callback_still_approves_the_device() {
    let t = setup().await;
    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    let to_dex = decide(&t, &page, &s.user_code, "continue").await;
    t.dex.delay_sign_in(std::time::Duration::from_millis(500));
    let left = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        t.back_from_dex(&to_dex.location(), ada(), &to_dex.cookies()),
    )
    .await;
    assert!(left.is_err(), "the callback should still be waiting");
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    t.advance(Duration::seconds(5));
    assert_eq!(poll(&t, &s.device_code).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_forwarded_device_link_approves_nothing() {
    let t = setup().await;
    // An attacker starts a device sign-in, presses Continue in their own
    // browser, and sends the victim Dex's link.
    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    let to_dex = decide(&t, &page, &s.user_code, "continue").await;
    assert_eq!(to_dex.status, StatusCode::FOUND);
    let victim = t.back_from_dex(&to_dex.location(), ada(), "").await;
    assert_eq!(victim.status, StatusCode::BAD_REQUEST, "{}", victim.text());
    assert_page(&victim, "another browser");
    assert!(t.store.list_users().await.unwrap().is_empty());
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );
}

#[tokio::test]
async fn a_cross_site_confirm_is_refused() {
    let t = setup().await;
    let s = start(&t).await;
    // The attacker's page posts their code to the victim's browser's
    // Sekisho: the victim's browser has no cookie for that form, or another
    // form's.
    let attackers = confirmation(&t, &s.user_code).await;
    let victims = confirmation(&t, &s.user_code).await;
    for cookies in [String::new(), victims.cookies()] {
        let reply = t
            .post_form_with(
                "/oauth/device/confirm",
                &[
                    ("user_code", &s.user_code),
                    ("decision", "continue"),
                    ("csrf", &csrf_of(&attackers)),
                ],
                &cookies,
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{cookies:?}");
        assert!(reply.header(header::LOCATION).is_none());
        assert_page(&reply, "cross-site");
    }
    t.advance(Duration::seconds(5));
    assert_eq!(
        error(&poll(&t, &s.device_code).await),
        oauth(400, "authorization_pending")
    );
}

#[tokio::test]
async fn pages_set_their_security_headers() {
    let t = setup().await;
    let s = start(&t).await;
    assert_page(&t.get("/oauth/device").await, "form");
    assert_page(
        &t.get(&format!("/oauth/device?user_code={}", s.user_code))
            .await,
        "prefilled form",
    );
    assert_page(&confirmation(&t, "BCDF-GHJK").await, "unknown code");
    let page = confirmation(&t, &s.user_code).await;
    assert_page(&page, "confirmation");
    let csrf_cookie = page.header(header::SET_COOKIE).unwrap().to_owned();
    for attribute in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(csrf_cookie.contains(attribute), "{csrf_cookie}");
    }
    assert!(csrf_cookie.starts_with("__Host-"), "{csrf_cookie}");
    let done = approve(&t, &s.user_code, ada()).await;
    assert_page(&done, "signed in");

    let s = start(&t).await;
    let page = confirmation(&t, &s.user_code).await;
    assert_page(&decide(&t, &page, &s.user_code, "deny").await, "cancelled");
}
