//! `sekisho run`'s loopback proxy (plan 4, Task 7; D191, D194): only the
//! run's secret gets through, only to the installed environments' gateway
//! paths, with a fresh Sekisho token in place of every credential the tool
//! sent; answers stream through and redirects are never followed.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::{ada, server};
use sekisho_cli::adapters::{RunSpec, Tool};
use sekisho_cli::config::{Dirs, ServerUrl};
use sekisho_cli::credentials::{MemoryStore, Store};
use sekisho_cli::install::lock::{EnvLock, Lock, ToolLock};
use sekisho_cli::install::{Scope, ToolHomes};
use sekisho_cli::login::{self, DeviceCode, Method, SignInUi};
use sekisho_cli::output::CliError;
use sekisho_cli::run::{self, TokenFn};
use sekisho_cli::server::Server;
use sekisho_cli::token::{self, Sources};
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc};
use url::Url;

/// One request the gateway saw.
#[derive(Clone, Debug)]
struct Seen {
    path_and_query: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// A gateway on a loopback port that records what reaches it.
struct Gateway {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Lets `/stream`'s last chunk go.
    release: Arc<Notify>,
}

impl Gateway {
    async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let release = Arc::new(Notify::new());
        let (s, r) = (Arc::clone(&seen), Arc::clone(&release));
        let app = Router::new().fallback(move |req: Request| {
            let (seen, release) = (Arc::clone(&s), Arc::clone(&r));
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, 1 << 20).await.unwrap().to_vec();
                let path_and_query = parts
                    .uri
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                seen.lock().unwrap().push(Seen {
                    path_and_query: path_and_query.clone(),
                    headers: parts.headers,
                    body,
                });
                if path_and_query.ends_with("/redirect") {
                    return (
                        StatusCode::FOUND,
                        [(header::LOCATION, "https://elsewhere.example.com/")],
                    )
                        .into_response();
                }
                if path_and_query.ends_with("/stream") {
                    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(4);
                    tokio::spawn(async move {
                        tx.send(Ok(b"data: one\n\n".to_vec())).await.unwrap();
                        release.notified().await;
                        tx.send(Ok(b"data: two\n\n".to_vec())).await.unwrap();
                    });
                    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
                    return Response::builder()
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap();
                }
                (StatusCode::OK, "{\"ok\":true}").into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            origin,
            seen,
            release,
        }
    }

