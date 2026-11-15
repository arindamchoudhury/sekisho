//! Provider kinds, the API formats agents speak to them, which client APIs
//! reach which models, the APIs an OpenAI-compatible host serves, and Azure
//! endpoints.

use serde::{Deserialize, Serialize};

use crate::names::EnvName;

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

    /// The client APIs on this format's route. A coding tool set up for
    /// the format may send any of them: Codex sends Responses, other
    /// OpenAI-format tools Chat Completions.
    pub fn client_apis(self) -> &'static [ClientApi] {
        match self {
            ApiFormat::Anthropic => &[ClientApi::Messages],
            ApiFormat::OpenAi => &[ClientApi::ChatCompletions, ClientApi::Responses],
            ApiFormat::Gemini => &[ClientApi::Gemini],
        }
    }
}

/// The API a client's request speaks, from its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientApi {
    /// Anthropic's Messages API, `count_tokens` included.
    Messages,
    /// OpenAI's Chat Completions API.
    ChatCompletions,
    /// OpenAI's Responses API.
    Responses,
    /// Gemini's `generateContent`, `streamGenerateContent` and `countTokens`.
    Gemini,
}

impl ClientApi {
    pub const ALL: [ClientApi; 4] = [
        ClientApi::Messages,
        ClientApi::ChatCompletions,
        ClientApi::Responses,
        ClientApi::Gemini,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ClientApi::Messages => "messages",
            ClientApi::ChatCompletions => "chat_completions",
            ClientApi::Responses => "responses",
            ClientApi::Gemini => "gemini",
        }
    }

    /// The API's name in a sentence, as Sekisho's refusals use it.
    pub fn described(self) -> &'static str {
        match self {
            ClientApi::Messages => "Anthropic Messages API",
            ClientApi::ChatCompletions => "Chat Completions API",
            ClientApi::Responses => "Responses API",
            ClientApi::Gemini => "Gemini API",
        }
    }

    /// The API of a client's path, when the path is exactly one of the
    /// model endpoints the gateway's authorization rule admits:
    ///
    /// - `/env/<env>/anthropic/v1/messages`, and `…/messages/count_tokens`;
    /// - `/env/<env>/openai/v1/chat/completions` and `…/openai/v1/responses`;
    /// - `/env/<env>/gemini/<v1|v1alpha|v1beta>/models/<model>:<generateContent|streamGenerateContent|countTokens>`,
    ///   with a model of `[A-Za-z0-9._-]`.
    ///
    /// `<env>` must be an [`EnvName`]. The path is the client's own, before
    /// the gateway rewrites it, and without its query: strip the query
    /// first, since a path that still has one is not a route.
    pub fn of_route(path: &str) -> Option<ClientApi> {
        let segments: Vec<&str> = path.strip_prefix("/env/")?.split('/').collect();
        let (env, route) = segments.split_first()?;
        EnvName::parse(env).ok()?;
        Some(match route {
            ["anthropic", "v1", "messages"] | ["anthropic", "v1", "messages", "count_tokens"] => {
                ClientApi::Messages
            }
            ["openai", "v1", "chat", "completions"] => ClientApi::ChatCompletions,
            ["openai", "v1", "responses"] => ClientApi::Responses,
            ["gemini", "v1" | "v1alpha" | "v1beta", "models", call] if is_gemini_call(call) => {
                ClientApi::Gemini
            }
            _ => return None,
        })
    }
}

/// `<model>:<method>`, the last segment of a Gemini model endpoint.
fn is_gemini_call(call: &str) -> bool {
    let Some((model, method)) = call.split_once(':') else {
        return false;
    };
    !model.is_empty()
        && model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        && matches!(
            method,
            "generateContent" | "streamGenerateContent" | "countTokens"
        )
}

/// Whether a model is Claude, as agentgateway decides it for the provider's
/// kind: every model of an Anthropic provider; on Bedrock, an ID containing
/// `anthropic.claude`; on Azure, a deployment name starting with `claude`.
/// Case is ignored, as in agentgateway v1.7.0's `is_anthropic_model`
/// (`crates/llm/src/bedrock.rs`, `crates/llm/src/azure.rs`).
pub fn is_claude(kind: ProviderKind, model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    match kind {
        ProviderKind::Anthropic => true,
        ProviderKind::Bedrock => model.contains("anthropic.claude"),
        ProviderKind::Azure => model.starts_with("claude"),
        ProviderKind::OpenAi | ProviderKind::Gemini | ProviderKind::OpenAiCompatible => false,
    }
}

