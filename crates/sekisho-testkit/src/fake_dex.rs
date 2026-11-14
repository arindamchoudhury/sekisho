//! A Dex in memory, for the sign-in tests: who each authorisation code signs
//! in, what refreshes answer and how long they take, and every refresh token
//! it was shown.
//!
//! Like Dex v2.46.1 it keeps one refresh token per person (a second sign-in
//! replaces the first) and rotates it on every refresh, so a token shown
//! twice is refused the second time. Like a server, it finishes a refresh it
//! was shown even when the caller stops waiting for the answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_auth::dex::{Dex, DexConnector, DexError, DexIdentity, DexPassword};
use sekisho_core::names::ConnectorId;
use sha2::{Digest, Sha256};
use url::Url;

/// Where [`FakeDex::authorize_url`] sends browsers.
pub const FAKE_DEX_ISSUER: &str = "http://dex.test/dex";

#[derive(Default)]
struct Inner {
    /// Authorisation codes not yet used, and whom they sign in.
    codes: HashMap<String, DexIdentity>,
    /// The PKCE challenge each sign-in's nonce came with.
    challenges: HashMap<String, String>,
    /// What a refresh answers for each subject.
    people: HashMap<String, DexIdentity>,
    /// Live refresh tokens and their subjects.
    tokens: HashMap<String, String>,
    /// Refreshes of these subjects fail this way.
    refresh_failures: HashMap<String, DexError>,
    refresh_delay: Duration,
    sign_in_delay: Duration,
    sign_in_failure: Option<DexError>,
    grpc_failure: Option<DexError>,
    withhold_refresh_tokens: bool,
    shown: Vec<String>,
    minted: u64,
    passwords: HashMap<String, DexPassword>,
    connectors: HashMap<String, (DexConnector, serde_json::Value)>,
    revoked: Vec<String>,
}

impl Inner {
    /// A new refresh token for `subject`, replacing any earlier one.
    fn mint(&mut self, subject: &str) -> String {
        self.minted += 1;
        let token = format!("dex-refresh-{}", self.minted);
        self.tokens.retain(|_, s| s != subject);
        self.tokens.insert(token.clone(), subject.to_owned());
        token
    }
}

/// Replays `refresh_token`: refused unless it is its subject's live token;
/// otherwise rotated, unless a failure is set for the subject.
fn refresh_now(inner: &mut Inner, refresh_token: &str) -> Result<DexIdentity, DexError> {
    let subject = inner.tokens.get(refresh_token).cloned().ok_or_else(|| {
        DexError::Refused(
            "invalid_grant: Refresh token is invalid or has already been claimed by \
                 another client."
                .to_owned(),
        )
    })?;
    if let Some(e) = inner.refresh_failures.get(&subject) {
        return Err(e.clone());
    }
    let who = inner
        .people
        .get(&subject)
        .cloned()
        .expect("a live token's subject has signed in");
    inner.tokens.remove(refresh_token);
    let token = inner.mint(&subject);
    Ok(DexIdentity {
        refresh_token: Some(token),
        ..who
    })
}

/// A [`Dex`] in memory. Cheap to share: every method takes `&self`.
#[derive(Default)]
pub struct FakeDex {
    inner: Arc<Mutex<Inner>>,
}

impl FakeDex {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("FakeDex's lock")
    }

    /// Dex's sign-in page: `code` signs in `who`, once. Their refresh token is
    /// made at the exchange.
    pub fn add_code(&self, code: &str, who: DexIdentity) {
        self.inner().codes.insert(code.to_owned(), who);
    }

    /// What refreshes of `who.subject` answer from now on (its refresh token
    /// is ignored).
    pub fn set_identity(&self, who: DexIdentity) {
        self.inner().people.insert(who.subject.clone(), who);
    }

    /// Refreshes of `subject` fail this way, until `None`; the token stays.
    pub fn fail_refresh(&self, subject: &str, error: Option<DexError>) {
        let mut inner = self.inner();
        match error {
            Some(e) => inner.refresh_failures.insert(subject.to_owned(), e),
            None => inner.refresh_failures.remove(subject),
        };
    }

    /// Every refresh waits this long before it answers.
    pub fn delay_refresh(&self, delay: Duration) {
        self.inner().refresh_delay = delay;
    }

    /// Every code exchange takes this long to answer, after Dex has made the
    /// new refresh token.
    pub fn delay_sign_in(&self, delay: Duration) {
        self.inner().sign_in_delay = delay;
    }

    /// Code exchanges fail this way, until `None`.
    pub fn fail_sign_in(&self, error: Option<DexError>) {
        self.inner().sign_in_failure = error;
    }

    /// Every gRPC call (passwords, connectors, revocation, the client) fails
    /// this way, until `None`.
    pub fn fail_grpc(&self, error: Option<DexError>) {
        self.inner().grpc_failure = error;
    }

    /// Sign-ins answer without a refresh token, as without `offline_access`.
    pub fn withhold_refresh_tokens(&self, withhold: bool) {
        self.inner().withhold_refresh_tokens = withhold;
    }

    /// Ends the subject's sign-in at Dex: its refresh token is refused from
    /// now on.
    pub fn revoke(&self, subject: &str) {
        self.inner().tokens.retain(|_, s| s != subject);
    }

    /// Every refresh token shown to [`Dex::refresh`], in order.
    pub fn refreshes_shown(&self) -> Vec<String> {
        self.inner().shown.clone()
    }

    /// The subject's refresh token as Dex holds it now.
    pub fn live_token(&self, subject: &str) -> Option<String> {
        self.inner()
            .tokens
            .iter()
            .find(|(_, s)| *s == subject)
            .map(|(t, _)| t.clone())
    }

    /// The password entry for `email`, as Sekisho last put it.
    pub fn password(&self, email: &str) -> Option<DexPassword> {
        self.inner().passwords.get(email).cloned()
    }

    /// The subjects [`Dex::revoke_refresh`] was called for, in order.
    pub fn revoked(&self) -> Vec<String> {
        self.inner().revoked.clone()
    }

    fn grpc(&self) -> Result<MutexGuard<'_, Inner>, DexError> {
        let inner = self.inner();
        match &inner.grpc_failure {
            Some(e) => Err(e.clone()),
            None => Ok(inner),
        }
    }
}