    fn env(&self, name: &str) -> ServerUrl {
        ServerUrl::parse(&format!("{}/env/{name}", self.origin)).unwrap()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Always the same token.
fn fixed(token: &'static str) -> TokenFn {
    Arc::new(move || Box::pin(async move { Ok::<_, CliError>(token.to_owned()) }))
}

async fn proxy_for(gw: &Gateway, envs: &[&str], tokens: TokenFn) -> run::Proxy {
    let gateways: BTreeMap<String, ServerUrl> =
        envs.iter().map(|e| ((*e).to_owned(), gw.env(e))).collect();
    run::start(gateways, tokens, None).await.unwrap()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// A request sent byte for byte, so its path isn't normalised first: the
/// status line's code.
async fn raw_status(proxy: &run::Proxy, path: &str, secret: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = proxy.url().strip_prefix("http://").unwrap().to_owned();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {secret}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    answer.split(' ').nth(1).unwrap().parse().unwrap()
}

#[tokio::test]
async fn the_proxy_needs_the_run_secret_and_stays_on_the_gateway() {
    let gw = Gateway::start().await;
    // `a` is the tool's model environment, `b` installed beside it (D216).
    let proxy = proxy_for(&gw, &["a", "b"], fixed("fresh-token")).await;
    let c = client();
    let url = |path: &str| format!("{}{path}", proxy.url());
    let messages = "/env/a/anthropic/v1/messages";

    let status = |r: reqwest::Response| r.status().as_u16();
    assert_eq!(status(c.post(url(messages)).send().await.unwrap()), 401);
    assert_eq!(
        status(
            c.post(url(messages))
                .bearer_auth("not-the-secret")
                .send()
                .await
                .unwrap()
        ),
        401
    );
    for path in [
        "/env/c/anthropic/v1/messages",
        "/v1/messages",
        "/env/a",
        "/token/x",
    ] {
        assert_eq!(
            status(
                c.post(url(path))
                    .bearer_auth(proxy.secret())
                    .send()
                    .await
                    .unwrap()
            ),
            404,
            "{path}"
        );
    }
    for path in [
        "/env/a/../c/anthropic/v1/messages",
        "/env/a/%2e%2e/c/x",
        "/env/a/x%2F..%2F..%2Fc/y",
        "/env/a/x%5c..%5cc",
    ] {
        assert_eq!(
            raw_status(&proxy, path, proxy.secret()).await,
            404,
            "{path}"
        );
    }
    assert!(gw.seen().is_empty(), "{:?}", gw.seen());

    // The secret in each header a tool sends its key in.
    for how in ["authorization", "x-api-key", "x-goog-api-key"] {
        let value = if how == "authorization" {
            format!("Bearer {}", proxy.secret())
        } else {
            proxy.secret().to_owned()
        };
        let answer = c
            .post(url(
                "/env/b/gemini/v1beta/models/m:generateContent?alt=sse&key=leak",
            ))
            .header(how, value)
            .header("x-api-key-extra", "kept")
            .header(header::COOKIE, "session=x")
            .header("proxy-authorization", "Basic eA==")
            .body("{\"contents\":[]}")
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 200, "{how}");
    }
    let seen = gw.seen();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        assert_eq!(
            s.path_and_query,
            "/env/b/gemini/v1beta/models/m:generateContent?alt=sse"
        );
        assert_eq!(s.headers[header::AUTHORIZATION], "Bearer fresh-token");
        assert_eq!(s.headers.get_all(header::AUTHORIZATION).iter().count(), 1);
        for gone in [
            "x-api-key",
            "x-goog-api-key",
            "cookie",
            "proxy-authorization",
        ] {
            assert!(s.headers.get(gone).is_none(), "{gone}");
        }
        assert_eq!(s.headers["x-api-key-extra"], "kept");
        assert_eq!(s.body, b"{\"contents\":[]}");
        assert!(!format!("{:?}", s.headers).contains(proxy.secret()));
    }

    proxy.stop().await;
}

#[tokio::test]
async fn the_proxy_streams_and_never_follows_redirects() {
    let gw = Gateway::start().await;
    let proxy = proxy_for(&gw, &["a"], fixed("t")).await;
    let c = client();

    // The first event arrives while the gateway still holds the second.
    let mut answer = c
        .get(format!("{}/env/a/stream", proxy.url()))
        .bearer_auth(proxy.secret())
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);
    let first = tokio::time::timeout(Duration::from_secs(5), answer.chunk())
        .await
        .expect("the first event, before the stream ends")
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"data: one\n\n");
    gw.release.notify_one();
    let rest = tokio::time::timeout(Duration::from_secs(5), answer.bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&rest[..], b"data: two\n\n");

    // A redirect from the gateway is an error, and its Location never
    // reaches the tool, which could follow it with the run's secret.
    let answer = c
        .get(format!("{}/env/a/redirect", proxy.url()))
        .bearer_auth(proxy.secret())
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 502);
    assert!(answer.headers().get(header::LOCATION).is_none());
    assert_eq!(
        gw.seen()
            .iter()
            .filter(|s| s.path_and_query.ends_with("/redirect"))
            .count(),
        1
    );
    proxy.stop().await;
}

/// Shows nothing; the test drives the browser.
struct Quiet(mpsc::UnboundedSender<String>);

impl SignInUi for Quiet {
    fn browser_link(&self, url: &Url) {
        let _ = self.0.send(url.to_string());
    }
    fn device_code(&self, code: &DeviceCode) {
        let _ = self.0.send(code.user_code.clone());
    }
}

