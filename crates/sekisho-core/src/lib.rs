//! Sekisho's domain types. Pure values and rules, no IO.

pub mod access;
pub mod catalog;
pub mod formats;
pub mod keys;
pub mod money;
pub mod names;
pub mod presets;
pub mod pricing;
pub mod secret_box;
pub mod session_token;
pub mod window;

/// The header a coding tool names itself in on each model call (D227):
/// `sekisho use` writes it where the tool takes headers, and `sekisho
/// run`'s proxy adds it for the rest. Its value is the tool's adapter name
/// (`claude-code`, `gemini-cli`, …). It is the client's own, so it is for
/// reports only (D228), and the gateway removes it before the provider.
pub const TOOL_HEADER: &str = "x-sekisho-tool";
