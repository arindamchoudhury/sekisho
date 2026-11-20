//! The delivery document (D198): everything `sekisho use` writes for one
//! environment, as `GET /api/v1/me/environments/{name}/delivery` answers
//! it. The server builds it; the CLI reads it. Files are fetched apart, by
//! their SHA-256, from the same environment's blob route.

use sekisho_core::formats::ClientApi;
use sekisho_core::names::{EnvName, ModelName};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::model::{EntrySpec, sha256_hex};
use crate::names::{EntryKind, EntryName, SourceName};

/// One environment, as a machine installs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub environment: EnvName,
    /// Shown to people when they start using the environment.
    pub notice: String,
    /// SHA-256 in hex of the document without this field
    /// ([`Delivery::compute_revision`]): it changes when anything delivered
    /// changes, and only then.
    pub revision: String,
    /// `<SEKISHO_GATEWAY_URL>/env/<name>`, the base of every model route.
    pub gateway_url: String,
    /// The models the environment lists, in its order.
    pub models: Vec<DeliveredModel>,
    pub defaults: Defaults,
    /// The environment's MCP endpoint; always `None` until plan 5.
    pub mcp: Option<McpEndpoint>,
    /// What the environment delivers, each entry once: its own entries,
    /// then what only its toolkits bring. Toolkits themselves never appear.
    pub entries: Vec<DeliveredEntry>,
    /// Entries the environment delivers but are quarantined: a machine
    /// removes them.
    pub quarantined: Vec<EntryAddress>,
    pub registries: Registries,
}

impl Delivery {
    /// SHA-256 in hex over the canonical JSON of the document without
    /// `revision` (object keys sorted).
    pub fn compute_revision(&self) -> String {
        let mut value = serde_json::to_value(self).expect("delivery documents serialize");
        if let Some(object) = value.as_object_mut() {
            object.remove("revision");
        }
        sha256_hex(&serde_json::to_vec(&value).expect("JSON values serialize"))
    }

    /// Sets `revision` from the rest of the document.
    pub fn seal(mut self) -> Self {
        self.revision = self.compute_revision();
        self
    }
}

/// A model and the client APIs that reach it through the gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredModel {
    pub name: ModelName,
    pub apis: Vec<ClientApi>,
}

/// Each API format's default model and the small model, with their
/// fallbacks applied (design 9.2): `None` when no listed model serves the
/// format, and the small model only as named.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    pub anthropic: Option<ModelName>,
    pub openai: Option<ModelName>,
    pub gemini: Option<ModelName>,
    pub small: Option<ModelName>,
}

/// The environment's MCP endpoint (plan 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpEndpoint {
    pub url: String,
    /// A client ID pre-registered for MCP sign-in, when there is one.
    pub client_id: Option<String>,
}

/// One delivered catalog entry, at its applied revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveredEntry {
    pub kind: EntryKind,
    pub source: SourceName,
    pub name: EntryName,
    /// The source commit the revision was read at.
    pub commit: String,
    /// The revision's digest ([`crate::model::CheckedEntry::digest`]).
    pub digest: String,
    /// When the revision was applied: a local server's publish-time cutoff
    /// (D200).
    #[serde(with = "time::serde::rfc3339")]
    pub applied_at: OffsetDateTime,
    /// An MCP server's allowed tools; `None` allows them all.
    pub tools: Option<Vec<String>>,
    pub spec: EntrySpec,
    /// A skill's, plugin's or prompt's files, sorted by path.
    pub files: Vec<DeliveredFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredFile {
    /// Relative to the entry's folder, `/`-separated.
    pub path: String,
    /// Lowercase hex: the blob route's key.
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
}

/// An entry's address: its kind, source and name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryAddress {
    pub kind: EntryKind,
    pub source: SourceName,
    pub name: EntryName,
}

/// Install-wide package registries (D199), each an `https://` URL.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registries {
    pub npm: Option<String>,
    pub python: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Delivery {
        Delivery {
            environment: EnvName::parse("payments").unwrap(),
            notice: String::new(),
            revision: String::new(),
            gateway_url: "https://gw.example.com/env/payments".into(),
            models: vec![DeliveredModel {
                name: ModelName::parse("sonnet").unwrap(),
                apis: vec![ClientApi::Messages, ClientApi::ChatCompletions],
            }],
            defaults: Defaults::default(),
            mcp: None,
            entries: Vec::new(),
            quarantined: Vec::new(),
            registries: Registries::default(),
        }
    }

    #[test]
    fn the_revision_covers_everything_but_itself() {
        let sealed = document().seal();
        assert_eq!(sealed.revision.len(), 64);
        assert_eq!(sealed.revision, sealed.compute_revision());
        let mut changed = sealed.clone();
        changed.notice = "Be careful.".into();
        assert_ne!(changed.compute_revision(), sealed.revision);
        let mut relabelled = sealed.clone();
        relabelled.revision = "x".into();
        assert_eq!(relabelled.compute_revision(), sealed.revision);
    }

    #[test]
    fn apis_are_spelled_snake_case() {
        let value = serde_json::to_value(document()).unwrap();
        assert_eq!(
            value["models"][0]["apis"],
            serde_json::json!(["messages", "chat_completions"])
        );
        let back: Delivery = serde_json::from_value(value).unwrap();
        assert_eq!(back, document());
    }
}
