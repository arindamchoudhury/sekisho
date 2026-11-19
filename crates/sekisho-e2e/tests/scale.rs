//! Many environments at the gateway (plan 1d, design risk 9): one `llm:`
//! block with a model entry per environment and model. It leaves 300
//! environments behind, which make each later config load take about half
//! a second, longer than `wait_route` waits, so run it last; `make e2e`
//! leaves it out of both runs of the suite and runs it once after them:
//!
//! ```text
//! cargo test -p sekisho-e2e --test scale -- --ignored --nocapture
//! docker compose -f e2e/docker-compose.yml down -v
//! ```

use std::time::Duration;

use reqwest::StatusCode;
use sekisho_e2e::*;
use serde_json::json;
use tokio::time::{Instant, sleep};

const MORE_ENVIRONMENTS: usize = 300;
const MODELS: [&str; 5] = [
    "scale-model-0",
    "scale-model-1",
    "scale-model-2",
    "scale-model-3",
    "scale-model-4",
];

/// The median time of `n` one-chunk chat requests to `env`'s first model,
/// after five to warm up.
async fn median_latency(env: &str, token: &str, n: usize) -> Duration {
    let client = http();
    let mut times = Vec::new();
    for i in 0..n + 5 {
        let started = Instant::now();
        let resp = chat_request(
            &client,
            &format!("/env/{env}/openai"),
            Some(token),
            MODELS[0],
            1,
            0,
            false,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{env}");
        resp.bytes().await.unwrap();
        if i >= 5 {
            times.push(started.elapsed());
        }
    }
    median(times)
}

#[tokio::test]
#[ignore]
async fn many_environments_add_no_latency() {
    seed().await;
    for model in MODELS {
        ensure(
            "/api/v1/models",
            json!({ "name": model, "provider": "mock", "input_price": "3", "output_price": "15" }),
        )
        .await;
    }
    let first = unique("via-scale-first");
    let saved = Instant::now();
    put_env_models(&first, &MODELS).await;
    let (_, token) = new_key(&first).await;
    // Not `wait_route`, whose fixed wait holds for small configs only: on
    // a stack that already has many environments a load takes longer.
    let live = wait_served(&first, &token, saved).await;
    println!("our first environment was live {live:?} after it was saved");
    let before = median_latency(&first, &token, 50).await;

    let started = Instant::now();
    let mut last = String::new();
    for i in 0..MORE_ENVIRONMENTS {
        last = unique(&format!("via-scale-{i:03}"));
        put_env_models(&last, &MODELS).await;
    }
    println!(
        "added {MORE_ENVIRONMENTS} environments x {} models in {:?}",
        MODELS.len(),
        started.elapsed()
    );
    let last_saved = Instant::now();
    let (_, last_token) = new_key(&last).await;
    // Not `wait_route`, whose fixed wait assumes agentgateway applies a
    // config in milliseconds: at about 1,000 environments each load takes
    // about half a second, and a burst of changes is applied one load
    // after another.
    let live = wait_served(&last, &last_token, last_saved).await;
    println!("the last of the {MORE_ENVIRONMENTS} was live {live:?} after it was saved");
    let after = median_latency(&last, &last_token, 50).await;
    println!(
        "median latency: {before:?} with one environment of ours, {after:?} with {} more",
        MORE_ENVIRONMENTS
    );
    assert!(
        after.saturating_sub(before) < Duration::from_millis(2),
        "the median grew by {:?}",
        after.saturating_sub(before)
    );

    // A new environment is still live within 5 s of being saved.
    let newest = unique("via-scale-newest");
    let saved = Instant::now();
    put_env_models(&newest, &MODELS).await;
    let (_, newest_token) = new_key(&newest).await;
    let live = wait_served(&newest, &newest_token, saved).await;
    println!("a new environment was live {live:?} after it was saved");
    println!("gateway config: {} bytes", gateway_config().len());
}

/// Polls a model request to `env` until it is served, and returns the time
/// since `saved`. Fails after 5 s, the design's target for a change to be
/// live at the gateway.
async fn wait_served(env: &str, token: &str, saved: Instant) -> Duration {
    loop {
        let resp = chat_request(
            &http(),
            &format!("/env/{env}/openai"),
            Some(token),
            MODELS[0],
            1,
            0,
            false,
        )
        .send()
        .await
        .unwrap();
        if resp.status() == StatusCode::OK {
            return saved.elapsed();
        }
        assert!(
            saved.elapsed() < Duration::from_secs(5),
            "{env} not live within 5 s: {}",
            resp.status()
        );
        sleep(Duration::from_millis(50)).await;
    }
}