#[tokio::test]
async fn tokens_are_refreshed_for_a_long_run() {
    let h = server().await;
    let dirs = Dirs::at(tempfile::tempdir().unwrap().keep());
    let server = Server::new(ServerUrl::parse(&h.base).unwrap()).unwrap();
    let store = Arc::new(MemoryStore::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let ui = Quiet(tx);
    let login = {
        let (server, store, dirs) = (server.clone(), Arc::clone(&store), dirs.clone());
        tokio::spawn(async move {
            login::login(
                &server,
                Method::Browser,
                &ui,
                store.as_ref(),
                &dirs,
                Duration::from_secs(30),
            )
            .await
        })
    };
    let link = rx.recv().await.unwrap();
    let back = h.browse(&link, ada()).await;
    assert_eq!(
        h.open(back.location.as_deref().unwrap(), "").await.status,
        200
    );
    login.await.unwrap().unwrap();
    let first = store.load(server.url().key()).unwrap().unwrap();

    // The run's clock, which the test moves on.
    let skew = Arc::new(AtomicI64::new(0));
    let tokens: TokenFn = {
        let (server, store, skew) = (server.clone(), Arc::clone(&store), Arc::clone(&skew));
        let lock = dirs.refresh_lock();
        Arc::new(move || {
            let (server, store, skew, lock) = (
                server.clone(),
                Arc::clone(&store),
                Arc::clone(&skew),
                lock.clone(),
            );
            Box::pin(async move {
                let now = move || {
                    OffsetDateTime::now_utc() + time::Duration::seconds(skew.load(Ordering::SeqCst))
                };
                let c =
                    token::current_with(&server, store.as_ref(), &Sources::default(), &lock, &now)
                        .await?;
                Ok(c.secret().to_owned())
            })
        })
    };
    let gw = Gateway::start().await;
    let proxy = proxy_for(&gw, &["a"], tokens).await;
    let send = || {
        client()
            .post(format!("{}/env/a/anthropic/v1/messages", proxy.url()))
            .header("x-api-key", proxy.secret())
            .send()
    };
    assert_eq!(send().await.unwrap().status(), 200);
    // An hour on: the access token has expired mid-run.
    skew.store(3600, Ordering::SeqCst);
    assert_eq!(send().await.unwrap().status(), 200);

    let after = store.load(server.url().key()).unwrap().unwrap();
    assert_ne!(
        after.refresh, first.refresh,
        "the run refreshed its session"
    );
    let seen = gw.seen();
    assert_eq!(
        seen[0].headers[header::AUTHORIZATION],
        format!("Bearer {}", first.access)
    );
    assert_eq!(
        seen[1].headers[header::AUTHORIZATION],
        format!("Bearer {}", after.access)
    );
    assert_eq!(h.me_status(&after.access).await, 200);
    proxy.stop().await;
}

#[tokio::test]
async fn token_inside_a_run_asks_the_proxy() {
    let gw = Gateway::start().await;
    let proxy = proxy_for(&gw, &["a"], fixed("run-token")).await;
    let got = token::from_run(proxy.url(), proxy.secret()).await.unwrap();
    assert_eq!(got.secret(), "run-token");
    let refused = token::from_run(proxy.url(), "wrong").await.unwrap_err();
    assert!(!refused.message.contains("run-token"), "{refused:?}");
    assert!(gw.seen().is_empty());

    // A token the run can't get is the tool's 401, with Sekisho's reason.
    let failing: TokenFn = Arc::new(|| {
        Box::pin(async { Err(CliError::not_signed_in("your Sekisho session has ended")) })
    });
    let proxy2 = proxy_for(&gw, &["a"], failing).await;
    let answer = client()
        .post(format!("{}/env/a/anthropic/v1/messages", proxy2.url()))
        .bearer_auth(proxy2.secret())
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 401);
    assert!(answer.text().await.unwrap().contains("session has ended"));
    assert!(gw.seen().is_empty());
    proxy.stop().await;
    proxy2.stop().await;
}

#[tokio::test]
async fn the_tools_exit_code_is_returned() {
    let gw = Gateway::start().await;
    let proxy = proxy_for(&gw, &["a"], fixed("t")).await;
    // A script, as npm installs the tools (a `.cmd` on Windows): it checks
    // that it sees the run's variables and its own with them filled in, and
    // exits with the argument given after `--`.
    let dir = tempfile::tempdir().unwrap();
    let script = if cfg!(windows) {
        let path = dir.path().join("tool.cmd");
        std::fs::write(
            &path,
            "@echo off\r\n\
             if not \"%BASE%\"==\"%SEKISHO_RUN_URL%/env/a/gemini\" exit /b 99\r\n\
             if not \"%KEY%\"==\"%SEKISHO_RUN_KEY%\" exit /b 98\r\n\
             if \"%SEKISHO_RUN_KEY%\"==\"\" exit /b 97\r\n\
             exit /b %1\r\n",
        )
        .unwrap();
        path
    } else {
        let path = dir.path().join("tool");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             [ \"$BASE\" = \"$SEKISHO_RUN_URL/env/a/gemini\" ] || exit 99\n\
             [ \"$KEY\" = \"$SEKISHO_RUN_KEY\" ] || exit 98\n\
             [ -n \"$SEKISHO_RUN_KEY\" ] || exit 97\n\
             exit \"$1\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };
    let spec = RunSpec {
        program: script.to_string_lossy().into_owned(),
        args: Vec::new(),
        env: [
            (
                "BASE".to_owned(),
                "${SEKISHO_RUN_URL}/env/a/gemini".to_owned(),
            ),
            ("KEY".to_owned(), "${SEKISHO_RUN_KEY}".to_owned()),
        ]
        .into(),
        tool_header: false,
    };
    let code = run::run_child(&spec, &["7".to_owned()], &proxy)
        .await
        .unwrap();
    assert_eq!(code, 7);
    let code = run::run_child(&spec, &["0".to_owned()], &proxy)
        .await
        .unwrap();
    assert_eq!(code, 0);
    let missing = RunSpec {
        program: "no-such-coding-tool-anywhere".to_owned(),
        ..RunSpec::default()
    };
    let e = run::run_child(&missing, &[], &proxy).await.unwrap_err();
    assert!(e.message.contains("no-such-coding-tool-anywhere"), "{e:?}");
    proxy.stop().await;
}

#[test]
fn the_folder_comes_first_then_the_global_install() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let config = tmp.path().join("config");
    let homes = ToolHomes::from_env(&|_| None, &tmp.path().join("home"));
    let scope = Scope::project(&project, &config, homes);
    let write = |path: &std::path::Path, envs: &[(&str, bool)], model: &str| {
        let mut lock = Lock {
            version: 1,
            ..Lock::default()
        };
        for (i, (env, tool)) in envs.iter().enumerate() {
            let mut tools = BTreeMap::new();
            if *tool {
                tools.insert(Tool::GeminiCli, ToolLock::default());
            }
            lock.environments.insert(
                (*env).to_owned(),
                EnvLock {
                    order: i as u64,
                    tools,
                    ..EnvLock::default()
                },
            );
        }
        lock.models
            .insert("gemini-cli".to_owned(), model.to_owned());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lock.to_bytes()).unwrap();
    };

    let e = run::plan(Tool::GeminiCli, &scope).unwrap_err();
    assert!(e.message.contains("gemini-cli"), "{e:?}");
    assert!(e.hint.unwrap().contains("sekisho use"));

    write(&scope.other().unwrap().lock_path(), &[("g", true)], "g");
    let global = run::plan(Tool::GeminiCli, &scope).unwrap();
    assert_eq!(global.env, "g");
    assert_eq!(global.envs, ["g"]);

    // The folder's, with every environment installed for the tool, but not
    // one installed only for other tools.
    write(
        &scope.lock_path(),
        &[("a", true), ("b", true), ("c", false)],
        "a",
    );
    let here = run::plan(Tool::GeminiCli, &scope).unwrap();
    assert_eq!(here.env, "a");
    assert_eq!(here.envs, ["a", "b"]);
}

