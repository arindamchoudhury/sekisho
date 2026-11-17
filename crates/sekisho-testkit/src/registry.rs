//! A container registry for tests: manifests answered by digest under
//! `/v2/`, and optionally the Bearer token flow of Docker's registries
//! (`/token`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};

/// The token the registry hands out and wants back.
pub const TOKEN: &str = "testkit-registry-token";

#[derive(Clone)]
struct Registry {
    /// `repo:tag` → `sha256:…`.
    images: Arc<Mutex<HashMap<String, String>>>,
    require_token: bool,
}

/// Answers `HEAD`/`GET /v2/<repo>/manifests/<tag>` with the digest in
/// `images` (`repo:tag`), else 404; with `require_token`, only after the
/// token flow.
pub fn router(images: Arc<Mutex<HashMap<String, String>>>, require_token: bool) -> Router {
    Router::new()
        .route("/v2/{*path}", any(manifest))
        .route("/token", get(token))
        .with_state(Registry {
            images,
            require_token,
        })
}

async fn manifest(
    State(r): State<Registry>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((repo, tag)) = path.split_once("/manifests/") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bearer = format!("Bearer {TOKEN}");
    let authorised = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(bearer.as_str());
    if r.require_token && !authorised {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1");
        let challenge = format!(
            "Bearer realm=\"http://{host}/token\",service=\"testkit-registry\",scope=\"repository:{repo}:pull\""
        );
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, challenge)],
        )
            .into_response();
    }
    let digest = r
        .images
        .lock()
        .unwrap()
        .get(&format!("{repo}:{tag}"))
        .cloned();
    match digest {
        Some(digest) => (
            StatusCode::OK,
            [
                ("docker-content-digest", digest),
                (
                    header::CONTENT_TYPE.as_str(),
                    "application/vnd.oci.image.index.v1+json".to_owned(),
                ),
            ],
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn token() -> impl IntoResponse {
    axum::Json(serde_json::json!({"token": TOKEN}))
}
