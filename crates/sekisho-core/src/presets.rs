//! Known OpenAI-compatible hosts. A preset fills in a new provider's base
//! URL, APIs and catalog key; the admin can still change each of them.
//!
//! The values are agentgateway v1.7.0's (`ProviderPreset` in
//! `crates/llm/src/custom.rs`), plus Scaleway from its own documentation.
//! Re-check them whenever the agentgateway pin moves.

use crate::formats::{HostApi, HostApiPath, HostApis};

/// A known host.
#[derive(Debug, PartialEq, Eq)]
pub struct HostPreset {
    /// The preset's ID in the admin API, such as `deepseek`.
    pub id: &'static str,
    /// The host's name for people.
    pub name: &'static str,
    /// The host's base URL, as it documents it. `None` when every install
    /// runs its own server, so the admin gives the URL.
    pub base_url: Option<&'static str>,
    /// The APIs the host serves, each with its whole path when it isn't the
    /// standard one under the base URL.
    pub apis: &'static [(HostApi, Option<&'static str>)],
    /// The key agentgateway's model catalog files the host's prices under,
    /// if the catalog has the host.
    pub catalog_key: Option<&'static str>,
}

impl HostPreset {
    pub fn host_apis(&self) -> HostApis {
        HostApis::new(
            self.apis
                .iter()
                .map(|(api, path)| HostApiPath {
                    api: *api,
                    path: path.map(Into::into),
                })
                .collect(),
        )
        .expect("every preset's APIs follow the rules (tested)")
    }
}

const CHAT: (HostApi, Option<&str>) = (HostApi::ChatCompletions, None);
const MESSAGES: (HostApi, Option<&str>) = (HostApi::Messages, None);
const RESPONSES: (HostApi, Option<&str>) = (HostApi::Responses, None);
const MESSAGES_UNDER_ANTHROPIC: (HostApi, Option<&str>) =
    (HostApi::Messages, Some("/anthropic/v1/messages"));

/// The known hosts, by ID.
pub static PRESETS: [HostPreset; 12] = [
    HostPreset {
        id: "baseten",
        name: "Baseten",
        base_url: Some("https://inference.baseten.co/v1"),
        apis: &[CHAT, MESSAGES],
        catalog_key: Some("baseten"),
    },
    HostPreset {
        id: "cerebras",
        name: "Cerebras",
        base_url: Some("https://api.cerebras.ai/v1"),
        apis: &[CHAT],
        catalog_key: Some("cerebras"),
    },
    HostPreset {
        id: "deepinfra",
        name: "DeepInfra",
        base_url: Some("https://api.deepinfra.com/v1/openai"),
        apis: &[CHAT, MESSAGES_UNDER_ANTHROPIC],
        catalog_key: Some("deepinfra"),
    },
    HostPreset {
        id: "deepseek",
        name: "DeepSeek",
        base_url: Some("https://api.deepseek.com/v1"),
        apis: &[CHAT, MESSAGES_UNDER_ANTHROPIC, RESPONSES],
        catalog_key: Some("deepseek"),
    },
    HostPreset {
        id: "fireworks",
        name: "Fireworks",
        base_url: Some("https://api.fireworks.ai/inference/v1"),
        apis: &[CHAT, MESSAGES, RESPONSES],
        catalog_key: Some("fireworks"),
    },
    HostPreset {
        id: "groq",
        name: "Groq",
        base_url: Some("https://api.groq.com/openai/v1"),
        apis: &[CHAT, RESPONSES],
        catalog_key: Some("groq"),
    },
    HostPreset {
        id: "huggingface",
        name: "Hugging Face",
        base_url: Some("https://router.huggingface.co/v1"),
        apis: &[CHAT, RESPONSES],
        catalog_key: Some("huggingface"),
    },
    HostPreset {
        id: "mistral",
        name: "Mistral",
        base_url: Some("https://api.mistral.ai/v1"),
        apis: &[CHAT],
        catalog_key: Some("mistral"),
    },
    HostPreset {
        id: "ollama",
        name: "Ollama",
        base_url: None,
        apis: &[CHAT, RESPONSES],
        catalog_key: Some("ollama"),
    },
    HostPreset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: Some("https://openrouter.ai/api/v1"),
        apis: &[CHAT, MESSAGES, RESPONSES],
        catalog_key: Some("openrouter"),
    },
    HostPreset {
        id: "scaleway",
        name: "Scaleway",
        base_url: Some("https://api.scaleway.ai/v1"),
        apis: &[CHAT],
        catalog_key: None,
    },
    HostPreset {
        id: "togetherai",
        name: "Together",
        base_url: Some("https://api.together.xyz/v1"),
        apis: &[CHAT],
        catalog_key: Some("togetherai"),
    },
];

