//! A project install's files in a marked block of `.git/info/exclude`,
//! never the tracked `.gitignore` (D206). Only a `.git` folder has one: a
//! worktree's `.git` file, or no Git at all, gets no block.

use std::path::Path;

use super::InstallError;
use super::files::{read, write_atomic};

const START: &str =
    "# sekisho: start (sekisho use writes this block, sekisho uninstall removes it)";
const END: &str = "# sekisho: end";

/// `text` without Sekisho's block.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == START {
            inside = true;
        } else if bare == END && inside {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    out
}

/// Makes the block list `paths` (each `/`-separated, from the project
/// folder), or removes it when there are none.
pub fn update(project: &Path, paths: &[String]) -> Result<(), InstallError> {
    let git = project.join(".git");
    if !std::fs::symlink_metadata(&git).is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    let file = git.join("info").join("exclude");
    let before = match read(&file)? {
        super::files::Disk::File(b) => String::from_utf8_lossy(&b).into_owned(),
        _ => String::new(),
    };
    let mut text = strip(&before);
    if !paths.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(START);
        text.push('\n');
        for p in paths {
            text.push('/');
            text.push_str(p);
            text.push('\n');
        }
        text.push_str(END);
        text.push('\n');
    }
    if text != before {
        write_atomic(&file, text.as_bytes(), false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_is_stripped_whole() {
        let text = format!("a\n{START}\n/x\n{END}\nb\n");
        assert_eq!(strip(&text), "a\nb\n");
    }
}
