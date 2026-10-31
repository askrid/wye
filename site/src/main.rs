//! Builds the Wye website into `site/out/`.
//!
//! Each page in `site/pages/` is HTML with directives in double braces that
//! pull content from the repository, so the site never keeps a second copy
//! of a number, a version, or a block of real output (`render.rs` lists
//! them). The build fails on an unknown directive or a missing source, then
//! checks the output: every resource is local and every local link resolves.

mod check;
mod markdown;
mod render;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match build() {
        Ok(pages) => {
            println!("site: built {pages} page(s) into site/out");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("site: {error}");
            ExitCode::FAILURE
        }
    }
}

fn build() -> Result<usize, String> {
    let site = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = site.parent().ok_or("site/ has no parent directory")?;
    let sources = render::Sources::load(repo)?;

    let out = site.join("out");
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|e| io_error(&out, e))?;
    }
    copy_dir(&site.join("assets"), &out.join("assets"))?;

    let mut pages = 0;
    for page in sorted_entries(&site.join("pages"))? {
        if page.extension().is_none_or(|ext| ext != "html") {
            continue;
        }
        let name = page.file_name().ok_or("a page without a file name")?;
        let template = fs::read_to_string(&page).map_err(|e| io_error(&page, e))?;
        let html =
            render::render(&template, &sources).map_err(|e| format!("{}: {e}", page.display()))?;
        let target = out.join(name);
        fs::write(&target, html).map_err(|e| io_error(&target, e))?;
        pages += 1;
    }

    check::check(&out)?;
    Ok(pages)
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| io_error(to, e))?;
    for entry in sorted_entries(from)? {
        let name = entry.file_name().ok_or("an asset without a file name")?;
        let target = to.join(name);
        if entry.is_dir() {
            copy_dir(&entry, &target)?;
        } else {
            fs::copy(&entry, &target).map_err(|e| io_error(&entry, e))?;
        }
    }
    Ok(())
}

/// A directory's entries in name order, so builds are reproducible.
fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|e| io_error(dir, e))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_error(dir, e))?;
    entries.sort();
    Ok(entries)
}

fn io_error(path: &Path, error: std::io::Error) -> String {
    format!("{}: {error}", path.display())
}
