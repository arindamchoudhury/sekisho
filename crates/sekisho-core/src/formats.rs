//! Provider kinds, the API formats agents speak to them, and Azure endpoints.

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
    pub const ALL: [ApiFormat; 3] = [ApiFormat::Anthropic, ApiFormat::OpenAi, ApiFormat::Gemini];

    /// The format's name, for storage as text and in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            ApiFormat::Anthropic => "anthropic",
            ApiFormat::OpenAi => "openai",
            ApiFormat::Gemini => "gemini",
        }
    }

    /// The route segment after `/env/<name>/`, the same as [`Self::as_str`].
    pub fn path_segment(self) -> &'static str {
        self.as_str()
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
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
    /// Amazon Bedrock, reached through its Converse API.
    Bedrock,
    /// Azure OpenAI or Microsoft Foundry.
    Azure,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 6] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::Gemini,
        ProviderKind::OpenAiCompatible,
        ProviderKind::Bedrock,
        ProviderKind::Azure,
    ];

    /// The same spelling as the serde name, for storage as text.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenAiCompatible => "openai_compatible",
            ProviderKind::Bedrock => "bedrock",
            ProviderKind::Azure => "azure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The formats a provider of this kind can serve. A provider serves one
    /// of them, chosen when it is added; single-format kinds have no choice.
    pub fn formats(self) -> &'static [ApiFormat] {
        match self {
            ProviderKind::Anthropic => &[ApiFormat::Anthropic],
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => &[ApiFormat::OpenAi],
            ProviderKind::Gemini => &[ApiFormat::Gemini],
            ProviderKind::Bedrock | ProviderKind::Azure => {
                &[ApiFormat::Anthropic, ApiFormat::OpenAi]
            }
        }
    }
}

/// Which Azure service an endpoint belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AzureResourceType {
    /// `https://<resource>.openai.azure.com`
    OpenAi,
    /// `https://<resource>.services.ai.azure.com`, optionally a project.
    Foundry,
}

/// An Azure provider's endpoint, as the Azure portal shows it.
/// agentgateway builds the upstream host from these parts, so
/// [`AzureEndpoint::parse`] accepts only names that can't point elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AzureEndpoint {
    pub resource_name: String,
    pub resource_type: AzureResourceType,
    /// A Foundry project; always `None` for Azure OpenAI.
    pub project_name: Option<String>,
}

const AZURE_OPENAI_HOST: &str = ".openai.azure.com";
const AZURE_FOUNDRY_HOST: &str = ".services.ai.azure.com";
const AZURE_PROJECTS: &str = "/api/projects/";
const AZURE_FORMS: &str = "use the endpoint the Azure portal shows:      https://<resource>.openai.azure.com for Azure OpenAI, or      https://<resource>.services.ai.azure.com/api/projects/<project> for a Foundry project";

impl AzureEndpoint {
    /// Accepts `https://<resource>.openai.azure.com` (optionally ending in
    /// `/` or `/openai/v1[/]`), `https://<resource>.services.ai.azure.com`
    /// and `https://<resource>.services.ai.azure.com/api/projects/<project>`
    /// (each optionally ending in `/`). The error never quotes the input.
    pub fn parse(s: &str) -> Result<Self, String> {
        let refused = || AZURE_FORMS.to_owned();
        let rest = s.strip_prefix("https://").ok_or_else(refused)?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if let Some(resource) = host.strip_suffix(AZURE_OPENAI_HOST) {
            if !is_azure_name(resource, false)
                || !matches!(path, "" | "/" | "/openai/v1" | "/openai/v1/")
            {
                return Err(refused());
            }
            return Ok(Self {
                resource_name: resource.to_owned(),
                resource_type: AzureResourceType::OpenAi,
                project_name: None,
            });
        }
        if let Some(resource) = host.strip_suffix(AZURE_FOUNDRY_HOST) {
            if !is_azure_name(resource, false) {
                return Err(refused());
            }
            let project_name = match path {
                "" | "/" => None,
                _ => {
                    let project = path.strip_prefix(AZURE_PROJECTS).ok_or_else(refused)?;
                    let project = project.strip_suffix('/').unwrap_or(project);
                    if !is_azure_name(project, true) {
                        return Err(refused());
                    }
                    Some(project.to_owned())
                }
            };
            return Ok(Self {
                resource_name: resource.to_owned(),
                resource_type: AzureResourceType::Foundry,
                project_name,
            });
        }
        Err(refused())
    }

