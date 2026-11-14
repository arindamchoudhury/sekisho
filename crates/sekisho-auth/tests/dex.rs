//! Sekisho's Dex client against a real Dex v2.46.1 (design risk 7). Needs
//! Docker.

mod common;

use std::time::{Duration, Instant};

use common::{PASSWORD, PAST_REUSE, REDIRECT_URI, TestDex, browse_sign_in, pkce, pw};
use sekisho_auth::dex::{Dex, DexClient, DexConnector, DexError, DexIdentity};
use sekisho_core::names::ConnectorId;
use serde_json::json;

/// Signs `email` in through Dex's login form as `client`.
async fn sign_in_as(dex: &TestDex, client: &DexClient, email: &str) -> DexIdentity {
    let (verifier, challenge) = pkce();
    let url = client.authorize_url("s", "n", &challenge);
    let code = browse_sign_in(dex, url, email, PASSWORD).await;
    client.sign_in(&code, &verifier, "n").await.unwrap()
}

/// A Dex with Sekisho's client and ada's local account, and ada signed in.
async fn ada_signed_in() -> (TestDex, DexIdentity) {
    let dex = common::dex().await;
    dex.ensure_client().await.unwrap();
    dex.put_password(&pw("ada@example.com", "u-ada"))
        .await
        .unwrap();
    let id = sign_in_as(&dex, &dex, "ada@example.com").await;
    (dex, id)
}

fn token(id: &DexIdentity) -> &str {
    id.refresh_token.as_deref().expect("a refresh token")
}

#[tokio::test]
async fn a_local_password_signs_in_with_federated_ids() {
    let dex = common::dex().await;
    dex.ensure_client().await.unwrap();
    dex.put_password(&pw("ada@example.com", "u-ada"))
        .await
        .unwrap(); // bcrypt cost 12
    let (verifier, challenge) = pkce();
    let url = dex.authorize_url("s", "n", &challenge);
    let code = browse_sign_in(&dex, url, "ada@example.com", PASSWORD).await;
    let id = dex.sign_in(&code, &verifier, "n").await.unwrap();
    assert_eq!(
        (id.connector_id.as_str(), id.connector_user_id.as_str()),
        ("local", "u-ada")
    );
    assert!(id.refresh_token.is_some() && !id.subject.is_empty());
    assert_eq!(
        (id.email.as_str(), id.email_verified, id.name.as_str()),
        ("ada@example.com", true, "ada")
    );
    assert!(id.groups.is_empty(), "{:?}", id.groups);
    // A code works once.
    let again = dex.sign_in(&code, &verifier, "n").await;
    assert!(matches!(again, Err(DexError::Refused(_))), "{again:?}");
}

#[tokio::test]
async fn sign_in_checks_the_verifier_and_the_nonce() {
    let dex = common::dex().await;
    dex.ensure_client().await.unwrap();
    dex.put_password(&pw("ada@example.com", "u-ada"))
        .await
        .unwrap();

    let (_, challenge) = pkce();
    let (other_verifier, _) = pkce();
    let url = dex.authorize_url("s", "n", &challenge);
    let code = browse_sign_in(&dex, url, "ada@example.com", PASSWORD).await;
    let r = dex.sign_in(&code, &other_verifier, "n").await;
    assert!(matches!(r, Err(DexError::Refused(_))), "{r:?}");

    let (verifier, challenge) = pkce();
    let url = dex.authorize_url("s", "n", &challenge);
    let code = browse_sign_in(&dex, url, "ada@example.com", PASSWORD).await;
    let r = dex.sign_in(&code, &verifier, "another-nonce").await;
    assert!(matches!(r, Err(DexError::Invalid(_))), "{r:?}");
}

#[tokio::test]
async fn refresh_rotates_the_token_and_keeps_the_identity() {
    let (dex, id) = ada_signed_in().await;
    tokio::time::sleep(PAST_REUSE).await;
    let again = dex.refresh(token(&id)).await.unwrap();
    assert_ne!(again.refresh_token, id.refresh_token);
    assert_eq!(
        (again.subject.as_str(), again.connector_user_id.as_str()),
        (id.subject.as_str(), "u-ada")
    );
    // The rotated-away token is refused once the reuse interval has passed.
    tokio::time::sleep(PAST_REUSE).await;
    let old = dex.refresh(token(&id)).await;
    assert!(matches!(old, Err(DexError::Refused(_))), "{old:?}");
}

