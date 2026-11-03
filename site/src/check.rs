//! Checks on the built site: everything a page loads comes from the site
//! itself (no request to another origin, so nothing tracks a reader), every
//! local link and anchor resolves, and no directive is left unexpanded.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Files by their path under `out/`, with the text of pages and stylesheets.
type Files = BTreeMap<String, Option<String>>;

pub fn check(out: &Path) -> Result<(), String> {
    let mut files = Files::new();
    collect(out, "", &mut files)?;
    let problems = problems(&files);
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
    let entries = fs::read_dir(dir).map_err(|e| crate::io_error(dir, e))?;
    for entry in entries {
        let path = entry.map_err(|e| crate::io_error(dir, e))?.path();
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
            let text = if relative.ends_with(".html") || relative.ends_with(".css") {
                Some(fs::read_to_string(&path).map_err(|e| crate::io_error(&path, e))?)
            } else {
                None
            };
            files.insert(relative, text);
        }
    }
    Ok(())
}

fn problems(files: &Files) -> Vec<String> {
    let mut found = Vec::new();
    for (path, text) in files {
        let Some(text) = text else { continue };
        if path.ends_with(".css") {
            for target in css_urls(text) {
                check_resource(files, path, target, &mut found);
            }
            continue;
        }
        if text.contains("{{") {
            found.push(format!("{path}: an unexpanded directive"));
        }
        for (tag, target) in references(text) {
            if tag == "a" {
                check_link(files, path, target, &mut found);
            } else {
                check_resource(files, path, target, &mut found);
            }
        }
    }
    found
}

/// A file the browser fetches on its own: it must be part of the site.
fn check_resource(files: &Files, from: &str, target: &str, found: &mut Vec<String>) {
    if target.starts_with("data:") {
        return;
    }
    if target.contains("://") || target.starts_with("//") {
        found.push(format!("{from}: loads {target} from another origin"));
        return;
    }
    let path = target.split(['#', '?']).next().unwrap_or(target);
    if resolve(from, path).is_none_or(|path| !files.contains_key(&path)) {
        found.push(format!("{from}: {target} is not in the site"));
    }
}

/// A link the reader follows: other sites are fine, local ones must resolve.
fn check_link(files: &Files, from: &str, target: &str, found: &mut Vec<String>) {
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
    if !ids(text).contains(anchor) {
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

/// Every `href` and `src` in the page, with the name of its tag.
fn references(html: &str) -> Vec<(String, &str)> {
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
        for attribute in ["href", "src"] {
            let mut tag_name = name.to_ascii_lowercase();
            // A canonical link names the page's address; nothing loads it,
            // so it is checked as a link, which may point anywhere.
            if tag_name == "link" && attribute_values(tag, "rel").contains(&"canonical") {
                tag_name = "a".to_owned();
            }
            found.extend(
                attribute_values(tag, attribute)
                    .into_iter()
                    .map(|value| (tag_name.clone(), value)),
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

fn ids(html: &str) -> BTreeSet<&str> {
    attribute_values(html, "id").into_iter().collect()
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
