//! An A2A agent's card, fetched and pinned at sync (D163, design 9.8):
//! from the entry's `card_url`, or else `<url>/.well-known/agent-card.json`
//! (v1.0), trying `<url>/.well-known/agent.json` (v0.2) only when that
//! answers 404; at most 1 MiB, checked by `sekisho_catalog::card`.

use http::header::{ACCEPT, HeaderMap, HeaderValue};
use sekisho_catalog::card::{CARD_LIMIT, check_card};
use sekisho_catalog::model::{A2aAgentSpec, PinnedCard};
use url::Url;

use super::outbound::Outbound;

const V10_PATH: &str = "/.well-known/agent-card.json";
const V02_PATH: &str = "/.well-known/agent.json";

pub async fn fetch_card(
    out: &Outbound,
    spec: &A2aAgentSpec,
    allow_private: bool,
) -> Result<PinnedCard, String> {
    let base = spec.url.trim_end_matches('/');
    let candidates: Vec<String> = match &spec.card_url {
        Some(url) => vec![url.clone()],
        None => vec![format!("{base}{V10_PATH}"), format!("{base}{V02_PATH}")],
    };
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let last = candidates.len() - 1;
    for (i, candidate) in candidates.iter().enumerate() {
        let url = Url::parse(candidate).map_err(|_| "the card's URL isn't a URL")?;
        let answer = out
            .get(&url, allow_private, headers.clone(), CARD_LIMIT)
            .await
            .map_err(|e| {
                if e.starts_with("the answer is larger") {
                    "the card is larger than 1 MiB".to_owned()
                } else {
                    e
                }
            })?;
        if answer.status == 404 && i < last {
            continue;
        }
        if answer.status != 200 {
            return Err(format!("the card answered HTTP {}", answer.status));
        }
        return check_card(&answer.body);
    }
    Err("the card answered HTTP 404".to_owned())
}
