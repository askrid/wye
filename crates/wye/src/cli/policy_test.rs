//! `wye policy-test`: checks committed sample requests against expected
//! decisions, so a config change that reroutes or refuses traffic fails CI.
//!
//! Each `<name>.json` in the directory is a request body, and
//! `<name>.expect.toml` states the decision:
//!
//! ```toml
//! decision = "refuse"        # or "forward"
//! status = 403               # optional
//! error = "permission_error" # optional
//! model = "claude-opus-5-5"  # optional: the model it runs on, after aliases
//! upstream = "team-a"        # optional: the route upstream it goes to
//! endpoint = "/v1/messages"  # optional, the default
//! session = "s-1"            # optional client session id
//!
//! [token]               # optional; claims taken as verified
//! subject = "alice"
//! models = ["claude-haiku-*"]
//! ```
//!
//! Tokens would expire in CI, so a case states the token's claims and
//! only the signature check is skipped; every later check is the server's.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use gateway_core::{config::Config, policy::ModelPattern, token::Identity};
use serde::Deserialize;

use super::{
    Env, Error, Exit,
    args::Parsed,
    explain::{CallerToken, explain, request_headers},
    files,
    paint::verdict,
};

/// A case's `<name>.expect.toml`, as the module docs describe it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    decision: Decision,
    status: Option<u16>,
    error: Option<String>,
    model: Option<String>,
    upstream: Option<String>,
    endpoint: Option<String>,
    session: Option<String>,
    token: Option<Claims>,
}

/// What the gateway does with a request.
#[derive(Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Forward,
    Refuse,
}

/// A case's `[token]`: the gateway token's claims, taken as verified.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    subject: String,
    models: Option<Vec<String>>,
    max_output_tokens: Option<u64>,
}

/// `wye policy-test`: every case in the directory, checked against the
/// config and printed under it; `Failed` when one fails.
pub fn run(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let (path, config) = files::load_config(parsed, env)?;
    // Key sets are files the gateway reads at startup, so CI checks them
    // too; secrets in the environment are `doctor`'s.
    files::named(
        &path,
        crate::startup::verifier(&config, files::config_dir(&path)),
    )?;
    let dir = match parsed.positional.first() {
        Some(dir) => PathBuf::from(dir),
        None => files::config_dir(&path).join(files::POLICY_TESTS_DIR),
    };
    let (report, passed) = policy_test(&config, &dir, super::unix_secs()?)?;
    Ok(super::checked(&super::about(&path, env, &report), passed))
}