/// Whether a client API reaches a model on a provider of this kind, passed
/// through or translated by agentgateway.
///
/// This mirrors agentgateway v1.7.0's `CHAT_TRANSLATIONS` and
/// `AIProvider::supported_chat_formats`
/// (`crates/agentgateway/src/llm/mod.rs`); re-check it on every
/// agentgateway bump:
///
/// - Messages and Chat Completions reach every kind. `count_tokens` too:
///   agentgateway estimates it locally for a provider without its own.
/// - Responses reaches every kind but Anthropic and Claude on Azure, which
///   Foundry serves only through Messages: there is no Responses to
///   Messages translation. Claude on Bedrock is reached through Converse,
///   the default for Bedrock's Runtime endpoint.
/// - Gemini reaches Gemini providers only.
pub fn reaches(api: ClientApi, kind: ProviderKind, model: &str) -> bool {
    match api {
        ClientApi::Messages | ClientApi::ChatCompletions => true,
        ClientApi::Responses => match kind {
            ProviderKind::Anthropic => false,
            ProviderKind::Azure => !is_claude(kind, model),
            ProviderKind::OpenAi
            | ProviderKind::Gemini
            | ProviderKind::OpenAiCompatible
            | ProviderKind::Bedrock => true,
        },
        ClientApi::Gemini => kind == ProviderKind::Gemini,
    }
}

/// Whether every API of the format reaches the model, as a format's default
/// model needs.
pub fn reaches_format(format: ApiFormat, kind: ProviderKind, model: &str) -> bool {
    format
        .client_apis()
        .iter()
        .all(|api| reaches(*api, kind, model))
}

/// An API an OpenAI-compatible host serves. Ordered as [`HostApis`] lists
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HostApi {
    ChatCompletions,
    Messages,
    Responses,
}

impl HostApi {
    pub const ALL: [HostApi; 3] = [
        HostApi::ChatCompletions,
        HostApi::Messages,
        HostApi::Responses,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HostApi::ChatCompletions => "chat_completions",
            HostApi::Messages => "messages",
            HostApi::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

/// One API a host serves, and its whole upstream path when it isn't the
/// standard one under the base URL (agentgateway's `formats[].path`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostApiPath {
    pub api: HostApi,
    pub path: Option<String>,
}

/// The APIs an OpenAI-compatible host serves: Chat Completions always, and
/// Messages and Responses where the host serves them, each at most once,
/// in [`HostApi`]'s order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostApis(Vec<HostApiPath>);

impl HostApis {
    /// Checks the rules and sorts by API. A path is `/` followed by up to
    /// 255 of `[A-Za-z0-9._~/-]`, with no empty, `.` or `..` segment. The
    /// error never quotes a path.
    pub fn new(mut apis: Vec<HostApiPath>) -> Result<HostApis, String> {
        apis.sort_by_key(|a| a.api);
        if let Some(pair) = apis.windows(2).find(|pair| pair[0].api == pair[1].api) {
            return Err(format!(
                "list {} at most once in the host's APIs",
                pair[0].api.as_str()
            ));
        }
        if apis.first().map(|a| a.api) != Some(HostApi::ChatCompletions) {
            return Err(
                "the host's APIs must include chat_completions, which every OpenAI-compatible host serves"
                    .to_owned(),
            );
        }
        if let Some(bad) = apis
            .iter()
            .find(|a| a.path.as_deref().is_some_and(|p| !is_host_path(p)))
        {
            return Err(format!(
                "the {} path must start with / and have at most 255 more letters, digits or . _ ~ / - characters, with no empty, . or .. segment",
                bad.api.as_str()
            ));
        }
        Ok(HostApis(apis))
    }

    /// Chat Completions at the standard path, all a host has to serve.
    pub fn chat_only() -> HostApis {
        HostApis(vec![HostApiPath {
            api: HostApi::ChatCompletions,
            path: None,
        }])
    }

