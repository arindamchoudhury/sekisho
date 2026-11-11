//! Token counts for answers that never got the provider's usage, counted
//! the way agentgateway v1.7.0 counts prompts (`crates/llm/src/tokenizer.rs`):
//! `o200k_base`, scaled up for Claude, whose own tokenizer isn't public.

use tiktoken::CoreBpe;

/// The `o200k_base` tokens in `text`, counted as agentgateway counts a text
/// part of a prompt.
pub fn count_text(text: &str) -> u64 {
    o200k_base().count_with_special_tokens(text) as u64
}

/// How much more Claude's tokenizers count than `o200k_base` for the same
/// text, as agentgateway assumes: 1.6 from Claude 4.7, 1.3 for any other
/// Claude model, 1.0 for every other model.
pub fn claude_factor(model: &str) -> f64 {
    match claude_version(model) {
        Some(version) if version >= (4, 7) => 1.6,
        Some(_) => 1.3,
        None => 1.0,
    }
}

/// An answer's estimated tokens: its `o200k_base` count scaled for the
/// model, rounded.
pub fn answer_tokens(raw: u64, model: &str) -> u64 {
    (raw as f64 * claude_factor(model)).round() as u64
}

/// The (major, minor) version in a Claude model name, in any provider's
/// naming scheme; a name without a version counts as an older model. Ported
/// from agentgateway v1.7.0 unchanged.
fn claude_version(model: &str) -> Option<(u32, u32)> {
    let rest = &model[model.find("claude")? + "claude".len()..];
    let is_version = |s: &&str| (1..=2).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    let mut segments = rest
        .split(['-', '.', '@', ':', '_'])
        .skip_while(|s| !is_version(s));
    let major = segments.next().map_or(0, |s| s.parse().unwrap_or(0));
    let minor = segments
        .next()
        .filter(is_version)
        .map_or(0, |s| s.parse().unwrap_or(0));
    Some((major, minor))
}

fn o200k_base() -> &'static CoreBpe {
    tiktoken::get_encoding("o200k_base").expect("the vocab-o200k_base feature is enabled")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_text_counts_o200k_tokens() {
        assert_eq!(count_text("hello world"), 2);
        assert_eq!(count_text(""), 0);
    }

    /// agentgateway's `claude_versions` cases at 26f5aeb, as factors: 1.6
    /// from Claude 4.7, 1.3 for any other Claude, 1.0 otherwise.
    #[test]
    fn claude_versions_scale_like_agentgateway() {
        for (model, factor) in [
            ("claude-opus-4-7", 1.6),
            ("claude-sonnet-4-5-20250929", 1.3),
            ("claude-opus-4-20250514", 1.3),
            ("claude-opus-4@20250514", 1.3),
            ("claude-3-7-sonnet-20250219", 1.3),
            ("us.anthropic.claude-sonnet-5-5-v1:0", 1.6),
            ("anthropic.claude-v2", 1.3),
            ("gpt-5", 1.0),
            ("us.anthropic.claude-mock-v1:0", 1.3),
            ("gpt-4o", 1.0),
        ] {
            assert_eq!(claude_factor(model), factor, "{model}");
        }
    }

    #[test]
    fn answer_tokens_round() {
        assert_eq!(answer_tokens(10, "claude-sonnet-4-5"), 13);
        assert_eq!(answer_tokens(10, "claude-opus-4-7"), 16);
        assert_eq!(answer_tokens(10, "gpt-4o"), 10);
        assert_eq!(answer_tokens(5, "claude-sonnet-4-5"), 7); // 6.5 rounds up
    }
}
