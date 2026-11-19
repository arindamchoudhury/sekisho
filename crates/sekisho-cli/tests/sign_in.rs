//! The CLI's sign-in against Sekisho's real server on a loopback port:
//! server URLs, redirects, both sign-in flows, reusing and refreshing the
//! access token, `SEKISHO_KEY`, logout, and secrets kept out of errors.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use common::{Harness, ada, query, server};
use sekisho_cli::config::{Config, Dirs, ServerUrl, Vars};
use sekisho_cli::credentials::{Credential, MemoryStore, Store, StoreError, Tokens};
use sekisho_cli::login::{self, DeviceCode, Method, SignInUi};
use sekisho_cli::server::Server;
use sekisho_cli::token::{self, Sources};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use url::Url;

/// What the CLI shows the person, as messages a test can wait for.
struct Shown(mpsc::UnboundedSender<String>);

impl SignInUi for Shown {
    fn browser_link(&self, url: &Url) {
        let _ = self.0.send(url.to_string());
    }

    fn device_code(&self, code: &DeviceCode) {
        let _ = self.0.send(code.user_code.clone());
    }
}

fn shown() -> (Arc<Shown>, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(Shown(tx)), rx)
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// A store that fails the test if anything reads or writes it.
struct Untouchable;

impl Store for Untouchable {
    fn load(&self, _: &str) -> Result<Option<Tokens>, StoreError> {
        panic!("the store was read")
    }
    fn save(&self, _: &str, _: &Tokens) -> Result<(), StoreError> {
        panic!("the store was written")
    }
    fn delete(&self, _: &str) -> Result<(), StoreError> {
        panic!("the store was written")
    }
}

/// Signed in through the browser flow as Ada: the server, the store holding
/// the tokens, the folders, and the tokens.
async fn signed_in(h: &Harness) -> (Server, Arc<MemoryStore>, Dirs, Tokens) {
    let dirs = Dirs::at(tempfile::tempdir().unwrap().keep());
    let server = Server::new(ServerUrl::parse(&h.base).unwrap()).unwrap();
    let store = Arc::new(MemoryStore::default());
    let (ui, mut rx) = shown();
    let login = {
        let (server, store, dirs) = (server.clone(), Arc::clone(&store), dirs.clone());
        tokio::spawn(async move {
            login::login(
                &server,
                Method::Browser,
                ui.as_ref(),
                store.as_ref(),
                &dirs,
                Duration::from_secs(30),
            )
            .await
        })
    };
    let link = rx.recv().await.unwrap();
    let back = h.browse(&link, ada()).await;
    let done = h.open(back.location.as_deref().unwrap(), "").await;
    assert_eq!(done.status, 200, "{}", done.text);
    let signed = login.await.unwrap().unwrap();
    assert_eq!(signed.email, "ada@example.com");
    let tokens = store.load(server.url().key()).unwrap().unwrap();
    (server, store, dirs, tokens)
}

#[test]
fn server_urls_are_https_or_loopback() {
    for ok in [
        "https://s.example.com",
        "https://s.example.com/",
        "http://127.0.0.1:8080",
        "http://[::1]:1",
        "http://localhost:9",
    ] {
        assert!(ServerUrl::parse(ok).is_ok(), "{ok}");
    }
    for refused in [
        "http://s.example.com",
        "ftp://x",
        "https://u:p@x",
        "https://u@x",
        "https://x/?q",
        "https://x/#f",
        "http://10.0.0.1:9",
        "s.example.com",
        "",
    ] {
        assert!(ServerUrl::parse(refused).is_err(), "{refused}");
    }
    assert_eq!(
        ServerUrl::parse("https://s.example.com/").unwrap().key(),
        "https://s.example.com"
    );
}

#[tokio::test]
async fn redirects_are_never_followed() {
    let seen = Arc::new(AtomicUsize::new(0));
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_url = format!("http://{}/elsewhere", other.local_addr().unwrap());
    {
        let seen = Arc::clone(&seen);
        let app = Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        });
        tokio::spawn(async move { axum::serve(other, app).await.unwrap() });
    }
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_url = format!("http://{}", first.local_addr().unwrap());
    let app = Router::new().fallback(move || {
        let to = other_url.clone();
        async move { (StatusCode::FOUND, [(header::LOCATION, to)]).into_response() }
    });
    tokio::spawn(async move { axum::serve(first, app).await.unwrap() });

    let server = Server::new(ServerUrl::parse(&first_url).unwrap()).unwrap();
    let error = server
        .get_json::<serde_json::Value>("/api/v1/me", Some(&Credential::Session("t".into())))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("302"), "{error}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "the other origin was reached"
    );
}

