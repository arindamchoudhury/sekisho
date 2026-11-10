//! What the gateway config is rendered from: a read of the store.

use std::collections::HashMap;
use std::fmt;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
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

/// One environment's route for one API format, served by one provider.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub format: ApiFormat,
    pub provider: ProviderName,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    /// The kind's settings: Bedrock's region, Azure's endpoint.
    pub options: ProviderOptions,
    /// The decrypted credential, written into the gateway config.
    pub credential: ProviderCredential,
    /// The models this route allows, in the environment's order.
    pub models: Vec<ModelName>,
}

/// Shows only the credential's shape, so snapshots can be logged.
impl fmt::Debug for ModelRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRoute")
            .field("format", &self.format)
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("options", &self.options)
            .field("credential", &self.credential.kind())
            .field("models", &self.models)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvRoutes {
    pub env: EnvName,
    /// Sorted by format; empty when the environment lists no models.
    pub routes: Vec<ModelRoute>,
}

/// A model with admin-set prices, in micro-USD per 1,000,000 tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricedModel {
    pub provider: ProviderName,
    pub kind: ProviderKind,
    pub model: ModelName,
    pub input: MicroUsd,
    pub output: MicroUsd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewaySnapshot {
    /// Sorted by name.
    pub environments: Vec<EnvRoutes>,
    /// Sorted by provider, then model.
    pub priced_models: Vec<PricedModel>,
    pub hooks: HookTargets,
}

/// Reads providers, models and environments, decrypting each provider
/// credential that a route needs.
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
    let environments = store.list_environments().await?;
    let provider_by_name: HashMap<&str, _> =
        providers.iter().map(|p| (p.name.as_str(), p)).collect();
    let model_by_name: HashMap<&str, _> = models.iter().map(|m| (m.name.as_str(), m)).collect();
    let mut credentials: HashMap<_, ProviderCredential> = HashMap::new();

    let mut snapshot_envs = Vec::with_capacity(environments.len());
    for env in &environments {
        let mut routes: Vec<ModelRoute> = Vec::new();
        for name in &env.spec.models {
            let Some(provider) = model_by_name
                .get(name.as_str())
                .and_then(|m| provider_by_name.get(m.provider.as_str()))
            else {
                tracing::warn!(env = %env.spec.name, model = %name, "model changed while rendering; skipped");
                continue;
            };
            let format = provider.format;
            match routes.iter_mut().find(|r| r.format == format) {
                Some(route) if route.provider == provider.name => route.models.push(name.clone()),
                // The store refuses this on save; skip rather than route to two providers.
                Some(route) => tracing::warn!(
                    env = %env.spec.name, model = %name, provider = %provider.name,
                    route_provider = %route.provider, "two providers for one API format; model skipped"
                ),
                None => {
                    let credential = match credentials.get(&provider.id) {
                        Some(credential) => credential.clone(),
                        None => {
                            let credential =
                                store.provider_credential(secret_box, provider.id).await?;
                            credentials.insert(provider.id, credential.clone());
                            credential
                        }
                    };
                    routes.push(ModelRoute {
                        format,
                        provider: provider.name.clone(),
                        kind: provider.kind,
                        base_url: provider.base_url.clone(),
                        options: provider.options.clone(),
                        credential,
                        models: vec![name.clone()],
                    });
                }
            }
        }
        routes.sort_by_key(|r| r.format.path_segment());
        snapshot_envs.push(EnvRoutes {
            env: env.spec.name.clone(),
            routes,
        });
    }

    let mut priced_models: Vec<PricedModel> = models
        .iter()
        .filter_map(|m| {
            Some(PricedModel {
                provider: m.provider.clone(),
                kind: m.provider_kind,
                model: m.name.clone(),
                input: m.input_price?,
                output: m.output_price?,
            })
        })
        .collect();
    priced_models.sort_by(|a, b| {
        (a.provider.as_str(), a.model.as_str()).cmp(&(b.provider.as_str(), b.model.as_str()))
    });
    Ok(GatewaySnapshot {
        environments: snapshot_envs,
        priced_models,
        hooks: hooks.clone(),
    })
}
