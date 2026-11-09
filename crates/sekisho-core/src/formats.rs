//! Provider kinds and the API formats agents speak to them.

use serde::{Deserialize, Serialize};

/// The API a route speaks. Each environment has at most one provider per
/// format in release 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiFormat {
    Anthropic,
    OpenAi,
    Gemini,
}

impl ApiFormat {
    /// The route segment after `/env/<name>/`.
    pub fn path_segment(self) -> &'static str {
        match self {
            ApiFormat::Anthropic => "anthropic",
            ApiFormat::OpenAi => "openai",
            ApiFormat::Gemini => "gemini",
        }
    }
}

/// The kind of provider an admin adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    Gemini,
    /// A custom endpoint that speaks OpenAI's API.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

impl ProviderKind {
    pub fn api_format(self) -> ApiFormat {
        match self {
            ProviderKind::Anthropic => ApiFormat::Anthropic,
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => ApiFormat::OpenAi,
            ProviderKind::Gemini => ApiFormat::Gemini,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_formats() {
        assert_eq!(
            ProviderKind::OpenAiCompatible.api_format(),
            ApiFormat::OpenAi
        );
        assert_eq!(ApiFormat::Gemini.path_segment(), "gemini");
    }

    #[test]
    fn every_kind_maps_to_its_format() {
        assert_eq!(ProviderKind::Anthropic.api_format(), ApiFormat::Anthropic);
        assert_eq!(ProviderKind::OpenAi.api_format(), ApiFormat::OpenAi);
        assert_eq!(ProviderKind::Gemini.api_format(), ApiFormat::Gemini);
        assert_eq!(ApiFormat::Anthropic.path_segment(), "anthropic");
        assert_eq!(ApiFormat::OpenAi.path_segment(), "openai");
    }

    #[test]
    fn provider_kind_wire_names() {
        for (kind, wire) in [
            (ProviderKind::Anthropic, "\"anthropic\""),
            (ProviderKind::OpenAi, "\"openai\""),
            (ProviderKind::Gemini, "\"gemini\""),
            (ProviderKind::OpenAiCompatible, "\"openai_compatible\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ProviderKind>(wire).unwrap(), kind);
        }
        assert!(serde_json::from_str::<ProviderKind>("\"open_ai\"").is_err());
    }
}
