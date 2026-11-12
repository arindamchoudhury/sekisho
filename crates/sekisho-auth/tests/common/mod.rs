//! A real Dex v2.46.1 in Docker, and a browser that signs in to it.

#![allow(dead_code)]

use std::ops::Deref;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_auth::dex::{DexClient, DexConfig, DexPassword};
use sha2::{Digest, Sha256};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use url::Url;

pub const ISSUER: &str = "http://dex.test:5556/dex";
pub const REDIRECT_URI: &str = "http://sekisho.test/oauth/callback";
pub const CLIENT_SECRET: &str = "sekisho-client-secret-one";
pub const PASSWORD: &str = "correct horse battery staple";
/// Dex's `reuseInterval` in `dex.yaml` plus a margin: a refresh within it
/// returns the same token without asking the connector.
pub const PAST_REUSE: Duration = Duration::from_millis(3_500);

const DEX_YAML: &str = include_str!("../dex.yaml");

/// A Dex container on its own PostgreSQL, and a client for it as Sekisho's
/// client `sekisho`. Derefs to the client; the containers and their network
/// are removed when the test ends.
pub struct TestDex {
    client: DexClient,
    pub http_port: u16,
    pub grpc_port: u16,
    _dex: ContainerAsync<GenericImage>,
    _postgres: ContainerAsync<GenericImage>,
}

impl Deref for TestDex {
    type Target = DexClient;

    fn deref(&self) -> &DexClient {
        &self.client
    }
}

impl TestDex {
    /// Sekisho's client at this Dex with another secret or redirect URI.
    pub fn config(&self, secret: &str, redirect_uri: &str) -> DexConfig {
        config_for(self.http_port, self.grpc_port, secret, redirect_uri)
    }

    pub fn client(&self, secret: &str, redirect_uri: &str) -> DexClient {
        DexClient::new(self.config(secret, redirect_uri))
    }
}

pub fn config_for(http_port: u16, grpc_port: u16, secret: &str, redirect_uri: &str) -> DexConfig {
    DexConfig {
        issuer: Url::parse(ISSUER).unwrap(),
        // 127.0.0.1, not localhost: on Windows, localhost tries ::1 first.
        http_url: Url::parse(&format!("http://127.0.0.1:{http_port}")).unwrap(),
        grpc: format!("127.0.0.1:{grpc_port}"),
        client_id: "sekisho".into(),
        client_secret: secret.into(),
        redirect_uri: Url::parse(redirect_uri).unwrap(),
    }
}

/// Starts Dex with `dex.yaml` and the connectors API turned on.
pub async fn dex() -> TestDex {
    dex_with(true).await
}