/// Dex v2.46.1 answers a connector's refusal to refresh (here, a deleted
/// local password) with HTTP 500 `invalid_request` and no description, the
/// same answer as for its own failures, so Sekisho can't tell that the person
/// was refused. Dex's `master` returns `invalid_grant` instead
/// (dexidp/dex#5132, #5142, #5144), which this client already reads as
/// `Refused`; no release has it yet.
#[tokio::test]
async fn a_deleted_password_fails_refresh_as_dex_s_own_error() {
    let (dex, id) = ada_signed_in().await;
    dex.delete_password("ada@example.com").await.unwrap();
    dex.delete_password("ada@example.com").await.unwrap(); // absent is Ok
    // Within the reuse interval Dex would hand back the same token without
    // asking the connector at all.
    tokio::time::sleep(PAST_REUSE).await;
    let r = dex.refresh(token(&id)).await;
    assert!(
        matches!(&r, Err(DexError::Unavailable(m)) if m.contains("HTTP 500")),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_second_sign_in_replaces_the_first_refresh_token() {
    // Dex keeps one refresh token per user and client (design risk 7, ACH's
    // finding).
    let (dex, first) = ada_signed_in().await;
    let second = sign_in_as(&dex, &dex, "ada@example.com").await;
    assert_eq!(second.subject, first.subject);
    let r = dex.refresh(token(&first)).await;
    assert!(matches!(r, Err(DexError::Refused(_))), "{r:?}");
    assert!(dex.refresh(token(&second)).await.is_ok());
}

#[tokio::test]
async fn revoking_the_refresh_token_ends_the_dex_sign_in() {
    let (dex, id) = ada_signed_in().await;
    dex.revoke_refresh(&id.subject).await.unwrap();
    dex.revoke_refresh(&id.subject).await.unwrap(); // absent is Ok
    let r = dex.refresh(token(&id)).await;
    assert!(matches!(r, Err(DexError::Refused(_))), "{r:?}");
}

#[tokio::test]
async fn connectors_round_trip() {
    let dex = common::dex().await;
    let id = ConnectorId::parse("github").unwrap();
    let github = |name: &str| DexConnector {
        id: id.clone(),
        kind: "github".into(),
        name: name.into(),
    };
    let config = json!({
        "clientID": "github-client-id",
        "clientSecret": "github-client-secret",
        "redirectURI": "http://dex.test:5556/dex/callback",
    });
    // Dex's own `local` connector is never listed.
    assert_eq!(dex.connectors().await.unwrap(), vec![]);
    dex.put_connector(&github("GitHub"), &config).await.unwrap();
    assert_eq!(dex.connectors().await.unwrap(), vec![github("GitHub")]);
    dex.put_connector(&github("GitHub Enterprise"), &config)
        .await
        .unwrap();
    assert_eq!(
        dex.connectors().await.unwrap(),
        vec![github("GitHub Enterprise")]
    );
    assert!(dex.delete_connector(&id).await.unwrap());
    assert!(!dex.delete_connector(&id).await.unwrap());
    assert_eq!(dex.connectors().await.unwrap(), vec![]);
}

#[tokio::test]
async fn connectors_need_dex_s_flag() {
    let dex = common::dex_with(false).await;
    let r = dex.connectors().await;
    assert!(matches!(r, Err(DexError::Invalid(_))), "{r:?}");
}

#[tokio::test]
async fn ensure_client_is_idempotent_and_replaces_a_wrong_secret() {
    let dex = common::dex().await;
    dex.ensure_client().await.unwrap();
    dex.ensure_client().await.unwrap();
    dex.put_password(&pw("ada@example.com", "u-ada"))
        .await
        .unwrap();
    sign_in_as(&dex, &dex, "ada@example.com").await;

    // Another secret: the client is registered again with it.
    let rotated = dex.client("sekisho-client-secret-two", REDIRECT_URI);
    rotated.ensure_client().await.unwrap();
    sign_in_as(&dex, &rotated, "ada@example.com").await;
    // The old secret no longer works, and that is never the person's refusal.
    let (verifier, challenge) = pkce();
    let code = browse_sign_in(
        &dex,
        rotated.authorize_url("s", "n", &challenge),
        "ada@example.com",
        PASSWORD,
    )
    .await;
    let r = dex.sign_in(&code, &verifier, "n").await;
    assert!(matches!(r, Err(DexError::Unavailable(_))), "{r:?}");

    // Another redirect URI: the client is updated.
    let moved = dex.client(
        "sekisho-client-secret-two",
        "http://sekisho.test:8080/oauth/callback",
    );
    moved.ensure_client().await.unwrap();
    sign_in_as(&dex, &moved, "ada@example.com").await;
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dex = DexClient::new(common::config_for(port, port, "s", REDIRECT_URI));
    let started = Instant::now();
    let r = dex.refresh("refresh-token").await;
    assert!(matches!(r, Err(DexError::Unreachable(_))), "{r:?}");
    let r = dex.put_password(&pw("ada@example.com", "u-ada")).await;
    assert!(matches!(r, Err(DexError::Unreachable(_))), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(11));
}

#[tokio::test]
async fn a_silent_dex_times_out_as_unreachable() {
    // Accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dex = DexClient::new(common::config_for(port, port, "s", REDIRECT_URI));
    let ada = pw("ada@example.com", "u-ada");
    let started = Instant::now();
    let (refresh, password) = tokio::join!(dex.refresh("refresh-token"), dex.put_password(&ada));
    assert!(
        matches!(refresh, Err(DexError::Unreachable(_))),
        "{refresh:?}"
    );
    assert!(
        matches!(password, Err(DexError::Unreachable(_))),
        "{password:?}"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(9) && took < Duration::from_secs(11),
        "{took:?}"
    );
    drop(listener);
}
