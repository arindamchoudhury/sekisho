//! A `prompts` source: every Markdown file but `README.md` (in any case),
//! outside hidden folders. A prompt's name is its file's stem; its one
//! file is stored as `<name>.md`.

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::check::CheckError;
use crate::model::{CheckedEntry, EntrySpec, PromptSpec, Refusal, SourceCheck};
use crate::names::{EntryKind, EntryName};
use crate::obot::Fields;
use crate::skills::frontmatter;
use crate::tree::{Limits, TreeFile};

/// Finds the prompts under `root`.
pub(crate) fn check(root: &Path, limits: &Limits, out: &mut SourceCheck) -> Result<(), CheckError> {
    walk(root, "", limits, out)
}

fn walk(
    dir: &Path,
    prefix: &str,
    limits: &Limits,
    out: &mut SourceCheck,
) -> Result<(), CheckError> {
    let unreadable = |e: std::io::Error| CheckError::Unreadable {
        reason: format!(
            "{}: {}",
            if prefix.is_empty() { "." } else { prefix },
            e.kind()
        ),
    };
    let mut items = fs::read_dir(dir)
        .map_err(unreadable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(unreadable)?;
    items.sort_by_key(|i| i.file_name());
    for item in items {
        let kind = item.file_type().map_err(unreadable)?;
        let name = item.file_name().to_string_lossy().into_owned();
        // Hidden folders and files aren't prompts.
        if name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{name}");
        if kind.is_dir() {
            walk(&item.path(), &format!("{rel}/"), limits, out)?;
            continue;
        }
        let lower = name.to_lowercase();
        if !lower.ends_with(".md") || lower == "readme.md" {
            continue;
        }
        let stem = name[..name.len() - 3].to_owned();
        let refuse = |out: &mut SourceCheck, reason: String| {
            out.refused.push(Refusal {
                path: rel.clone(),
                kind: Some(EntryKind::Prompt),
                name: Some(stem.clone()),
                reason,
            })
        };
        if !kind.is_file() {
            refuse(out, "a symlink or special file isn't read".to_owned());
            continue;
        }
        match read_prompt(&item.path(), &stem, limits) {
            Ok((name, spec, file)) => out.entries.push(CheckedEntry {
                kind: EntryKind::Prompt,
                name,
                path: rel,
                spec: EntrySpec::Prompt(spec),
                files: vec![file],
            }),
            Err(reason) => refuse(out, reason),
        }
    }
    Ok(())
}

fn read_prompt(
    path: &Path,
    stem: &str,
    limits: &Limits,
) -> Result<(EntryName, PromptSpec, TreeFile), String> {
    let name = EntryName::parse(stem).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limits.file_bytes + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("can't be read: {}", e.kind()))?;
    if bytes.len() as u64 > limits.file_bytes {
        return Err(format!("is larger than {} bytes", limits.file_bytes));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "isn't UTF-8 text")?;
    let description = match frontmatter(text)? {
        Some(map) => Fields::root(&map).text("description")?.unwrap_or_default(),
        None => String::new(),
    };
    let file = TreeFile {
        path: format!("{name}.md"),
        bytes,
        executable: false,
    };
    Ok((name, PromptSpec { description }, file))
}