/// The preset with this ID.
pub fn preset(id: &str) -> Option<&'static HostPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::HostApi::*;

    /// The plan's table, written out independently of [`PRESETS`].
    #[allow(clippy::type_complexity)]
    const TABLE: [(
        &str,
        &str,
        Option<&str>,
        &[(HostApi, Option<&str>)],
        Option<&str>,
    ); 12] = [
        (
            "baseten",
            "Baseten",
            Some("https://inference.baseten.co/v1"),
            &[(ChatCompletions, None), (Messages, None)],
            Some("baseten"),
        ),
        (
            "cerebras",
            "Cerebras",
            Some("https://api.cerebras.ai/v1"),
            &[(ChatCompletions, None)],
            Some("cerebras"),
        ),
        (
            "deepinfra",
            "DeepInfra",
            Some("https://api.deepinfra.com/v1/openai"),
            &[
                (ChatCompletions, None),
                (Messages, Some("/anthropic/v1/messages")),
            ],
            Some("deepinfra"),
        ),
        (
            "deepseek",
            "DeepSeek",
            Some("https://api.deepseek.com/v1"),
            &[
                (ChatCompletions, None),
                (Messages, Some("/anthropic/v1/messages")),
                (Responses, None),
            ],
            Some("deepseek"),
        ),
        (
            "fireworks",
            "Fireworks",
            Some("https://api.fireworks.ai/inference/v1"),
            &[(ChatCompletions, None), (Messages, None), (Responses, None)],
            Some("fireworks"),
        ),
        (
            "groq",
            "Groq",
            Some("https://api.groq.com/openai/v1"),
            &[(ChatCompletions, None), (Responses, None)],
            Some("groq"),
        ),
        (
            "huggingface",
            "Hugging Face",
            Some("https://router.huggingface.co/v1"),
            &[(ChatCompletions, None), (Responses, None)],
            Some("huggingface"),
        ),
        (
            "mistral",
            "Mistral",
            Some("https://api.mistral.ai/v1"),
            &[(ChatCompletions, None)],
            Some("mistral"),
        ),
        (
            "ollama",
            "Ollama",
            None,
            &[(ChatCompletions, None), (Responses, None)],
            Some("ollama"),
        ),
        (
            "openrouter",
            "OpenRouter",
            Some("https://openrouter.ai/api/v1"),
            &[(ChatCompletions, None), (Messages, None), (Responses, None)],
            Some("openrouter"),
        ),
        (
            "scaleway",
            "Scaleway",
            Some("https://api.scaleway.ai/v1"),
            &[(ChatCompletions, None)],
            None,
        ),
        (
            "togetherai",
            "Together",
            Some("https://api.together.xyz/v1"),
            &[(ChatCompletions, None)],
            Some("togetherai"),
        ),
    ];

    #[test]
    fn presets_match_agentgateway_and_are_valid() {
        assert_eq!(PRESETS.len(), TABLE.len());
        for (preset, (id, name, base_url, apis, catalog_key)) in PRESETS.iter().zip(TABLE) {
            assert_eq!(preset.id, id);
            assert_eq!(preset.name, name, "{id}");
            assert_eq!(preset.base_url, base_url, "{id}");
            assert_eq!(preset.apis, apis, "{id}");
            assert_eq!(preset.catalog_key, catalog_key, "{id}");
            assert!(std::ptr::eq(super::preset(id).unwrap(), preset), "{id}");

            let raw = preset
                .apis
                .iter()
                .map(|(api, path)| HostApiPath {
                    api: *api,
                    path: path.map(Into::into),
                })
                .collect();
            let host_apis = HostApis::new(raw).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(preset.host_apis(), host_apis, "{id}");
            assert!(host_apis.get(ChatCompletions).is_some(), "{id}");

            match preset.base_url {
                Some(url) => assert!(url.starts_with("https://"), "{id}"),
                None => assert_eq!(id, "ollama"),
            }
            if let Some(key) = preset.catalog_key {
                let b = key.as_bytes();
                assert!(
                    b.len() <= 64
                        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
                        && b.iter().all(|c| c.is_ascii_lowercase()
                            || c.is_ascii_digit()
                            || b"._-".contains(c)),
                    "{id}"
                );
            }
        }
        let mut ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PRESETS.len());

        let deepseek = super::preset("deepseek").unwrap().host_apis();
        assert_eq!(
            deepseek
                .iter()
                .map(|a| (a.api, a.path.as_deref()))
                .collect::<Vec<_>>(),
            [
                (ChatCompletions, None),
                (Messages, Some("/anthropic/v1/messages")),
                (Responses, None)
            ]
        );
        assert_eq!(super::preset("scaleway").unwrap().catalog_key, None);
        assert!(super::preset("nope").is_none());
        assert!(super::preset("").is_none());
        assert!(super::preset("DeepSeek").is_none());
    }
}
