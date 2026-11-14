//! The `sekisho` server: the API for people and admins, sign-in, gateway
//! config publishing, and agentgateway's hooks (served by `sekisho serve`).

mod api;
pub mod app;
pub mod auth;
pub mod config;

pub use api::ApiError;
