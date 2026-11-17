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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_characters_not_bytes() {
        let s = scan("éé\u{202E}");
        assert_eq!((s.critical[0].line, s.critical[0].column), (1, 3));
    }

    #[test]
    fn crlf_lines_count_once() {
        let s = scan("a\r\nb\u{2066}");
        assert_eq!((s.critical[0].line, s.critical[0].column), (2, 2));
    }
}