#[tokio::test]
async fn the_device_flow_signs_in() {
    let h = server().await;
    let dirs = Dirs::at(tempfile::tempdir().unwrap().keep());
    let server = Server::new(ServerUrl::parse(&h.base).unwrap()).unwrap();
    let store = Arc::new(MemoryStore::default());
    let (ui, mut rx) = shown();
    let login = {
        let (server, store, dirs) = (server.clone(), Arc::clone(&store), dirs.clone());
        tokio::spawn(async move {
            login::login(
                &server,
                Method::Device,
                ui.as_ref(),
                store.as_ref(),
                &dirs,
                Duration::from_secs(60),
            )
            .await
        })
    };
    let user_code = rx.recv().await.unwrap();
    h.approve_device(&user_code, ada()).await;
    let signed = tokio::time::timeout(Duration::from_secs(30), login)
        .await
        .expect("the CLI saw the approval")
        .unwrap()
        .unwrap();
    assert_eq!(signed.email, "ada@example.com");
    let tokens = store
        .load(server.url().key())
        .unwrap()
        .expect("tokens saved");
    assert_eq!(h.me_status(&tokens.access).await, 200);
    let config = Config::load(&dirs).unwrap();
    assert_eq!(config.server.as_deref(), Some(h.base.as_str()));
    assert_eq!(config.email.as_deref(), Some("ada@example.com"));
}

#[tokio::test]
async fn the_browser_flow_checks_state() {
    let h = server().await;
    let dirs = Dirs::at(tempfile::tempdir().unwrap().keep());
    let server = Server::new(ServerUrl::parse(&h.base).unwrap()).unwrap();
    let store = Arc::new(MemoryStore::default());
    let (ui, mut rx) = shown();
    let login = {
        let (server, store, dirs) = (server.clone(), Arc::clone(&store), dirs.clone());
        tokio::spawn(async move {
            login::login(
                &server,
                Method::Browser,
                ui.as_ref(),
                store.as_ref(),
                &dirs,
                Duration::from_secs(30),
            )
            .await
        })
    };
    let link = rx.recv().await.unwrap();
    let q = query(&link);
    assert_eq!(q["client_id"], "sekisho-cli");
    assert_eq!(q["code_challenge_method"], "S256");
    let redirect = q["redirect_uri"].clone();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");

    // Someone else's code at the CLI's port, with another state: refused,
    // and the CLI keeps waiting.
    let forged = h
        .open(
            &format!("{redirect}?code=forged&state=wrong&iss={}", h.base),
            "",
        )
        .await;
    assert_eq!(forged.status, 400, "{}", forged.text);
    assert!(store.load(server.url().key()).unwrap().is_none());

    let back = h.browse(&link, ada()).await;
    let location = back.location.unwrap();
    assert_eq!(query(&location)["state"], q["state"]);
    let done = h.open(&location, "").await;
    assert_eq!(done.status, 200, "{}", done.text);
    login.await.unwrap().unwrap();
    let tokens = store
        .load(server.url().key())
        .unwrap()
        .expect("tokens saved");
    assert_eq!(h.me_status(&tokens.access).await, 200);
}

#[tokio::test]
async fn an_access_token_is_reused_until_near_expiry() {
    let h = server().await;
    let (server, store, dirs, tokens) = signed_in(&h).await;
    let lock = dirs.refresh_lock();
    let none = Sources::default();
    let t0 = now();
    let first = token::current_with(&server, store.as_ref(), &none, &lock, &|| t0)
        .await
        .unwrap();
    let second = token::current_with(&server, store.as_ref(), &none, &lock, &|| {
        t0 + time::Duration::seconds(1)
    })
    .await
    .unwrap();
    // An access token issued in the same second for the same session is the
    // same text, so a refresh shows as a rotated refresh token.
    assert_eq!(first, Credential::Session(tokens.access.clone()));
    assert_eq!(second, first);
    assert_eq!(
        store.load(server.url().key()).unwrap().unwrap(),
        tokens,
        "refreshed with an hour left"
    );

    let late = tokens.access_expires_at - time::Duration::minutes(4);
    let third = token::current_with(&server, store.as_ref(), &none, &lock, &|| late)
        .await
        .unwrap();
    let saved = store.load(server.url().key()).unwrap().unwrap();
    assert_ne!(
        saved.refresh, tokens.refresh,
        "not refreshed 4 minutes before expiry"
    );
    assert_eq!(third, Credential::Session(saved.access.clone()));
    assert_eq!(h.me_status(&saved.access).await, 200);
}

