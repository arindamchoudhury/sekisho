//! A2A agents' cards for tests, served at `/a2a/<path>` from a map keyed by
//! that path, such as `planner/.well-known/agent-card.json`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;

type Cards = Arc<Mutex<HashMap<String, Value>>>;

pub fn router(cards: Cards) -> Router {
    Router::new()
        .route("/a2a/{*path}", get(card))
        .with_state(cards)
}

async fn card(State(cards): State<Cards>, Path(path): Path<String>) -> Response {
    match cards.lock().unwrap().get(&path).cloned() {
        Some(card) => axum::Json(card).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
