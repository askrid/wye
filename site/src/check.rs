//! Checks on the built site: everything a page or a script loads comes from
//! the site itself (no request to another origin, so nothing tracks a
//! reader), every local link and anchor resolves, every link into the
//! repository names a file it has, and no directive is left unexpanded.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Files by their path under `out/`, with the text of pages, stylesheets,
/// and scripts.
type Files = BTreeMap<String, Option<String>>;

/// The repository the pages link into: where its files are on GitHub at the
/// page's commit, and whether a path names one of them.
pub struct Repo<'a> {
    pub files_url: &'a str,
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Every problem in the built site under `out`, all reported at once.
pub fn check(out: &Path, repo: &Repo) -> Result<(), String> {
    let mut files = Files::new();
    collect(out, "", &mut files)?;
    let problems = problems(&files, repo);
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the built site has {} problem(s):\n  {}",
        problems.len(),
        problems.join("\n  ")
    ))
}

fn collect(dir: &Path, prefix: &str, files: &mut Files) -> Result<(), String> {
    for path in crate::sorted_entries(dir)? {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect(&path, &relative, files)?;
        } else {
            let text = if [".html", ".css", ".js"]
                .iter()
                .any(|kind| relative.ends_with(kind))
            {
                Some(fs::read_to_string(&path).map_err(|e| crate::io_error(&path, e))?)
            } else {
                None
            };
            files.insert(relative, text);
        }
    }
    Ok(())
}

fn problems(files: &Files, repo: &Repo) -> Vec<String> {
    let mut found = Vec::new();
    for (path, text) in files {
        let Some(text) = text else { continue };
        if path.ends_with(".css") {
            for target in css_urls(text) {
                check_resource(files, path, target, &mut found);
            }
            continue;
        }
        if path.ends_with(".js") {
            // A script's URLs resolve against the page that runs it, and
            // every page is at the site's root.
            for target in script_urls(text) {
                match target {
                    Some(target) => {
                        check_resource_from(files, path, "index.html", target, &mut found)
                    }
                    None => found.push(format!("{path}: loads a URL the check cannot read")),
                }
            }
            continue;
        }
        if text.contains("{{") {
            found.push(format!("{path}: an unexpanded directive"));
        }
        for target in styles(text).flat_map(css_urls) {
            check_resource(files, path, target, &mut found);
        }
        for (reference, target) in references(text) {
            match reference {
                Reference::Link => check_link(files, repo, path, target, &mut found),
                Reference::Resource => check_resource(files, path, target, &mut found),
            }
        }
    }
    found
}

/// A file the browser fetches on its own: it must be part of the site.
fn check_resource(files: &Files, from: &str, target: &str, found: &mut Vec<String>) {
    check_resource_from(files, from, from, target, found);
}

/// As `check_resource`, `target` resolved against the page `base`.
fn check_resource_from(
    files: &Files,
    from: &str,
    base: &str,
    target: &str,
    found: &mut Vec<String>,
) {
    if target.starts_with("data:") {
        return;
    }
    if target.contains("://") || target.starts_with("//") {
        found.push(format!("{from}: loads {target} from another origin"));
        return;
    }
    let path = target.split(['#', '?']).next().unwrap_or(target);
    if resolve(base, path).is_none_or(|path| !files.contains_key(&path)) {
        found.push(format!("{from}: {target} is not in the site"));
    }
}

/// A link the reader follows: other sites are fine, one into the
/// repository must name a file it has, and local ones must resolve.
fn check_link(files: &Files, repo: &Repo, from: &str, target: &str, found: &mut Vec<String>) {
    if let Some(file) = target
        .strip_prefix(repo.files_url)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        let file = file.split(['#', '?']).next().unwrap_or(file);
        if !(repo.exists)(file) {
            found.push(format!(
                "{from}: the link {target} names no file in the repository"
            ));
        }
        return;
    }
    if target.contains("://") || target.starts_with("mailto:") {
        return;
    }
    let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
    let page = if path.is_empty() {
        Some(from.to_owned())
    } else {
        resolve(from, path)
    };
    let Some(page) = page.filter(|page| files.contains_key(page)) else {
        found.push(format!("{from}: the link {target} leads nowhere"));
        return;
    };
    if anchor.is_empty() {
        return;
    }
    let text = files
        .get(&page)
        .and_then(Option::as_deref)
        .unwrap_or_default();
    if !attribute_values(text, "id").contains(&anchor) {
        found.push(format!("{from}: the link {target} names a missing anchor"));
    }
}

