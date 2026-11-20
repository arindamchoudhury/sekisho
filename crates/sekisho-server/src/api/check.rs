//! `POST /api/v1/check/references` (D212): whether each catalog reference
//! names exactly one entry, for `sekisho check --server` in CI. Any
//! signed-in person or environment key may ask: entry names aren't secret,
//! since every delivery document carries them.

use axum::Json;
use axum::extract::State;
use sekisho_catalog::names::{EntryKind, EntryRef};
use sekisho_store::ResolveError;
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiJson};
use crate::app::AppState;

/// The most references one request may check.
const MAX_REFERENCES: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckBody {
    references: Vec<ReferenceBody>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceBody {
    kind: EntryKind,
    reference: String,
}

/// The entry the reference names, or why it names none or several.
#[derive(Serialize)]
pub struct AnswerBody {
    kind: EntryKind,
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Answers in the order asked.
pub async fn references(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CheckBody>,
) -> Result<Json<Vec<AnswerBody>>, ApiError> {
    if body.references.len() > MAX_REFERENCES {
        return Err(ApiError::BadRequest(format!(
            "check at most {MAX_REFERENCES} references at once"
        )));
    }
    let mut answers = Vec::with_capacity(body.references.len());
    for r in body.references {
        let mut answer = AnswerBody {
            kind: r.kind,
            reference: r.reference,
            source: None,
            name: None,
            error: None,
        };
        match EntryRef::parse(&answer.reference) {
            Err(e) => answer.error = Some(e.to_string()),
            Ok(reference) => match state.store().resolve_key(r.kind, &reference).await {
                Ok(key) => {
                    answer.source = Some(key.source.to_string());
                    answer.name = Some(key.name.to_string());
                }
                Err(ResolveError::Store(e)) => return Err(e.into()),
                Err(e @ ResolveError::NotFound) => answer.error = Some(e.to_string()),
                Err(e @ ResolveError::Ambiguous(_)) => {
                    answer.error = Some(format!(
                        "names an entry {e}; write name@source to choose one"
                    ));
                }
            },
        }
        answers.push(answer);
    }
    Ok(Json(answers))
}
