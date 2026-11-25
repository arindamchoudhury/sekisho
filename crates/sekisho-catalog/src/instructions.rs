//! An `instructions` source (D220): every Markdown file at the folder's
//! top level but `README.md` (in any case), each one instruction named by
//! its file's stem. Other files and folders are ignored. An instruction's
//! one file is stored as `<name>.md`.

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::check::CheckError;
use crate::model::{CheckedEntry, EntrySpec, InstructionSpec, Refusal, SourceCheck};
use crate::names::{EntryKind, EntryName};
use crate::tree::TreeFile;
use crate::unicode::invisible;

/// An instruction's most bytes (D221).
pub const MAX_INSTRUCTION_BYTES: usize = 4096;

/// The most bytes of instructions one environment delivers (D221).
pub const MAX_ENVIRONMENT_INSTRUCTION_BYTES: usize = 16384;

/// Checks one instruction's text: UTF-8, not empty, no frontmatter, at most
/// [`MAX_INSTRUCTION_BYTES`], and no invisible character (D221). The reason
/// never quotes the text. Run at sync and again on machines.
pub fn check(bytes: &[u8]) -> Result<&str, String> {
    if bytes.len() > MAX_INSTRUCTION_BYTES {
        return Err(format!(
            "is {} bytes; an instruction is at most {MAX_INSTRUCTION_BYTES}",
            bytes.len()
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "isn't UTF-8 text".to_owned())?;
    if text.trim().is_empty() {
        return Err("is empty".to_owned());
    }
    if text.lines().next().is_some_and(|l| l.trim_end() == "---") {
        return Err(
            "starts with frontmatter (`---`); Sekisho writes each tool's own, so an instruction is plain Markdown"
                .to_owned(),
        );
    }
    if let Some(found) = invisible(text) {
        return Err(format!("holds an invisible character at {found}"));
    }
    Ok(text)
}

/// The text's first heading, when its first non-blank line is one.
pub fn title(text: &str) -> Option<&str> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let rest = line.trim_start_matches('#');
    let level = line.len() - rest.len();
    if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
        let t = rest.trim().trim_end_matches('#').trim();
        (!t.is_empty()).then_some(t)
    } else {
        None
    }
}

/// A plugin's APM instruction (`instructions/*.instructions.md`, D224):
/// the globs its `applyTo` names, `None` for every file (no frontmatter,
/// no `applyTo`, or `**`), and the text after the frontmatter.
pub fn apply_to_and_body(text: &str) -> Result<(Option<Vec<String>>, String), String> {
    let (_, body) = crate::description_and_body(text)?;
    let Some(map) = crate::skills::frontmatter(text)? else {
        return Ok((None, body));
    };
    let globs = match map.get("applyTo") {
        None | Some(serde_yaml_ng::Value::Null) => return Ok((None, body)),
        Some(serde_yaml_ng::Value::String(s)) => s,
        Some(_) => return Err("its applyTo isn't text".to_owned()),
    };
    let globs: Vec<String> = globs
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_owned)
        .collect();
    if globs.is_empty() {
        return Err("its applyTo names no files".to_owned());
    }
    if globs.iter().any(|g| g == "**") {
        return Ok((None, body));
    }
    Ok((Some(globs), body))
}

/// Finds the instructions at the top of `root`.
pub(crate) fn check_source(root: &Path, out: &mut SourceCheck) -> Result<(), CheckError> {
    let unreadable = |e: std::io::Error| CheckError::Unreadable {
        reason: format!(".: {}", e.kind()),
    };
    let mut items = fs::read_dir(root)
        .map_err(unreadable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(unreadable)?;
    items.sort_by_key(|i| i.file_name());
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        if name.starts_with('.') || !lower.ends_with(".md") || lower == "readme.md" {
            continue;
        }
        let kind = item.file_type().map_err(unreadable)?;
        if kind.is_dir() {
            continue;
        }
        let stem = name[..name.len() - 3].to_owned();
        let refuse = |out: &mut SourceCheck, reason: String| {
            out.refused.push(Refusal {
                path: name.clone(),
                kind: Some(EntryKind::Instruction),
                name: Some(stem.clone()),
                reason,
            })
        };
        if !kind.is_file() {
            refuse(out, "a symlink or special file isn't read".to_owned());
            continue;
        }
        match read(&item.path(), &stem) {
            Ok((entry_name, spec, file)) => out.entries.push(CheckedEntry {
                kind: EntryKind::Instruction,
                name: entry_name,
                path: name.clone(),
                spec: EntrySpec::Instruction(spec),
                files: vec![file],
            }),
            Err(reason) => refuse(out, reason),
        }
    }
    Ok(())
}

fn read(path: &Path, stem: &str) -> Result<(EntryName, InstructionSpec, TreeFile), String> {
    let name = EntryName::parse(stem).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| {
            f.take(MAX_INSTRUCTION_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| format!("can't be read: {}", e.kind()))?;
    let text = check(&bytes)?;
    let description = title(text).unwrap_or_default().to_owned();
    let file = TreeFile {
        path: format!("{name}.md"),
        bytes,
        executable: false,
    };
    Ok((name, InstructionSpec { description }, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_instructions_say_where_they_apply() {
        let text = "---\ndescription: x\napplyTo: \"**/*.rs, src/**\"\n---\nUse rustfmt.\n";
        let (paths, body) = apply_to_and_body(text).unwrap();
        assert_eq!(paths, Some(vec!["**/*.rs".to_owned(), "src/**".to_owned()]));
        assert_eq!(body, "Use rustfmt.\n");
        // `**`, no applyTo, or no frontmatter at all: every file.
        for text in [
            "---\napplyTo: '**'\n---\nA\n",
            "---\ndescription: d\n---\nA\n",
            "A\n",
        ] {
            let (paths, body) = apply_to_and_body(text).unwrap();
            assert_eq!(paths, None, "{text}");
            assert_eq!(body, "A\n");
        }
        assert!(apply_to_and_body("---\napplyTo: [1]\n---\nA\n").is_err());
        assert!(apply_to_and_body("---\napplyTo: \" , \"\n---\nA\n").is_err());
    }

    #[test]
    fn titles_are_first_headings() {
        assert_eq!(title("# Payments style\n\nText"), Some("Payments style"));
        assert_eq!(title("\n\n## Two ##\n"), Some("Two"));
        assert_eq!(title("#hashtag\n"), None);
        assert_eq!(title("Text\n# Later\n"), None);
        assert_eq!(title("#\n"), None);
    }
}
