//! Git mode's applier (D175): every environment a commit holds is saved as
//! a save through the API would be, each file on its own; a file that fails
//! keeps its environment as it was. A sync never deletes an environment:
//! one whose file is gone is listed, and can then be deleted.

use sekisho_catalog::model::SourceCheck;
use sekisho_core::names::EnvName;
use sekisho_store::{EnvironmentSpec, Source, StoreError, SyncFound};
use serde_json::json;

use crate::app::AppState;

/// One file that couldn't be applied, as the report gives it.
fn refusal(path: &str, name: Option<&str>, fields: serde_json::Value) -> serde_json::Value {
    json!({ "path": path, "name": name, "fields": fields })
}

/// The path relative to the repository's root.
fn repo_path(source: &Source, path: &str) -> String {
    match source.folder.as_str() {
        "" => path.to_owned(),
        folder => format!("{folder}/{path}"),
    }
}

/// Applies what the commit holds and answers the sync's findings, the
/// report's `environments` being `{applied, refused, not_in_git}`.
pub async fn apply(
    state: &AppState,
    source: &Source,
    check: SourceCheck,
) -> Result<SyncFound, String> {
    let store = state.store();
    let owner = match store.git_mode().await {
        Ok(Some(mode)) => mode.enabled_by,
        Ok(None) => return Err("Git mode is off".to_owned()),
        Err(_) => return Err("Git mode's settings can't be read".to_owned()),
    };
    let mut applied: Vec<EnvName> = Vec::new();
    let mut refused = Vec::new();
    // Environments still in Git: applied, or named by a file that failed.
    let mut found: Vec<EnvName> = Vec::new();
    // Files that failed keep their environments, matched by path too.
    let mut kept_paths: Vec<String> = Vec::new();
    for r in &check.refused {
        let path = repo_path(source, &r.path);
        if let Some(name) = r.name.as_deref().and_then(|n| EnvName::parse(n).ok()) {
            found.push(name);
        }
        refused.push(refusal(
            &path,
            r.name.as_deref(),
            json!([{ "field": "", "message": r.reason }]),
        ));
        kept_paths.push(path);
    }
    for (path, doc) in check.environments {
        let path = repo_path(source, &path);
        let name = doc.name.clone();
        found.push(name.clone());
        let spec = EnvironmentSpec::from_doc(doc, None);
        match store.apply_git_environment(spec, &path, owner).await {
            Ok(_) => applied.push(name),
            Err(StoreError::Validation(fields)) => {
                let fields: Vec<_> = fields
                    .iter()
                    .map(|f| json!({ "field": f.field, "message": f.message }))
                    .collect();
                refused.push(refusal(&path, Some(name.as_str()), json!(fields)));
                kept_paths.push(path);
            }
            Err(error) => {
                tracing::warn!(%path, %error, "Git mode can't save an environment");
                refused.push(refusal(
                    &path,
                    Some(name.as_str()),
                    json!([{ "field": "", "message": "it couldn't be saved; see the server's log" }]),
                ));
                kept_paths.push(path);
            }
        }
    }
    let not_in_git = store
        .set_git_paths_missing(&found, &kept_paths)
        .await
        .map_err(|_| "environments no longer in Git can't be marked".to_owned())?;
    let names = |list: &[EnvName]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    tracing::info!(
        applied = applied.len(),
        refused = refused.len(),
        not_in_git = not_in_git.len(),
        "Git mode applied a commit"
    );
    Ok(SyncFound {
        warnings: check.warnings,
        environments: json!({
            "applied": names(&applied),
            "refused": refused,
            "not_in_git": names(&not_in_git),
        }),
        ..SyncFound::default()
    })
}
