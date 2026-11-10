//! Light highlighting for a fenced block of code, by its language, and the
//! accent marking a word in the highlighted text.

use crate::markdown::escape;

/// HTML for a code block: prompts, comments, TOML tables and keys, JSON keys,
/// and shell words with a hyphen get a class; everything else is plain text.
pub fn highlight(lang: &str, code: &str) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / 2);
    for (i, line) in code.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match lang {
            "console" => console_line(&mut out, line),
            "bash" | "sh" => shell_line(&mut out, line),
            "toml" => toml_line(&mut out, line),
            "json" => json_line(&mut out, line),
            _ => out.push_str(&escape(line)),
        }
    }
    out
}

fn span(out: &mut String, class: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    out.push_str("<span class=\"");
    out.push_str(class);
    out.push_str("\">");
    out.push_str(&escape(text));
    out.push_str("</span>");
}

fn console_line(out: &mut String, line: &str) {
    match line.strip_prefix("$ ") {
        Some(command) => {
            span(out, "prompt", "$");
            out.push(' ');
            out.push_str(&escape(command));
        }
        None => out.push_str(&escape(line)),
    }
}

fn shell_line(out: &mut String, line: &str) {
    // A comment starts at a # that opens the line or follows a space.
    let at = if line.starts_with('#') {
        Some(0)
    } else {
        line.find(" #").map(|i| i + 1)
    };
    let (code, comment) = line.split_at(at.unwrap_or(line.len()));
    for (i, word) in code.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // A browser may wrap a narrow block after a hyphen, as in "-" then
        // "-locked"; a word with one is kept whole, so lines break at spaces.
        if word.contains('-') {
            span(out, "w", word);
        } else {
            out.push_str(&escape(word));
        }
    }
    span(out, "c", comment);
}

fn toml_line(out: &mut String, line: &str) {
    let (code, comment) = split_comment(line);
    if code.trim_start().starts_with('[') {
        span(out, "t", code);
    } else if let Some((key, value)) = code.split_once('=') {
        span(out, "k", key);
        out.push('=');
        out.push_str(&escape(value));
    } else {
        out.push_str(&escape(code));
    }
    span(out, "c", comment);
}

/// A TOML line split at its comment, if a `#` outside a string starts one.
fn split_comment(line: &str) -> (&str, &str) {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => return line.split_at(i),
            _ => {}
        }
    }
    (line, "")
}

fn json_line(out: &mut String, line: &str) {
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        let (before, quoted) = rest.split_at(start);
        out.push_str(&escape(before));
        let (string, after) = quoted.split_at(string_end(quoted).unwrap_or(quoted.len()));
        if after.trim_start().starts_with(':') {
            span(out, "k", string);
        } else {
            out.push_str(&escape(string));
        }
        rest = after;
    }
    out.push_str(&escape(rest));
}

/// The byte index just past the JSON string that opens `text`.
fn string_end(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(i + 1),
            _ => escaped = false,
        }
    }
    None
}

/// `html` with each `word` in its text, never inside a tag, wrapped in the
/// accent span; and how many it wrapped.
pub fn mark(html: &str, word: &str) -> (String, usize) {
    let marked = format!("<span class=\"accent\">{word}</span>");
    let (mut out, mut count, mut rest) = (String::with_capacity(html.len()), 0, html);
    while !rest.is_empty() {
        let (text, tag) = rest.split_at(rest.find('<').unwrap_or(rest.len()));
        count += text.matches(word).count();
        out.push_str(&text.replace(word, &marked));
        let end = tag.find('>').map_or(tag.len(), |i| i + 1);
        out.push_str(&tag[..end]);
        rest = &tag[end..];
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_by_language() {
        assert_eq!(
            highlight(
                "toml",
                "[server]\nlisten = \"127.0.0.1:8080\"  # local\nurl = \"a#b\""
            ),
            "<span class=\"t\">[server]</span>\n<span class=\"k\">listen </span>= &quot;127.0.0.1:8080&quot;  <span class=\"c\"># local</span>\n<span class=\"k\">url </span>= &quot;a#b&quot;"
        );
        assert_eq!(
            highlight("json", "  \"event\": \"a\\\"b\",\n  \"n\": 1"),
            "  <span class=\"k\">&quot;event&quot;</span>: &quot;a\\&quot;b&quot;,\n  <span class=\"k\">&quot;n&quot;</span>: 1"
        );
        assert_eq!(
            highlight("bash", "wye init   # writes it"),
            "wye init   <span class=\"c\"># writes it</span>"
        );
        assert_eq!(
            highlight("bash", "cargo install a --locked  # b-c"),
            "cargo install a <span class=\"w\">--locked</span>  <span class=\"c\"># b-c</span>"
        );
        assert_eq!(
            highlight("console", "$ a < b\nout"),
            "<span class=\"prompt\">$</span> a &lt; b\nout"
        );
        assert_eq!(highlight("text", "<x>"), "&lt;x&gt;");
    }
}
