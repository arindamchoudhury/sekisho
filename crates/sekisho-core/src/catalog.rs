//! Prices from Sekisho's copy of agentgateway's model catalog: for pricing
//! an estimate when a model has no admin price, and for telling whether an
//! OpenAI-compatible host's model needs one (design 9.3). The copy is
//! agentgateway v1.7.0's `catalog/model-catalog.json` and is replaced
//! whenever the agentgateway pin moves.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;

use crate::money::MicroUsd;
use crate::pricing::{CacheDefaults, ModelPrices, Rates};

const CATALOG_JSON: &str = include_str!("../catalog/agentgateway-v1.7.0.model-catalog.json");

/// Catalog key → model → prices. Entries without both an input and an
/// output price are left out.
static CATALOG: LazyLock<HashMap<String, HashMap<String, ModelPrices>>> =
    LazyLock::new(|| parse(CATALOG_JSON));

fn parse(json: &str) -> HashMap<String, HashMap<String, ModelPrices>> {
    let catalog: Value = serde_json::from_str(json).expect("the catalog copy is valid JSON");
    let mut providers = HashMap::new();
    for (provider, entry) in catalog["providers"].as_object().into_iter().flatten() {
        let mut models = HashMap::new();
        for (model, entry) in entry["models"].as_object().into_iter().flatten() {
            let rate = |name: &str| {
                entry["rates"][name]
                    .as_str()
                    .and_then(|usd| MicroUsd::from_usd_str(usd).ok())
            };
            if let (Some(input), Some(output)) = (rate("input"), rate("output")) {
                let prices = ModelPrices {
                    input,
                    output,
                    cache_read: rate("cacheRead"),
                    cache_write: rate("cacheWrite"),
                };
                models.insert(model.clone(), prices);
            }
        }
        providers.insert(provider.clone(), models);
    }
    providers
}

/// The catalog's prices for `model` under `catalog_key` (a built-in kind's
/// [`crate::formats::ProviderKind::catalog_key`], or an OpenAI-compatible
/// host's), with any missing cache price taken from `defaults`.
pub fn rates(catalog_key: &str, defaults: CacheDefaults, model: &str) -> Option<Rates> {
    CATALOG
        .get(catalog_key)?
        .get(model)
        .map(|prices| prices.rates(defaults))
}

/// Whether the catalog has both an input and an output price for `model`
/// under `catalog_key`.
pub fn knows(catalog_key: &str, model: &str) -> bool {
    CATALOG
        .get(catalog_key)
        .is_some_and(|models| models.contains_key(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::ProviderKind;
    use crate::money::MicroUsd;

    fn rates(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Rates {
        Rates {
            input: MicroUsd(input),
            output: MicroUsd(output),
            cache_read: MicroUsd(cache_read),
            cache_write: MicroUsd(cache_write),
        }
    }

    #[test]
    fn catalog_prices_known_models() {
        assert_eq!(
            super::rates("anthropic", CacheDefaults::Anthropic, "claude-fable-5"),
            Some(rates(10_000_000, 50_000_000, 1_000_000, 12_500_000))
        );
        // No cacheWrite in the catalog: OpenAI's default, the input price.
        assert_eq!(
            super::rates("openai", CacheDefaults::OpenAi, "gpt-4o-2024-08-06"),
            Some(rates(2_500_000, 10_000_000, 1_250_000, 2_500_000))
        );
        assert_eq!(
            super::rates("openai", CacheDefaults::OpenAi, "no-such-model"),
            None
        );
        assert_eq!(
            super::rates("no-such-host", CacheDefaults::OpenAi, "gpt-4o"),
            None
        );
    }

    #[test]
    fn every_kind_has_its_catalog_key() {
        // Bedrock and Azure models sit under agentgateway's provider names.
        for (kind, key) in [
            (ProviderKind::Anthropic, Some("anthropic")),
            (ProviderKind::OpenAi, Some("openai")),
            (ProviderKind::Gemini, Some("gcp.gemini")),
            (ProviderKind::Bedrock, Some("aws.bedrock")),
            (ProviderKind::Azure, Some("azure")),
            (ProviderKind::OpenAiCompatible, None),
        ] {
            assert_eq!(kind.catalog_key(), key, "{kind:?}");
            if let Some(key) = key {
                assert!(CATALOG.contains_key(key), "the copy has no {key}");
            }
        }
    }

    #[test]
    fn the_catalog_is_read_by_key() {
        // In the copy at 0.2 / 0.6 USD per million, with no cache prices.
        const MODEL: &str = "Qwen/Qwen3-235B-A22B-Instruct-2507-tput";
        assert_eq!(
            super::rates("togetherai", CacheDefaults::InputPrice, MODEL),
            Some(rates(200_000, 600_000, 200_000, 200_000))
        );
        assert!(knows("togetherai", MODEL));
        assert!(!knows("scaleway", MODEL));
        assert!(!knows("openai", MODEL));
        assert!(!knows("togetherai", "no-such-model"));
        assert_eq!(
            super::rates("scaleway", CacheDefaults::InputPrice, MODEL),
            None
        );
    }

    /// Entries without both an input and an output price aren't known: an
    /// estimate couldn't price them.
    #[test]
    fn entries_without_both_prices_are_left_out() {
        let parsed = parse(
            r#"{"providers": {"host": {"models": {
                "both": {"rates": {"input": "1", "output": "2"}},
                "input-only": {"rates": {"input": "1"}},
                "output-only": {"rates": {"output": "2"}},
                "bad": {"rates": {"input": "x", "output": "2"}}
            }}}}"#,
        );
        let models = &parsed["host"];
        assert!(models.contains_key("both"));
        assert_eq!(models.len(), 1, "{:?}", models.keys().collect::<Vec<_>>());
    }

    /// agentgateway's committed blob at 26f5aeb (784,448 bytes, LF only).
    #[test]
    fn the_copy_is_agentgateways_file() {
        assert_eq!(CATALOG_JSON.len(), 784_448);
        assert!(!CATALOG_JSON.contains('\r'));
    }
}
