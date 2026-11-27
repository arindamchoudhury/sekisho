//! Instructions on machines (D220–D225): what every adapter shares. Each
//! instruction becomes a file of Sekisho's own, starting with a heading
//! that says where it comes from; a plugin's APM instructions
//! (`instructions/*.instructions.md`) too, path-scoped when their
//! `applyTo` names some files (D224). Nothing here writes a repository's
//! own `AGENTS.md` or `CLAUDE.md` (D223); the engine refuses those paths
//! whatever an adapter asks.

use sekisho_catalog::delivery::{DeliveredEntry, Delivery, EntryAddress};
use sekisho_catalog::instructions::{apply_to_and_body, check, title};
use sekisho_catalog::names::EntryKind;
use sekisho_catalog::unicode::invisible;

use crate::adapters::{AdapterError, Origin, RelPath, RenderContext, Root, Skipped};

/// More than this many bytes of instructions in every session, the most
/// one environment may deliver, is a cost every session pays (D221):
/// `use` warns past it, when several environments add up.
pub const ALWAYS_LOADED_WARNING_BYTES: u64 =
    sekisho_catalog::instructions::MAX_ENVIRONMENT_INSTRUCTION_BYTES as u64;

/// Files that are a repository's or a person's own instructions, which no
/// install writes (D223), lowercase: in a project, and at the top of a
/// tool's or the user's home folder.
const PROJECT_FILES: [&str; 5] = [
    "agents.md",
    "claude.md",
    ".claude/claude.md",
    "agents.override.md",
    ".github/copilot-instructions.md",
];
const HOME_FILES: [&str; 4] = [
    "agents.md",
    "claude.md",
    "agents.override.md",
    // Copilot CLI's user-level file, in `~/.copilot`.
    "copilot-instructions.md",
];

/// `path` at `root` is one of the files [`PROJECT_FILES`] and
/// [`HOME_FILES`] name, in any case (Windows' and macOS' file systems
/// ignore it).
pub fn is_own_instructions_file(root: Root, path: &RelPath) -> bool {
    let lower = path.as_str().to_lowercase();
    match root {
        Root::Project => PROJECT_FILES.contains(&lower.as_str()),
        Root::ToolHome(_) | Root::UserHome => HOME_FILES.contains(&lower.as_str()),
        Root::SekishoConfig => false,
    }
}

/// A plugin's top-level `AGENTS.md`, which no tool gets (D223).
pub fn is_agents_md(plugin_path: &str) -> bool {
    plugin_path.eq_ignore_ascii_case("AGENTS.md")
}

/// One instruction, ready for a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    /// The file's stem: the instruction's name, or `<plugin>-<name>` for a
    /// plugin's.
    pub stem: String,
    /// The text, starting with its origin heading.
    pub text: String,
    /// The globs it applies to; `None` for every session.
    pub paths: Option<Vec<String>>,
    pub origin: Origin,
    /// For messages: `instruction style`, `plugin fmt's instruction rust`.
    pub label: String,
}

impl Instruction {
    /// The text with `applyTo:` frontmatter, as Copilot CLI reads an
    /// `.instructions.md` file: `"**"` for every path, else its globs.
    pub fn with_apply_to(&self) -> String {
        let quoted = |g: &str| serde_json::to_string(g).expect("strings serialize");
        let apply = match &self.paths {
            None => format!("applyTo: {}\n", quoted("**")),
            Some(globs) => {
                let list: String = globs
                    .iter()
                    .map(|g| format!("  - {}\n", quoted(g)))
                    .collect();
                format!("applyTo:\n{list}")
            }
        };
        format!("---\n{apply}---\n{}", self.text)
    }

    /// The text with `paths:` frontmatter when path-scoped, as Claude Code
    /// and Qwen Code read it.
    pub fn with_paths(&self) -> String {
        match &self.paths {
            None => self.text.clone(),
            Some(globs) => {
                let list: String = globs
                    .iter()
                    .map(|g| {
                        let quoted = serde_json::to_string(g).expect("strings serialize");
                        format!("  - {quoted}\n")
                    })
                    .collect();
                format!("---\npaths:\n{list}---\n{}", self.text)
            }
        }
    }
}

/// The first line of an instruction's file: its text's own first heading,
/// or its name, then where it comes from.
pub fn heading(env: &str, plugin: Option<&str>, name: &str, text: &str) -> String {
    let shown = title(text).unwrap_or(name);
    let plugin = plugin.map(|p| format!("plugin {p}, ")).unwrap_or_default();
    format!("# {shown} (Sekisho environment {env}, {plugin}instruction {name})")
}

