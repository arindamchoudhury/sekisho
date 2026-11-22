//! A `skills` source: Agent Skills folders, read by Obot's rules (Obot
//! `5e471f101`, `pkg/skillformat/validate.go` and
//! `pkg/controller/handlers/skillrepository/scan.go`). A skill is a folder
//! with a regular-file `SKILL.md`; its folder isn't searched for more.

use std::fs;
use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

use crate::check::CheckError;
use crate::model::{CheckedEntry, EntrySpec, Refusal, SkillSpec, SourceCheck};
use crate::names::{EntryKind, EntryName};
use crate::obot::Fields;
use crate::tree::{Limits, read_entry_files};

const SKILL_MD: &str = "SKILL.md";
const MAX_DESCRIPTION: usize = 1024;
const MAX_COMPATIBILITY: usize = 500;

/// YAML frontmatter between `---` lines, as Obot's `ParseFrontmatter`
/// finds it: `None` when the text doesn't start with one.
pub(crate) fn frontmatter(text: &str) -> Result<Option<Mapping>, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < 3 || lines[0].trim() != "---" {
        return Ok(None);
    }
    let Some(end) = lines.iter().skip(1).position(|l| l.trim() == "---") else {
        return Err("its frontmatter has no closing ---".to_owned());
    };
    let yaml = lines[1..=end].join("\n");
    match serde_yaml_ng::from_str::<Value>(&yaml) {
        Ok(Value::Null) => Ok(Some(Mapping::new())),
        Ok(Value::Mapping(map)) => Ok(Some(map)),
        Ok(_) => Err("its frontmatter isn't a map".to_owned()),
        Err(e) => Err(format!("its frontmatter isn't valid YAML: {e}")),
    }
}

/// A Markdown prompt or command's frontmatter `description` (empty when it
/// has none) and its body after the frontmatter, for the tools that take
/// prompts in another form (plan 4, Task 7).
pub fn description_and_body(text: &str) -> Result<(String, String), String> {
    let description = match frontmatter(text)? {
        Some(map) => crate::obot::Fields::root(&map)
            .text("description")?
            .unwrap_or_default(),
        None => return Ok((String::new(), text.to_owned())),
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let end = lines
        .iter()
        .skip(1)
        .position(|l| l.trim() == "---")
        .map_or(0, |i| i + 2);
    Ok((description, lines[end..].join("\n")))
}

/// Finds the skills under `root`.
pub(crate) fn check(root: &Path, limits: &Limits, out: &mut SourceCheck) -> Result<(), CheckError> {
    // A skill at the root has no folder name to match, as in Obot.
    if fs::symlink_metadata(root.join(SKILL_MD)).is_ok() {
        out.refused.push(Refusal {
            path: SKILL_MD.to_owned(),
            kind: Some(EntryKind::Skill),
            name: None,
            reason: "a skill needs a folder of its own; this SKILL.md is at the source's root"
                .to_owned(),
        });
    }
    walk(root, root, "", limits, out)
}

fn walk(
    root: &Path,
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
        // Symlinked folders aren't followed, and .git isn't content.
        if !kind.is_dir() || name == ".git" {
            continue;
        }
        let rel = format!("{prefix}{name}");
        let path = item.path();
        match fs::symlink_metadata(path.join(SKILL_MD)) {
            Ok(meta) if meta.is_file() => match read_skill(root, &path, &rel, &name, limits) {
                Ok(entry) => out.entries.push(entry),
                Err(reason) => out.refused.push(Refusal {
                    path: rel,
                    kind: Some(EntryKind::Skill),
                    name: Some(name),
                    reason,
                }),
            },
            Ok(meta) if meta.file_type().is_symlink() => out.refused.push(Refusal {
                path: format!("{rel}/{SKILL_MD}"),
                kind: Some(EntryKind::Skill),
                name: Some(name),
                reason: "SKILL.md is a symlink".to_owned(),
            }),
            _ => walk(root, &path, &format!("{rel}/"), limits, out)?,
        }
    }
    Ok(())
}

fn chars_over(text: &str, limit: usize) -> bool {
    text.chars().count() > limit
}

fn read_skill(
    root: &Path,
    dir: &Path,
    rel: &str,
    folder: &str,
    limits: &Limits,
) -> Result<CheckedEntry, String> {
    let files = read_entry_files(root, dir, limits).map_err(|e| e.to_string())?;
    let skill_md = files
        .iter()
        .find(|f| f.path == SKILL_MD)
        .ok_or("SKILL.md can't be read")?;
    if skill_md.bytes.len() as u64 > limits.skill_md_bytes {
        return Err(format!(
            "SKILL.md is larger than {} bytes",
            limits.skill_md_bytes
        ));
    }
    let text = std::str::from_utf8(&skill_md.bytes).map_err(|_| "SKILL.md isn't UTF-8 text")?;
    let map = frontmatter(text)
        .map_err(|e| format!("SKILL.md: {e}"))?
        .ok_or("SKILL.md has no frontmatter; it needs a name and a description")?;
    let f = Fields::root(&map);

    let name = f
        .text("name")?
        .ok_or("SKILL.md's frontmatter needs a name")?;
    let name = EntryName::parse(&name).map_err(|e| e.to_string())?;
    if name.as_str() != folder {
        return Err(format!(
            "the skill's name {name} doesn't match its folder {folder}; they must be the same"
        ));
    }
    let description = f
        .text("description")?
        .ok_or("SKILL.md's frontmatter needs a description")?;
    if chars_over(&description, MAX_DESCRIPTION) {
        return Err(format!(
            "the description is longer than {MAX_DESCRIPTION} characters"
        ));
    }
    let compatibility = f.text("compatibility")?;
    if compatibility
        .as_deref()
        .is_some_and(|c| chars_over(c, MAX_COMPATIBILITY))
    {
        return Err(format!(
            "compatibility is longer than {MAX_COMPATIBILITY} characters"
        ));
    }
    Ok(CheckedEntry {
        kind: EntryKind::Skill,
        name,
        path: rel.to_owned(),
        spec: EntrySpec::Skill(SkillSpec {
            description,
            license: f.text("license")?,
            compatibility,
        }),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_found_as_obot_finds_it() {
        let map = frontmatter("---\r\nname: a\r\n---\r\nbody")
            .unwrap()
            .unwrap();
        assert_eq!(map.get("name").and_then(Value::as_str), Some("a"));
        assert!(frontmatter("# title\n\nbody").unwrap().is_none());
        assert!(frontmatter("---\nonly two lines").unwrap().is_none());
        assert!(frontmatter("---\nname: a\nno end\n").is_err());
        assert!(frontmatter("---\n- a\n---\n").is_err());
        assert!(frontmatter("---\n---\nbody").unwrap().unwrap().is_empty());
    }
}
