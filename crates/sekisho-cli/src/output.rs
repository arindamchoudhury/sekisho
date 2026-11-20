//! How the CLI ends: `error: <what>` and, where there is one, `hint: <what
//! to do>` on stderr, and an exit code a script can act on.

use std::fmt;
use std::process::ExitCode;

/// The CLI's exit codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// 0: done.
    Done,
    /// 1: an error.
    Error,
    /// 2: the command line was wrong.
    Usage,
    /// 3: refused: a check failed, or an answer is needed and there is no
    /// terminal.
    Refused,
    /// 4: not signed in.
    NotSignedIn,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Done => 0,
            Exit::Error => 1,
            Exit::Usage => 2,
            Exit::Refused => 3,
            Exit::NotSignedIn => 4,
        }
    }
}

/// An error the CLI reports. Its text never holds a token: every place that
/// makes one builds it from what went wrong, not from what was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
            hint: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Exit::Error, message)
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Exit::Usage, message)
    }

    pub fn not_signed_in(message: impl Into<String>) -> Self {
        Self::new(Exit::NotSignedIn, message).with_hint("run `sekisho login`")
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Prints `error:` and `hint:` lines to stderr and gives the exit code.
pub fn finish(result: Result<(), CliError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.message);
            if let Some(hint) = &e.hint {
                eprintln!("hint: {hint}");
            }
            ExitCode::from(e.exit.code())
        }
    }
}

/// A warning on stderr.
pub fn warn(text: &str) {
    eprintln!("warning: {text}");
}

/// Progress or a prompt on stderr: stdout carries only results.
pub fn note(text: &str) {
    eprintln!("{text}");
}

/// Rows in aligned columns under a header, two spaces apart, with no
/// trailing spaces; each line ends with a newline.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut text = String::new();
        for (i, (cell, w)) in cells.zip(&widths).enumerate() {
            if i > 0 {
                text.push_str("  ");
            }
            text.push_str(cell);
            text.extend(std::iter::repeat_n(' ', w - cell.chars().count()));
        }
        let mut text = text.trim_end().to_owned();
        text.push('\n');
        text
    };
    let mut out = line(&mut header.iter().copied());
    for row in rows {
        out.push_str(&line(&mut row.iter().map(String::as_str)));
    }
    out
}

/// A value as `--json` prints it: pretty, with a final newline.
pub fn json<T: serde::Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("answers serialize");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_line_up_without_trailing_spaces() {
        let out = table(
            &["A", "LONGER", "C"],
            &[
                vec!["xyz".into(), "1".into(), String::new()],
                vec!["q".into(), "22".into(), "end".into()],
            ],
        );
        assert_eq!(out, "A    LONGER  C\nxyz  1\nq    22      end\n");
    }
}