/// `target` relative to the page `from`, as a path under `out/`; a
/// directory means its `index.html`. None if it climbs out of the site.
fn resolve(from: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    if target.is_empty() || target.ends_with('/') || target == "." {
        parts.push("index.html");
    }
    Some(parts.join("/"))
}

#[derive(Clone, Copy)]
enum Reference {
    /// Followed by the reader: `<a>`, and a canonical `<link>`.
    Link,
    /// Fetched by the browser on its own: everything else.
    Resource,
}

/// Every `href` and `src` in the page, with what the browser does with it.
fn references(html: &str) -> Vec<(Reference, &str)> {
    let mut found = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        let (_, tag) = rest.split_at(start + 1);
        let (tag, after) = tag.split_at(tag.find('>').unwrap_or(tag.len()));
        rest = after;
        let name: String = tag
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if name.is_empty() {
            continue; // a closing tag, a comment, or the doctype
        }
        let name = name.to_ascii_lowercase();
        // A canonical link names the page's address; nothing loads it, so it
        // is checked as a link, which may point anywhere.
        let reference = if name == "a"
            || (name == "link" && attribute_values(tag, "rel").contains(&"canonical"))
        {
            Reference::Link
        } else {
            Reference::Resource
        };
        for attribute in ["href", "src"] {
            found.extend(
                attribute_values(tag, attribute)
                    .into_iter()
                    .map(|value| (reference, value)),
            );
        }
    }
    found
}

/// The values of `name="…"` where the name starts an attribute.
fn attribute_values<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let pattern = format!("{name}=\"");
    text.match_indices(pattern.as_str())
        .filter(|(at, _)| {
            text.get(..*at)
                .and_then(|before| before.chars().last())
                .is_some_and(char::is_whitespace)
        })
        .filter_map(|(at, _)| {
            text.get(at + pattern.len()..)?
                .split_once('"')
                .map(|(value, _)| value)
        })
        .collect()
}

/// The text of each `<style>` element in a page.
fn styles(html: &str) -> impl Iterator<Item = &str> {
    html.split("<style>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</style>").map(|(css, _)| css))
}

/// The first argument of each call a script makes that loads a URL: the
/// literal, or None where it is not one (a variable, a template with `${`),
/// which the check cannot follow.
fn script_urls(script: &str) -> Vec<Option<&str>> {
    const LOADS: [&str; 8] = [
        "new Worker(",
        "new SharedWorker(",
        "importScripts(",
        "fetch(",
        "import(",
        "new EventSource(",
        "new WebSocket(",
        "sendBeacon(",
    ];
    let mut found = Vec::new();
    for call in LOADS {
        for (at, _) in script.match_indices(call) {
            let named = script
                .get(..at)
                .and_then(|before| before.chars().last())
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
            if named && !call.starts_with("new ") {
                continue; // prefetch(, myimport(: another function
            }
            let argument = script
                .get(at + call.len()..)
                .unwrap_or_default()
                .trim_start();
            let literal = argument
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\'' | '`'));
            found.push(literal.and_then(|quote| {
                let (url, _) = argument.get(1..)?.split_once(quote)?;
                (!url.contains("${")).then_some(url)
            }));
        }
    }
    found
}

