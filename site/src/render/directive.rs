//! A page directive as written, `{{name ARG|ARG}}`, parsed into what it asks
//! for, so expanding one is a match the compiler checks and a misspelled
//! name or a wrong count of arguments fails before any file is read.
//!
//! - `{{version}}`: the wye crate's version.
//! - `{{release}}`: `prerelease` while the version has a pre-release part,
//!   else `stable`.
//! - `{{commit}}`: the commit the page is built from: `GITHUB_SHA` in CI, else
//!   the checkout's `HEAD`.
//! - `{{license}}`, `{{repo}}`: the workspace's license and repository URL.
//! - `{{homepage}}`: the site's own address, `homepage` in site/Cargo.toml, so
//!   a move changes one line.
//! - `{{stylesheet}}`: site/pages/site.css minified (render/sources.rs),
//!   for a `<style>` element, so the page draws without waiting for a second file.
//! - `{{fence FILE|PREFIX|WORD}}`: the first fenced block in FILE whose first
//!   line starts with PREFIX, highlighted; WORD, if given, in the accent
//!   wherever it appears in the block's text.
//! - `{{field FILE|PREFIX|KEY}}`: KEY's value in that block, read as a JSON
//!   object; a dotted KEY reaches inside (`failovers.0.reason`).
//! - `{{opening FILE|PART}}`: FILE's opening `tagline`, its bold first
//!   paragraph without the closing period; the paragraph after it, rendered
//!   (`lede`) or as plain `text`.
//! - `{{section FILE|HEADING}}`: the Markdown under `## HEADING`, rendered.
//! - `{{item FILE|HEADING|LEAD}}`: the item of that section's list whose bold
//!   lead starts with LEAD, rendered.
//! - `{{rest FILE|HEADING}}`: every item of that list the page has not placed
//!   yet, so a new one is never dropped.
//! - `{{paragraph FILE|HEADING|START}}`: the paragraph of that section that
//!   starts with START, rendered.
//! - `{{numbers FILE|HEADING|FIRST}}`: that section's table whose first column
//!   is headed FIRST, as a table of figures.
//! - `{{relative FILE|HEADING|ROW}}`: a bar chart of the table row that
//!   starts with ROW, each gateway column's figure as a multiple of Wye's.
//! - `{{ratios FILE|HEADING|COLUMN}}`: a bar chart of that section's table
//!   column of multiples of Wye ("7×").

/// Every directive's name, so one with the wrong arguments says so.
const NAMES: [&str; 17] = [
    "version",
    "release",
    "commit",
    "license",
    "repo",
    "homepage",
    "stylesheet",
    "fence",
    "field",
    "opening",
    "section",
    "item",
    "rest",
    "paragraph",
    "numbers",
    "relative",
    "ratios",
];

/// A document and one of its `## ` headings.
#[derive(Clone, Copy)]
pub struct Heading<'a> {
    pub file: &'a str,
    pub heading: &'a str,
}

/// The part of a document's opening `{{opening}}` draws.
#[derive(Clone, Copy)]
pub enum Part {
    Tagline,
    Lede,
    Text,
}

/// What a directive asks for, its arguments borrowed from the page.
pub enum Directive<'a> {
    Version,
    Release,
    Commit,
    License,
    Repo,
    Homepage,
    Stylesheet,
    Fence {
        file: &'a str,
        prefix: &'a str,
        mark: Option<&'a str>,
    },
    Field {
        file: &'a str,
        prefix: &'a str,
        key: &'a str,
    },
    Opening {
        file: &'a str,
        part: Part,
    },
    Section(Heading<'a>),
    /// A list item, by the start of its bold lead.
    Item(Heading<'a>, &'a str),
    Rest(Heading<'a>),
    /// A paragraph, by how it starts.
    Paragraph(Heading<'a>, &'a str),
    /// A table, by its first column's heading.
    Numbers(Heading<'a>, &'a str),
    /// A chart of a table row, by how the row starts.
    Relative(Heading<'a>, &'a str),
    /// A chart of a table column, by its heading.
    Ratios(Heading<'a>, &'a str),
}

impl<'a> Directive<'a> {
    /// Parses a directive's text, its `{{ }}` and surrounding space removed.
    pub fn parse(text: &'a str) -> Result<Self, String> {
        let (name, args) = text.split_once(' ').unwrap_or((text, ""));
        let args: Vec<&str> = match args.trim() {
            "" => Vec::new(),
            args => args.split('|').map(str::trim).collect(),
        };
        let at = |file, heading| Heading { file, heading };
        Ok(match (name, args.as_slice()) {
            ("version", []) => Self::Version,
            ("release", []) => Self::Release,
            ("commit", []) => Self::Commit,
            ("license", []) => Self::License,
            ("repo", []) => Self::Repo,
            ("homepage", []) => Self::Homepage,
            ("stylesheet", []) => Self::Stylesheet,
            ("fence", [file, prefix]) => Self::Fence {
                file,
                prefix,
                mark: None,
            },
            ("fence", [file, prefix, mark]) => Self::Fence {
                file,
                prefix,
                mark: Some(mark),
            },
            ("field", [file, prefix, key]) => Self::Field { file, prefix, key },
            ("opening", [file, part]) => Self::Opening {
                file,
                part: match *part {
                    "tagline" => Part::Tagline,
                    "lede" => Part::Lede,
                    "text" => Part::Text,
                    other => return Err(format!("{other:?} is not tagline, text, or lede")),
                },
            },
            ("section", [file, heading]) => Self::Section(at(file, heading)),
            ("item", [file, heading, lead]) => Self::Item(at(file, heading), lead),
            ("rest", [file, heading]) => Self::Rest(at(file, heading)),
            ("paragraph", [file, heading, start]) => Self::Paragraph(at(file, heading), start),
            ("numbers", [file, heading, first]) => Self::Numbers(at(file, heading), first),
            ("relative", [file, heading, row]) => Self::Relative(at(file, heading), row),
            ("ratios", [file, heading, column]) => Self::Ratios(at(file, heading), column),
            (name, args) if NAMES.contains(&name) => {
                return Err(format!("{name} does not take {} arguments", args.len()));
            }
            _ => return Err("unknown directive".to_owned()),
        })
    }

    /// The document heading a directive draws from, if it names one.
    pub fn heading(&self) -> Option<Heading<'a>> {
        match *self {
            Self::Section(at)
            | Self::Item(at, _)
            | Self::Rest(at)
            | Self::Paragraph(at, _)
            | Self::Numbers(at, _)
            | Self::Relative(at, _)
            | Self::Ratios(at, _) => Some(at),
            _ => None,
        }
    }
}