/// Runs every case in `dir` at Unix time `now`; returns the report and
/// whether all cases passed.
pub fn policy_test(config: &Config, dir: &Path, now: u64) -> Result<(String, bool), String> {
    let cases = match files::json_files(dir) {
        Ok(cases) => cases,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    if cases.is_empty() {
        return Err(format!(
            "no policy tests in {}: write <name>.json and <name>.expect.toml there \
             (`wye init -c /tmp/wye/wye.toml` writes samples to copy)",
            dir.display()
        ));
    }
    let mut report = String::new();
    let mut failed = 0;
    for request in &cases {
        let name = request
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match run_case(config, request, now) {
            Ok(()) => report.push_str(&verdict(true, &name)),
            Err(why) => {
                failed += 1;
                report.push_str(&verdict(false, &format!("{name}: {why}")));
            }
        }
    }
    let _ = writeln!(report, "{} passed, {failed} failed", cases.len() - failed);
    Ok((report, failed == 0))
}

/// Checks the case of the request file `request` against `config`; the
/// error is what follows `FAIL <name>:` in the report.
fn run_case(config: &Config, request: &Path, now: u64) -> Result<(), String> {
    let cannot_read = |path: &Path, e| format!("cannot read {}: {e}", path.display());
    let expect_path = request.with_extension("expect.toml");
    let text = std::fs::read_to_string(&expect_path).map_err(|e| cannot_read(&expect_path, e))?;
    let expect: Expect = files::named(&expect_path, toml::from_str(&text))?;
    let body = std::fs::read(request).map_err(|e| cannot_read(request, e))?;
    let credential = match expect.token {
        None => CallerToken::None,
        Some(claims) => CallerToken::Trusted(Identity {
            subject: claims.subject.into(),
            key_id: "policy-test".into(),
            expires_at: u64::MAX,
            max_output_tokens: claims.max_output_tokens,
            models: claims
                .models
                .map(|list| {
                    list.iter()
                        .map(|p| {
                            ModelPattern::parse(p)
                                .ok_or_else(|| format!("token model {p:?} is not a pattern"))
                        })
                        .collect::<Result<Box<[_]>, _>>()
                })
                .transpose()?,
        }),
    };
    let path = expect.endpoint.as_deref().unwrap_or("/v1/messages");
    let headers = request_headers(path, expect.session.as_deref(), &[])?;
    let result = explain(config, path, credential, &headers, &body, now, &[]);
    let got = match &result.refused {
        Some(_) => Decision::Refuse,
        None => Decision::Forward,
    };
    let explain_tail = || result.steps.last().cloned().unwrap_or_default();
    if got != expect.decision {
        let expected = match expect.decision {
            Decision::Forward => "forward",
            Decision::Refuse => "refuse",
        };
        return Err(format!(
            "expected decision {expected}, got {}",
            explain_tail()
        ));
    }
    if let Some(model) = &expect.model
        && result.model.as_ref() != Some(model)
    {
        return Err(format!(
            "expected model {model}, got {}",
            result
                .model
                .as_deref()
                .unwrap_or("none (the body was not read)")
        ));
    }
    if let Some(upstream) = &expect.upstream
        && result.upstream.as_ref() != Some(upstream)
    {
        return Err(format!(
            "expected upstream {upstream}, got {}",
            result.upstream.as_deref().unwrap_or("none")
        ));
    }
    if let Some((status, kind, _)) = &result.refused {
        if let Some(want) = expect.status
            && want != *status
        {
            return Err(format!("expected status {want}, got {}", explain_tail()));
        }
        if let Some(want) = expect.error.as_deref()
            && want != *kind
        {
            return Err(format!("expected error {want}, got {}", explain_tail()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reports_passing_and_failing_cases() {
        let dir = wye_testkit::TempDir::new("policy");
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[[upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             provider_key = { env = \"K\" }\n[auth]\naudience = \"wye\"\nmodels = [\"claude-*\"]\n\
             signing_keys = [\"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"]",
        )
        .unwrap();
        let case = |name: &str, body: &str, expect: &str| {
            std::fs::write(dir.join(format!("{name}.json")), body).unwrap();
            std::fs::write(dir.join(format!("{name}.expect.toml")), expect).unwrap();
        };
        let haiku_only = "[token]\nsubject = \"ci\"\nmodels = [\"claude-haiku-*\"]";
        case(
            "a-haiku",
            r#"{"model":"claude-haiku-4-5"}"#,
            &format!("decision = \"forward\"\nmodel = \"claude-haiku-4-5\"\n{haiku_only}"),
        );
        case(
            "e-model",
            r#"{"model":"claude-haiku-4-5"}"#,
            &format!("decision = \"forward\"\nmodel = \"claude-opus-5-5\"\n{haiku_only}"),
        );
        case(
            "b-sonnet",
            r#"{"model":"claude-sonnet-4-5"}"#,
            &format!(
                "decision = \"refuse\"\nstatus = 403\nerror = \"permission_error\"\n{haiku_only}"
            ),
        );
        case(
            "c-anonymous",
            r#"{"model":"x"}"#,
            "decision = \"refuse\"\nstatus = 401",
        );
        case(
            "d-wrong",
            r#"{"model":"gpt-5"}"#,
            "decision = \"forward\"\n[token]\nsubject = \"ci\"",
        );
        let (report, passed) = policy_test(&config, &dir, 0).unwrap();
        assert!(!passed, "{report}");
        assert!(report.contains("ok    a-haiku"), "{report}");
        assert!(report.contains("ok    b-sonnet"), "{report}");
        assert!(report.contains("ok    c-anonymous"), "{report}");
        assert!(
            report.contains("FAIL  d-wrong: expected decision forward, got decision: refuse 403"),
            "{report}"
        );
        assert!(
            report.contains("FAIL  e-model: expected model claude-opus-5-5, got claude-haiku-4-5"),
            "{report}"
        );
        assert!(report.ends_with("3 passed, 2 failed\n"), "{report}");
    }

    #[test]
    fn checks_the_upstream_a_session_reaches() {
        let dir = wye_testkit::TempDir::new("policy-route");
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [[upstreams]]\nid = \"team-a\"\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"A\" }\n\
             [[upstreams]]\nid = \"team-b\"\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"B\" }\n",
        )
        .unwrap();
        let route = config.routes.claude.as_ref().unwrap();
        let id = |session: &str| route.upstreams()[route.pick(session.as_bytes())].id.clone();
        let (right, wrong) = if &*id("s-1") == "team-a" {
            ("team-a", "team-b")
        } else {
            ("team-b", "team-a")
        };
        for (name, upstream) in [("a-right", right), ("b-wrong", wrong)] {
            std::fs::write(dir.join(format!("{name}.json")), "{}").unwrap();
            std::fs::write(
                dir.join(format!("{name}.expect.toml")),
                format!("decision = \"forward\"\nsession = \"s-1\"\nupstream = \"{upstream}\""),
            )
            .unwrap();
        }
        let (report, passed) = policy_test(&config, &dir, 0).unwrap();
        assert!(!passed, "{report}");
        assert!(report.contains("ok    a-right"), "{report}");
        assert!(
            report.contains(&format!(
                "FAIL  b-wrong: expected upstream {wrong}, got {right}"
            )),
            "{report}"
        );
    }

    /// A refusal with another status or error than expected names both as
    /// written, not as Rust debug output.
    #[test]
    fn a_wrong_refusal_names_the_status_and_error_expected() {
        let dir = wye_testkit::TempDir::new("policy-refusal");
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[[upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
             provider_key = { env = \"K\" }\n[auth]\naudience = \"wye\"\n\
             signing_keys = [\"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"]",
        )
        .unwrap();
        // Without a gateway token, the request is refused with 401.
        for (name, expect) in [
            ("a-status", "status = 403"),
            ("b-error", "error = \"permission_error\""),
        ] {
            std::fs::write(dir.join(format!("{name}.json")), r#"{"model":"m"}"#).unwrap();
            std::fs::write(
                dir.join(format!("{name}.expect.toml")),
                format!("decision = \"refuse\"\n{expect}\n"),
            )
            .unwrap();
        }
        let (report, _) = policy_test(&config, &dir, 0).unwrap();
        for line in [
            "FAIL  a-status: expected status 403, got decision: refuse 401 ",
            "FAIL  b-error: expected error permission_error, got decision: refuse 401 ",
        ] {
            assert!(report.contains(line), "{line}\n---\n{report}");
        }
    }

    /// A case whose files cannot be read fails, naming the file.
    #[test]
    fn a_case_it_cannot_read_names_the_file() {
        let dir = wye_testkit::TempDir::new("policy-unreadable");
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [[upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"K\" }\n",
        )
        .unwrap();
        // A request that is a directory, and one without its expectation.
        std::fs::create_dir(dir.join("a-dir.json")).unwrap();
        std::fs::write(dir.join("a-dir.expect.toml"), "decision = \"forward\"\n").unwrap();
        std::fs::write(dir.join("b-alone.json"), "{}").unwrap();
        let (report, passed) = policy_test(&config, &dir, 0).unwrap();
        assert!(!passed, "{report}");
        for (name, file) in [("a-dir", "a-dir.json"), ("b-alone", "b-alone.expect.toml")] {
            let shown = dir.join(file).display().to_string();
            assert!(
                report.contains(&format!("FAIL  {name}: cannot read {shown}: ")),
                "{report}"
            );
        }
    }
}
