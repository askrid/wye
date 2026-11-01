//! Text for a terminal: every line fits [`WIDTH`] columns.

/// CLI text is wrapped to this width, which fits a split terminal.
pub const WIDTH: usize = 80;

/// `text` wrapped at word boundaries to [`WIDTH`] characters, every line
/// indented by `indent`; ends with a newline. Only ASCII whitespace breaks a
/// line, so a no-break space keeps two words together.
pub fn wrap(text: &str, indent: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    for word in text
        .split(|c: char| c.is_ascii_whitespace())
        .filter(|w| !w.is_empty())
    {
        let width = word.chars().count();
        if line > indent && line + 1 + width > WIDTH {
            out.push('\n');
            line = 0;
        }
        if line == 0 {
            out.push_str(&" ".repeat(indent));
            line = indent;
        } else {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += width;
    }
    out.push('\n');
    out
}
