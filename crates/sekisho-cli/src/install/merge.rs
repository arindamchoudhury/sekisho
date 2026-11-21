//! One key in a shared config file, the rest of which is the user's: JSON
//! and JSON with comments through `jsonc-parser`'s syntax tree, TOML
//! through `toml_edit`, so comments, order and formatting survive.

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table, TableLike};

use crate::adapters::Format;

/// Why a document couldn't be read or changed.
pub type MergeError = String;

/// The value at `key`, if any.
pub fn get(format: Format, text: &str, key: &[String]) -> Result<Option<Value>, MergeError> {
    match format {
        Format::Json | Format::Jsonc => {
            let root = parse_json(text)?;
            let Some(mut object) = root.object_value() else {
                return Ok(None);
            };
            let (last, parents) = key.split_last().ok_or("an empty key")?;
            for segment in parents {
                match object.object_value(segment) {
                    Some(o) => object = o,
                    None => return Ok(None),
                }
            }
            Ok(object.get(last).and_then(|p| p.to_serde_value()))
        }
        Format::Toml => {
            let doc = parse_toml(text)?;
            let mut item: &Item = doc.as_item();
            for segment in key {
                match item.get(segment) {
                    Some(i) => item = i,
                    None => return Ok(None),
                }
            }
            Ok(Some(toml_to_json(item)))
        }
    }
}

/// `text` (or an empty document) with `key` set to `value`.
pub fn set(
    format: Format,
    text: Option<&str>,
    key: &[String],
    value: &Value,
) -> Result<String, MergeError> {
    let (last, parents) = key.split_last().ok_or("an empty key")?;
    match format {
        Format::Json | Format::Jsonc => {
            let root = parse_json(text.unwrap_or("{}\n"))?;
            let mut object = root.object_value_or_set();
            for segment in parents {
                object = object.object_value_or_set(segment);
            }
            let input = json_input(value);
            match object.get(last) {
                Some(prop) => prop.set_value(input),
                None => {
                    object.append(last, input);
                }
            }
            ensure_multiline(&root);
            Ok(root.to_string())
        }
        Format::Toml => {
            let mut doc = parse_toml(text.unwrap_or(""))?;
            let mut table: &mut dyn TableLike = doc.as_table_mut();
            for segment in parents {
                let entry = table.entry(segment).or_insert_with(|| {
                    let mut t = Table::new();
                    t.set_implicit(true);
                    Item::Table(t)
                });
                table = entry
                    .as_table_like_mut()
                    .ok_or_else(|| format!("{segment} isn't a table"))?;
            }
            table.insert(last, json_to_toml(value, true)?);
            Ok(doc.to_string())
        }
    }
}

/// `text` without `key`, each table or object it leaves empty removed too,
/// and whether nothing is left in the document.
pub fn remove(format: Format, text: &str, key: &[String]) -> Result<(String, bool), MergeError> {
    match format {
        Format::Json | Format::Jsonc => {
            let root = parse_json(text)?;
            let Some(top) = root.object_value() else {
                return Ok((text.to_owned(), false));
            };
            // The objects along the key, from the top.
            let mut path: Vec<CstObject> = vec![top];
            for segment in &key[..key.len().saturating_sub(1)] {
                match path.last().and_then(|o| o.object_value(segment)) {
                    Some(o) => path.push(o),
                    None => return Ok((text.to_owned(), is_empty_json(&root))),
                }
            }
            if let Some(prop) = path.last().and_then(|o| o.get(&key[key.len() - 1])) {
                prop.remove();
            }
            // Empty parents go, innermost first.
            for depth in (1..path.len()).rev() {
                if path[depth].properties().is_empty()
                    && let Some(prop) = path[depth - 1].get(&key[depth - 1])
                {
                    prop.remove();
                } else {
                    break;
                }
            }
            let empty = is_empty_json(&root);
            Ok((root.to_string(), empty))
        }
        Format::Toml => {
            let mut doc = parse_toml(text)?;
            remove_toml(doc.as_table_mut(), key);
            let empty = doc.as_table().is_empty();
            Ok((doc.to_string(), empty))
        }
    }
}

