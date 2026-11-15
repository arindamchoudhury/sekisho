//! agentgateway's configuration, rendered from Sekisho's store.
//!
//! [`build_snapshot`] reads the store; [`render`] turns the snapshot into
//! agentgateway v1.7.0's local config YAML; [`validate`] checks YAML against
//! the schema vendored from that release. [`agent`] keeps a gateway's
//! config file current.

pub mod agent;
mod render;
mod snapshot;

pub use render::{MODEL_ROUTE_RULE, RenderOptions, render, usd, validate};
pub use snapshot::{
    GatewaySnapshot, HookTargets, ModelEntry, PricedModel, ProviderEntry, build_snapshot,
};
