//! Validated names for environments, providers, models, groups, sign-in
//! connectors, catalog sources and catalog entries.

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

/// The name Git mode's source takes; no other source may.
const GIT_MODE_SOURCE: &str = "environments";
const SOURCE_RULE: &str = "use 1-63 lowercase letters, digits or hyphens, starting and ending with a letter or digit; environments is reserved for Git mode's source";

/// A slug other than `environments`.
fn is_source_name(s: &str) -> bool {
    is_slug(s) && s != GIT_MODE_SOURCE
}

const ENTRY_RULE: &str = "use 1-64 lowercase letters, digits or single hyphens, starting and ending with a letter or digit";

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, 1–64 characters: the Agent Skills name rule.
fn is_entry_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b.split(|c| *c == b'-').all(|part| {
            !part.is_empty()
                && part
                    .iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
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

        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        /// Checked like [`Self::parse`].
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(d)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
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
    /// A Sekisho group's name. Identity providers' groups don't follow this
    /// rule: Sekisho names them `<connector>/<group>`
    /// ([`crate::access::idp_group`]).
    GroupName, "group", is_slug, SLUG_RULE
);
name_type!(
    /// The ID of an identity provider's connector at Dex, such as `github`.
    ConnectorId, "connector", is_connector_id, CONNECTOR_RULE
);
name_type!(
    /// A catalog source's name, unique across the install. `environments`
    /// is Git mode's alone ([`SourceName::git_mode`]).
    SourceName, "source", is_source_name, SOURCE_RULE
);
name_type!(
    /// A catalog entry's name: a skill, plugin, prompt, MCP server or A2A
    /// agent, unique within its source and kind.
    EntryName, "entry", is_entry_name, ENTRY_RULE
);

impl SourceName {
    /// The source that holds the environments of Git mode.
    pub fn git_mode() -> Self {
        Self(GIT_MODE_SOURCE.to_owned())
    }
}

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

    #[test]
    fn names_are_json_strings_checked_on_the_way_in() {
        let name = EntryName::parse("pdf").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"pdf\"");
        assert_eq!(serde_json::from_str::<EntryName>("\"pdf\"").unwrap(), name);
        let err = serde_json::from_str::<EntryName>("\"Pdf\"").unwrap_err();
        assert!(err.to_string().contains("invalid entry name"), "{err}");
        assert!(serde_json::from_str::<SourceName>("\"environments\"").is_err());
    }
}