fn remove_toml(table: &mut dyn TableLike, key: &[String]) {
    match key {
        [] => {}
        [last] => {
            table.remove(last);
        }
        [first, rest @ ..] => {
            let now_empty = match table.get_mut(first).and_then(Item::as_table_like_mut) {
                Some(inner) => {
                    remove_toml(inner, rest);
                    inner.is_empty()
                }
                None => false,
            };
            if now_empty {
                table.remove(first);
            }
        }
    }
}

fn parse_json(text: &str) -> Result<CstRootNode, MergeError> {
    CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| e.to_string())
}

fn parse_toml(text: &str) -> Result<DocumentMut, MergeError> {
    text.parse::<DocumentMut>().map_err(|e| e.to_string())
}

fn is_empty_json(root: &CstRootNode) -> bool {
    root.object_value()
        .is_some_and(|o| o.properties().is_empty())
}

/// New objects on several lines, as people write config files.
fn ensure_multiline(root: &CstRootNode) {
    fn walk(object: &CstObject) {
        if !object.properties().is_empty() {
            object.ensure_multiline();
        }
        for prop in object.properties() {
            if let Some(inner) = prop.object_value() {
                walk(&inner);
            }
        }
    }
    if let Some(top) = root.object_value() {
        walk(&top);
    }
}

fn json_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(json_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_input(v)))
                .collect(),
        ),
    }
}

/// A JSON value as TOML: objects as tables at a key (`as_table`), inline
/// inside arrays. TOML has no null.
fn json_to_toml(value: &Value, as_table: bool) -> Result<Item, MergeError> {
    Ok(match value {
        Value::Object(map) if as_table => {
            let mut t = Table::new();
            for (k, v) in map {
                t.insert(k, json_to_toml(v, true)?);
            }
            Item::Table(t)
        }
        other => Item::Value(json_to_toml_value(other)?),
    })
}

fn json_to_toml_value(value: &Value) -> Result<toml_edit::Value, MergeError> {
    Ok(match value {
        Value::Null => return Err("TOML has no null".into()),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.into(),
            (None, Some(f)) => f.into(),
            _ => return Err(format!("{n} doesn't fit TOML")),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => {
            let mut a = toml_edit::Array::new();
            for item in items {
                a.push(json_to_toml_value(item)?);
            }
            toml_edit::Value::Array(a)
        }
        Value::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in map {
                t.insert(k, json_to_toml_value(v)?);
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}

/// A TOML item as JSON, for comparing with what Sekisho wrote.
fn toml_to_json(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(v) => toml_value_to_json(v),
        Item::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), toml_to_json(v)))
                .collect(),
        ),
        Item::ArrayOfTables(a) => Value::Array(
            a.iter()
                .map(|t| {
                    Value::Object(
                        t.iter()
                            .map(|(k, v)| (k.to_owned(), toml_to_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

fn toml_value_to_json(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Value::String(s.value().clone()),
        T::Integer(i) => Value::from(*i.value()),
        T::Float(f) => serde_json::Number::from_f64(*f.value()).map_or(Value::Null, Value::Number),
        T::Boolean(b) => Value::Bool(*b.value()),
        T::Datetime(d) => Value::String(d.value().to_string()),
        T::Array(a) => Value::Array(a.iter().map(toml_value_to_json).collect()),
        T::InlineTable(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), toml_value_to_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn json_keys_are_set_read_and_removed() {
        let text = set(
            Format::Json,
            None,
            &key(&["mcpServers", "db"]),
            &serde_json::json!({"url": "u"}),
        )
        .unwrap();
        assert_eq!(
            get(Format::Json, &text, &key(&["mcpServers", "db", "url"])).unwrap(),
            Some(serde_json::json!("u"))
        );
        let (text, empty) = remove(Format::Json, &text, &key(&["mcpServers", "db"])).unwrap();
        assert!(empty, "{text}");
    }

    #[test]
    fn toml_tables_left_empty_go() {
        let text = set(
            Format::Toml,
            Some("a = 1\n"),
            &key(&["x", "y"]),
            &serde_json::json!("v"),
        )
        .unwrap();
        assert!(text.contains("a = 1"), "{text}");
        let (text, empty) = remove(Format::Toml, &text, &key(&["x", "y"])).unwrap();
        assert!(!empty);
        assert!(!text.contains('x'), "{text}");
    }
}
