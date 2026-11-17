//! The `sekisho` server: the API for people and admins, sign-in, gateway
//! config publishing, agentgateway's hooks and catalog syncs (served by
//! `sekisho serve`).

mod api;
pub mod app;
pub mod auth;
pub mod catalog;
pub mod config;

pub use api::ApiError;
