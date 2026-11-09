//! The `sekisho` server: admin API, gateway config publishing and, from
//! later tasks, agentgateway's hooks.

pub mod admin_auth;
mod api;
pub mod app;
pub mod config;

pub use api::ApiError;
