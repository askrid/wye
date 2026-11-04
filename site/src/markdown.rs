//! The Markdown the site reads from the repository: fenced blocks with light
//! highlighting, and sections rendered with their links pointed at GitHub,
//! or at the page itself where it renders the heading they name.

use std::collections::HashMap;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

/// Repository headings the page renders in one of its own sections, by
/// `FILE#slug`, with that section's id: `README.md#performance` → `stream`.
pub type Anchors = HashMap<String, String>;

/// `text` safe in HTML text and in a double-quoted attribute.
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

/// The paragraph that opens a document, before its first section, split at
/// its bold tagline: `**LLM gateway without bloat.** Wye sits…` gives the
/// tagline and the rest.
pub fn opening(doc: &str) -> Option<(&str, &str)> {
    let first = paragraph(doc.split("\n## ").next()?, "**")?;
    let (tagline, rest) = first.strip_prefix("**")?.split_once("**")?;
    Some((tagline, rest.trim()))
}

/// The first paragraph of `text` that starts with `start`, trimmed.
pub fn paragraph<'a>(text: &'a str, start: &str) -> Option<&'a str> {
    text.split("\n\n")
        .map(str::trim)
        .find(|paragraph| paragraph.starts_with(start))
}

/// Every Markdown table in `text`, each as rows of trimmed cells, header
/// first, the separator row left out.
pub fn tables(text: &str) -> Vec<Vec<Vec<String>>> {
    let mut tables = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(inner) = line.strip_prefix('|') {
            let cells: Vec<String> = inner
                .strip_suffix('|')
                .unwrap_or(inner)
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect();
            let separator = cells
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':')));
            if !separator {
                rows.push(cells);
            }
        } else if !rows.is_empty() {
            tables.push(std::mem::take(&mut rows));
        }
    }
    if !rows.is_empty() {
        tables.push(rows);
    }
    tables
}

/// The table in `text` whose first column is headed `first`, as a table of
/// figures: rows headed by their first cell, each value labelled with its
/// column for narrow screens, and an empty cell a dash that reads "not
/// measured". Cells are plain text.
pub fn numbers(text: &str, first: &str) -> Option<String> {
    let rows = tables(text).into_iter().find(|rows| {
        rows.first()
            .and_then(|header| header.first())
            .is_some_and(|cell| cell == first)
    })?;
    let (header, body) = rows.split_first()?;
    let mut out = String::from("<div class=\"table-scroll\"><table class=\"numbers\"><thead><tr>");
    for (i, cell) in header.iter().enumerate() {
        let class = if i == 0 { "" } else { " class=\"n\"" };
        out.push_str(&format!("<th scope=\"col\"{class}>{}</th>", escape(cell)));
    }
    out.push_str("</tr></thead><tbody>");
    for row in body {
        let Some((name, values)) = row.split_first() else {
            continue;
        };
        out.push_str(&format!("<tr><th scope=\"row\">{}</th>", escape(name)));
        for (column, value) in header.iter().skip(1).zip(values) {
            if value.is_empty() {
                out.push_str(
                    "<td class=\"n na\"><span aria-hidden=\"true\">–</span>\
                     <span class=\"visually-hidden\">not measured</span></td>",
                );
            } else {
                out.push_str(&format!(
                    "<td class=\"n\" data-path=\"{}\">{}</td>",
                    escape(column),
                    escape(value)
                ));
            }
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
    Some(out)
}

/// A heading's anchor as GitHub writes it: lowercase, punctuation dropped,
/// spaces as hyphens. `What Wye will not do` gives `what-wye-will-not-do`.
pub fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// Inline Markdown as one line of plain text, for a page description: the
/// words of links, code, and emphasis kept, their markup dropped.
pub fn plain(markdown: &str) -> String {
    let text: String = Parser::new(markdown)
        .filter_map(|event| match event {
            Event::Text(text) | Event::Code(text) => Some(text.into_string()),
            Event::SoftBreak | Event::HardBreak => Some(" ".to_owned()),
            _ => None,
        })
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The bold lead that opens a list item: `**Shows usage and cost.** One…`.
pub fn lead(item: &str) -> Option<&str> {
    item.strip_prefix("**")?
        .split_once("**")
        .map(|(lead, _)| lead)
}

/// Renders Markdown from `source` (a path relative to the repository root).
pub fn to_html(markdown: &str, repo_url: &str, source: &str, anchors: &Anchors) -> String {
    let events = Parser::new_ext(markdown, Options::ENABLE_TABLES).map(|event| match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::from(link_url(&dest_url, repo_url, source, anchors)),
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
/// site: a heading of the same document that the page renders stays on the
/// page; other files and anchors open on GitHub, as they do from the file.
fn link_url(dest: &str, repo_url: &str, source: &str, anchors: &Anchors) -> String {
    if dest.contains("://") || dest.starts_with("mailto:") {
        return dest.to_owned();
    }
    if let Some(anchor) = dest.strip_prefix('#') {
        if let Some(id) = anchors.get(&format!("{source}#{anchor}")) {
            return format!("#{id}");
        }
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
    fn plain_text_keeps_the_words_of_links_and_code() {
        assert_eq!(
            plain("Adds **under** a [millisecond](#performance) per\n`request` & more."),
            "Adds under a millisecond per request & more."
        );
    }

    #[test]
    fn links_open_on_the_page_or_github() {
        let repo = "https://github.com/askrid/wye";
        let none = Anchors::new();
        let page: Anchors = [("README.md#performance".to_owned(), "stream".to_owned())].into();
        assert_eq!(
            link_url("#performance", repo, "README.md", &page),
            "#stream"
        );
        assert_eq!(
            link_url("#performance", repo, "docs/guide.md", &page),
            "https://github.com/askrid/wye/blob/main/docs/guide.md#performance"
        );
        assert_eq!(slug("What Wye will not do"), "what-wye-will-not-do");
        assert_eq!(
            link_url("docs/roadmap.md", repo, "README.md", &none),
            "https://github.com/askrid/wye/blob/main/docs/roadmap.md"
        );
        assert_eq!(
            link_url("#features", repo, "README.md", &none),
            "https://github.com/askrid/wye#features"
        );
        assert_eq!(
            link_url("#a", repo, "docs/guide.md", &none),
            "https://github.com/askrid/wye/blob/main/docs/guide.md#a"
        );
        assert_eq!(
            link_url("product.md", repo, "docs/guide.md", &none),
            "https://github.com/askrid/wye/blob/main/docs/product.md"
        );
        assert_eq!(
            link_url("https://x.dev/", repo, "README.md", &none),
            "https://x.dev/"
        );
        let html = to_html("See [Features](#features).", repo, "README.md", &none);
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
