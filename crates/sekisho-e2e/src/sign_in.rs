//! Signing people in through Dex, as a browser and the CLI would: the code
//! flow with PKCE (as `sekisho-cli`, redirecting to a loopback address no one
//! listens on, where the code is read from the redirect), and the device
//! flow. The browser keeps cookies between hops, since every sign-in is bound
//! to the browser that started it.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use url::Url;
use uuid::Uuid;

use crate::{ADMIN, BOOTSTRAP, by, http};

/// Dex's issuer origin, as browsers reach it.
pub const DEX: &str = "http://127.0.0.1:5556";
/// The CLI's pre-registered client.
pub const CLI: &str = "sekisho-cli";
/// A loopback redirect no one listens on: the code is read from the
/// redirect itself.
pub const REDIRECT: &str = "http://127.0.0.1:1/callback";
/// The Sekisho group every environment the tests make admits.
pub const GROUP: &str = "e2e";
/// The first admin's local account, made on `/setup`.
pub const ADMIN_EMAIL: &str = "e2e-admin@example.com";
pub const ADMIN_PASSWORD: &str = "plumber galaxy orbit fern e2e admin 42";

/// What Sekisho's token endpoint gave.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

impl Tokens {
    fn from(body: &Value) -> Self {
        Self {
            access: body["access_token"].as_str().unwrap().to_owned(),
            refresh: body["refresh_token"].as_str().unwrap().to_owned(),
        }
    }
}

/// The admin's session for this test binary, and when it was made.
pub(crate) static ADMIN_SESSION: Mutex<Option<(Tokens, Instant)>> = Mutex::const_new(None);

/// The admin's access token: signed in once per run and refreshed after 50
/// minutes, since access tokens last an hour.
pub async fn admin_token() -> String {
    let mut held = ADMIN_SESSION.lock().await;
    if let Some((tokens, at)) = held.as_ref()
        && at.elapsed() < Duration::from_secs(50 * 60)
    {
        return tokens.access.clone();
    }
    let refreshed = match held.take() {
        Some((tokens, _)) => refresh(&tokens).await.ok(),
        None => None,
    };
    let tokens = match refreshed {
        Some(tokens) => tokens,
        None => bootstrap_admin().await,
    };
    let access = tokens.access.clone();
    *held = Some((tokens, Instant::now()));
    access
}

