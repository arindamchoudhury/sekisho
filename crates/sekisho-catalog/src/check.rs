//! Checking a source's folder: what it holds, what is refused and why.
//! The server's sync and `sekisho check` (plan 4) both call this; it does
//! no network or database work.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde_yaml_ng::Value;

use crate::model::{CheckedEntry, EntrySpec, ObotVmcp, Refusal, SourceCheck, Warning};
use crate::names::{EntryKind, EntryName, SourceKind};
use crate::tree::Limits;
use crate::unicode::scan;
use crate::{obot, plugins, prompts, sekisho_format, skills};

/// How a check reads URLs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckOptions {
    /// Lets remote servers and agents use `http://`: tests and the e2e
    /// build only.
    pub allow_plain_http: bool,
}

/// A source that can't be read at all. Anything less is a refused item.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("the source's folder doesn't exist in the repository")]
    Missing {},
    #[error("the source has more than {limit} catalog files")]
    TooManyCatalogFiles { limit: usize },
    #[error("the source can't be read: {reason}")]
    Unreadable { reason: String },
    #[error("sources of kind {kind} can't be read yet")]
    NotYet { kind: &'static str },
}

/// What one catalog item became.
pub(crate) enum Item {
    Entry {
        kind: EntryKind,
        name: EntryName,
        spec: EntrySpec,
    },
    Vmcp(ObotVmcp),
    Refused {
        kind: Option<EntryKind>,
        name: Option<String>,
        reason: String,
    },
}

/// An item and the remarks made while reading it.
pub(crate) struct ItemRead {
    pub item: Item,
    pub warnings: Vec<String>,
}

/// Checks the folder `root` of a source of `kind`.
pub fn check_source(kind: SourceKind, root: &Path) -> Result<SourceCheck, CheckError> {
    check_source_with(kind, root, &CheckOptions::default())
}

pub fn check_source_with(
    kind: SourceKind,
    root: &Path,
    options: &CheckOptions,
) -> Result<SourceCheck, CheckError> {
    let is_folder = fs::symlink_metadata(root).is_ok_and(|m| m.is_dir());
    if !is_folder {
        return Err(CheckError::Missing {});
    }
    let limits = Limits::DEFAULT;
    let mut out = SourceCheck::default();
    let mut vmcps = Vec::new();
    match kind {
        SourceKind::Catalog => check_catalog(root, options, &limits, &mut out, &mut vmcps)?,
        SourceKind::Skills => skills::check(root, &limits, &mut out)?,
        SourceKind::Plugins => plugins::check(root, options, &limits, &mut out)?,
        SourceKind::Prompts => prompts::check(root, &limits, &mut out)?,
        other => {
            return Err(CheckError::NotYet {
                kind: other.as_str(),
            });
        }
    }
    scan_entry_files(&mut out);
    refuse_duplicates(&mut out, vmcps);
    Ok(out)
}

fn check_catalog(
    root: &Path,
    options: &CheckOptions,
    limits: &Limits,
    out: &mut SourceCheck,
    vmcps: &mut Vec<(String, ObotVmcp)>,
) -> Result<(), CheckError> {
    let found = obot::catalog_files(root, limits.catalog_files)?;
    out.refused.extend(found.refused);
    out.warnings.extend(found.warnings);
    for (rel, path) in &found.files {
        read_catalog_file(rel, path, options, limits, out, vmcps);
    }
    Ok(())
}

/// Every text file of an entry is scanned for hidden characters: a
/// critical one refuses the entry, a warning is listed with the file's
/// path in the source, its line and its column. Files that aren't UTF-8
/// aren't text and aren't scanned.
fn scan_entry_files(out: &mut SourceCheck) {
    for entry in std::mem::take(&mut out.entries) {
        let mut critical = None;
        let mut warnings = Vec::new();
        for file in &entry.files {
            let Ok(text) = std::str::from_utf8(&file.bytes) else {
                continue;
            };
            let found = scan(text);
            if critical.is_none()
                && let Some(c) = found.critical.first()
            {
                critical = Some(format!("{} holds a hidden character at {c}", file.path));
            }
            let path = match (entry.kind, entry.path.as_str()) {
                (EntryKind::Prompt, p) => p.to_owned(),
                (_, ".") => file.path.clone(),
                (_, p) => format!("{p}/{}", file.path),
            };
            warnings.extend(found.warnings.iter().map(|w| Warning {
                path: path.clone(),
                message: format!("hidden character at {w}"),
            }));
        }
        match critical {
            Some(reason) => out.refused.push(Refusal {
                path: entry.path,
                kind: Some(entry.kind),
                name: Some(entry.name.to_string()),
                reason,
            }),
            None => {
                out.warnings.extend(warnings);
                out.entries.push(entry);
            }
        }
    }
}

