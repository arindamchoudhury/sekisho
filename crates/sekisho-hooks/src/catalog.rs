//! Prices from Sekisho's copy of agentgateway's model catalog, for pricing
//! an estimate when a model has no admin price (design 9.3). The copy is
//! agentgateway v1.7.0's `catalog/model-catalog.json` and is replaced
//! whenever the agentgateway pin moves.

use std::collections::HashMap;
use std::sync::LazyLock;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::pricing::{ModelPrices, Rates};
use serde_json::Value;

const CATALOG_JSON: &str = include_str!("../catalog/agentgateway-v1.7.0.model-catalog.json");

/// Catalog provider name → model → prices. Entries without both an input
/// and an output price are left out.
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

/// The name agentgateway's catalog files a kind's models under (its
/// providers' `NAME`). OpenAI-compatible providers have none.
fn catalog_name(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Anthropic => Some("anthropic"),
        ProviderKind::OpenAi => Some("openai"),
        ProviderKind::Gemini => Some("gcp.gemini"),
        ProviderKind::Bedrock => Some("aws.bedrock"),
        ProviderKind::Azure => Some("azure"),
        ProviderKind::OpenAiCompatible => None,
    }
}

/// The catalog's prices for `model` on a provider of `kind`, with any
/// missing cache price taken from `format`'s usual ratio.
pub fn catalog_rates(kind: ProviderKind, format: ApiFormat, model: &str) -> Option<Rates> {
    CATALOG
        .get(catalog_name(kind)?)?
        .get(model)
        .map(|prices| prices.rates(format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekisho_core::money::MicroUsd;

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
            catalog_rates(
                ProviderKind::Anthropic,
                ApiFormat::Anthropic,
                "claude-fable-5"
            ),
            Some(rates(10_000_000, 50_000_000, 1_000_000, 12_500_000))
        );
        // No cacheWrite in the catalog: the OpenAI format's default (the input price).
        assert_eq!(
            catalog_rates(ProviderKind::OpenAi, ApiFormat::OpenAi, "gpt-4o-2024-08-06"),
            Some(rates(2_500_000, 10_000_000, 1_250_000, 2_500_000))
        );
        assert_eq!(
            catalog_rates(ProviderKind::OpenAi, ApiFormat::OpenAi, "no-such-model"),
            None
        );
        assert_eq!(
            catalog_rates(ProviderKind::OpenAiCompatible, ApiFormat::OpenAi, "gpt-4o"),
            None
        );
    }

    #[test]
    fn every_kind_has_its_catalog_name() {
        // Bedrock and Azure models sit under agentgateway's provider names.
        let any_bedrock = CATALOG_JSON.contains("\"aws.bedrock\"");
        let any_azure = CATALOG_JSON.contains("\"azure\"");
        assert!(any_bedrock && any_azure);
        assert_eq!(catalog_name(ProviderKind::Gemini), Some("gcp.gemini"));
        assert_eq!(catalog_name(ProviderKind::Bedrock), Some("aws.bedrock"));
        assert_eq!(catalog_name(ProviderKind::Azure), Some("azure"));
        assert_eq!(catalog_name(ProviderKind::OpenAiCompatible), None);
    }

    /// agentgateway's committed blob at 26f5aeb (784,448 bytes, LF only).
    #[test]
    fn the_copy_is_agentgateways_file() {
        assert_eq!(CATALOG_JSON.len(), 784_448);
        assert!(!CATALOG_JSON.contains('\r'));
    }
}
