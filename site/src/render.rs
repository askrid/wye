//! Page directives: each `{{name ARG|ARG}}` in a page becomes text drawn from
//! the repository (`render/sources.rs` reads it). One that cannot be filled
//! fails the build, and the error names the directive as written.
//!
//! - `{{version}}`: the wye crate's version.
//! - `{{release}}`: the version, with ", a prerelease" while it has a
//!   pre-release part.
//! - `{{commit}}`: the commit the page is built from: `GITHUB_SHA` in CI, else
//!   the checkout's `HEAD`.
//! - `{{license}}`, `{{repo}}`: the workspace's license and repository URL.
//! - `{{homepage}}`: the site's own address, `homepage` in site/Cargo.toml, so
//!   a move changes one line.
//! - `{{machine}}`: the benchmark machine in `bench/baseline.json`.
//! - `{{metric NAME|DECIMALS}}`: a metric from `bench/baseline.json`;
//!   `NAME/1000` divides it first.
//! - `{{fence FILE|PREFIX}}`: the first fenced block in FILE whose first line
//!   starts with PREFIX, highlighted.
//! - `{{field FILE|PREFIX|KEY}}`: KEY's value in that block, read as a JSON
//!   object.
//! - `{{opening FILE|PART}}`: FILE's opening paragraph: its bold `tagline`,
//!   the `rest` rendered, or the rest as plain `text`.
//! - `{{section FILE|HEADING}}`: the Markdown under `## HEADING`, rendered.
//! - `{{item FILE|HEADING|LEAD}}`: the item of that section's list whose bold
//!   lead starts with LEAD, rendered.
//! - `{{rest FILE|HEADING}}`: every item of that list the page has not placed
//!   yet, so a new one is never dropped.
//! - `{{paragraph FILE|HEADING|START}}`: the paragraph of that section that
//!   starts with START, rendered.
//! - `{{ratios FILE|HEADING|COLUMN}}`: a bar chart of that section's table
//!   column of multiples of Wye ("7×").
//! - `{{versus FILE|HEADING|ROW}}`: a bar chart of the first figure in each
//!   gateway column of the row that starts with ROW.

use std::collections::{BTreeSet, HashMap};

use crate::{chart, markdown};

mod sources;

/// The documents directives may read, relative to the repository root.
const DOCUMENTS: [&str; 3] = ["README.md", "CHANGELOG.md", "docs/benchmarks.md"];

/// Everything a page may draw from the repository, read once per build.
pub struct Sources {
    version: String,
    commit: String,
    license: String,
    repo_url: String,
    homepage: String,
    machine: String,
    metrics: HashMap<String, f64>,
    documents: HashMap<&'static str, String>,
}