fn refuse_file(out: &mut SourceCheck, rel: &str, reason: String) {
    out.refused.push(Refusal {
        path: rel.to_owned(),
        kind: None,
        name: None,
        reason,
    });
}

fn read_catalog_file(
    rel: &str,
    path: &Path,
    options: &CheckOptions,
    limits: &Limits,
    out: &mut SourceCheck,
    vmcps: &mut Vec<(String, ObotVmcp)>,
) {
    let mut bytes = Vec::new();
    let read =
        fs::File::open(path).and_then(|f| f.take(limits.file_bytes + 1).read_to_end(&mut bytes));
    if let Err(e) = read {
        return refuse_file(out, rel, format!("can't be read: {}", e.kind()));
    }
    if bytes.len() as u64 > limits.file_bytes {
        return refuse_file(
            out,
            rel,
            format!("is larger than {} bytes", limits.file_bytes),
        );
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return refuse_file(out, rel, "isn't UTF-8 text".to_owned());
    };
    let hidden = scan(&text);
    for finding in &hidden.warnings {
        out.warnings.push(Warning {
            path: rel.to_owned(),
            message: format!("hidden character at {finding}"),
        });
    }
    let value: Value = match serde_yaml_ng::from_str(&text) {
        Ok(v) => v,
        Err(e) => return refuse_file(out, rel, format!("isn't valid YAML or JSON: {e}")),
    };
    let items = match value {
        Value::Null => return refuse_file(out, rel, "is empty".to_owned()),
        Value::Sequence(items) => items,
        other => vec![other],
    };
    for (i, item) in items.iter().enumerate() {
        let read = match item {
            Value::Mapping(map) if map.contains_key("apiVersion") => {
                sekisho_format::read_item(map, options)
            }
            Value::Mapping(map) => obot::read_item(map, options),
            _ => ItemRead {
                item: Item::Refused {
                    kind: None,
                    name: None,
                    reason: format!("item {} isn't a map", i + 1),
                },
                warnings: Vec::new(),
            },
        };
        out.warnings
            .extend(read.warnings.into_iter().map(|message| Warning {
                path: rel.to_owned(),
                message,
            }));
        // A critical hidden character refuses every item of its file.
        let item = match (hidden.critical.first(), read.item) {
            (None, item) => item,
            (Some(found), item) => {
                let (kind, name) = match item {
                    Item::Entry { kind, name, .. } => (Some(kind), Some(name.to_string())),
                    Item::Vmcp(v) => (None, Some(v.key.to_string())),
                    Item::Refused { kind, name, .. } => (kind, name),
                };
                Item::Refused {
                    kind,
                    name,
                    reason: format!("the file holds a hidden character at {found}"),
                }
            }
        };
        match item {
            Item::Entry { kind, name, spec } => out.entries.push(CheckedEntry {
                kind,
                name,
                path: rel.to_owned(),
                spec,
                files: Vec::new(),
            }),
            Item::Vmcp(v) => vmcps.push((rel.to_owned(), v)),
            Item::Refused { kind, name, reason } => out.refused.push(Refusal {
                path: rel.to_owned(),
                kind,
                name,
                reason,
            }),
        }
    }
}

/// Two entries of one kind with one name are both refused, as are two
/// virtual servers with one key: neither can be told apart from the other.
fn refuse_duplicates(out: &mut SourceCheck, vmcps: Vec<(String, ObotVmcp)>) {
    let mut seen: HashMap<(EntryKind, String), Vec<String>> = HashMap::new();
    for e in &out.entries {
        seen.entry((e.kind, e.name.to_string()))
            .or_default()
            .push(e.path.clone());
    }
    let entries = std::mem::take(&mut out.entries);
    for e in entries {
        let paths = &seen[&(e.kind, e.name.to_string())];
        if paths.len() == 1 {
            out.entries.push(e);
        } else {
            out.refused.push(Refusal {
                reason: format!(
                    "{} items of this kind are named {} (in {}); names must be unique",
                    paths.len(),
                    e.name,
                    paths.join(", ")
                ),
                path: e.path,
                kind: Some(e.kind),
                name: Some(e.name.to_string()),
            });
        }
    }

    let mut keys: HashMap<String, usize> = HashMap::new();
    for (_, v) in &vmcps {
        *keys.entry(v.key.to_string()).or_default() += 1;
    }
    for (path, v) in vmcps {
        let count = keys[&v.key.to_string()];
        if count == 1 {
            out.vmcps.push(v);
        } else {
            out.refused.push(Refusal {
                path,
                kind: None,
                name: Some(v.key.to_string()),
                reason: format!(
                    "{count} virtual servers have the key {}; keys must be unique",
                    v.key
                ),
            });
        }
    }
}
