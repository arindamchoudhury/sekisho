//! Git mode for admins (D175): turning it on, changing and reading its
//! settings, turning it off, and proposing environment changes as GitHub
//! pull requests. While it is on, environments change only through Git.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sekisho_catalog::environment;
use sekisho_catalog::names::SourceName;
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_core::names::EnvName;
use sekisho_store::{EnvironmentSpec, FieldError, GitMode, GitModeSettings, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use super::catalog_sources::{SourceBody, SyncReportBody, sync_in_background};
use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;
use crate::catalog::github::{GitHub, Proposal, RepoPath};
use crate::catalog::outbound::Outbound;

/// GitHub's API for repositories on github.com.
const GITHUB_API: &str = "https://api.github.com";

/// Refuses a change to environments through the API while Git mode is on.
pub(crate) async fn refuse_in_git_mode(state: &AppState, name: &str) -> Result<(), ApiError> {
    if state.store().git_mode().await?.is_some() {
        return Err(ApiError::Conflict(format!(
            "environments are kept in Git: propose the change with POST /api/v1/environments/{name}/proposals"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct GitModeBody {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SourceBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    github_api_url: Option<String>,
    /// The GitHub token is stored; it is never answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    has_github_token: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_by: Option<Uuid>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    enabled_at: Option<OffsetDateTime>,
    /// The latest sync: its `environments` holds `applied`, `refused` and
    /// `not_in_git`.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_sync: Option<SyncReportBody>,
}

impl GitModeBody {
    fn off() -> Self {
        Self {
            enabled: false,
            source: None,
            github_api_url: None,
            has_github_token: None,
            enabled_by: None,
            enabled_at: None,
            latest_sync: None,
        }
    }
}

async fn body(state: &AppState, mode: Option<GitMode>) -> Result<GitModeBody, ApiError> {
    let Some(mode) = mode else {
        return Ok(GitModeBody::off());
    };
    let latest = match state
        .store()
        .latest_sync_report(&SourceName::git_mode())
        .await
    {
        Ok(report) => Some(report.into()),
        Err(StoreError::NotFound) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(GitModeBody {
        enabled: true,
        source: Some(mode.source.into()),
        github_api_url: Some(mode.github_api_url),
        has_github_token: Some(true),
        enabled_by: Some(mode.enabled_by),
        enabled_at: Some(mode.enabled_at),
        latest_sync: latest,
    })
}

pub async fn get(State(state): State<AppState>) -> Result<Json<GitModeBody>, ApiError> {
    let mode = state.store().git_mode().await?;
    Ok(Json(body(&state, mode).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitModePut {
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    allow_private_network: bool,
    #[serde(default)]
    github_api_url: Option<String>,
    #[serde(default)]
    github_token: Option<String>,
}

/// GitHub's API URL: `https://` (or `http://` in builds that allow it),
/// with no credentials, query or fragment.
fn api_url(s: &str, allow_plain_http: bool) -> Result<Url, &'static str> {
    let url = Url::parse(s).map_err(|_| "it isn't a URL")?;
    match url.scheme() {
        "https" => {}
        "http" if allow_plain_http => {}
        _ => return Err("use https://"),
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none_or(str::is_empty)
    {
        return Err("give the API's https:// address, with no credentials, query or fragment");
    }
    Ok(url)
}

/// Turns Git mode on, or replaces its settings. Its first sync starts at
/// once. Tokens not given are cleared; `github_token` is required.
pub async fn put(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ApiJson(b): ApiJson<GitModePut>,
) -> Result<Json<GitModeBody>, ApiError> {
    let plain = state.catalog().allow_plain_http;
    let mut fields = Fields::default();
    let url = fields
        .required("url", b.url)
        .and_then(|u| fields.check("url", SourceUrl::parse(&u, plain)));
    if let Some(u) = &url
        && RepoPath::from_url(u).is_none()
    {
        fields.push(
            "url",
            "give the repository's URL, such as https://github.com/<owner>/<repo>.git",
        );
    }
    let git_ref = match b.git_ref {
        None => Some(None),
        Some(r) => fields.check("ref", GitRef::parse(&r)).map(Some),
    };
    let folder = fields.check("folder", Folder::parse(b.folder.as_deref().unwrap_or("")));
    let github_api_url = match (b.github_api_url, &url) {
        (Some(a), _) => fields
            .check("github_api_url", api_url(&a, plain))
            .map(|u| u.to_string()),
        (None, Some(u)) if u.as_url().host_str() == Some("github.com") => {
            Some(GITHUB_API.to_owned())
        }
        (None, Some(_)) => {
            fields.push(
                "github_api_url",
                "required for a host other than github.com, such as https://ghe.example.com/api/v3",
            );
            None
        }
        (None, None) => None,
    };
    let github_token = fields.required("github_token", b.github_token);
    let (Some(url), Some(git_ref), Some(folder), Some(github_api_url), Some(github_token)) =
        (url, git_ref, folder, github_api_url, github_token)
    else {
        return Err(fields.fail());
    };
    let mode = state
        .store()
        .enable_git_mode(
            state.secret_box(),
            GitModeSettings {
                url,
                git_ref,
                folder,
                username: b.username,
                token: b.token,
                allow_private_network: b.allow_private_network,
                github_api_url,
                github_token,
            },
            caller.user.id,
        )
        .await?;
    tracing::info!(by = %caller.user.id, "Git mode is on");
    sync_in_background(&state, SourceName::git_mode());
    Ok(Json(body(&state, Some(mode)).await?))
}

/// Turns Git mode off; environments stay, and the API edits them again.
pub async fn delete(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<StatusCode, ApiError> {
    state.store().disable_git_mode().await?;
    tracing::info!(by = %caller.user.id, "Git mode is off");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalBody {
    /// The environment as JSON, or its YAML as a string.
    #[serde(default)]
    environment: Option<Value>,
    #[serde(default)]
    delete: bool,
}

#[derive(Serialize)]
pub struct ProposedBody {
    number: u64,
    url: String,
}

/// Proposes a change to environment `name` as a GitHub pull request: its
/// new document (checked as a save would, saved nowhere), or its deletion.
pub async fn propose(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<ProposalBody>,
) -> Result<Response, ApiError> {
    let name = EnvName::parse(&name).map_err(|_| ApiError::NotFound)?;
    let store = state.store();
    let Some(mode) = store.git_mode().await? else {
        return Err(ApiError::Conflict(
            "Git mode is off: save environments with PUT /api/v1/environments/{name}".into(),
        ));
    };
    let existing = match store.get_environment(&name).await {
        Ok(e) => Some(e),
        Err(StoreError::NotFound) => None,
        Err(e) => return Err(e.into()),
    };
    let content = match (b.environment, b.delete) {
        (Some(_), true) | (None, false) => {
            return Err(ApiError::BadRequest(
                "give either environment or delete: true".into(),
            ));
        }
        (None, true) => {
            if existing.is_none() {
                return Err(ApiError::NotFound);
            }
            None
        }
        (Some(value), false) => {
            let doc = match value {
                Value::String(yaml) => environment::parse_yaml(&yaml),
                Value::Object(mut map) => {
                    map.entry("name").or_insert_with(|| json!(name.as_str()));
                    environment::from_json_value(Value::Object(map), false)
                }
                _ => {
                    return Err(ApiError::BadRequest(
                        "environment is a JSON object or YAML text".into(),
                    ));
                }
            }
            .map_err(|problems| {
                ApiError::Validation(
                    problems
                        .into_iter()
                        .map(|p| FieldError::new(p.field, p.message))
                        .collect(),
                )
            })?;
            if doc.name != name {
                let mut fields = Fields::default();
                fields.push("name", "must match the name in the path");
                return Err(fields.fail());
            }
            let spec = EnvironmentSpec::from_doc(doc, None);
            store
                .check_environment(spec.clone(), caller.user.id)
                .await?;
            Some(spec.to_doc().to_yaml())
        }
    };

    let source = &mode.source;
    let path = existing
        .and_then(|e| e.git_path)
        .unwrap_or_else(|| match source.folder.as_str() {
            "" => format!("{name}.yaml"),
            folder => format!("{folder}/{name}.yaml"),
        });
    let repo = RepoPath::from_url(&source.url)
        .ok_or_else(|| ApiError::Conflict("Git mode's URL names no GitHub repository".into()))?;
    let token = store
        .github_token(state.secret_box())
        .await?
        .ok_or_else(|| ApiError::Conflict("Git mode has no GitHub token".into()))?;
    let github = GitHub {
        api: Url::parse(&mode.github_api_url).map_err(|_| ApiError::Internal)?,
        token,
        out: Outbound::new(state.catalog().allow_plain_http),
        allow_private: source.allow_private_network,
    };
    let action = if content.is_some() {
        "update"
    } else {
        "delete"
    };
    let title = format!("Sekisho: {action} environment {name}");
    let pr_body = format!(
        "Proposed in Sekisho by {}.\n\nWhen this is merged, Sekisho applies the change with the \
         repository's next commit.",
        caller.user.email
    );
    let pull = github
        .propose(
            &repo,
            source.git_ref.as_ref().map(GitRef::as_str),
            Proposal {
                environment: name.as_str(),
                path: &path,
                content,
                title: &title,
                body: &pr_body,
            },
        )
        .await
        .map_err(|e| ApiError::Unavailable(format!("GitHub: {e}")))?;
    tracing::info!(environment = %name, number = pull.number, by = %caller.user.id, "proposed an environment change");
    Ok((
        StatusCode::CREATED,
        Json(ProposedBody {
            number: pull.number,
            url: pull.url,
        }),
    )
        .into_response())
}
