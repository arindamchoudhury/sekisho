//! Validated names for environments, providers, models, groups and
//! sign-in connectors.

use std::fmt;

/// Why a name was refused. The message is shown to admins as is.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what} name {name:?}: {rule}")]
pub struct NameError {
    what: &'static str,
    name: String,
    rule: &'static str,
}

const SLUG_RULE: &str =
    "use 1-63 lowercase letters, digits or hyphens, starting and ending with a letter or digit";
const MODEL_RULE: &str =
    "use 1-128 letters, digits or the characters . _ : / -, starting with a letter or digit";

/// Lowercase `[a-z0-9-]`, 1–63 characters, no leading or trailing hyphen.
fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    let edge_ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=63).contains(&b.len())
        && b.first().is_some_and(edge_ok)
        && b.last().is_some_and(edge_ok)
        && b.iter().all(|c| edge_ok(c) || *c == b'-')
}

/// Dex's own connector for local accounts.
const DEX_LOCAL: &str = "local";
const CONNECTOR_RULE: &str = "use 1-63 lowercase letters, digits or hyphens, starting and ending with a letter or digit; local is reserved for Dex's local accounts";

/// A slug other than `local`.
fn is_connector_id(s: &str) -> bool {
    is_slug(s) && s != DEX_LOCAL
}

/// `^[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}$`
fn is_model_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._:/-".contains(c))
}

macro_rules! name_type {
    ($(#[$doc:meta])* $ty:ident, $what:literal, $valid:ident, $rule:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $ty(String);

        impl $ty {
            pub fn parse(s: &str) -> Result<Self, NameError> {
                if $valid(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(NameError { what: $what, name: s.to_owned(), rule: $rule })
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// An environment's name, which is also its route prefix (`/env/<name>/…`).
    EnvName, "environment", is_slug, SLUG_RULE
);
name_type!(
    /// A provider's name, unique across the install.
    ProviderName, "provider", is_slug, SLUG_RULE
);
name_type!(
    /// A model's name as the provider spells it, such as `claude-sonnet-5-5`.
    ModelName, "model", is_model_name, MODEL_RULE
);
name_type!(
    /// A Sekisho group's name. Identity providers' groups are plain strings;
    /// only groups made in Sekisho follow this rule.
    GroupName, "group", is_slug, SLUG_RULE
);
name_type!(
    /// The ID of an identity provider's connector at Dex, such as `github`.
    ConnectorId, "connector", is_connector_id, CONNECTOR_RULE
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_name_rules() {
        assert!(EnvName::parse("data-team").is_ok());
        for bad in ["", "Data", "-x", "x-", "a_b", &"a".repeat(64)] {
            assert!(EnvName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_name_edges() {
        for good in ["a", "0", "a-b-c", "ml2", &"a".repeat(63)] {
            assert_eq!(EnvName::parse(good).unwrap().as_str(), good);
        }
        for bad in ["-", "a b", "a.b", "é", "data-team\n"] {
            assert!(EnvName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn provider_name_rules() {
        assert_eq!(
            ProviderName::parse("anthropic").unwrap().as_str(),
            "anthropic"
        );
        for bad in ["", "OpenAI", "-x", "x-", "a_b", &"a".repeat(64)] {
            assert!(ProviderName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn model_name_rules() {
        for good in [
            "claude-sonnet-5-5",
            "gpt-5.1",
            "models/gemini-3-pro",
            "meta-llama/Llama-3:8b",
            "M",
            &"a".repeat(128),
        ] {
            assert_eq!(ModelName::parse(good).unwrap().as_str(), good);
        }
        for bad in ["", "-x", ".x", "/x", "a b", "é", &"a".repeat(129)] {
            assert!(ModelName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn group_and_connector_names() {
        for good in ["data-team", "a", &"a".repeat(63)] {
            assert_eq!(GroupName::parse(good).unwrap().as_str(), good);
        }
        for bad in ["acme:team", "Data", "", "-x", "x-", "a_b", &"a".repeat(64)] {
            assert!(GroupName::parse(bad).is_err(), "{bad:?}");
        }
        for good in ["github", "corp-ldap", "local2", "my-local"] {
            assert_eq!(ConnectorId::parse(good).unwrap().as_str(), good);
        }
        for bad in ["local", "Local", "acme:team", "", "-x", &"a".repeat(64)] {
            assert!(ConnectorId::parse(bad).is_err(), "{bad:?}");
        }
        let refused = ConnectorId::parse("local").unwrap_err().to_string();
        assert!(refused.contains("reserved"), "{refused}");
    }
}
