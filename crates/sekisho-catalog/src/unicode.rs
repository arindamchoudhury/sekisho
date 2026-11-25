//! Hidden characters (D168), as APM's content scanner sorts them: those
//! that can make text read differently from what a coding tool sees refuse
//! an entry; the rest are listed as warnings.

/// One hidden character, with its place: lines and columns count from 1,
/// columns in characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub column: usize,
    pub codepoint: char,
    pub what: &'static str,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}, column {}: U+{:04X} ({})",
            self.line, self.column, self.codepoint as u32, self.what
        )
    }
}

/// What [`scan`] found in one text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// These refuse the entry.
    pub critical: Vec<Finding>,
    pub warnings: Vec<Finding>,
}

enum Class {
    Critical(&'static str),
    Warning(&'static str),
}

fn classify(c: char, first: bool) -> Option<Class> {
    use Class::{Critical, Warning};
    Some(match c {
        '\u{202A}'..='\u{202E}' => Critical("bidirectional embedding or override"),
        '\u{2066}'..='\u{2069}' => Critical("bidirectional isolate"),
        '\u{E0001}'..='\u{E007F}' => Critical("tag character"),
        '\u{E0100}'..='\u{E01EF}' => Critical("variation selector supplement"),
        '\u{200B}'..='\u{200F}' => Warning("zero-width character or direction mark"),
        '\u{2060}'..='\u{2064}' => Warning("word joiner or invisible operator"),
        '\u{206A}'..='\u{206F}' => Warning("deprecated format character"),
        '\u{00AD}' => Warning("soft hyphen"),
        '\u{061C}' => Warning("Arabic letter mark"),
        '\u{FE00}'..='\u{FE0E}' => Warning("variation selector"),
        '\u{FFF9}'..='\u{FFFB}' => Warning("interlinear annotation"),
        '\u{FEFF}' if !first => Warning("byte order mark after the start"),
        _ => return None,
    })
}

/// Finds every hidden character in `text`.
pub fn scan(text: &str) -> Scan {
    let mut out = Scan::default();
    let (mut line, mut column) = (1, 0);
    for (i, c) in text.char_indices() {
        if c == '\n' {
            line += 1;
            column = 0;
            continue;
        }
        column += 1;
        let finding = |what| Finding {
            line,
            column,
            codepoint: c,
            what,
        };
        match classify(c, i == 0) {
            Some(Class::Critical(what)) => out.critical.push(finding(what)),
            Some(Class::Warning(what)) => out.warnings.push(finding(what)),
            None => {}
        }
    }
    out
}

/// The first character that makes an instruction read differently from
/// what a person sees (D221): zero-width (U+200B–U+200D, U+2060, U+FEFF
/// anywhere), bidirectional (U+061C, U+200E, U+200F, U+202A–U+202E,
/// U+2066–U+2069) or tag (U+E0000–U+E007F). Instructions are always in the
/// model's context, so these refuse, where [`scan`] only warns of some.
pub fn invisible(text: &str) -> Option<Finding> {
    let (mut line, mut column) = (1, 0);
    for c in text.chars() {
        if c == '\n' {
            line += 1;
            column = 0;
            continue;
        }
        column += 1;
        let what = match c {
            '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}' => "zero-width character",
            '\u{061C}' | '\u{200E}' | '\u{200F}' => "direction mark",
            '\u{202A}'..='\u{202E}' => "bidirectional embedding or override",
            '\u{2066}'..='\u{2069}' => "bidirectional isolate",
            '\u{E0000}'..='\u{E007F}' => "tag character",
            _ => continue,
        };
        return Some(Finding {
            line,
            column,
            codepoint: c,
            what,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_characters_not_bytes() {
        let s = scan("éé\u{202E}");
        assert_eq!((s.critical[0].line, s.critical[0].column), (1, 3));
    }

    #[test]
    fn findings_say_where_and_what() {
        let s = scan("a\u{202E}");
        assert_eq!(
            s.critical[0].to_string(),
            "line 1, column 2: U+202E (bidirectional embedding or override)"
        );
    }

    #[test]
    fn crlf_lines_count_once() {
        let s = scan("a\r\nb\u{2066}");
        assert_eq!((s.critical[0].line, s.critical[0].column), (2, 2));
    }
}
