//! What the gateway config is rendered from: a read of the store.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;

use sekisho_core::formats::{HostApis, ProviderKind};
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sekisho_core::secret_box::SecretBox;
use sekisho_store::{ProviderCredential, ProviderOptions, Store, StoreError};

/// Where agentgateway reaches Sekisho's hooks, each as `host:port`, and the
/// secret its rate-limit calls carry.
#[derive(Clone, PartialEq, Eq)]
pub struct HookTargets {
    /// The HTTP `extAuthz` service.
    pub authz_http: String,
    /// The gRPC rate-limit and `ext_proc` services.
    pub grpc: String,
    /// Sent in every rate-limit descriptor, so the charging hook only
    /// records spend that this install's agentgateway reports.
    pub charge_secret: String,
}

/// Leaves out the charge secret.
impl fmt::Debug for HookTargets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookTargets")
            .field("authz_http", &self.authz_http)
            .field("grpc", &self.grpc)
            .finish_non_exhaustive()
    }
}

/// A provider some environment uses, written once in the config with its
/// credential (`llm.providers`).
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub name: ProviderName,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    /// The kind's settings: Bedrock's region, Azure's endpoint.
    pub options: ProviderOptions,
    /// The decrypted credential, written into the gateway config.
    pub credential: ProviderCredential,
    /// The APIs an OpenAI-compatible host serves. Until providers store
    /// theirs (plan 1d Task 4), Chat Completions only.
    pub apis: HostApis,
    /// The key an OpenAI-compatible host's prices are filed under in
    /// agentgateway's catalog; its name when `None`.
    pub catalog_key: Option<String>,
}

/// Shows only the credential's shape, so snapshots can be logged.
impl fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("options", &self.options)
            .field("credential", &self.credential.kind())
            .field("apis", &self.apis)
            .field("catalog_key", &self.catalog_key)
            .finish()
    }
}

/// One model an environment lists, and the provider that serves it
/// (`llm.models`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub env: EnvName,
    pub model: ModelName,
    pub provider: ProviderName,
}

/// A model with admin-set prices, in micro-USD per 1,000,000 tokens, the key
/// the inline catalog files it under, and where its missing cache prices
/// come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricedModel {
    pub provider: ProviderName,
    pub kind: ProviderKind,
    /// The kind's own catalog key, or for an OpenAI-compatible provider its
    /// name (its `providerOverride`).
    pub catalog_key: String,
    pub model: ModelName,
    pub prices: ModelPrices,
    pub cache: CacheDefaults,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewaySnapshot {
    /// Only the providers some environment uses, sorted by name.
    pub providers: Vec<ProviderEntry>,
    /// By environment name, then the environment's own order.
    pub models: Vec<ModelEntry>,
    /// Sorted by provider, then model.
    pub priced_models: Vec<PricedModel>,
    pub hooks: HookTargets,
}

/// Reads providers, models and environments, decrypting the credential of
/// each provider an environment uses, once.
///
/// The three reads are separate, so a change committed between them can
/// leave a model that the environment list names but the model list lacks.
/// Such a model is skipped with a warning; the change's own notification
/// triggers the next build.
pub async fn build_snapshot(
    store: &Store,
    secret_box: &SecretBox,
    hooks: &HookTargets,
) -> Result<GatewaySnapshot, StoreError> {
    let providers = store.list_providers().await?;
    let models = store.list_models().await?;
    let mut environments = store.list_environments().await?;
    environments.sort_by(|a, b| a.spec.name.as_str().cmp(b.spec.name.as_str()));
    let provider_by_name: HashMap<&str, _> =
        providers.iter().map(|p| (p.name.as_str(), p)).collect();
    let model_by_name: HashMap<&str, _> = models.iter().map(|m| (m.name.as_str(), m)).collect();
    // Each provider an environment uses, as saved together with its
    // credential (see below).
    let mut used: HashMap<_, ProviderEntry> = HashMap::new();

    let mut entries = Vec::new();
    for env in &environments {
        for name in &env.spec.models {
            let Some(provider) = model_by_name
                .get(name.as_str())
                .and_then(|m| provider_by_name.get(m.provider.as_str()))
            else {
                tracing::warn!(env = %env.spec.name, model = %name, "model changed while rendering; skipped");
                continue;
            };
            if let Entry::Vacant(slot) = used.entry(provider.id) {
                // The credential, base URL and settings come from one read
                // of the provider, not the list above: a change between the
                // two could otherwise send the new credential to the old
                // host. The name and kind never change.
                let (current, credential) = store
                    .provider_with_credential(secret_box, provider.id)
                    .await?;
                slot.insert(ProviderEntry {
                    name: provider.name.clone(),
                    kind: provider.kind,
                    base_url: current.base_url,
                    options: current.options,
                    credential,
                    apis: HostApis::chat_only(),
                    catalog_key: None,
                });
            }
            entries.push(ModelEntry {
                env: env.spec.name.clone(),
                model: name.clone(),
                provider: provider.name.clone(),
            });
        }
    }
    let mut provider_entries: Vec<ProviderEntry> = used.into_values().collect();
    provider_entries.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    let mut priced_models: Vec<PricedModel> = models
        .iter()
        .filter_map(|m| {
            // A provider deleted since the list was read leaves its model out.
            provider_by_name.get(m.provider.as_str())?;
            Some(PricedModel {
                provider: m.provider.clone(),
                kind: m.provider_kind,
                catalog_key: m
                    .provider_kind
                    .catalog_key()
                    .unwrap_or(m.provider.as_str())
                    .to_owned(),
                model: m.name.clone(),
                prices: m.prices()?,
                cache: CacheDefaults::for_model(m.provider_kind, m.name.as_str()),
            })
        })
        .collect();
    priced_models.sort_by(|a, b| {
        (a.provider.as_str(), a.model.as_str()).cmp(&(b.provider.as_str(), b.model.as_str()))
    });
    Ok(GatewaySnapshot {
        providers: provider_entries,
        models: entries,
        priced_models,
        hooks: hooks.clone(),
    })
}
