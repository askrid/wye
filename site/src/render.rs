//! Page directives: each `{{name ARG|ARG}}` in a page becomes text drawn from
//! the repository (`render/sources.rs` reads it). One that cannot be filled
//! fails the build, and the error names the directive as written.
//!
//! - `{{version}}`: the wye crate's version.
//! - `{{release}}`: `prerelease` while the version has a pre-release part,
//!   else `stable`.
//! - `{{commit}}`: the commit the page is built from: `GITHUB_SHA` in CI, else
//!   the checkout's `HEAD`.
//! - `{{license}}`, `{{repo}}`: the workspace's license and repository URL.
//! - `{{homepage}}`: the site's own address, `homepage` in site/Cargo.toml, so
//!   a move changes one line.
//! - `{{stylesheet}}`: site/pages/site.css as it is, for a `<style>`
//!   element, so the page draws without waiting for a second file.
//! - `{{fence FILE|PREFIX}}`: the first fenced block in FILE whose first line
//!   starts with PREFIX, highlighted.
//! - `{{field FILE|PREFIX|KEY}}`: KEY's value in that block, read as a JSON
//!   object.
//! - `{{opening FILE|PART}}`: FILE's opening `tagline`, its bold first
//!   paragraph without the closing period; the list after it, rendered
//!   (`lede`); or that paragraph as plain `text`.
//! - `{{section FILE|HEADING}}`: the Markdown under `## HEADING`, rendered.
//! - `{{item FILE|HEADING|LEAD}}`: the item of that section's list whose bold
//!   lead starts with LEAD, rendered.
//! - `{{rest FILE|HEADING}}`: every item of that list the page has not placed
//!   yet, so a new one is never dropped.
//! - `{{paragraph FILE|HEADING|START}}`: the paragraph of that section that
//!   starts with START, rendered.
//! - `{{numbers FILE|HEADING|FIRST}}`: that section's table whose first column
//!   is headed FIRST, as a table of figures.
//! - `{{ratios FILE|HEADING|COLUMN}}`: a bar chart of that section's table
//!   column of multiples of Wye ("7×").

use std::collections::{BTreeSet, HashMap};

use crate::{chart, markdown};

mod sources;

/// The documents directives may read, relative to the repository root.
const DOCUMENTS: [&str; 3] = ["README.md", "CHANGELOG.md", "docs/operations.md"];

/// The directives whose first two arguments name a document and one of its
/// headings.
const HEADED: [&str; 6] = ["section", "item", "rest", "paragraph", "numbers", "ratios"];

/// Everything a page may draw from the repository, read once per build.
pub struct Sources {
    version: String,
    commit: String,
    license: String,
    repo_url: String,
    homepage: String,
    stylesheet: String,
    documents: HashMap<&'static str, String>,
}

/// What expanding one page carries from directive to directive.
struct Page {
    /// The list items rendered so far.
    placed: BTreeSet<String>,
    anchors: markdown::Anchors,
}