#[tokio::test]
async fn concurrent_refreshes_use_the_token_once() {
    let h = server().await;
    let (server, store, dirs, tokens) = signed_in(&h).await;
    let expired = Tokens {
        access_expires_at: now() - time::Duration::minutes(1),
        ..tokens.clone()
    };
    store.save(server.url().key(), &expired).unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (server, store, lock) = (server.clone(), Arc::clone(&store), dirs.refresh_lock());
        tasks.push(tokio::spawn(async move {
            token::current_with(&server, store.as_ref(), &Sources::default(), &lock, &now).await
        }));
    }
    let mut got = Vec::new();
    for task in tasks {
        got.push(task.await.unwrap().unwrap());
    }
    assert!(
        got.iter().all(|c| *c == got[0]),
        "different tokens: one refresh each?"
    );
    let saved = store.load(server.url().key()).unwrap().unwrap();
    assert_ne!(saved.refresh, tokens.refresh, "never refreshed");
    assert_eq!(got[0], Credential::Session(saved.access.clone()));

    // A refresh token used twice ends the session (plan 2); it is alive.
    assert_eq!(h.me_status(&saved.access).await, 200);
    assert_eq!(h.refresh_status(&saved.refresh).await.0, 200);
}

#[tokio::test]
async fn sekisho_key_wins_and_needs_no_config() {
    let missing = std::env::temp_dir().join(format!("sekisho-none-{}", std::process::id()));
    let dirs = Dirs::at(missing.clone());
    let vars = Vars::from_pairs(&[
        ("SEKISHO_URL", "https://s.example.com"),
        ("SEKISHO_KEY", "sek_abc_def"),
    ]);
    let url = Config::server(&vars, &dirs).unwrap();
    assert_eq!(url.key(), "https://s.example.com");
    let server = Server::new(url).unwrap();
    let got = token::current_with(
        &server,
        &Untouchable,
        &Sources::from_vars(&vars),
        &dirs.refresh_lock(),
        &now,
    )
    .await
    .unwrap();
    assert_eq!(got, Credential::Key("sek_abc_def".into()));
    assert!(!missing.exists(), "a config folder was made");
}

#[tokio::test]
async fn logout_revokes_and_forgets() {
    let h = server().await;
    let (server, store, dirs, tokens) = signed_in(&h).await;
    login::logout(&server, store.as_ref(), &dirs).await.unwrap();
    assert_eq!(
        h.refresh_status(&tokens.refresh).await,
        (400, "invalid_grant".to_owned())
    );
    assert!(store.load(server.url().key()).unwrap().is_none());
    assert_eq!(Config::load(&dirs).unwrap().email, None);
}

#[tokio::test]
async fn tokens_never_reach_logs_or_errors() {
    let secret = "very-secret-token-text-0123456789";
    let tokens = Tokens {
        access: secret.to_owned(),
        access_expires_at: now(),
        refresh: format!("{secret}-refresh"),
    };
    let mut texts = vec![
        format!("{tokens:?}"),
        format!("{:?}", Credential::Session(secret.to_owned())),
        format!("{:?}", Credential::Key(secret.to_owned())),
    ];
    let bad = StoreError::from(keyring_core::Error::BadEncoding(secret.as_bytes().to_vec()));
    texts.push(format!("{bad} {bad:?}"));

    // A call that can't connect, made with the token.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let nowhere = Server::new(ServerUrl::parse(&closed_url).unwrap()).unwrap();
    let error = nowhere
        .get_json::<serde_json::Value>("/api/v1/me", Some(&Credential::Session(secret.into())))
        .await
        .unwrap_err();
    texts.push(format!("{error} {error:?}"));

    // A refresh the server refuses: the session was revoked.
    let h = server().await;
    let (server, store, dirs, signed) = signed_in(&h).await;
    login::logout(&server, store.as_ref(), &dirs).await.unwrap();
    let stale = Tokens {
        access_expires_at: now() - time::Duration::minutes(1),
        ..signed.clone()
    };
    store.save(server.url().key(), &stale).unwrap();
    let refused = token::current_with(
        &server,
        store.as_ref(),
        &Sources::default(),
        &dirs.refresh_lock(),
        &now,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.exit, sekisho_cli::output::Exit::NotSignedIn);
    texts.push(format!("{refused} {refused:?}"));

    for text in texts {
        for needle in [secret, signed.access.as_str(), signed.refresh.as_str()] {
            assert!(!text.contains(needle), "a token in: {text}");
        }
    }
}
