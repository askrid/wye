//! Where a user's files live, and how a command finds and reads its config.

use std::path::{Path, PathBuf};

use gateway_core::config::Config;

use super::args::Parsed;

/// The config, in the home directory.
pub const CONFIG_FILE: &str = "wye.toml";
/// The key `wye token issue` signs with, in the home directory.
pub const SIGNING_KEY_FILE: &str = "issuer.key";
/// Where `wye service` keeps usage records and `wye report` reads them.
pub const USAGE_LOG_FILE: &str = "usage.jsonl";
/// Where `wye service` keeps the gateway's messages.
pub const ERRORS_FILE: &str = "wye.log";

/// Where a user's files live: `$WYE_HOME`, else `~/.wye`.
pub fn home(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |name| env(name).filter(|v| !v.is_empty());
    set("WYE_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|home| Path::new(&home).join(".wye")))
}

/// The config a command reads: `--config`, else `$WYE_CONFIG`, else
/// `wye.toml` in the home directory.
///
/// Never one found in the working directory: a config decides where the
/// caller's credentials go, so the one in a repository just cloned must not
/// apply unless it is named.
pub fn find_config(
    flag: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(path) = flag
        .map(str::to_owned)
        .or_else(|| env("WYE_CONFIG").filter(|v| !v.is_empty()))
    {
        return Ok(path.into());
    }
    let home = home(env).map(|home| home.join(CONFIG_FILE));
    if let Some(path) = home.as_ref().filter(|path| exists(path)) {
        return Ok(path.clone());
    }
    let shown = home.map_or_else(|| "~/.wye/wye.toml".to_owned(), |p| tilde(&p, env));
    let here = Path::new(CONFIG_FILE);
    Err(if exists(here) {
        format!("no config at {shown}, and ./wye.toml is read only when named: -c wye.toml")
    } else {
        format!("no config at {shown}: write one with `wye init`, or pass -c <path>")
    })
}

/// `path` with the home directory shown as `~`.
pub fn tilde(path: &Path, env: &impl Fn(&str) -> Option<String>) -> String {
    match env("HOME").filter(|h| !h.is_empty()) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// The file `name` in the home directory; `flag` is how to do without one.
pub fn in_home(
    name: &str,
    flag: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    home(env)
        .map(|home| home.join(name))
        .ok_or_else(|| format!("no home directory: {flag}"))
}

/// The config a command was given or finds.
pub fn config_path(
    parsed: &Parsed<'_>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    find_config(parsed.value("config"), env, Path::exists)
}

/// The config in the file `path`; errors name the file, which may not be the
/// one the user had in mind.
pub fn load(path: &Path) -> Result<Config, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    named(path, Config::from_toml_str(&source))
}

/// `result`, its error naming the config it came from.
pub fn named<T>(path: &Path, result: Result<T, impl std::fmt::Display>) -> Result<T, String> {
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// The directory a config's relative paths start from.
pub fn config_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// The gateway token in the file `path`.
pub fn read_token(path: &str) -> Result<String, String> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {path}: {e}"))?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err(format!("{path}: the gateway token is empty"));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_config_is_found_in_order_and_never_in_the_working_directory() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let none = |_: &Path| false;
        let named = env(&[("HOME", "/u"), ("WYE_CONFIG", "/etc/wye.toml")]);
        assert_eq!(
            find_config(Some("x.toml"), &named, none).unwrap(),
            PathBuf::from("x.toml")
        );
        assert_eq!(
            find_config(None, &named, none).unwrap(),
            PathBuf::from("/etc/wye.toml")
        );
        let user = env(&[("HOME", "/u")]);
        // Both exist: the user's own, not the one in the working directory.
        assert_eq!(
            find_config(None, &user, |_| true).unwrap(),
            PathBuf::from("/u/.wye/wye.toml")
        );
        // Only the working directory's: not read, and the error says how.
        let here = find_config(None, &user, |p| p == Path::new("wye.toml")).unwrap_err();
        assert_eq!(
            here,
            "no config at ~/.wye/wye.toml, and ./wye.toml is read only when named: -c wye.toml"
        );
        assert_eq!(
            find_config(None, &user, none).unwrap_err(),
            "no config at ~/.wye/wye.toml: write one with `wye init`, or pass -c <path>"
        );
        let moved = env(&[("HOME", "/u"), ("WYE_HOME", "/data/wye")]);
        assert_eq!(home_of(&moved), "/data/wye");
        assert_eq!(
            find_config(None, &moved, |_| true).unwrap(),
            PathBuf::from("/data/wye/wye.toml")
        );
        assert_eq!(
            tilde(Path::new("/u/.wye/usage.jsonl"), &moved),
            "~/.wye/usage.jsonl"
        );
        assert_eq!(tilde(Path::new("/elsewhere"), &moved), "/elsewhere");
    }

    fn home_of(env: &impl Fn(&str) -> Option<String>) -> String {
        home(env).unwrap().display().to_string()
    }
}