impl Sources {
    fn expand(&self, directive: &str, page: &mut Page) -> Result<String, String> {
        let (name, args) = directive.split_once(' ').unwrap_or((directive, ""));
        let args: Vec<&str> = match args.trim() {
            "" => Vec::new(),
            args => args.split('|').map(str::trim).collect(),
        };
        match (name, args.as_slice()) {
            ("version", []) => Ok(markdown::escape(&self.version)),
            ("release", []) => Ok(if self.version.contains('-') {
                "prerelease"
            } else {
                "stable"
            }
            .to_owned()),
            ("commit", []) => Ok(markdown::escape(&self.commit)),
            ("license", []) => Ok(markdown::escape(&self.license)),
            ("repo", []) => Ok(markdown::escape(&self.repo_url)),
            ("homepage", []) => Ok(markdown::escape(&self.homepage)),
            ("stylesheet", []) => Ok(self.stylesheet.clone()),
            ("fence", [file, prefix]) => {
                let (lang, code) = self.fence(file, prefix)?;
                Ok(format!(
                    // Focusable, so a keyboard can scroll a block wider
                    // than the page.
                    "<pre class=\"code\" data-lang=\"{}\" tabindex=\"0\"><code>{}</code></pre>",
                    markdown::escape(lang),
                    markdown::highlight(lang, &code)
                ))
            }
            ("field", [file, prefix, key]) => {
                let (_, code) = self.fence(file, prefix)?;
                let object: serde_json::Value = serde_json::from_str(&code).map_err(|e| {
                    format!("{file}: the block starting with {prefix:?} is not JSON: {e}")
                })?;
                let value = object.get(key).ok_or_else(|| {
                    format!("{file}: the block starting with {prefix:?} has no {key:?}")
                })?;
                Ok(markdown::escape(&match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                }))
            }
            ("opening", [file, part]) => {
                let (tagline, _) = markdown::opening(self.document(file)?).ok_or_else(|| {
                    format!(
                        "{file}: no opening paragraph with a bold tagline before its first section"
                    )
                })?;
                match *part {
                    "tagline" => Ok(markdown::escape(tagline.trim_end_matches('.'))),
                    "lede" | "text" => {
                        let lede = markdown::opening_lede(self.document(file)?)
                            .ok_or_else(|| format!("{file}: no paragraph after the tagline"))?;
                        if *part == "lede" {
                            return Ok(markdown::to_html(
                                lede,
                                &self.files_url(),
                                file,
                                &page.anchors,
                            ));
                        }
                        Ok(markdown::escape(&markdown::plain(lede)))
                    }
                    other => Err(format!("{other:?} is not tagline, text, or lede")),
                }
            }
            ("section", [file, heading]) => Ok(markdown::to_html(
                self.section(file, heading)?,
                &self.files_url(),
                file,
                &page.anchors,
            )),
            ("item", [file, heading, prefix]) => {
                let items = self.items(file, heading)?;
                let item = items
                    .iter()
                    .find(|item| markdown::lead(item).is_some_and(|lead| lead.starts_with(prefix)))
                    .ok_or_else(|| {
                        format!("{file}: no item under \"## {heading}\" opens with {prefix:?}")
                    })?;
                if !page.placed.insert(item_key(file, heading, item)) {
                    return Err(format!("{file}: the item {prefix:?} is placed twice"));
                }
                Ok(self.item_html(item, file, &page.anchors))
            }
            ("rest", [file, heading]) => {
                let mut out = String::new();
                for item in self.items(file, heading)? {
                    if page.placed.insert(item_key(file, heading, &item)) {
                        out.push_str(&self.item_html(&item, file, &page.anchors));
                    }
                }
                Ok(out)
            }
            ("paragraph", [file, heading, start]) => {
                let paragraph = markdown::paragraph(self.section(file, heading)?, start)
                    .ok_or_else(|| {
                        format!("{file}: no paragraph under \"## {heading}\" starts with {start:?}")
                    })?;
                Ok(markdown::to_html(
                    paragraph,
                    &self.files_url(),
                    file,
                    &page.anchors,
                ))
            }
            ("numbers", [file, heading, first]) => {
                markdown::numbers(self.section(file, heading)?, first).ok_or_else(|| {
                    format!("{file}: no table under \"## {heading}\" has a first column {first:?}")
                })
            }
            ("ratios", [file, heading, column]) => {
                chart::ratios(self.section(file, heading)?, column)
            }
            _ => Err("unknown directive".to_owned()),
        }
    }

    fn items(&self, file: &str, heading: &str) -> Result<Vec<String>, String> {
        Ok(markdown::list_items(self.section(file, heading)?))
    }

    fn fence(&self, file: &str, prefix: &str) -> Result<(&str, String), String> {
        markdown::fence(self.document(file)?, prefix)
            .ok_or_else(|| format!("{file}: no fenced block starts with {prefix:?}"))
    }

    fn section(&self, file: &str, heading: &str) -> Result<&str, String> {
        markdown::section(self.document(file)?, heading)
            .ok_or_else(|| format!("{file}: no section \"## {heading}\""))
    }

    fn item_html(&self, item: &str, file: &str, anchors: &markdown::Anchors) -> String {
        format!(
            "<div class=\"item\">{}</div>",
            markdown::to_html(item, &self.files_url(), file, anchors)
        )
    }

    /// Where the repository's files are at the page's commit.
    fn files_url(&self) -> String {
        format!("{}/blob/{}", self.repo_url, self.commit)
    }

    fn document(&self, file: &str) -> Result<&str, String> {
        self.documents.get(file).map(String::as_str).ok_or_else(|| {
            format!(
                "{file} is not a site source (one of {})",
                DOCUMENTS.join(", ")
            )
        })
    }
}

