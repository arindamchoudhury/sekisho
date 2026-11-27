//! One key in a shared config file, the rest of which is the user's: JSON
//! and JSON with comments through `jsonc-parser`'s syntax tree, TOML
//! through `toml_edit`, so comments, order and formatting survive.

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstArray, CstInputValue, CstNode, CstObject, CstRootNode};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table, TableLike};

use crate::adapters::Format;

/// Why a document couldn't be read or changed.
pub type MergeError = String;

/// Starts a key's last segment that names one item of an array rather than
/// a key, so a tool's list (Claude Code's `enabledMcpjsonServers`) can hold
/// Sekisho's items beside the user's, each owned on its own.
pub const ITEM: char = '\u{1}';

/// The key of `item` in the array at `array`.
pub fn item_key(array: &[&str], item: &str) -> Vec<String> {
    let mut key: Vec<String> = array.iter().map(|s| (*s).to_owned()).collect();
    key.push(format!("{ITEM}{item}"));
    key
}

/// The array's key and the item, when `key` names an item.
fn split_item(key: &[String]) -> Option<(&[String], &str)> {
    let (last, array) = key.split_last()?;
    let item = last.strip_prefix(ITEM)?;
    (!array.is_empty()).then_some((array, item))
}

/// Starts a key's last segment that names one `Name: Value` line of a
/// string of such lines rather than a key, so a variable holding several
/// headers (Claude Code's `ANTHROPIC_CUSTOM_HEADERS`) can hold Sekisho's
/// line beside the user's. The line is found by its name, in any case.
pub const LINE: char = '\u{2}';

/// The key of the line named `name` in the string at `at`. Its value is
/// the whole line, `Name: Value`.
pub fn line_key(at: &[&str], name: &str) -> Vec<String> {
    let mut key: Vec<String> = at.iter().map(|s| (*s).to_owned()).collect();
    key.push(format!("{LINE}{name}"));
    key
}

/// The string's key and the line's name, when `key` names a line.
fn split_line(key: &[String]) -> Option<(&[String], &str)> {
    let (last, at) = key.split_last()?;
    let name = last.strip_prefix(LINE)?;
    (!at.is_empty()).then_some((at, name))
}

/// `line` is a `Name: Value` line named `name`.
fn names(line: &str, name: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(n, _)| n.trim().eq_ignore_ascii_case(name))
}

/// What is at `at`: `None` when nothing is.
fn value_at(
    format: Format,
    text: Option<&str>,
    at: &[String],
) -> Result<Option<Value>, MergeError> {
    match text {
        Some(t) => get(format, t, at),
        None => Ok(None),
    }
}

/// Starts a key's last segment that names one marked section of a string
/// rather than a key, so a value a tool takes once per scope (Codex's
/// `developer_instructions`) holds a section per environment, beside the
/// person's own text. A section is `<!-- sekisho:<id> -->`, its text, then
/// `<!-- /sekisho:<id> -->`, on lines of their own.
pub const SECTION: char = '\u{3}';

/// The key of the section `id` in the string at `at`. Its value is the
/// section's text.
pub fn section_key(at: &[&str], id: &str) -> Vec<String> {
    let mut key: Vec<String> = at.iter().map(|s| (*s).to_owned()).collect();
    key.push(format!("{SECTION}{id}"));
    key
}

/// The string's key and the section's id, when `key` names a section.
fn split_section(key: &[String]) -> Option<(&[String], &str)> {
    let (last, at) = key.split_last()?;
    let id = last.strip_prefix(SECTION)?;
    (!at.is_empty()).then_some((at, id))
}

/// Where the section `id` is in `s`: its start and end, past its last line.
fn find_section(s: &str, id: &str) -> Option<(usize, usize)> {
    let open = format!("<!-- sekisho:{id} -->");
    let close = format!("<!-- /sekisho:{id} -->");
    let start = s
        .match_indices(&open)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || s[..i].ends_with('\n'))?;
    let from = start + open.len();
    let end = s[from..].find(&close)? + from + close.len();
    Some((start, end))
}

/// `s` with the section `id` holding `text`, added at the end when new.
fn with_section(s: &str, id: &str, text: &str) -> String {
    let section = format!(
        "<!-- sekisho:{id} -->\n{}\n<!-- /sekisho:{id} -->",
        text.trim_end_matches(['\n', '\r'])
    );
    match find_section(s, id) {
        Some((start, end)) => format!("{}{section}{}", &s[..start], &s[end..]),
        None if s.trim().is_empty() => section,
        None => format!("{}\n\n{section}", s.trim_end()),
    }
}