#[test]
fn the_gateway_comes_from_the_servers_delivery_document() {
    let envs = ["a".to_owned(), "b".to_owned()];
    let d = |url: &str| sekisho_catalog::delivery::Delivery {
        environment: sekisho_core::names::EnvName::parse("a").unwrap(),
        notice: String::new(),
        revision: String::new(),
        gateway_url: url.to_owned(),
        models: Vec::new(),
        defaults: Default::default(),
        mcp: None,
        entries: Vec::new(),
        quarantined: Vec::new(),
        registries: Default::default(),
    };
    let gateways = run::gateways(&d("https://gw.example.com/ai/env/a"), &envs).unwrap();
    assert_eq!(gateways["a"].key(), "https://gw.example.com/ai/env/a");
    assert_eq!(gateways["b"].key(), "https://gw.example.com/ai/env/b");
    for bad in [
        "https://gw.example.com/env/other",
        "http://gw.example.com/env/a",
        "https://gw.example.com/env/a?x=1",
    ] {
        assert!(run::gateways(&d(bad), &envs).is_err(), "{bad}");
    }
}

/// D227: for a tool that sends no header of its own, the proxy sets
/// `x-sekisho-tool` on every request it forwards, replacing any copy the
/// tool sent; a run that doesn't ask for it forwards none of its own.
#[tokio::test]
async fn the_proxy_sets_the_tool_header_and_overwrites_a_copy() {
    let gw = Gateway::start().await;
    let gateways: BTreeMap<String, ServerUrl> = [("a".to_owned(), gw.env("a"))].into();
    let proxy = run::start(gateways, fixed("t"), Some(Tool::GeminiCli))
        .await
        .unwrap();
    let path = "/env/a/gemini/v1beta/models/flash:generateContent";
    let c = client();
    c.post(format!("{}{path}", proxy.url()))
        .header("x-goog-api-key", proxy.secret())
        .send()
        .await
        .unwrap();
    c.post(format!("{}{path}", proxy.url()))
        .header("x-goog-api-key", proxy.secret())
        .header("x-sekisho-tool", "claude-code")
        .header("X-Sekisho-Tool", "codex")
        .send()
        .await
        .unwrap();
    let seen = gw.seen();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        let sent: Vec<_> = s.headers.get_all("x-sekisho-tool").iter().collect();
        assert_eq!(sent, ["gemini-cli"], "{:?}", s.headers);
    }
    proxy.stop().await;

    let gw = Gateway::start().await;
    let proxy = proxy_for(&gw, &["a"], fixed("t")).await;
    c.post(format!("{}{path}", proxy.url()))
        .header("x-goog-api-key", proxy.secret())
        .send()
        .await
        .unwrap();
    assert!(gw.seen()[0].headers.get("x-sekisho-tool").is_none());
}
