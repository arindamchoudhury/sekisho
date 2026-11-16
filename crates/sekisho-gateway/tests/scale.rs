//! How the rendered config grows with the number of environments (plan 1d,
//! design risk 9). Ignored by default; run it in a release build:
//!
//! ```text
//! cargo test --release -p sekisho-gateway --test scale -- --ignored --nocapture
//! ```

use std::time::Instant;

use sekisho_core::formats::{HostApis, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sekisho_gateway::{
    GatewaySnapshot, HookTargets, ModelEntry, PricedModel, ProviderEntry, RenderOptions, render,
    validate,
};
use sekisho_store::{ProviderCredential, ProviderOptions};

const ENVIRONMENTS: usize = 1_000;
/// Five providers of different kinds, two models each.
const PROVIDERS: [(&str, ProviderKind, Option<&str>); 5] = [
    ("anthropic", ProviderKind::Anthropic, None),
    ("openai", ProviderKind::OpenAi, None),
    ("gemini", ProviderKind::Gemini, None),
    ("bedrock", ProviderKind::Bedrock, None),
    (
        "host",
        ProviderKind::OpenAiCompatible,
        Some("https://api.example.com/v1"),
    ),
];

fn key(provider: &str) -> String {
    format!("{provider}-scale-key-0123456789")
}

fn snapshot() -> GatewaySnapshot {
    let providers = PROVIDERS
        .iter()
        .map(|(name, kind, base_url)| ProviderEntry {
            name: ProviderName::parse(name).unwrap(),
            kind: *kind,
            base_url: base_url.map(String::from),
            options: match kind {
                ProviderKind::Bedrock => ProviderOptions::Bedrock {
                    region: "us-east-1".into(),
                },
                _ => ProviderOptions::None,
            },
            credential: ProviderCredential::Key(key(name)),
            apis: HostApis::chat_only(),
            catalog_key: None,
        })
        .collect();
    let models: Vec<(String, &str, ProviderKind)> = PROVIDERS
        .iter()
        .flat_map(|(name, kind, _)| {
            (0..2).map(move |i| (format!("{name}-model-{i}"), *name, *kind))
        })
        .collect();
    let entries = (0..ENVIRONMENTS)
        .flat_map(|e| {
            let env = EnvName::parse(&format!("team-{e:04}")).unwrap();
            models.iter().map(move |(model, provider, _)| ModelEntry {
                env: env.clone(),
                model: ModelName::parse(model).unwrap(),
                provider: ProviderName::parse(provider).unwrap(),
            })
        })
        .collect();
    let priced_models = models
        .iter()
        .map(|(model, provider, kind)| PricedModel {
            provider: ProviderName::parse(provider).unwrap(),
            kind: *kind,
            catalog_key: kind.catalog_key().unwrap_or(provider).to_owned(),
            cache: CacheDefaults::for_model(*kind, model),
            model: ModelName::parse(model).unwrap(),
            prices: ModelPrices {
                input: MicroUsd(3_000_000),
                output: MicroUsd(15_000_000),
                cache_read: None,
                cache_write: None,
            },
        })
        .collect();
    GatewaySnapshot {
        providers,
        models: entries,
        priced_models,
        hooks: HookTargets {
            authz_http: "sekisho:9080".into(),
            grpc: "sekisho:9081".into(),
            charge_secret: "scale-charge-secret".into(),
        },
    }
}

#[test]
#[ignore]
fn render_scales_with_environments() {
    let snapshot = snapshot();
    let started = Instant::now();
    let yaml = render(&snapshot, &RenderOptions::default());
    let rendered = started.elapsed();
    let started = Instant::now();
    let valid = validate(&yaml);
    let validated = started.elapsed();
    println!(
        "{ENVIRONMENTS} environments x 10 models from 5 providers: {} model entries, \
         {} bytes of YAML; render {rendered:?}, validate {validated:?}",
        snapshot.models.len(),
        yaml.len()
    );
    assert_eq!(valid, Ok(()));
    for (name, _, _) in PROVIDERS {
        assert_eq!(yaml.matches(&key(name)).count(), 1, "{name}'s key");
    }
    assert!(rendered.as_secs_f64() < 1.0, "render took {rendered:?}");
    assert!(validated.as_secs_f64() < 5.0, "validate took {validated:?}");
}