/// `s` without the section `id`, and the blank lines it leaves.
fn without_section(s: &str, id: &str) -> String {
    let Some((start, end)) = find_section(s, id) else {
        return s.to_owned();
    };
    let before = s[..start].trim_end();
    let after = s[end..].trim_start_matches(['\n', '\r']);
    match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_owned(),
        (false, true) => before.to_owned(),
        (false, false) => format!("{before}\n\n{after}"),
    }
}

/// The string `key`'s section lives in holds text of the person's and no
/// section of Sekisho's: adding one changes what they wrote (D202).
pub fn foreign_text(format: Format, text: &str, key: &[String]) -> bool {
    let Some((at, _)) = split_section(key) else {
        return false;
    };
    match get(format, text, at) {
        Ok(Some(Value::String(s))) => !s.trim().is_empty() && !s.contains("<!-- sekisho:"),
        _ => false,
    }
}

/// `key` for people: an item as `[name]`, a line as `[name:]`, a section
/// as `[sekisho:<id>]`.
pub fn show_key(key: &[String]) -> String {
    key.iter()
        .map(|s| {
            if let Some(item) = s.strip_prefix(ITEM) {
                format!("[{item}]")
            } else if let Some(line) = s.strip_prefix(LINE) {
                format!("[{line}:]")
            } else if let Some(id) = s.strip_prefix(SECTION) {
                format!("[sekisho:{id}]")
            } else {
                s.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// The object `parents` lead to, if every one exists.
fn find_object(root: &CstRootNode, parents: &[String]) -> Option<CstObject> {
    let mut object = root.object_value()?;
    for segment in parents {
        object = object.object_value(segment)?;
    }
    Some(object)
}

/// The array's element that is the string `item`.
fn holds(array: &CstArray, item: &str) -> Option<CstNode> {
    array
        .elements()
        .into_iter()
        .find(|e| e.to_serde_value() == Some(Value::String(item.to_owned())))
}

/// The first line of the block Sekisho manages in a Markdown file.
pub const BLOCK_START: &str =
    "<!-- sekisho:start: managed by `sekisho use`; edits are overwritten -->";
/// Its last line.
pub const BLOCK_END: &str = "<!-- sekisho:end -->";

/// The block's start and end lines (indexes into `lines`), if there is one.
fn find_block(lines: &[&str]) -> Result<Option<(usize, usize)>, MergeError> {
    let Some(start) = lines
        .iter()
        .position(|l| l.trim_end().starts_with("<!-- sekisho:start"))
    else {
        return Ok(None);
    };
    match lines[start + 1..]
        .iter()
        .position(|l| l.trim_end() == BLOCK_END)
    {
        Some(i) => Ok(Some((start, start + 1 + i))),
        None => Err("Sekisho's block has no end line".into()),
    }
}

/// The text holds Sekisho's block.
pub fn has_block(text: &str) -> bool {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    matches!(find_block(&lines), Ok(Some(_)))
}

/// The one line a Markdown block key names.
fn block_line(key: &[String]) -> Result<&str, MergeError> {
    match key {
        [line] if !line.contains(['\n', '\r']) => Ok(line),
        _ => Err("a Markdown block key is one line".into()),
    }
}

fn block_get(text: &str, key: &[String]) -> Result<Option<Value>, MergeError> {
    let line = block_line(key)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    Ok(find_block(&lines)?.and_then(|(start, end)| {
        lines[start + 1..end]
            .iter()
            .any(|l| l.trim_end() == line)
            .then_some(Value::Bool(true))
    }))
}

fn block_set(text: Option<&str>, key: &[String], value: &Value) -> Result<String, MergeError> {
    let line = block_line(key)?;
    if *value != Value::Bool(true) {
        return Err("a Markdown block line's value is true".into());
    }
    let text = text.unwrap_or("");
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    match find_block(&lines)? {
        Some((start, end)) => {
            if lines[start + 1..end].iter().any(|l| l.trim_end() == line) {
                return Ok(text.to_owned());
            }
            let eol = if lines[end].ends_with("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let mut out: String = lines[..end].concat();
            out.push_str(line);
            out.push_str(eol);
            out.push_str(&lines[end..].concat());
            Ok(out)
        }
        None => {
            let mut out = text.to_owned();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(&format!("{BLOCK_START}\n{line}\n{BLOCK_END}\n"));
            Ok(out)
        }
    }
}

/// `text` without the line; the block goes with its last line.
fn block_remove(text: &str, key: &[String]) -> Result<(String, bool), MergeError> {
    let line = block_line(key)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((start, end)) = find_block(&lines)? else {
        return Ok((text.to_owned(), text.trim().is_empty()));
    };
    let kept: Vec<&str> = lines[start + 1..end]
        .iter()
        .copied()
        .filter(|l| l.trim_end() != line)
        .collect();
    let mut out: String = lines[..start].concat();
    if !kept.is_empty() {
        out.push_str(lines[start]);
        out.push_str(&kept.concat());
        out.push_str(lines[end]);
    }
    out.push_str(&lines[end + 1..].concat());
    let empty = out.trim().is_empty();
    Ok((out, empty))
}

/// The value at `key`, if any; for an item key, the item while the array
/// holds it.
pub fn get(format: Format, text: &str, key: &[String]) -> Result<Option<Value>, MergeError> {
    if format == Format::MarkdownBlock {
        return block_get(text, key);
    }
    if let Some((at, id)) = split_section(key) {
        let found = match get(format, text, at)? {
            Some(Value::String(s)) => find_section(&s, id).map(|(start, end)| {
                let inner = &s[start..end];
                let body = inner
                    .split_once('\n')
                    .map_or("", |(_, rest)| rest)
                    .rsplit_once('\n')
                    .map_or("", |(body, _)| body);
                body.to_owned()
            }),
            _ => None,
        };
        return Ok(found.map(Value::String));
    }
    if let Some((at, name)) = split_line(key) {
        let found = match get(format, text, at)? {
            Some(Value::String(s)) => s.lines().find(|l| names(l, name)).map(str::to_owned),
            _ => None,
        };
        return Ok(found.map(Value::String));
    }
    if let Some((array, item)) = split_item(key) {
        if format == Format::Toml {
            return Err("array items are kept only in JSON".into());
        }
        let root = parse_json(text)?;
        let (name, parents) = array.split_last().expect("not empty");
        let found = find_object(&root, parents)
            .and_then(|o| o.array_value(name))
            .and_then(|a| holds(&a, item));
        return Ok(found.map(|_| Value::String(item.to_owned())));
    }
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
        Format::MarkdownBlock => unreachable!("handled above"),
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
    if format == Format::MarkdownBlock {
        return block_set(text, key, value);
    }
    if let Some((at, id)) = split_section(key) {
        let body = value.as_str().ok_or("a section is a string")?;
        let now = match value_at(format, text, at)? {
            Some(Value::String(s)) => s,
            None => String::new(),
            Some(_) => return Err(format!("{} isn't a string", show_key(at))),
        };
        return set(
            format,
            text,
            at,
            &Value::String(with_section(&now, id, body)),
        );
    }
    if let Some((at, name)) = split_line(key) {
        let line = value.as_str().ok_or("a line is a string")?;
        let now = match value_at(format, text, at)? {
            Some(Value::String(s)) => s,
            None => String::new(),
            Some(_) => return Err(format!("{} isn't a string", show_key(at))),
        };
        // The line takes the place of the first of its name; any other
        // copy goes, so the tool sends one.
        let mut lines: Vec<&str> = Vec::new();
        let mut placed = false;
        for l in now.lines().filter(|l| !l.trim().is_empty()) {
            if !names(l, name) {
                lines.push(l);
            } else if !placed {
                lines.push(line);
                placed = true;
            }
        }
        if !placed {
            lines.push(line);
        }
        return set(format, text, at, &Value::String(lines.join("\n")));
    }
    let (last, parents) = key.split_last().ok_or("an empty key")?;
    if let Some((array, item)) = split_item(key) {
        if format == Format::Toml {
            return Err("array items are kept only in JSON".into());
        }
        let root = parse_json(text.unwrap_or("{}\n"))?;
        let (name, parents) = array.split_last().expect("not empty");
        let mut object = root.object_value_or_set();
        for segment in parents {
            object = object.object_value_or_set(segment);
        }
        let list = object.array_value_or_set(name);
        if holds(&list, item).is_none() {
            list.append(CstInputValue::String(item.to_owned()));
        }
        ensure_multiline(&root);
        return Ok(root.to_string());
    }
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
        Format::MarkdownBlock => unreachable!("handled above"),
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
    if format == Format::MarkdownBlock {
        return block_remove(text, key);
    }
    if let Some((at, id)) = split_section(key) {
        let now = match get(format, text, at)? {
            Some(Value::String(s)) => s,
            None => return remove(format, text, at),
            Some(_) => return Ok((text.to_owned(), false)),
        };
        let left = without_section(&now, id);
        if left.trim().is_empty() {
            return remove(format, text, at);
        }
        let text = set(format, Some(text), at, &Value::String(left))?;
        return Ok((text, false));
    }
    if let Some((at, name)) = split_line(key) {
        let now = match get(format, text, at)? {
            Some(Value::String(s)) => s,
            // Nothing there: removing the missing key changes nothing.
            None => return remove(format, text, at),
            Some(_) => return Ok((text.to_owned(), false)),
        };
        let left: Vec<&str> = now
            .lines()
            .filter(|l| !l.trim().is_empty() && !names(l, name))
            .collect();
        if left.is_empty() {
            return remove(format, text, at);
        }
        let text = set(format, Some(text), at, &Value::String(left.join("\n")))?;
        return Ok((text, false));
    }
    if let Some((array, item)) = split_item(key) {
        if format == Format::Toml {
            return Err("array items are kept only in JSON".into());
        }
        let root = parse_json(text)?;
        let (name, parents) = array.split_last().expect("not empty");
        if let Some(list) = find_object(&root, parents).and_then(|o| o.array_value(name)) {
            if let Some(node) = holds(&list, item) {
                node.remove();
            }
            if list.elements().is_empty() {
                // The array goes too, and the objects it leaves empty.
                return remove(format, &root.to_string(), array);
            }
        }
        let empty = is_empty_json(&root);
        return Ok((root.to_string(), empty));
    }
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
        Format::MarkdownBlock => unreachable!("handled above"),
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
    fn markdown_block_lines_are_set_read_and_removed() {
        let md = Format::MarkdownBlock;
        let a = key(&["@./a.md"]);
        let b = key(&["@./b.md"]);
        let yes = serde_json::json!(true);
        // A new file holds just the block.
        let text = set(md, None, &a, &yes).unwrap();
        assert_eq!(text, format!("{BLOCK_START}\n@./a.md\n{BLOCK_END}\n"));
        assert_eq!(get(md, &text, &a).unwrap(), Some(yes.clone()));
        assert_eq!(get(md, &text, &b).unwrap(), None);
        // The person's file gets the block at its end, after a blank line.
        let mine = "# Mine\n\n@./outside.md\n";
        let text = set(md, Some(mine), &a, &yes).unwrap();
        assert_eq!(
            text,
            format!("# Mine\n\n@./outside.md\n\n{BLOCK_START}\n@./a.md\n{BLOCK_END}\n")
        );
        assert_eq!(get(md, &text, &key(&["@./outside.md"])).unwrap(), None);
        assert!(has_block(&text) && !has_block(mine));
        // Lines are added at the block's end, once.
        let text = set(md, Some(&text), &b, &yes).unwrap();
        assert_eq!(set(md, Some(&text), &a, &yes).unwrap(), text);
        let around = text.replace("# Mine", "# Mine, edited") + "Below.\n";
        // Removing a line keeps the rest; the last one takes the block.
        let (text, empty) = remove(md, &around, &a).unwrap();
        assert!(!empty);
        assert_eq!(
            text,
            format!(
                "# Mine, edited\n\n@./outside.md\n\n{BLOCK_START}\n@./b.md\n{BLOCK_END}\nBelow.\n"
            )
        );
        let (text, empty) = remove(md, &text, &b).unwrap();
        assert!(!empty);
        assert_eq!(text, "# Mine, edited\n\n@./outside.md\n\nBelow.\n");
        let (_, empty) = remove(md, &set(md, None, &a, &yes).unwrap(), &a).unwrap();
        assert!(empty);
        // CRLF files keep working; only `true` is a value; a block without
        // its end is refused.
        let crlf = format!("# Mine\r\n{BLOCK_START}\r\n@./a.md\r\n{BLOCK_END}\r\n");
        assert_eq!(get(md, &crlf, &a).unwrap(), Some(yes.clone()));
        assert_eq!(
            set(md, Some(&crlf), &b, &yes).unwrap(),
            format!("# Mine\r\n{BLOCK_START}\r\n@./a.md\r\n@./b.md\r\n{BLOCK_END}\r\n")
        );
        assert!(set(md, None, &a, &serde_json::json!("x")).is_err());
        assert!(get(md, &format!("{BLOCK_START}\n@./a.md\n"), &a).is_err());
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
    fn array_items_are_owned_one_by_one() {
        let db = item_key(&["enabledMcpjsonServers"], "db");
        let mine = r#"{"enabledMcpjsonServers": ["mine"]}"#;
        let text = set(Format::Json, Some(mine), &db, &serde_json::json!("db")).unwrap();
        let again = set(Format::Json, Some(&text), &db, &serde_json::json!("db")).unwrap();
        let list: Value = serde_json::from_str(&again).unwrap();
        assert_eq!(
            list["enabledMcpjsonServers"],
            serde_json::json!(["mine", "db"])
        );
        assert_eq!(
            get(Format::Json, &again, &db).unwrap(),
            Some(serde_json::json!("db"))
        );
        assert_eq!(show_key(&db), "enabledMcpjsonServers.[db]");
        let (text, empty) = remove(Format::Json, &again, &db).unwrap();
        assert!(!empty);
        let list: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(list["enabledMcpjsonServers"], serde_json::json!(["mine"]));
        // The last item takes its array with it.
        let only = set(Format::Json, None, &db, &serde_json::json!("db")).unwrap();
        let (text, empty) = remove(Format::Json, &only, &db).unwrap();
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

    #[test]
    fn header_lines_are_set_read_and_removed_beside_the_users() {
        let at = ["env", "H"];
        let k = line_key(&at, "x-tool");
        let line = serde_json::json!("x-tool: a");
        let only = set(Format::Json, None, &k, &line).unwrap();
        assert_eq!(get(Format::Json, &only, &k).unwrap(), Some(line.clone()));
        let (text, empty) = remove(Format::Json, &only, &k).unwrap();
        assert!(empty, "{text}");

        let mine = "{\"env\": {\"H\": \"x-team: pay\\nX-Tool: old\\nx-tool: dup\"}}";
        assert_eq!(
            get(Format::Json, mine, &k).unwrap(),
            Some(serde_json::json!("X-Tool: old"))
        );
        let text = set(Format::Json, Some(mine), &k, &line).unwrap();
        assert_eq!(
            get(Format::Json, &text, &key(&at)).unwrap(),
            Some(serde_json::json!("x-team: pay\nx-tool: a"))
        );
        let (text, empty) = remove(Format::Json, &text, &k).unwrap();
        assert!(!empty);
        assert_eq!(
            get(Format::Json, &text, &key(&at)).unwrap(),
            Some(serde_json::json!("x-team: pay"))
        );
        // Nothing to remove changes nothing.
        let (same, _) = remove(Format::Json, &text, &k).unwrap();
        assert_eq!(same, text);
        assert_eq!(show_key(&k), "env.H.[x-tool:]");
    }

    #[test]
    fn sections_are_owned_one_by_one_beside_the_persons_text() {
        let at = ["developer_instructions"];
        let a = section_key(&at, "a");
        let b = section_key(&at, "b");
        let text = set(Format::Toml, None, &a, &serde_json::json!("A one.\n")).unwrap();
        let text = set(Format::Toml, Some(&text), &b, &serde_json::json!("B one.")).unwrap();
        assert_eq!(
            get(Format::Toml, &text, &key(&at)).unwrap(),
            Some(serde_json::json!(
                "<!-- sekisho:a -->\nA one.\n<!-- /sekisho:a -->\n\n<!-- sekisho:b -->\nB one.\n<!-- /sekisho:b -->"
            ))
        );
        assert_eq!(
            get(Format::Toml, &text, &a).unwrap(),
            Some(serde_json::json!("A one."))
        );
        let text = set(Format::Toml, Some(&text), &a, &serde_json::json!("A two.")).unwrap();
        let (text, empty) = remove(Format::Toml, &text, &b).unwrap();
        assert!(!empty);
        assert_eq!(
            get(Format::Toml, &text, &key(&at)).unwrap(),
            Some(serde_json::json!(
                "<!-- sekisho:a -->\nA two.\n<!-- /sekisho:a -->"
            ))
        );
        let (_, empty) = remove(Format::Toml, &text, &a).unwrap();
        assert!(empty);

        let mine = "developer_instructions = \"Be terse.\"\n";
        assert!(foreign_text(Format::Toml, mine, &a));
        let text = set(Format::Toml, Some(mine), &a, &serde_json::json!("A.")).unwrap();
        assert!(!foreign_text(Format::Toml, &text, &b));
        let (text, _) = remove(Format::Toml, &text, &a).unwrap();
        assert_eq!(
            get(Format::Toml, &text, &key(&at)).unwrap(),
            Some(serde_json::json!("Be terse."))
        );
        assert_eq!(show_key(&a), "developer_instructions.[sekisho:a]");
    }
}