/// The heading, then the text without its own first heading.
fn render(env: &str, plugin: Option<&str>, name: &str, text: &str) -> String {
    let mut body = text;
    if title(text).is_some() {
        let start = text.len() - text.trim_start().len();
        let rest = &text[start..];
        body = rest.split_once('\n').map_or("", |(_, after)| after);
    }
    let body = body.trim_start_matches(['\n', '\r']);
    let mut out = heading(env, plugin, name, text);
    out.push('\n');
    if !body.trim().is_empty() {
        out.push('\n');
        out.push_str(body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The scope's text for tools that take one value or one file: each
/// instruction under its heading, in order, a blank line between.
pub fn joined(list: &[&Instruction]) -> String {
    list.iter()
        .map(|i| i.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn blob<'a>(ctx: &'a RenderContext<'_>, sha: &str) -> Result<&'a [u8], AdapterError> {
    ctx.blobs
        .get(sha)
        .map(Vec::as_slice)
        .ok_or_else(|| AdapterError(format!("no file {sha}")))
}

/// The environment's instructions in its order, then its plugins' (each
/// plugin's in file order), and what was skipped: a plugin's top-level
/// `AGENTS.md`, and plugin instructions that can't be read. An instruction
/// that fails the checks it passed at sync (D221) refuses the install.
pub fn collect(
    d: &Delivery,
    ctx: &RenderContext<'_>,
) -> Result<(Vec<Instruction>, Vec<Skipped>), AdapterError> {
    let env = d.environment.as_str();
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for e in d
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Instruction)
    {
        let [file] = e.files.as_slice() else {
            return Err(AdapterError(format!(
                "instruction {}@{} has {} files, not one",
                e.name,
                e.source,
                e.files.len()
            )));
        };
        let text = check(blob(ctx, &file.sha256)?)
            .map_err(|why| AdapterError(format!("instruction {}@{} {why}", e.name, e.source)))?;
        out.push(Instruction {
            stem: e.name.to_string(),
            text: render(env, None, e.name.as_str(), text),
            paths: None,
            origin: Origin::of(e),
            label: format!("instruction {}", e.name),
        });
    }
    for e in d.entries.iter().filter(|e| e.kind == EntryKind::Plugin) {
        plugin(env, e, ctx, &mut out, &mut skipped)?;
    }
    Ok((out, skipped))
}

fn plugin(
    env: &str,
    e: &DeliveredEntry,
    ctx: &RenderContext<'_>,
    out: &mut Vec<Instruction>,
    skipped: &mut Vec<Skipped>,
) -> Result<(), AdapterError> {
    let address = EntryAddress {
        kind: e.kind,
        source: e.source.clone(),
        name: e.name.clone(),
    };
    let plugin = ctx
        .plugin_names
        .get(&address)
        .cloned()
        .unwrap_or_else(|| e.name.to_string());
    for f in &e.files {
        if is_agents_md(&f.path) {
            skipped.push(Skipped {
                what: format!("plugin {plugin}'s AGENTS.md"),
                why: "Sekisho never writes an AGENTS.md: the repository's own would be \
                      replaced or joined by it (D223)"
                    .into(),
            });
            continue;
        }
        let Some(stem) = f
            .path
            .strip_prefix("instructions/")
            .and_then(|p| p.strip_suffix(".instructions.md"))
            .filter(|s| !s.is_empty() && !s.contains('/'))
        else {
            continue;
        };
        let label = format!("plugin {plugin}'s instruction {stem}");
        let read = std::str::from_utf8(blob(ctx, &f.sha256)?)
            .map_err(|_| "it isn't UTF-8 text".to_owned())
            .and_then(apply_to_and_body);
        let (paths, body) = match read {
            Ok(read) => read,
            Err(why) => {
                skipped.push(Skipped { what: label, why });
                continue;
            }
        };
        if let Some(found) = invisible(&body) {
            return Err(AdapterError(format!(
                "{label} holds an invisible character at {found}"
            )));
        }
        out.push(Instruction {
            stem: format!("{plugin}-{stem}"),
            text: render(env, Some(&plugin), stem, &body),
            paths,
            origin: Origin::of(e),
            label,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heading_replaces_the_texts_own() {
        assert_eq!(
            render("a", None, "style", "# Payments style\n\nShort.\n"),
            "# Payments style (Sekisho environment a, instruction style)\n\nShort.\n"
        );
        assert_eq!(
            render("a", None, "review", "Ask for tests."),
            "# review (Sekisho environment a, instruction review)\n\nAsk for tests.\n"
        );
        assert_eq!(
            render("a", Some("fmt"), "rust", "## Rust\nUse rustfmt.\n"),
            "# Rust (Sekisho environment a, plugin fmt, instruction rust)\n\nUse rustfmt.\n"
        );
        assert_eq!(
            render("a", None, "t", "# Only a title\n"),
            "# Only a title (Sekisho environment a, instruction t)\n"
        );
    }

    #[test]
    fn own_instruction_files_are_known_in_any_case() {
        let p = |s: &str| RelPath::parse(s).unwrap();
        assert!(is_own_instructions_file(Root::Project, &p("Agents.md")));
        assert!(is_own_instructions_file(
            Root::Project,
            &p(".claude/CLAUDE.md")
        ));
        assert!(!is_own_instructions_file(Root::Project, &p("GEMINI.md")));
        assert!(!is_own_instructions_file(
            Root::Project,
            &p("docs/AGENTS.md")
        ));
        assert!(is_own_instructions_file(
            Root::ToolHome(crate::adapters::Tool::Codex),
            &p("AGENTS.override.md")
        ));
        assert!(!is_own_instructions_file(
            Root::SekishoConfig,
            &p("AGENTS.md")
        ));
    }
}