    /// The canonical form, without a trailing slash.
    pub fn url(&self) -> String {
        match (self.resource_type, &self.project_name) {
            (AzureResourceType::OpenAi, _) => {
                format!("https://{}{AZURE_OPENAI_HOST}", self.resource_name)
            }
            (AzureResourceType::Foundry, None) => {
                format!("https://{}{AZURE_FOUNDRY_HOST}", self.resource_name)
            }
            (AzureResourceType::Foundry, Some(project)) => format!(
                "https://{}{AZURE_FOUNDRY_HOST}{AZURE_PROJECTS}{project}",
                self.resource_name
            ),
        }
    }
}

/// `^[A-Za-z0-9][A-Za-z0-9-]{0,62}[A-Za-z0-9]$`, with `_` allowed anywhere
/// when `underscore` is set (project names).
fn is_azure_name(s: &str, underscore: bool) -> bool {
    let b = s.as_bytes();
    let edge_ok = |c: &u8| c.is_ascii_alphanumeric() || (underscore && *c == b'_');
    (2..=64).contains(&b.len())
        && b.first().is_some_and(edge_ok)
        && b.last().is_some_and(edge_ok)
        && b.iter().all(|c| edge_ok(c) || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_formats() {
        assert_eq!(
            ProviderKind::OpenAiCompatible.formats(),
            [ApiFormat::OpenAi]
        );
        assert_eq!(ApiFormat::Gemini.path_segment(), "gemini");
    }

    #[test]
    fn every_kind_maps_to_its_formats() {
        assert_eq!(ProviderKind::Anthropic.formats(), [ApiFormat::Anthropic]);
        assert_eq!(ProviderKind::OpenAi.formats(), [ApiFormat::OpenAi]);
        assert_eq!(ProviderKind::Gemini.formats(), [ApiFormat::Gemini]);
        for cloud in [ProviderKind::Bedrock, ProviderKind::Azure] {
            assert_eq!(cloud.formats(), [ApiFormat::Anthropic, ApiFormat::OpenAi]);
        }
        assert_eq!(ApiFormat::Anthropic.path_segment(), "anthropic");
        assert_eq!(ApiFormat::OpenAi.path_segment(), "openai");
    }

    #[test]
    fn api_format_text() {
        assert_eq!(ApiFormat::ALL.len(), 3);
        for format in ApiFormat::ALL {
            assert_eq!(format.as_str(), format.path_segment());
            assert_eq!(ApiFormat::parse(format.as_str()), Some(format));
        }
        assert_eq!(ApiFormat::parse("OpenAI"), None);
        assert_eq!(ApiFormat::parse("open_ai"), None);
    }

    #[test]
    fn provider_kind_wire_names() {
        for (kind, wire) in [
            (ProviderKind::Anthropic, "\"anthropic\""),
            (ProviderKind::OpenAi, "\"openai\""),
            (ProviderKind::Gemini, "\"gemini\""),
            (ProviderKind::OpenAiCompatible, "\"openai_compatible\""),
            (ProviderKind::Bedrock, "\"bedrock\""),
            (ProviderKind::Azure, "\"azure\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ProviderKind>(wire).unwrap(), kind);
        }
        assert!(serde_json::from_str::<ProviderKind>("\"open_ai\"").is_err());
    }

    #[test]
    fn provider_kind_text_matches_wire_names() {
        assert_eq!(ProviderKind::ALL.len(), 6);
        for kind in ProviderKind::ALL {
            let wire = serde_json::to_string(&kind).unwrap();
            assert_eq!(format!("\"{}\"", kind.as_str()), wire);
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProviderKind::parse("open_ai"), None);
        assert_eq!(ProviderKind::parse("OpenAI"), None);
    }

    fn openai(resource: &str) -> AzureEndpoint {
        AzureEndpoint {
            resource_name: resource.into(),
            resource_type: AzureResourceType::OpenAi,
            project_name: None,
        }
    }

    fn foundry(resource: &str, project: Option<&str>) -> AzureEndpoint {
        AzureEndpoint {
            resource_name: resource.into(),
            resource_type: AzureResourceType::Foundry,
            project_name: project.map(Into::into),
        }
    }

    #[test]
    fn azure_endpoints_parse() {
        for (input, want, canonical) in [
            (
                "https://my-resource.openai.azure.com",
                openai("my-resource"),
                "https://my-resource.openai.azure.com",
            ),
            (
                "https://my-resource.openai.azure.com/",
                openai("my-resource"),
                "https://my-resource.openai.azure.com",
            ),
            (
                "https://my-resource.openai.azure.com/openai/v1",
                openai("my-resource"),
                "https://my-resource.openai.azure.com",
            ),
            (
                "https://my-resource.openai.azure.com/openai/v1/",
                openai("my-resource"),
                "https://my-resource.openai.azure.com",
            ),
            (
                "https://My-Foundry2.services.ai.azure.com",
                foundry("My-Foundry2", None),
                "https://My-Foundry2.services.ai.azure.com",
            ),
            (
                "https://my-foundry.services.ai.azure.com/",
                foundry("my-foundry", None),
                "https://my-foundry.services.ai.azure.com",
            ),
            (
                "https://my-foundry.services.ai.azure.com/api/projects/team_project-1",
                foundry("my-foundry", Some("team_project-1")),
                "https://my-foundry.services.ai.azure.com/api/projects/team_project-1",
            ),
            (
                "https://my-foundry.services.ai.azure.com/api/projects/team/",
                foundry("my-foundry", Some("team")),
                "https://my-foundry.services.ai.azure.com/api/projects/team",
            ),
        ] {
            let parsed = AzureEndpoint::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(parsed, want, "{input}");
            assert_eq!(parsed.url(), canonical, "{input}");
            assert_eq!(AzureEndpoint::parse(&parsed.url()), Ok(parsed), "{input}");
        }

        // agentgateway builds <resource>.openai.azure.com and the like from
        // these, so anything that could name another host is refused. Each
        // form is also tried with a valid resource name ("rs"), so the rule
        // itself is what refuses it.
        for refused in [
            "http://r.openai.azure.com",
            "http://rs.openai.azure.com",
            "https://evil.com/x.openai.azure.com",
            "https://evil.com/rs.openai.azure.com",
            "https://a.b.openai.azure.com",
            "https://u@r.openai.azure.com",
            "https://u@rs.openai.azure.com",
            "https://r.openai.azure.com:8443",
            "https://rs.openai.azure.com:8443",
            "https://r.openai.azure.com/?x=1",
            "https://rs.openai.azure.com/?x=1",
            "https://rs.openai.azure.com?x=1",
            "https://rs.openai.azure.com#x",
            "https://rs.openai.azure.com/openai/v2",
            "https://rs.openai.azure.com/api/projects/p",
            "https://rs.services.ai.azure.com/api/projects/",
            "https://rs.services.ai.azure.com/api/projects/a/b",
            "https://rs.services.ai.azure.com/api/projects/a?b",
            "https://rs.services.ai.azure.com/openai/v1",
            "https://-rs.openai.azure.com",
            "https://rs-.openai.azure.com",
            "https://rs_x.openai.azure.com",
            "https://rs.openai.azure.com.evil.com",
            "https://openai.azure.com",
            "https://.openai.azure.com",
            "rs.openai.azure.com",
            "",
        ] {
            assert!(
                AzureEndpoint::parse(refused).is_err(),
                "{refused} should be refused"
            );
        }
        let long = format!("https://{}.openai.azure.com", "a".repeat(65));
        assert!(AzureEndpoint::parse(&long).is_err());
        let longest = format!("https://{}.openai.azure.com", "a".repeat(64));
        assert!(AzureEndpoint::parse(&longest).is_ok());

        let err = AzureEndpoint::parse("https://r.cognitiveservices.azure.com").unwrap_err();
        assert!(err.contains("https://<resource>.openai.azure.com"), "{err}");
        assert!(
            err.contains("https://<resource>.services.ai.azure.com/api/projects/<project>"),
            "{err}"
        );
    }
}
