//! Page directives and the repository sources they read.
//!
//! | Directive | Becomes |
//! | --- | --- |
//! | `{{version}}` | the wye crate's version |
//! | `{{release}}` | the version, with ", a prerelease" while it has a pre-release part |
//! | `{{commit}}` | the commit the page is built from: `GITHUB_SHA` in CI, else the checkout's `HEAD` |
//! | `{{license}}`, `{{repo}}` | the workspace's license and repository URL |
//! | `{{machine}}` | the benchmark machine in `bench/baseline.json` |
//! | `{{metric NAME\|DECIMALS}}` | a metric from `bench/baseline.json`; `NAME/1000` divides it first |
//! | `{{fence FILE\|PREFIX}}` | the first fenced block in FILE whose first line starts with PREFIX, highlighted |
//! | `{{field FILE\|PREFIX\|KEY}}` | KEY's value in that fenced block, read as a JSON object |
//! | `{{section FILE\|HEADING}}` | the Markdown under `## HEADING` in FILE, rendered |
//! | `{{item FILE\|HEADING\|LEAD}}` | the item of that section's list whose bold lead starts with LEAD, rendered |
//! | `{{rest FILE\|HEADING}}` | every item of that list the page has not placed yet, so a new one is never dropped |
//! | `{{paragraph FILE\|HEADING\|START}}` | the paragraph of that section that starts with START, rendered |
//! | `{{ratios FILE\|HEADING\|COLUMN}}` | a bar chart of that section's table column of multiples of Wye ("7×") |
//! | `{{versus FILE\|HEADING\|ROW}}` | a bar chart of the first figure in each gateway column of the row starting with ROW |

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::{chart, markdown};

/// The documents directives may read, relative to the repository root.
const DOCUMENTS: [&str; 3] = ["README.md", "CHANGELOG.md", "docs/benchmarks.md"];

/// Everything a page may draw from the repository, read once per build.
pub struct Sources {
    version: String,
    commit: String,
    license: String,
    repo_url: String,
    machine: String,
    metrics: HashMap<String, f64>,
    documents: HashMap<&'static str, String>,
}

impl Sources {
    pub fn load(repo: &Path) -> Result<Self, String> {
        let read = |relative: &str| {
            let path = repo.join(relative);
            fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
        };
        let crate_manifest = read("crates/wye/Cargo.toml")?;
        let workspace_manifest = read("Cargo.toml")?;
        let baseline: serde_json::Value = serde_json::from_str(&read("bench/baseline.json")?)
            .map_err(|e| format!("bench/baseline.json: {e}"))?;
        let metrics = baseline
            .get("metrics")
            .and_then(|metrics| metrics.as_object())
            .ok_or("bench/baseline.json: no metrics")?
            .iter()
            .filter_map(|(name, value)| Some((name.clone(), value.as_f64()?)))
            .collect();
        let mut documents = HashMap::new();
        for name in DOCUMENTS {
            documents.insert(name, read(name)?);
        }
        Ok(Self {
            version: manifest_field(&crate_manifest, "version")
                .ok_or("crates/wye/Cargo.toml: no version")?,
            commit: commit(repo)?,
            license: manifest_field(&workspace_manifest, "license")
                .ok_or("Cargo.toml: no license")?,
            repo_url: manifest_field(&workspace_manifest, "repository")
                .ok_or("Cargo.toml: no repository")?,
            machine: machine(&baseline).ok_or("bench/baseline.json: no machine cpu and os")?,
            metrics,
            documents,
        })
    }

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
            ("machine", []) => Ok(markdown::escape(&self.machine)),
            ("metric", [metric, decimals]) => self.metric(metric, decimals),
            ("fence", [file, prefix]) => {
                let (lang, code) = markdown::fence(self.document(file)?, prefix)
                    .ok_or_else(|| format!("{file}: no fenced block starts with {prefix:?}"))?;
                Ok(format!(
                    "<pre class=\"code\" data-lang=\"{}\"><code>{}</code></pre>",
                    markdown::escape(lang),
                    markdown::highlight(lang, &code)
                ))
            }
            ("field", [file, prefix, key]) => {
                let (_, code) = markdown::fence(self.document(file)?, prefix)
                    .ok_or_else(|| format!("{file}: no fenced block starts with {prefix:?}"))?;
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
            ("section", [file, heading]) => {
                let text = markdown::section(self.document(file)?, heading)
                    .ok_or_else(|| format!("{file}: no section \"## {heading}\""))?;
                Ok(markdown::to_html(text, &self.repo_url, file))
            }
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
                let paragraph = self
                    .section(file, heading)?
                    .split("\n\n")
                    .map(str::trim)
                    .find(|paragraph| paragraph.starts_with(start))
                    .ok_or_else(|| {
                        format!("{file}: no paragraph under \"## {heading}\" starts with {start:?}")
                    })?;
                Ok(markdown::to_html(paragraph, &self.repo_url, file))
            }
            ("ratios", [file, heading, column]) => {
                chart::ratios(self.section(file, heading)?, column)
                    .map_err(|e| format!("{file}, \"## {heading}\": {e}"))
            }
            ("versus", [file, heading, row]) => chart::versus(self.section(file, heading)?, row)
                .map_err(|e| format!("{file}, \"## {heading}\": {e}")),
            _ => Err(format!("unknown directive {{{{{directive}}}}}")),
        }
    }

    fn items(&self, file: &str, heading: &str) -> Result<Vec<String>, String> {
        Ok(markdown::list_items(self.section(file, heading)?))
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
                _ => {
                    return Err(format!(
                        "metric {expression}: the divisor is not a positive number"
                    ));
                }
            },
            None => (expression, 1.0),
        };
        let value = self
            .metrics
            .get(name)
            .ok_or_else(|| format!("bench/baseline.json: no metric {name}"))?;
        let decimals: usize = decimals.parse().map_err(|_| {
            format!("metric {expression}: {decimals:?} is not a number of decimals")
        })?;
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
    while let Some(start) = rest.find("{{") {
        let (before, directive) = rest.split_at(start);
        out.push_str(before);
        let directive = directive.strip_prefix("{{").unwrap_or(directive);
        let end = directive.find("}}").ok_or("a {{ without its }}")?;
        let (directive, after) = directive.split_at(end);
        out.push_str(&sources.expand(directive.trim(), &mut placed)?);
        rest = after.strip_prefix("}}").unwrap_or(after);
    }
    out.push_str(rest);
    Ok(out)
}