/// Sets up the first admin through `/setup` (once per stack), signs them
/// in, and makes them a member of [`GROUP`].
pub async fn bootstrap_admin() -> Tokens {
    let page = http()
        .get(format!("{ADMIN}/setup"))
        .send()
        .await
        .expect("Sekisho reachable (is the stack up?)");
    if page.status() == StatusCode::OK {
        let done = http()
            .post(format!("{ADMIN}/setup"))
            .form(&[
                ("bootstrap_token", BOOTSTRAP),
                ("email", ADMIN_EMAIL),
                ("name", "E2E Admin"),
                ("password", ADMIN_PASSWORD),
                ("confirm", ADMIN_PASSWORD),
            ])
            .send()
            .await
            .unwrap();
        let status = done.status();
        assert_eq!(status, StatusCode::OK, "{}", done.text().await.unwrap());
    }
    let tokens = sign_in(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let (_, me) = by(&tokens.access, "GET", "/api/v1/me", None).await;
    let id: Uuid = me["id"].as_str().unwrap().parse().unwrap();
    join(&tokens.access, GROUP, id).await;
    tokens
}

/// Makes the Sekisho group unless it exists, and puts `user` in it.
pub async fn join(admin: &str, group: &str, user: Uuid) {
    let (s, body) = by(
        admin,
        "POST",
        "/api/v1/groups",
        Some(json!({ "name": group })),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "group {group}: {s} {body}"
    );
    let (s, body) = by(
        admin,
        "PUT",
        &format!("/api/v1/groups/{group}/members/{user}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{body}");
}

/// Takes `user` out of the Sekisho group.
pub async fn leave(admin: &str, group: &str, user: Uuid) {
    let (s, body) = by(
        admin,
        "DELETE",
        &format!("/api/v1/groups/{group}/members/{user}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{body}");
}

/// A password no run used before, hard enough to guess for Sekisho's rule.
fn new_password() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("violet anchor {} quietly drums", nanos % 1_000_000_000)
}

/// Invites `email` as a local account in `groups` (as the admin whose
/// access token is `admin`), and sets their password through the setup
/// link: their user ID and password.
pub async fn invite(admin: &str, email: &str, groups: &[&str]) -> (Uuid, String) {
    let name = email.split('@').next().unwrap();
    let (s, body) = by(
        admin,
        "POST",
        "/api/v1/local-accounts",
        Some(json!({ "email": email, "name": name })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let id: Uuid = body["user"]["id"].as_str().unwrap().parse().unwrap();
    for group in groups {
        join(admin, group, id).await;
    }
    let password = new_password();
    set_password(body["setup_url"].as_str().unwrap(), &password).await;
    (id, password)
}

/// Sets a password through a setup link.
pub async fn set_password(setup_url: &str, password: &str) {
    let done = http()
        .post(setup_url)
        .form(&[("password", password), ("confirm", password)])
        .send()
        .await
        .unwrap();
    let status = done.status();
    assert_eq!(status, StatusCode::OK, "{}", done.text().await.unwrap());
}

/// A PKCE pair: the verifier and its S256 challenge.
fn pkce() -> (String, String) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let verifier = format!("e2e-verifier-{nanos:040}");
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Signs in with the code flow; panics if it fails.
pub async fn sign_in(email: &str, password: &str) -> Tokens {
    try_sign_in(email, password)
        .await
        .unwrap_or_else(|e| panic!("{email} couldn't sign in: {e}"))
}

/// Signs in with the code flow as `sekisho-cli`. `Err` is the OAuth error
/// Sekisho redirected with, or Dex refusing the password.
pub async fn try_sign_in(email: &str, password: &str) -> Result<Tokens, String> {
    let (verifier, challenge) = pkce();
    let mut authorize = Url::parse(&format!("{ADMIN}/oauth/authorize")).unwrap();
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLI)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("state", "e2e-state")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    let mut browser = Browser::new();
    let first = browser.get_when_ready(&authorize).await;
    let Arrival::Redirect(back) = browser
        .through_dex(first, authorize, email, password, |u| {
            u.as_str().starts_with(REDIRECT)
        })
        .await?
    else {
        return Err("the sign-in ended on a page, not at the client".into());
    };
    let q: BTreeMap<String, String> = back.query_pairs().into_owned().collect();
    // RFC 9207: every answer names the issuer.
    assert_eq!(q.get("iss").map(String::as_str), Some(ADMIN), "{back}");
    assert_eq!(
        q.get("state").map(String::as_str),
        Some("e2e-state"),
        "{back}"
    );
    if let Some(error) = q.get("error") {
        return Err(error.clone());
    }
    let resp = http()
        .post(format!("{ADMIN}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLI),
            ("code", &q["code"]),
            ("redirect_uri", REDIRECT),
            ("code_verifier", &verifier),
        ])
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(Tokens::from(&body))
}

/// Signs in with the device flow: the device asks for a code, a browser
/// enters it, confirms and signs in at Dex, and the device polls once.
pub async fn device_sign_in(email: &str, password: &str) -> Tokens {
    let start = Instant::now();
    let started: Value = loop {
        let resp = http()
            .post(format!("{ADMIN}/oauth/device_authorization"))
            .form(&[("client_id", CLI)])
            .send()
            .await
            .unwrap();
        if resp.status() == StatusCode::OK {
            break resp.json().await.unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "device_authorization: {}",
            resp.status()
        );
        sleep(Duration::from_millis(250)).await;
    };
    let user_code = started["user_code"].as_str().unwrap();
    let device_code = started["device_code"].as_str().unwrap();
    assert_eq!(
        started["verification_uri"],
        format!("{ADMIN}/oauth/device"),
        "{started}"
    );

    let mut browser = Browser::new();
    let page = browser
        .post(
            &format!("{ADMIN}/oauth/device"),
            &[("user_code", user_code)],
        )
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    let text = page.text().await.unwrap();
    let csrf = hidden(&text, "csrf").expect("the confirmation form's token");
    let confirm = Url::parse(&format!("{ADMIN}/oauth/device/confirm")).unwrap();
    let to_dex = browser
        .post(
            confirm.as_str(),
            &[
                ("user_code", user_code),
                ("decision", "continue"),
                ("csrf", &csrf),
            ],
        )
        .await;
    match browser
        .through_dex(to_dex, confirm, email, password, |_| false)
        .await
    {
        Ok(Arrival::Page(status, body)) => {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(
                body.contains("Signed in. Return to your terminal."),
                "{body}"
            );
        }
        other => panic!("the device sign-in didn't finish: {other:?}"),
    }
    let resp = http()
        .post(format!("{ADMIN}/oauth/token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", CLI),
            ("device_code", device_code),
        ])
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    Tokens::from(&body)
}

/// Refreshes the session: the new tokens, or the error answer.
pub async fn refresh(tokens: &Tokens) -> Result<Tokens, (StatusCode, Value)> {
    let resp = http()
        .post(format!("{ADMIN}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLI),
            ("refresh_token", &tokens.refresh),
        ])
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    if status == StatusCode::OK {
        Ok(Tokens::from(&body))
    } else {
        Err((status, body))
    }
}

/// Logs out: revokes the session by its refresh token (RFC 7009).
pub async fn revoke(tokens: &Tokens) {
    let resp = http()
        .post(format!("{ADMIN}/oauth/revoke"))
        .form(&[("token", tokens.refresh.as_str()), ("client_id", CLI)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Where a walk through Dex ended.
#[derive(Debug)]
pub enum Arrival {
    /// A redirect the caller was waiting for.
    Redirect(Url),
    /// A page from Sekisho.
    Page(StatusCode, String),
}

/// A browser: no automatic redirects, and one cookie jar for 127.0.0.1
/// (cookies are scoped by host, not port, so Sekisho's and Dex's share it).
pub struct Browser {
    http: Client,
    cookies: BTreeMap<String, String>,
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}

impl Browser {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            cookies: BTreeMap::new(),
        }
    }

    async fn send(&mut self, mut request: RequestBuilder) -> Response {
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.header(COOKIE, jar);
        }
        let response = request.send().await.unwrap();
        for set in response.headers().get_all(SET_COOKIE) {
            let set = set.to_str().unwrap();
            let Some((name, value)) = set.split(';').next().and_then(|p| p.split_once('=')) else {
                continue;
            };
            if set.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        response
    }

    pub async fn get(&mut self, url: &str) -> Response {
        let request = self.http.get(url);
        self.send(request).await
    }

    pub async fn post(&mut self, url: &str, form: &[(&str, &str)]) -> Response {
        let request = self.http.post(url).form(form);
        self.send(request).await
    }

    /// A GET that waits out Sekisho's 503 while it registers at Dex.
    pub async fn get_when_ready(&mut self, url: &Url) -> Response {
        let start = Instant::now();
        loop {
            let response = self.get(url.as_str()).await;
            if response.status() != StatusCode::SERVICE_UNAVAILABLE
                || start.elapsed() > Duration::from_secs(30)
            {
                return response;
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// Follows `first` (an answer for `here`) through Dex, choosing its
    /// password connector and filling its login form, until a redirect that
    /// `done` accepts or a page from Sekisho. `Err` when Dex refuses the
    /// password.
    pub async fn through_dex(
        &mut self,
        first: Response,
        mut here: Url,
        email: &str,
        password: &str,
        done: impl Fn(&Url) -> bool,
    ) -> Result<Arrival, String> {
        let mut response = first;
        let mut posted = false;
        for _ in 0..25 {
            let status = response.status();
            if status.is_redirection() {
                let location = response.headers()[LOCATION].to_str().unwrap();
                let next = here.join(location).unwrap();
                if done(&next) {
                    return Ok(Arrival::Redirect(next));
                }
                response = self.get(next.as_str()).await;
                here = next;
                continue;
            }
            let body = response.text().await.unwrap();
            if !here.as_str().starts_with(DEX) {
                return Ok(Arrival::Page(status, body));
            }
            if let Some(action) = form_action(&body) {
                if posted {
                    return Err("Dex refused the password".into());
                }
                posted = true;
                let next = here.join(&action).unwrap();
                response = self
                    .post(next.as_str(), &[("login", email), ("password", password)])
                    .await;
                here = next;
                continue;
            }
            // Dex's chooser, when it has more than one connector.
            if let Some(local) = link_to(&body, "/auth/local") {
                let next = here.join(&local).unwrap();
                response = self.get(next.as_str()).await;
                here = next;
                continue;
            }
            return Err(format!("{status} at {here}: {body}"));
        }
        Err("no end after 25 steps".into())
    }
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#43;", "+")
        .replace("&#61;", "=")
        .replace("&#39;", "'")
        .replace("&#34;", "\"")
        .replace("&quot;", "\"")
}

/// The action of the page's `POST` form.
pub fn form_action(html: &str) -> Option<String> {
    let form = &html[html.find("<form method=\"post\"")?..];
    let start = form.find("action=\"")? + "action=\"".len();
    let end = start + form[start..].find('"')?;
    Some(unescape(&form[start..end]))
}

/// The first link whose target contains `part`.
pub fn link_to(html: &str, part: &str) -> Option<String> {
    html.split("href=\"").skip(1).find_map(|rest| {
        let href = &rest[..rest.find('"')?];
        href.contains(part).then(|| unescape(href))
    })
}

/// The value of the hidden field `name`.
pub fn hidden(html: &str, name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html.find(&marker)? + marker.len();
    let end = start + html[start..].find('"')?;
    Some(unescape(&html[start..end]))
}