/// Expands every `{{directive}}` in a page.
pub fn render(template: &str, sources: &Sources) -> Result<String, String> {
    let mut out = String::with_capacity(template.len() * 2);
    let mut page = Page {
        placed: BTreeSet::new(),
        anchors: anchors(template),
    };
    let mut rest = template;
    while let Some((before, open)) = rest.split_once("{{") {
        out.push_str(before);
        let (directive, after) = open.split_once("}}").ok_or("a {{ without its }}")?;
        let directive = directive.trim();
        out.push_str(
            &sources
                .expand(directive, &mut page)
                .map_err(|e| format!("{{{{{directive}}}}}: {e}"))?,
        );
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
}

/// Headings the page renders in exactly one of its `<section id="…">`
/// elements, by `FILE#slug`; a directive belongs to the last section opened
/// before it. A heading spread over several sections, such as a list placed
/// item by item, keeps its links on GitHub.
fn anchors(template: &str) -> markdown::Anchors {
    let mut found: HashMap<String, BTreeSet<Option<&str>>> = HashMap::new();
    for (i, chunk) in template.split("<section").enumerate() {
        let id = (i > 0)
            .then(|| chunk.split_once('>'))
            .flatten()
            .and_then(|(tag, _)| tag.split_once(" id=\""))
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(id, _)| id);
        for opened in chunk.split("{{").skip(1) {
            let Some((directive, _)) = opened.split_once("}}") else {
                continue;
            };
            let directive = directive.trim();
            let (name, args) = directive.split_once(' ').unwrap_or((directive, ""));
            let mut args = args.split('|').map(str::trim);
            if let (true, Some(file), Some(heading)) =
                (HEADED.contains(&name), args.next(), args.next())
            {
                let key = format!("{file}#{}", markdown::slug(heading));
                found.entry(key).or_default().insert(id);
            }
        }
    }
    found
        .into_iter()
        .filter_map(
            |(key, ids)| match ids.into_iter().collect::<Vec<_>>().as_slice() {
                [Some(id)] => Some((key, (*id).to_owned())),
                _ => None,
            },
        )
        .collect()
}

