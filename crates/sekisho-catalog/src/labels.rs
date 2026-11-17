//! Environment labels (D172): free-form key-value pairs for admins'
//! own bookkeeping, such as a cost centre.

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

/// At most this many labels on one environment.
pub const MAX_LABELS: usize = 32;
const MAX_KEY: usize = 63;
const MAX_VALUE: usize = 256;

/// One refusal: the field it is about (`labels` or `labels.<key>`) and why.
pub type LabelError = (String, String);

/// A checked set of labels, sorted by key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// Checks every label, and names each one refused.
    pub fn new(map: BTreeMap<String, String>) -> Result<Self, Vec<LabelError>> {
        let mut errors = Vec::new();
        if map.len() > MAX_LABELS {
            errors.push((
                "labels".to_owned(),
                format!("at most {MAX_LABELS} labels are allowed"),
            ));
            return Err(errors);
        }
        for (key, value) in &map {
            if let Some(why) = key_problem(key).or_else(|| value_problem(value)) {
                errors.push((format!("labels.{key}"), why.to_owned()));
            }
        }
        if errors.is_empty() {
            Ok(Self(map))
        } else {
            Err(errors)
        }
    }

    /// Reads labels from YAML, where numbers and booleans become their text
    /// (`cost-centre: 4711` is `"4711"`). `~` is no labels; anything but a
    /// map of scalars is refused.
    pub fn from_yaml(value: &Value) -> Result<Self, Vec<LabelError>> {
        let mapping = match value {
            Value::Null => return Ok(Self::default()),
            Value::Mapping(m) => m,
            _ => {
                return Err(vec![(
                    "labels".to_owned(),
                    "labels must be a map of keys to text".to_owned(),
                )]);
            }
        };
        let mut map = BTreeMap::new();
        let mut errors = Vec::new();
        for (key, value) in mapping {
            let Some(key) = scalar_text(key) else {
                errors.push(("labels".to_owned(), "a label's key must be text".to_owned()));
                continue;
            };
            match scalar_text(value) {
                Some(text) => {
                    map.insert(key, text);
                }
                None => errors.push((
                    format!("labels.{key}"),
                    "a label's value must be text, a number or true or false".to_owned(),
                )),
            }
        }
        match Self::new(map) {
            Ok(labels) if errors.is_empty() => Ok(labels),
            Ok(_) => Err(errors),
            Err(more) => {
                errors.extend(more);
                Err(errors)
            }
        }
    }

    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn key_problem(key: &str) -> Option<&'static str> {
    let b = key.as_bytes();
    let edge = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let ok = (1..=MAX_KEY).contains(&b.len())
        && b.first().is_some_and(edge)
        && b.last().is_some_and(edge)
        && b.iter().all(|c| edge(c) || b"._-".contains(c));
    if !ok {
        Some(
            "a key has 1-63 lowercase letters, digits, dots, underscores or hyphens, starting and ending with a letter or digit",
        )
    } else if key.starts_with("sekisho") {
        Some("keys starting with sekisho are reserved")
    } else {
        None
    }
}

fn value_problem(value: &str) -> Option<&'static str> {
    let chars = value.chars().count();
    if !(1..=MAX_VALUE).contains(&chars) {
        Some("a value has 1-256 characters")
    } else if value.chars().any(char::is_control) {
        Some("a value can't hold control characters")
    } else if value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace) {
        Some("a value can't start or end with whitespace")
    } else {
        None
    }
}