impl Sources {
    /// `placed` names the list items this page has rendered so far.
    fn expand(&self, directive: &str, placed: &mut BTreeSet<String>) -> Result<String, String> {
        let (name, args) = directive.split_once(' ').unwrap_or((directive, ""));
        let args: Vec<&str> = match args.trim() {
            "" => Vec::new(),
            args => args.split('|').map(str::trim).collect(),
        };
        match (name, args.as_slice()) {
            ("version", []) => Ok(markdown::escape(&self.version)),
            ("release", []) => Ok(markdown::escape(&if self.version.contains('-') {
                format!("{}, a prerelease", self.version)
            } else {
                self.version.clone()
            })),
            ("commit", []) => Ok(markdown::escape(&self.commit)),
            ("license", []) => Ok(markdown::escape(&self.license)),
            ("repo", []) => Ok(markdown::escape(&self.repo_url)),
            ("homepage", []) => Ok(markdown::escape(&self.homepage)),
            ("machine", []) => Ok(markdown::escape(&self.machine)),
            ("metric", [metric, decimals]) => self.metric(metric, decimals),
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
                let (tagline, rest) = markdown::opening(self.document(file)?).ok_or_else(|| {
                    format!(
                        "{file}: no opening paragraph with a bold tagline before its first section"
                    )
                })?;
                match *part {
                    "tagline" => Ok(markdown::escape(tagline)),
                    "rest" => Ok(markdown::to_html(rest, &self.repo_url, file)),
                    "text" => Ok(markdown::escape(
                        &rest
                            .replace(['`', '*'], "")
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                    )),
                    other => Err(format!("{other:?} is not tagline, rest, or text")),
                }
            }
            ("section", [file, heading]) => Ok(markdown::to_html(
                self.section(file, heading)?,
                &self.repo_url,
                file,
            )),
            ("item", [file, heading, prefix]) => {
                let items = self.items(file, heading)?;
                let item = items
                    .iter()
                    .find(|item| markdown::lead(item).is_some_and(|lead| lead.starts_with(prefix)))
                    .ok_or_else(|| {
                        format!("{file}: no item under \"## {heading}\" opens with {prefix:?}")
                    })?;
                if !placed.insert(item_key(file, heading, item)) {
                    return Err(format!("{file}: the item {prefix:?} is placed twice"));
                }
                Ok(self.item_html(item, file))
            }
            ("rest", [file, heading]) => {
                let mut out = String::new();
                for item in self.items(file, heading)? {
                    if placed.insert(item_key(file, heading, &item)) {
                        out.push_str(&self.item_html(&item, file));
                    }
                }
                Ok(out)
            }
            ("paragraph", [file, heading, start]) => {
                let paragraph = markdown::paragraph(self.section(file, heading)?, start)
                    .ok_or_else(|| {
                        format!("{file}: no paragraph under \"## {heading}\" starts with {start:?}")
                    })?;
                Ok(markdown::to_html(paragraph, &self.repo_url, file))
            }
            ("ratios", [file, heading, column]) => {
                chart::ratios(self.section(file, heading)?, column)
            }
            ("versus", [file, heading, row]) => chart::versus(self.section(file, heading)?, row),
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

    fn item_html(&self, item: &str, file: &str) -> String {
        format!(
            "<div class=\"item\">{}</div>",
            markdown::to_html(item, &self.repo_url, file)
        )
    }

    fn metric(&self, expression: &str, decimals: &str) -> Result<String, String> {
        let (name, divisor) = match expression.split_once('/') {
            Some((name, divisor)) => match divisor.parse::<f64>() {
                Ok(divisor) if divisor > 0.0 => (name, divisor),
                _ => return Err("the divisor is not a positive number".to_owned()),
            },
            None => (expression, 1.0),
        };
        let value = self
            .metrics
            .get(name)
            .ok_or_else(|| format!("bench/baseline.json: no metric {name}"))?;
        let decimals: usize = decimals
            .parse()
            .map_err(|_| format!("{decimals:?} is not a number of decimals"))?;
        Ok(format!("{:.*}", decimals, value / divisor))
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
    let mut placed = BTreeSet::new();
    let mut rest = template;
    while let Some((before, open)) = rest.split_once("{{") {
        out.push_str(before);
        let (directive, after) = open.split_once("}}").ok_or("a {{ without its }}")?;
        let directive = directive.trim();
        out.push_str(
            &sources
                .expand(directive, &mut placed)
                .map_err(|e| format!("{{{{{directive}}}}}: {e}"))?,
        );
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
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
            machine: "Intel Core i9-9880H, macOS".into(),
            metrics: [
                ("unary_added_p50_us".to_owned(), 281.0),
                ("cpu".to_owned(), 120.5),
            ]
            .into_iter()
            .collect(),
            documents: [(
                "README.md",
                "# Wye\n\n**Nothing stops at a wye.** Wye is one\n`binary`.\n\n\
                 ```console\n$ wye report\n3 requests\n```\n\n\
                 ```json\n{\n  \"cost_usd\": \"0.0181\",\n  \"input_tokens\": 12\n}\n```\n\n\
                 ## Status\n\nBuilt.\n\n\
                 ## Performance\n\nNumbers.\n\nWhere others lead: one, on\n[two](docs/b.md).\n\n\
                 ## What it does\n\n- **Shows usage.** Records.\n- **Gives access.** Tokens.\n\
                 - **Runs.** Anywhere.\n"
                    .to_owned(),
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn expands_values_and_metrics() {
        let page = "v{{version}} {{ metric unary_added_p50_us/1000|2 }} ms, {{metric cpu|0}} µs";
        assert_eq!(
            render(page, &sources()).unwrap(),
            "v0.1.0-rc.1 0.28 ms, 120 µs"
        );
        assert_eq!(
            render("{{release}} at {{commit}}", &sources()).unwrap(),
            "0.1.0-rc.1, a prerelease at 81e7951"
        );
        let mut stable = sources();
        stable.version = "1.0.0".into();
        assert_eq!(render("{{release}}", &stable).unwrap(), "1.0.0");
    }

    #[test]
    fn splits_the_opening_paragraph_at_its_tagline() {
        let page =
            "{{opening README.md|tagline}}|{{opening README.md|rest}}|{{opening README.md|text}}";
        assert_eq!(
            render(page, &sources()).unwrap(),
            "Nothing stops at a wye.|<p>Wye is one\n<code>binary</code>.</p>\n|Wye is one binary."
        );
        assert!(
            render("{{opening README.md|title}}", &sources())
                .unwrap_err()
                .contains("\"title\" is not tagline, rest, or text")
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
             <a href=\"https://github.com/askrid/wye/blob/main/docs/b.md\">two</a>.</p>\n"
        );
        assert!(
            render("{{paragraph README.md|Performance|Nobody}}", &sources())
                .unwrap_err()
                .contains("starts with \"Nobody\"")
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
            render("{{metric missing|1}}", &s)
                .unwrap_err()
                .contains("no metric missing")
        );
        assert!(render("{{metric cpu/0|1}}", &s).is_err());
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
            render("x {{ metric cpu/0|1 }}", &s).unwrap_err(),
            "{{metric cpu/0|1}}: the divisor is not a positive number"
        );
        assert_eq!(
            render("{{ratios README.md|Status|Speed}}", &s).unwrap_err(),
            "{{ratios README.md|Status|Speed}}: no table with a column \"Speed\""
        );
    }
}
