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

use std::{fmt::Write as _, path::Path};

use gateway_core::{config::Config, policy::ModelPattern, token::Identity};
use serde::Deserialize;

use super::{
    explain::{CallerToken, explain, request_headers},
    paint::verdict,
};

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

#[derive(Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Forward,
    Refuse,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    subject: String,
    models: Option<Vec<String>>,
    max_output_tokens: Option<u64>,
}

/// Runs every case in `dir` at Unix time `now`; returns the report and
/// whether all cases passed.
pub fn policy_test(config: &Config, dir: &Path, now: u64) -> Result<(String, bool), String> {
    let mut cases: Vec<_> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    cases.sort();
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

fn run_case(config: &Config, request: &Path, now: u64) -> Result<(), String> {
    let expect_path = request.with_extension("expect.toml");
    let expect: Expect = toml::from_str(
        &std::fs::read_to_string(&expect_path)
            .map_err(|e| format!("{}: {e}", expect_path.display()))?,
    )
    .map_err(|e| format!("{}: {e}", expect_path.display()))?;
    let body = std::fs::read(request).map_err(|e| e.to_string())?;
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
        if expect.status.is_some_and(|s| s != *status) {
            return Err(format!(
                "expected status {:?}, got {}",
                expect.status,
                explain_tail()
            ));
        }
        if expect.error.as_deref().is_some_and(|e| e != *kind) {
            return Err(format!(
                "expected error {:?}, got {}",
                expect.error,
                explain_tail()
            ));
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
            "[server]\nlisten = \"127.0.0.1:8080\"\n[[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\n\
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
            "[server]\nlisten = \"127.0.0.1:8080\"\n[routes.claude]\n\
             [[routes.claude.upstreams]]\nid = \"team-a\"\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"A\" }\n\
             [[routes.claude.upstreams]]\nid = \"team-b\"\nbase_url = \"https://api.anthropic.com\"\nprovider_key = { env = \"B\" }\n",
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
}
