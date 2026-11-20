//! `sekisho env list [--json]`: the environments the signed-in person may
//! use, with a `*` beside each one installed in the folder and the tools
//! that take their models from it (D216).

use serde::{Deserialize, Serialize};

use super::Context;
use crate::install::lock;
use crate::output::{CliError, json, table};

/// One environment, as `GET /api/v1/me/environments` answers it.
#[derive(Deserialize)]
struct Usable {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    notice: String,
    #[serde(default)]
    models: Vec<String>,
}

/// One row of `env list`; `--json` prints these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvRow {
    pub name: String,
    pub description: String,
    pub notice: String,
    pub models: Vec<String>,
    /// Installed in this folder.
    pub installed: bool,
    /// The tools in this folder that take their models from it.
    pub model_tools: Vec<String>,
}

pub async fn env_list(ctx: &Context<'_>, as_json: bool) -> Result<String, CliError> {
    let installed = lock::installed(&lock::project_lock(&ctx.folder))?;
    let (server, credential) = ctx.person().await?;
    let usable: Vec<Usable> = server
        .get_json("/api/v1/me/environments", Some(&credential))
        .await?;
    let rows: Vec<EnvRow> = usable
        .into_iter()
        .map(|e| EnvRow {
            installed: installed.has(&e.name),
            model_tools: installed.model_tools(&e.name),
            name: e.name,
            description: e.description,
            notice: e.notice,
            models: e.models,
        })
        .collect();
    if as_json {
        return Ok(json(&rows));
    }
    if rows.is_empty() {
        return Ok(
            "You may use no environments yet: ask an admin to add one of your groups to one.\n"
                .to_owned(),
        );
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                format!("{} {}", if r.installed { "*" } else { " " }, r.name),
                r.models.join(", "),
                r.model_tools.join(", "),
                r.description.clone(),
            ]
        })
        .collect();
    Ok(table(
        &["  NAME", "MODELS", "MODELS FOR", "DESCRIPTION"],
        &cells,
    ))
}