fn css_urls(css: &str) -> Vec<&str> {
    css.split("url(")
        .skip(1)
        .filter_map(|rest| {
            rest.split_once(')')
                .map(|(url, _)| url.trim().trim_matches(['"', '\'']))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES_URL: &str = "https://github.com/askrid/wye/blob/abc";

    fn problems(files: &Files) -> Vec<String> {
        super::problems(
            files,
            &Repo {
                files_url: FILES_URL,
                exists: &|file| file != "docs/gone.md",
            },
        )
    }

    fn site(pages: &[(&str, &str)], others: &[&str]) -> Files {
        let mut files: Files = pages
            .iter()
            .map(|(p, t)| ((*p).to_owned(), Some((*t).to_owned())))
            .collect();
        files.extend(others.iter().map(|p| ((*p).to_owned(), None)));
        files
    }

    #[test]
    fn a_self_contained_site_passes() {
        let files = site(
            &[
                (
                    "index.html",
                    "<link rel=\"canonical\" href=\"https://askrid.github.io/wye/\">\n<link rel=\"stylesheet\" href=\"assets/site.css\">\n<h2 id=\"costs\">Costs</h2>\n<a href=\"#costs\">x</a> <a href=\"./\">home</a> <a href=\"https://github.com/askrid/wye\">GitHub</a>",
                ),
                (
                    "assets/site.css",
                    "@font-face { src: url(\"fonts/o.woff2\") format(\"woff2\"); }",
                ),
            ],
            &["assets/fonts/o.woff2"],
        );
        assert_eq!(problems(&files), Vec::<String>::new());
    }

    #[test]
    fn outside_resources_broken_links_and_leftovers_fail() {
        let files = site(
            &[
                (
                    "index.html",
                    "<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2\">\n<img src=\"a.png\">\n<a href=\"#nowhere\">x</a> <a href=\"guide.html\">g</a> {{version}}",
                ),
                (
                    "assets/site.css",
                    "body { background: url(//cdn.example/x.png) }",
                ),
            ],
            &[],
        );
        assert_eq!(
            problems(&files),
            [
                "assets/site.css: loads //cdn.example/x.png from another origin",
                "index.html: an unexpanded directive",
                "index.html: loads https://fonts.googleapis.com/css2 from another origin",
                "index.html: a.png is not in the site",
                "index.html: the link #nowhere names a missing anchor",
                "index.html: the link guide.html leads nowhere",
            ]
        );
    }

    #[test]
    fn a_pages_own_styles_load_from_the_site_too() {
        let page = "<style>@font-face { src: url(\"assets/fonts/o.woff2\") }\
                    body { background: url(//cdn.example/x.png) }</style>";
        assert_eq!(
            problems(&site(&[("index.html", page)], &["assets/fonts/o.woff2"])),
            ["index.html: loads //cdn.example/x.png from another origin"]
        );
    }

    #[test]
    fn a_scripts_loads_come_from_the_site_too() {
        let mut files = site(
            &[("index.html", "<script src=\"assets/site.js\"></script>")],
            &["assets/strings.js"],
        );
        files.insert(
            "assets/site.js".into(),
            Some("new Worker(\"assets/strings.js\"); new Worker(\"assets/gone.js\"); fetch(\"https://t.example/p\"); fetch(url); prefetch(x)".into()),
        );
        assert_eq!(
            problems(&files),
            [
                "assets/site.js: assets/gone.js is not in the site",
                "assets/site.js: loads https://t.example/p from another origin",
                "assets/site.js: loads a URL the check cannot read",
            ]
        );
    }

    #[test]
    fn links_into_the_repository_name_its_files() {
        let page = format!(
            "<a href=\"{FILES_URL}/docs/guide.md#aliases\">g</a> <a href=\"{FILES_URL}/docs/gone.md\">x</a>"
        );
        assert_eq!(
            problems(&site(&[("index.html", &page)], &[])),
            [format!(
                "index.html: the link {FILES_URL}/docs/gone.md names no file in the repository"
            )]
        );
    }

    #[test]
    fn attributes_match_only_whole_names() {
        assert_eq!(
            attribute_values("<a data-href=\"x\" href=\"y\">", "href"),
            ["y"]
        );
    }

    #[test]
    fn paths_resolve_inside_the_site_only() {
        assert_eq!(resolve("index.html", "../x.css"), None);
        assert_eq!(
            resolve("docs/guide.html", "../assets/s.css").as_deref(),
            Some("assets/s.css")
        );
    }
}
