//! Page directives and the repository sources they read.
//!
//! | Directive | Becomes |
//! | --- | --- |
//! | `{{version}}` | the wye crate's version |
//! | `{{license}}`, `{{repo}}` | the workspace's license and repository URL |
//! | `{{machine}}` | the benchmark machine in `bench/baseline.json` |
//! | `{{metric NAME\|DECIMALS}}` | a metric from `bench/baseline.json`; `NAME/1000` divides it first |
//! | `{{fence FILE\|PREFIX}}` | the first fenced block in FILE whose first line starts with PREFIX, highlighted |
//! | `{{section FILE\|HEADING}}` | the Markdown under `## HEADING` in FILE, rendered |

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::markdown;

/// The documents `fence` and `section` may read, relative to the repository root.
const DOCUMENTS: [&str; 2] = ["README.md", "CHANGELOG.md"];

/// Everything a page may draw from the repository, read once per build.
pub struct Sources {
    version: String,
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
            license: manifest_field(&workspace_manifest, "license")
                .ok_or("Cargo.toml: no license")?,
            repo_url: manifest_field(&workspace_manifest, "repository")
                .ok_or("Cargo.toml: no repository")?,
            machine: machine(&baseline).ok_or("bench/baseline.json: no machine cpu and os")?,
            metrics,
            documents,
        })
    }

    fn expand(&self, directive: &str) -> Result<String, String> {
        let (name, args) = directive.split_once(' ').unwrap_or((directive, ""));
        let args: Vec<&str> = match args.trim() {
            "" => Vec::new(),
            args => args.split('|').map(str::trim).collect(),
        };
        match (name, args.as_slice()) {
            ("version", []) => Ok(markdown::escape(&self.version)),
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
            ("section", [file, heading]) => {
                let text = markdown::section(self.document(file)?, heading)
                    .ok_or_else(|| format!("{file}: no section \"## {heading}\""))?;
                Ok(markdown::to_html(text, &self.repo_url, file))
            }
            _ => Err(format!("unknown directive {{{{{directive}}}}}")),
        }
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
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let (before, directive) = rest.split_at(start);
        out.push_str(before);
        let directive = directive.strip_prefix("{{").unwrap_or(directive);
        let end = directive.find("}}").ok_or("a {{ without its }}")?;
        let (directive, after) = directive.split_at(end);
        out.push_str(&sources.expand(directive.trim())?);
        rest = after.strip_prefix("}}").unwrap_or(after);
    }
    out.push_str(rest);
    Ok(out)
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
                "# Wye\n\n```console\n$ wye report\n3 requests\n```\n\n## Status\n\nBuilt.\n"
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
