//! Reading the repository: everything a page may draw from, once per build.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use super::{DOCUMENTS, Sources};

impl Sources {
    /// Reads every source a directive may use from the repository at `repo`.
    pub fn load(repo: &Path) -> Result<Self, String> {
        let read = |relative: &str| {
            let path = repo.join(relative);
            fs::read_to_string(&path).map_err(|e| crate::io_error(&path, e))
        };
        let crate_manifest = read("crates/wye/Cargo.toml")?;
        let workspace_manifest = read("Cargo.toml")?;
        let site_manifest = read("site/Cargo.toml")?;
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
            homepage: manifest_field(&site_manifest, "homepage")
                .ok_or("site/Cargo.toml: no homepage")?,
            machine: machine(&baseline).ok_or("bench/baseline.json: no machine cpu and os")?,
            metrics,
            documents,
        })
    }
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