    pub fn iter(&self) -> std::slice::Iter<'_, HostApiPath> {
        self.0.iter()
    }

    pub fn get(&self, api: HostApi) -> Option<&HostApiPath> {
        self.0.iter().find(|a| a.api == api)
    }
}

/// `/` then up to 255 of `[A-Za-z0-9._~/-]`, with no empty, `.` or `..`
/// segment.
fn is_host_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    rest.len() <= 255
        && rest
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._~/-".contains(&c))
        && rest.split('/').all(|s| !matches!(s, "" | "." | ".."))
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

    /// The key agentgateway's model catalog files this kind's models under
    /// (its providers' `NAME`). An OpenAI-compatible provider has none of
    /// its own kind: its host's key, if any, is the provider's.
    pub fn catalog_key(self) -> Option<&'static str> {
        match self {
            ProviderKind::Anthropic => Some("anthropic"),
            ProviderKind::OpenAi => Some("openai"),
            ProviderKind::Gemini => Some("gcp.gemini"),
            ProviderKind::Bedrock => Some("aws.bedrock"),
            ProviderKind::Azure => Some("azure"),
            ProviderKind::OpenAiCompatible => None,
        }
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

    #[test]
    fn routes_are_classified_exactly() {
        use ClientApi::*;
        for (path, api) in [
            ("/env/team-a/anthropic/v1/messages", Messages),
            ("/env/team-a/anthropic/v1/messages/count_tokens", Messages),
            ("/env/team-a/openai/v1/chat/completions", ChatCompletions),
            ("/env/team-a/openai/v1/responses", Responses),
            (
                "/env/team-a/gemini/v1beta/models/gemini-2.5-pro:generateContent",
                Gemini,
            ),
            (
                "/env/team-a/gemini/v1beta/models/gemini-2.5-pro:streamGenerateContent",
                Gemini,
            ),
            (
                "/env/team-a/gemini/v1beta/models/gemini-2.5-pro:countTokens",
                Gemini,
            ),
            (
                "/env/team-a/gemini/v1/models/gemini-2.5-pro:generateContent",
                Gemini,
            ),
            (
                "/env/team-a/gemini/v1alpha/models/gemini_2.5-pro:generateContent",
                Gemini,
            ),
            ("/env/a/openai/v1/responses", Responses),
            (
                &format!("/env/{}/openai/v1/responses", "a".repeat(63)),
                Responses,
            ),
        ] {
            assert_eq!(ClientApi::of_route(path), Some(api), "{path}");
        }

        for path in [
            "/env/team-a/openai/v1/models",
            "/env/team-a/openai/v1/embeddings",
            "/env/team-a/anthropic/v1/messages/batches",
            "/env/team-a/anthropic/v1/chat/completions",
            "/env/team-a/openai/v1/messages",
            "/env/team-a/openai/v1/responses/compact",
            "/env/team-a/openai/v1beta/models/gemini-2.5-pro:generateContent",
            "/env/team-a/gemini/v1beta/models/a/b:generateContent",
            "/env/team-a/gemini/v2/models/m:generateContent",
            "/env/team-a/gemini/v1beta/models/m:embedContent",
            "/env/team-a/gemini/v1beta/models/:generateContent",
            "/env/team-a/gemini/v1beta/models/m",
            "/env/team-a/gemini/v1beta/models/m:generateContent:x",
            "/env/team-a/anthropic/v1/messages/",
            "/env/team-a/openai/v1/chat/completions/",
            "/env/team-a/anthropic/v1//messages",
            "/env/team-a//anthropic/v1/messages",
            "//env/team-a/anthropic/v1/messages",
            "/env/team%2Fa/anthropic/v1/messages",
            "/env/team-a/anthropic%2Fv1/messages",
            "/env/team-a/openai/v1/chat%2Fcompletions",
            "/env/team-a/gemini/v1beta/models/a%2Fb:generateContent",
            "/env/Team-A/anthropic/v1/messages",
            "/env/-a/anthropic/v1/messages",
            "/env/a-/anthropic/v1/messages",
            "/env/a..b/../anthropic/v1/messages",
            "/env/../anthropic/v1/messages",
            &format!("/env/{}/anthropic/v1/messages", "a".repeat(64)),
            "/env",
            "/env/",
            "/env/team-a",
            "/env/team-a/",
            "/env/team-a/anthropic",
            "/bare/openai/v1/chat/completions",
            "/v1/messages",
            "",
            // The caller strips the query first: a path with one is no route.
            "/env/team-a/anthropic/v1/messages?beta=true",
            "/env/team-a/gemini/v1beta/models/m:streamGenerateContent?alt=sse",
        ] {
            assert_eq!(ClientApi::of_route(path), None, "{path}");
        }
    }

    #[test]
    fn client_api_names() {
        use ClientApi::*;
        assert_eq!(
            ClientApi::ALL,
            [Messages, ChatCompletions, Responses, Gemini]
        );
        for (api, text, described) in [
            (Messages, "messages", "Anthropic Messages API"),
            (ChatCompletions, "chat_completions", "Chat Completions API"),
            (Responses, "responses", "Responses API"),
            (Gemini, "gemini", "Gemini API"),
        ] {
            assert_eq!(api.as_str(), text);
            assert_eq!(api.described(), described);
        }
        assert_eq!(ApiFormat::Anthropic.client_apis(), [Messages]);
        assert_eq!(
            ApiFormat::OpenAi.client_apis(),
            [ChatCompletions, Responses]
        );
        assert_eq!(ApiFormat::Gemini.client_apis(), [Gemini]);
    }

    #[test]
    fn reach_follows_agentgateway() {
        use ClientApi::*;
        use ProviderKind as K;
        for (kind, model, reached) in [
            (K::Anthropic, "claude-x", false),
            (K::Azure, "claude-sonnet", false),
            (K::Azure, "CLAUDE-x", false),
            (K::Azure, "gpt-5", true),
            (K::Bedrock, "us.anthropic.claude-sonnet-4-5-v1:0", true),
            (K::Bedrock, "amazon.nova-pro", true),
            (K::OpenAiCompatible, "qwen3", true),
            (K::OpenAiCompatible, "claude-x", true),
            (K::Gemini, "gemini-2.5-pro", true),
            (K::OpenAi, "gpt-5", true),
        ] {
            assert_eq!(reaches(Responses, kind, model), reached, "{kind:?} {model}");
        }
        for kind in ProviderKind::ALL {
            for model in [
                "claude-sonnet-4-5",
                "anthropic.claude-3",
                "gpt-5",
                "gemini-2.5-pro",
            ] {
                assert!(reaches(Messages, kind, model), "{kind:?} {model}");
                assert!(reaches(ChatCompletions, kind, model), "{kind:?} {model}");
                assert_eq!(
                    reaches(Gemini, kind, model),
                    kind == K::Gemini,
                    "{kind:?} {model}"
                );
            }
        }
    }

    #[test]
    fn a_format_reaches_a_model_through_every_api() {
        use ProviderKind as K;
        assert!(!reaches_format(
            ApiFormat::OpenAi,
            K::Anthropic,
            "claude-sonnet-4-5"
        ));
        assert!(!reaches_format(
            ApiFormat::OpenAi,
            K::Azure,
            "claude-sonnet-4-5"
        ));
        assert!(reaches_format(ApiFormat::OpenAi, K::Azure, "gpt-5"));
        assert!(reaches_format(
            ApiFormat::OpenAi,
            K::Bedrock,
            "anthropic.claude-sonnet-4-5-v1:0"
        ));
        assert!(reaches_format(
            ApiFormat::Anthropic,
            K::Gemini,
            "gemini-2.5-pro"
        ));
        assert!(reaches_format(
            ApiFormat::Anthropic,
            K::OpenAiCompatible,
            "qwen3"
        ));
        assert!(!reaches_format(
            ApiFormat::Gemini,
            K::OpenAiCompatible,
            "qwen3"
        ));
        assert!(reaches_format(
            ApiFormat::Gemini,
            K::Gemini,
            "gemini-2.5-pro"
        ));
    }

    #[test]
    fn claude_is_recognised_as_agentgateway_does() {
        use ProviderKind as K;
        for (kind, model, claude) in [
            (K::Bedrock, "anthropic.claude-3", true),
            (K::Bedrock, "us.anthropic.claude-sonnet-4-5-v1:0", true),
            (K::Bedrock, "ANTHROPIC.CLAUDE-x", true),
            (K::Bedrock, "amazon.nova-pro", false),
            (K::Bedrock, "claude-sonnet-4-5", false),
            (K::Azure, "claude-sonnet-4-5", true),
            (K::Azure, "Claude-x", true),
            (K::Azure, "my-claude", false),
            (K::Anthropic, "claude-sonnet-4-5", true),
            (K::Anthropic, "anything", true),
            (K::OpenAi, "claude-x", false),
            (K::OpenAiCompatible, "claude-x", false),
            (K::Gemini, "claude-x", false),
        ] {
            assert_eq!(is_claude(kind, model), claude, "{kind:?} {model}");
        }
    }

    fn api(api: HostApi, path: Option<&str>) -> HostApiPath {
        HostApiPath {
            api,
            path: path.map(Into::into),
        }
    }

    #[test]
    fn host_apis_follow_their_rules() {
        use HostApi::*;
        let chat = HostApis::new(vec![api(ChatCompletions, None)]).unwrap();
        assert_eq!(chat, HostApis::chat_only());
        assert_eq!(
            chat.iter().collect::<Vec<_>>(),
            [&api(ChatCompletions, None)]
        );

        let sorted = HostApis::new(vec![api(Responses, None), api(ChatCompletions, None)]).unwrap();
        assert_eq!(
            sorted.iter().map(|a| a.api).collect::<Vec<_>>(),
            [ChatCompletions, Responses]
        );
        assert_eq!(sorted.get(Responses), Some(&api(Responses, None)));
        assert_eq!(sorted.get(Messages), None);

        let deepseek = HostApis::new(vec![
            api(Messages, Some("/anthropic/v1/messages")),
            api(ChatCompletions, None),
        ])
        .unwrap();
        assert_eq!(
            deepseek.get(Messages).and_then(|a| a.path.as_deref()),
            Some("/anthropic/v1/messages")
        );
        let longest = format!("/{}", "a".repeat(255));
        assert!(HostApis::new(vec![api(ChatCompletions, Some(&longest))]).is_ok());
        assert!(
            HostApis::new(vec![api(
                ChatCompletions,
                Some("/v1/chat.completions_~x-y")
            )])
            .is_ok()
        );

        assert!(HostApis::new(vec![]).is_err());
        let no_chat = HostApis::new(vec![api(Messages, None)]).unwrap_err();
        assert!(no_chat.contains("chat_completions"), "{no_chat}");
        let twice = HostApis::new(vec![api(ChatCompletions, None), api(ChatCompletions, None)])
            .unwrap_err();
        assert!(twice.contains("once"), "{twice}");
        assert!(
            HostApis::new(vec![
                api(ChatCompletions, None),
                api(Messages, None),
                api(Messages, Some("/m"))
            ])
            .is_err()
        );

        let too_long = format!("/{}", "a".repeat(256));
        for bad in [
            "",
            "/",
            "v1/x",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/a/",
            "/..",
            "/a?b",
            "/a#b",
            "/a b",
            "/a%2Fb",
            "/é",
            "https://evil.com/x",
            &too_long,
        ] {
            let err = HostApis::new(vec![api(ChatCompletions, None), api(Messages, Some(bad))])
                .unwrap_err();
            assert!(err.contains("messages"), "{bad:?}: {err}");
            if bad.len() > 2 {
                assert!(!err.contains(bad), "{bad:?}: the error quotes the path");
            }
        }
    }

    #[test]
    fn host_api_text() {
        for (host_api, text) in [
            (HostApi::ChatCompletions, "chat_completions"),
            (HostApi::Messages, "messages"),
            (HostApi::Responses, "responses"),
        ] {
            assert_eq!(host_api.as_str(), text);
            assert_eq!(HostApi::parse(text), Some(host_api));
        }
        assert_eq!(HostApi::parse("completions"), None);
        assert_eq!(HostApi::parse("Messages"), None);
    }
}