/// Names a list item by its document, section, and lead (the whole item if it
/// has no lead).
fn item_key(file: &str, heading: &str, item: &str) -> String {
    format!("{file}|{heading}|{}", markdown::lead(item).unwrap_or(item))
}

/// The commit a page is built from, abbreviated: `GITHUB_SHA` in a GitHub
/// workflow, else the checkout's `HEAD`.
fn commit(repo: &Path) -> Result<String, String> {
    if let Some(sha) = std::env::var("GITHUB_SHA")
        .ok()
        .and_then(|sha| sha.get(..7).map(str::to_owned))
    {
        return Ok(sha);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse: {e}"))?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || sha.is_empty() {
        return Err("no commit: not a Git checkout, and GITHUB_SHA is not set".into());
    }
    Ok(sha)
}

/// The value of the first `key = "value"` line in a Cargo manifest.
fn manifest_field(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let value = line
            .trim_start()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?;
        Some(
            value
                .trim()
                .strip_prefix('"')?
                .strip_suffix('"')?
                .to_owned(),
        )
    })
}

/// "Intel Core i9-9880H, macOS" from the baseline's raw CPU brand and OS.
fn machine(baseline: &serde_json::Value) -> Option<String> {
    let machine = baseline.get("machine")?;
    let cpu = machine
        .get("cpu")?
        .as_str()?
        .replace("(R)", "")
        .replace("(TM)", "");
    let cpu = cpu.split(" CPU @").next().unwrap_or(&cpu).trim().to_owned();
    let os = match machine.get("os")?.as_str()? {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    Some(format!("{cpu}, {os}"))
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
            machine: "Intel Core i9-9880H, macOS".into(),
            metrics: [
                ("unary_added_p50_us".to_owned(), 281.0),
                ("cpu".to_owned(), 120.5),
            ]
            .into_iter()
            .collect(),
            documents: [(
                "README.md",
                "# Wye\n\n```console\n$ wye report\n3 requests\n```\n\n\
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
    fn reads_a_field_of_a_json_block() {
        assert_eq!(
            render(
                "{{field README.md|{|input_tokens}} in, ${{field README.md|{|cost_usd}}",
                &sources()
            )
            .unwrap(),
            "12 in, $0.0181"
        );
        assert!(render("{{field README.md|{|model}}", &sources()).is_err());
        assert!(render("{{field README.md|$ wye|model}}", &sources()).is_err());
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
        assert!(render("{{paragraph README.md|Performance|Nobody}}", &sources()).is_err());
    }

    #[test]
    fn expands_fences_and_sections() {
        let page = render(
            "{{fence README.md|$ wye report}}{{section README.md|Status}}",
            &sources(),
        )
        .unwrap();
        assert!(page.starts_with("<pre class=\"code\" data-lang=\"console\"><code><span class=\"prompt\">$</span> wye report\n3 requests</code></pre>"));
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
        assert!(render("{{fence README.md|nothing}}", &s).is_err());
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
        assert!(render("{{version", &s).is_err());
    }

    #[test]
    fn reads_manifests_and_the_machine() {
        let manifest =
            "[package]\nname = \"wye\"\nrust-version = \"1.98\"\nversion = \"0.1.0-rc.1\"\n";
        assert_eq!(
            manifest_field(manifest, "version").as_deref(),
            Some("0.1.0-rc.1")
        );
        let baseline = serde_json::json!({
            "machine": { "cpu": "Intel(R) Core(TM) i9-9880H CPU @ 2.30GHz", "os": "macos" }
        });
        assert_eq!(
            machine(&baseline).as_deref(),
            Some("Intel Core i9-9880H, macOS")
        );
    }
}