/// Starts PostgreSQL 18 and Dex on a network of their own, with or without
/// `DEX_API_CONNECTORS_CRUD=true`.
pub async fn dex_with(connectors_crud: bool) -> TestDex {
    let name = format!("sekisho-dex-{}", sekisho_core::keys::generate_key().id);
    let db_host = format!("{name}-db");
    // The entrypoint's temporary initdb server logs its ready line to stdout;
    // only the real server logs it to stderr.
    let postgres = GenericImage::new("postgres", "18")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_network(&name)
        .with_container_name(&db_host)
        .with_env_var("POSTGRES_PASSWORD", "sekisho")
        .with_cmd(["postgres", "-c", "fsync=off"])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .expect("start postgres:18 (is Docker running?)");
    let config = DEX_YAML.replace("DEX_DB_HOST", &db_host);
    let mut request = GenericImage::new("ghcr.io/dexidp/dex", "v2.46.1")
        .with_exposed_port(5556.tcp())
        .with_exposed_port(5557.tcp())
        // Logged once both the HTTP and the gRPC server are set up.
        .with_wait_for(WaitFor::message_on_stderr("server=grpc"))
        .with_network(&name)
        .with_cmd(["dex", "serve", "/etc/dex/sekisho.yaml"])
        .with_copy_to("/etc/dex/sekisho.yaml", config.into_bytes())
        .with_startup_timeout(Duration::from_secs(120));
    if connectors_crud {
        request = request.with_env_var("DEX_API_CONNECTORS_CRUD", "true");
    }
    let dex = request
        .start()
        .await
        .expect("start ghcr.io/dexidp/dex:v2.46.1");
    let http_port = dex.get_host_port_ipv4(5556).await.unwrap();
    let grpc_port = dex.get_host_port_ipv4(5557).await.unwrap();
    let discovery = format!("http://127.0.0.1:{http_port}/dex/.well-known/openid-configuration");
    let http = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..50 {
        if http
            .get(&discovery)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(ready, "Dex never served {discovery}");
    TestDex {
        client: DexClient::new(config_for(
            http_port,
            grpc_port,
            CLIENT_SECRET,
            REDIRECT_URI,
        )),
        http_port,
        grpc_port,
        _dex: dex,
        _postgres: postgres,
    }
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = sekisho_core::keys::random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// A local account's password entry, hashed at cost 12 as Sekisho will.
pub fn pw(email: &str, user_id: &str) -> DexPassword {
    DexPassword {
        email: email.into(),
        bcrypt_hash: bcrypt::hash(PASSWORD, 12).unwrap(),
        username: email.split('@').next().unwrap().into(),
        user_id: user_id.into(),
    }
}

enum Step {
    Get(Url),
    Post(Url, Vec<(&'static str, String)>),
}

/// Plays the browser from `authorize_url` to the redirect back to the
/// client: follows Dex's redirects, posts `login` and `password` to the login
/// form, and returns the `code` the client gets. Dex's issuer origin is
/// rewritten to the container's mapped port.
pub async fn browse_sign_in(
    dex: &TestDex,
    authorize_url: Url,
    login: &str,
    password: &str,
) -> String {
    let issuer = Url::parse(ISSUER).unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mapped = |u: &Url| {
        let mut u = u.clone();
        u.set_host(Some("127.0.0.1")).unwrap();
        u.set_port(Some(dex.http_port)).unwrap();
        u
    };
    let mut step = Step::Get(authorize_url);
    for _ in 0..20 {
        let (here, response) = match step {
            Step::Get(u) => {
                let r = http.get(mapped(&u)).send().await.unwrap();
                (u, r)
            }
            Step::Post(u, form) => {
                let r = http.post(mapped(&u)).form(&form).send().await.unwrap();
                (u, r)
            }
        };
        let status = response.status();
        if status.is_redirection() {
            let location = response.headers()["location"].to_str().unwrap();
            let next = here.join(location).unwrap();
            if next.origin() != issuer.origin() {
                let mut q = next.query_pairs();
                return match q.find(|(k, _)| k == "code") {
                    Some((_, code)) => code.into_owned(),
                    None => panic!("Dex redirected to the client without a code: {next}"),
                };
            }
            step = Step::Get(next);
            continue;
        }
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{status} from {here}: {body}");
        let action =
            form_action(&body).unwrap_or_else(|| panic!("no login form at {here}: {body}"));
        let form = vec![
            ("login", login.to_owned()),
            ("password", password.to_owned()),
        ];
        step = Step::Post(here.join(&action).unwrap(), form);
    }
    panic!("no redirect to the client after 20 steps (wrong password?)");
}

/// The `action` of the page's `<form method="post">`, HTML-unescaped.
fn form_action(html: &str) -> Option<String> {
    let form = &html[html.find("<form method=\"post\"")?..];
    let start = form.find("action=\"")? + "action=\"".len();
    let end = start + form[start..].find('"')?;
    Some(
        form[start..end]
            .replace("&amp;", "&")
            .replace("&#43;", "+")
            .replace("&#61;", "=")
            .replace("&#39;", "'")
            .replace("&#34;", "\"")
            .replace("&quot;", "\""),
    )
}
