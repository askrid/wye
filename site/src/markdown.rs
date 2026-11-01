//! The Markdown the site reads from the repository: fenced blocks with light
//! highlighting, and sections rendered with their links pointed at GitHub.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The first fenced block whose first line starts with `prefix`: its info
/// string and its lines.
pub fn fence<'a>(doc: &'a str, prefix: &str) -> Option<(&'a str, String)> {
    let mut lines = doc.lines();
    while let Some(line) = lines.next() {
        let Some(lang) = line.strip_prefix("```") else {
            continue;
        };
        let code: Vec<&str> = lines
            .by_ref()
            .take_while(|l| !l.starts_with("```"))
            .collect();
        if code.first().is_some_and(|first| first.starts_with(prefix)) {
            return Some((lang.trim(), code.join("\n")));
        }
    }
    None
}

/// The text under `## heading`, up to the next heading of level one or two.
pub fn section<'a>(doc: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("## {heading}");
    let (mut offset, mut start, mut in_fence) = (0, None, false);
    for line in doc.split_inclusive('\n') {
        let text = line.trim_end();
        if text.starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            match start {
                Some(start) if text.starts_with("# ") || text.starts_with("## ") => {
                    return doc.get(start..offset);
                }
                None if text == marker => start = Some(offset + line.len()),
                _ => {}
            }
        }
        offset += line.len();
    }
    doc.get(start?..)
}

/// The items of the first `- ` list in a section, each without its marker
/// and with its continuation lines unindented. The list ends at the first
/// unindented line that does not start an item.
pub fn list_items(section: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for line in section.lines() {
        if let Some(text) = line.strip_prefix("- ") {
            items.push(text.to_owned());
        } else if let Some(item) = items.last_mut() {
            if line.starts_with("  ") {
                item.push('\n');
                item.push_str(line.trim_start());
            } else if !line.trim().is_empty() {
                break;
            }
        }
    }
    items
}

/// The bold lead that opens a list item: `**Shows usage and cost.** One…`.
pub fn lead(item: &str) -> Option<&str> {
    item.strip_prefix("**")?
        .split_once("**")
        .map(|(lead, _)| lead)
}

/// Renders Markdown from `source` (a path relative to the repository root).
pub fn to_html(markdown: &str, repo_url: &str, source: &str) -> String {
    let events = Parser::new_ext(markdown, Options::ENABLE_TABLES).map(|event| match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::from(link_url(&dest_url, repo_url, source)),
            title,
            id,
        }),
        other => other,
    });
    let mut out = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut out, events);
    out
}

/// Where a link in a repository document points once its text is on the
/// site: other files and anchors open on GitHub, as they do from the file.
fn link_url(dest: &str, repo_url: &str, source: &str) -> String {
    if dest.contains("://") || dest.starts_with("mailto:") {
        return dest.to_owned();
    }
    if let Some(anchor) = dest.strip_prefix('#') {
        return match source {
            "README.md" => format!("{repo_url}#{anchor}"),
            _ => format!("{repo_url}/blob/main/{source}#{anchor}"),
        };
    }
    match source.rsplit_once('/') {
        Some((dir, _)) => format!("{repo_url}/blob/main/{dir}/{dest}"),
        None => format!("{repo_url}/blob/main/{dest}"),
    }
}

/// HTML for a code block: prompts, comments, TOML tables and keys, and JSON
/// keys get a class; everything else is plain text.
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
    out.push_str(&escape(code));
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

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# T\n\n```bash\nls\n```\n\n```console\n$ wye report\nok\n```\n\n## Status\n\nOne.\n\n```md\n## not a heading\n```\n\nTwo.\n\n## Next\n\nThree.\n";

    #[test]
    fn finds_a_fence_by_its_first_line() {
        assert_eq!(
            fence(DOC, "$ wye"),
            Some(("console", "$ wye report\nok".to_owned()))
        );
        assert_eq!(fence(DOC, "ls"), Some(("bash", "ls".to_owned())));
        assert_eq!(fence(DOC, "nothing"), None);
    }

    #[test]
    fn splits_a_list_into_items_with_their_leads() {
        let section = "Intro.\n\n- **One.** first\n  continued\n\n- **Two.** second\n- plain\n\nAfter the list.\n- not an item\n";
        let items = list_items(section);
        assert_eq!(
            items,
            ["**One.** first\ncontinued", "**Two.** second", "plain"]
        );
        assert_eq!(
            items.iter().map(|i| lead(i)).collect::<Vec<_>>(),
            [Some("One."), Some("Two."), None]
        );
    }

    #[test]
    fn a_section_ends_at_the_next_heading_outside_a_fence() {
        let status = section(DOC, "Status").unwrap();
        assert!(
            status.contains("One.")
                && status.contains("## not a heading")
                && status.contains("Two.")
        );
        assert!(!status.contains("Three."));
        assert_eq!(section(DOC, "Next"), Some("\nThree.\n"));
        assert_eq!(section(DOC, "Missing"), None);
    }

    #[test]
    fn links_open_on_github() {
        let repo = "https://github.com/askrid/wye";
        assert_eq!(
            link_url("docs/roadmap.md", repo, "README.md"),
            "https://github.com/askrid/wye/blob/main/docs/roadmap.md"
        );
        assert_eq!(
            link_url("#features", repo, "README.md"),
            "https://github.com/askrid/wye#features"
        );
        assert_eq!(
            link_url("#a", repo, "docs/guide.md"),
            "https://github.com/askrid/wye/blob/main/docs/guide.md#a"
        );
        assert_eq!(
            link_url("product.md", repo, "docs/guide.md"),
            "https://github.com/askrid/wye/blob/main/docs/product.md"
        );
        assert_eq!(
            link_url("https://x.dev/", repo, "README.md"),
            "https://x.dev/"
        );
        let html = to_html("See [Features](#features).", repo, "README.md");
        assert_eq!(
            html,
            "<p>See <a href=\"https://github.com/askrid/wye#features\">Features</a>.</p>\n"
        );
    }

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
            highlight("console", "$ a < b\nout"),
            "<span class=\"prompt\">$</span> a &lt; b\nout"
        );
        assert_eq!(highlight("text", "<x>"), "&lt;x&gt;");
    }
}