/// Names a list item by its document, section, and lead (the whole item if it
/// has no lead).
fn item_key(file: &str, heading: &str, item: &str) -> String {
    format!("{file}|{heading}|{}", markdown::lead(item).unwrap_or(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        Sources {
            version: "0.1.0-rc.1".into(),
            commit: "81e7951".into(),
            license: "MIT".into(),
            repo_url: "https://github.com/askrid/wye".into(),
            homepage: "https://askrid.github.io/wye/".into(),
            stylesheet: "nav > a { color: #ededed }".into(),
            documents: [(
                "README.md",
                "# Wye\n\n**LLM gateway without bloat.**\n\n\
                 Small and [fast](#performance): one\n`binary`.\n\n\
                 ```console\n$ wye report\n3 requests\n```\n\n\
                 ```json\n{\n  \"cost_usd\": \"0.0181\",\n  \"input_tokens\": 12\n}\n```\n\n\
                 ## Status\n\nBuilt.\n\n\
                 ## Performance\n\nNumbers.\n\n\
                 | What Wye adds | Plain | TLS & h2 |\n|---|---|---|\n| Latency | 0.28 ms | |\n\n\
                 Where others lead: one, on\n[two](docs/b.md).\n\n\
                 ## What it does\n\n- **Shows usage.** Records.\n- **Gives access.** Tokens.\n\
                 - **Runs.** Anywhere.\n"
                    .to_owned(),
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn expands_values() {
        assert_eq!(render("v{{ version }}", &sources()).unwrap(), "v0.1.0-rc.1");
        // The stylesheet goes in as written: escaping would break `>`.
        assert_eq!(
            render("<style>{{stylesheet}}</style>", &sources()).unwrap(),
            "<style>nav > a { color: #ededed }</style>"
        );
        assert_eq!(
            render("{{release}} at {{commit}}", &sources()).unwrap(),
            "prerelease at 81e7951"
        );
        let mut stable = sources();
        stable.version = "1.0.0".into();
        assert_eq!(render("{{release}}", &stable).unwrap(), "stable");
    }

    #[test]
    fn splits_the_opening_paragraph_at_its_tagline() {
        let page = "{{opening README.md|tagline}}|{{opening README.md|text}}";
        assert_eq!(
            render(page, &sources()).unwrap(),
            "LLM gateway without bloat|Small and fast: one binary."
        );
        assert!(
            render("{{opening README.md|title}}", &sources())
                .unwrap_err()
                .contains("\"title\" is not tagline, text, or lede")
        );
        assert_eq!(
            render("{{opening README.md|lede}}", &sources()).unwrap(),
            "<p>Small and \
             <a href=\"https://github.com/askrid/wye/blob/81e7951/README.md#performance\">fast</a>: one\n\
             <code>binary</code>.</p>\n"
        );
    }

    #[test]
    fn reads_a_field_of_a_json_block() {
        assert_eq!(
            render(
                "{{field README.md|{|input_tokens}} in, ${{field README.md|{|cost_usd}}",
                &sources()
            )
            .unwrap(),
            "12 in, $0.0181"
        );
        assert!(
            render("{{field README.md|{|model}}", &sources())
                .unwrap_err()
                .contains("has no \"model\"")
        );
        assert!(
            render("{{field README.md|$ wye|model}}", &sources())
                .unwrap_err()
                .contains("is not JSON")
        );
    }

    #[test]
    fn renders_a_paragraph_by_how_it_starts() {
        assert_eq!(
            render(
                "{{paragraph README.md|Performance|Where others lead}}",
                &sources()
            )
            .unwrap(),
            "<p>Where others lead: one, on\n\
             <a href=\"https://github.com/askrid/wye/blob/81e7951/docs/b.md\">two</a>.</p>\n"
        );
        assert!(
            render("{{paragraph README.md|Performance|Nobody}}", &sources())
                .unwrap_err()
                .contains("starts with \"Nobody\"")
        );
    }

    #[test]
    fn keeps_links_to_headings_the_page_renders_whole() {
        let page = "<section class=\"hero\">{{opening README.md|lede}}</section>\
                    <section class=\"part\" id=\"compare\">{{section README.md|How it compares}}</section>\
                    <section id=\"one\">{{item README.md|What it does|Shows}}</section>\
                    <section id=\"two\">{{ item README.md|What it does|Gives }}</section>";
        let anchors = anchors(page);
        assert_eq!(
            anchors.get("README.md#how-it-compares").map(String::as_str),
            Some("compare")
        );
        assert_eq!(anchors.get("README.md#what-it-does"), None);
        assert_eq!(anchors.len(), 1);
    }

    #[test]
    fn renders_a_table_of_figures() {
        assert_eq!(
            render(
                "{{numbers README.md|Performance|What Wye adds}}",
                &sources()
            )
            .unwrap(),
            "<div class=\"table-scroll\"><table class=\"numbers\"><thead><tr>\
             <th scope=\"col\">What Wye adds</th><th scope=\"col\" class=\"n\">Plain</th>\
             <th scope=\"col\" class=\"n\">TLS &amp; h2</th></tr></thead><tbody><tr>\
             <th scope=\"row\">Latency</th><td class=\"n\" data-path=\"Plain\">0.28 ms</td>\
             <td class=\"n na\"><span aria-hidden=\"true\">–</span>\
             <span class=\"visually-hidden\">not measured</span></td></tr></tbody></table></div>"
        );
    }

    #[test]
    fn expands_fences_and_sections() {
        let page = render(
            "{{fence README.md|$ wye report}}{{section README.md|Status}}",
            &sources(),
        )
        .unwrap();
        assert!(page.starts_with("<pre class=\"code\" data-lang=\"console\" tabindex=\"0\"><code><span class=\"prompt\">$</span> wye report\n3 requests</code></pre>"));
        assert!(page.ends_with("<p>Built.</p>\n"));
    }

    #[test]
    fn places_list_items_and_the_rest() {
        let s = sources();
        let page = render(
            "{{item README.md|What it does|Gives}}|{{rest README.md|What it does}}",
            &s,
        )
        .unwrap();
        assert_eq!(
            page,
            "<div class=\"item\"><p><strong>Gives access.</strong> Tokens.</p>\n</div>|\
             <div class=\"item\"><p><strong>Shows usage.</strong> Records.</p>\n</div>\
             <div class=\"item\"><p><strong>Runs.</strong> Anywhere.</p>\n</div>"
        );
        // Everything placed: the rest adds nothing.
        let all = "{{item README.md|What it does|Shows}}{{item README.md|What it does|Gives}}\
                   {{item README.md|What it does|Runs}}{{rest README.md|What it does}}";
        assert_eq!(
            render(all, &s)
                .unwrap()
                .matches("<div class=\"item\">")
                .count(),
            3
        );
        let twice = "{{item README.md|What it does|Runs}}{{item README.md|What it does|Runs}}";
        assert!(render(twice, &s).unwrap_err().contains("placed twice"));
        assert!(
            render("{{item README.md|What it does|Nothing}}", &s)
                .unwrap_err()
                .contains("opens with")
        );
    }

    #[test]
    fn refuses_what_it_cannot_fill() {
        let s = sources();
        assert!(
            render("{{numbers README.md|Performance|Speed}}", &s)
                .unwrap_err()
                .contains("has a first column \"Speed\"")
        );
        assert!(
            render("{{fence README.md|nothing}}", &s)
                .unwrap_err()
                .contains("no fenced block starts with \"nothing\"")
        );
        assert!(
            render("{{section docs/guide.md|Status}}", &s)
                .unwrap_err()
                .contains("not a site source")
        );
        assert!(
            render("{{nonsense}}", &s)
                .unwrap_err()
                .contains("unknown directive")
        );
        assert_eq!(render("{{version", &s).unwrap_err(), "a {{ without its }}");
    }

    #[test]
    fn errors_name_their_directive() {
        let s = sources();
        assert_eq!(
            render("x {{ opening README.md|title }}", &s).unwrap_err(),
            "{{opening README.md|title}}: \"title\" is not tagline, text, or lede"
        );
        assert_eq!(
            render("{{ratios README.md|Status|Speed}}", &s).unwrap_err(),
            "{{ratios README.md|Status|Speed}}: no table with a column \"Speed\""
        );
    }
}