#[async_trait]
impl Dex for FakeDex {
    fn authorize_url(&self, state: &str, nonce: &str, code_challenge: &str) -> Url {
        self.inner()
            .challenges
            .insert(nonce.to_owned(), code_challenge.to_owned());
        let mut url = Url::parse(&format!("{FAKE_DEX_ISSUER}/auth")).expect("a URL");
        url.query_pairs_mut()
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        url
    }

    async fn sign_in(
        &self,
        code: &str,
        code_verifier: &str,
        nonce: &str,
    ) -> Result<DexIdentity, DexError> {
        let (answer, delay) = {
            let mut inner = self.inner();
            if let Some(e) = &inner.sign_in_failure {
                return Err(e.clone());
            }
            let who = inner
                .codes
                .remove(code)
                .ok_or_else(|| DexError::Refused("invalid_grant: unknown code".to_owned()))?;
            let challenge = inner
                .challenges
                .get(nonce)
                .ok_or_else(|| DexError::Invalid("ID token has another nonce".to_owned()))?;
            if URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes())) != *challenge {
                return Err(DexError::Refused(
                    "invalid_grant: the PKCE verifier doesn't match".to_owned(),
                ));
            }
            let token = inner.mint(&who.subject);
            inner.people.insert(who.subject.clone(), who.clone());
            let answer = DexIdentity {
                refresh_token: (!inner.withhold_refresh_tokens).then_some(token),
                ..who
            };
            (answer, inner.sign_in_delay)
        };
        // Dex has made the new token; only the answer is still on its way.
        tokio::time::sleep(delay).await;
        Ok(answer)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<DexIdentity, DexError> {
        let delay = {
            let mut inner = self.inner();
            inner.shown.push(refresh_token.to_owned());
            inner.refresh_delay
        };
        // Answered on a task of its own, as a server would.
        let inner = Arc::clone(&self.inner);
        let token = refresh_token.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            refresh_now(&mut inner.lock().expect("FakeDex's lock"), &token)
        })
        .await
        .expect("FakeDex's refresh")
    }

    async fn put_password(&self, p: &DexPassword) -> Result<(), DexError> {
        self.grpc()?.passwords.insert(p.email.clone(), p.clone());
        Ok(())
    }

    async fn delete_password(&self, email: &str) -> Result<(), DexError> {
        self.grpc()?.passwords.remove(email);
        Ok(())
    }

    async fn revoke_refresh(&self, subject: &str) -> Result<(), DexError> {
        let mut inner = self.grpc()?;
        inner.tokens.retain(|_, s| s != subject);
        inner.revoked.push(subject.to_owned());
        Ok(())
    }

    async fn connectors(&self) -> Result<Vec<DexConnector>, DexError> {
        let mut listed: Vec<DexConnector> = self
            .grpc()?
            .connectors
            .values()
            .map(|(c, _)| c.clone())
            .collect();
        listed.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(listed)
    }

    async fn put_connector(
        &self,
        c: &DexConnector,
        config: &serde_json::Value,
    ) -> Result<(), DexError> {
        self.grpc()?
            .connectors
            .insert(c.id.as_str().to_owned(), (c.clone(), config.clone()));
        Ok(())
    }

    async fn delete_connector(&self, id: &ConnectorId) -> Result<bool, DexError> {
        Ok(self.grpc()?.connectors.remove(id.as_str()).is_some())
    }

    async fn ensure_client(&self) -> Result<(), DexError> {
        self.grpc().map(drop)
    }
}
