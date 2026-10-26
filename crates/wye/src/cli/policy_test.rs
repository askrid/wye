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
//! path = "/v1/messages"      # optional, the default
//! session = "s-1"            # optional Claude Code session id
//!
//! [credential]               # optional; claims taken as verified
//! subject = "alice"
//! models = ["claude-haiku-*"]
//! ```
//!
//! Tokens would expire in CI, so a case states the credential's claims and
//! only the signature check is skipped; every later check is the server's.

use std::{fmt::Write as _, path::Path};

use gateway_core::{config::Config, credential::Identity, policy::ModelPattern};
use serde::Deserialize;

use super::explain::{Credential, explain};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    decision: Decision,
    status: Option<u16>,
    error: Option<String>,
    model: Option<String>,
    path: Option<String>,
    session: Option<String>,
    credential: Option<Claims>,
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
    let mut cases: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    cases.sort();
    if cases.is_empty() {
        return Err(format!("{}: no *.json requests", dir.display()));
    }
    let mut report = String::new();
    let mut failed = 0;
    for request in &cases {
        let name = request
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match run_case(config, request, now) {
            Ok(()) => {
                let _ = writeln!(report, "ok    {name}");
            }
            Err(why) => {
                failed += 1;
                let _ = writeln!(report, "FAIL  {name}: {why}");
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
    let credential = match expect.credential {
        None => Credential::None,
        Some(claims) => Credential::Trusted(Identity {
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
                                .ok_or_else(|| format!("credential model {p:?} is not a pattern"))
                        })
                        .collect::<Result<Box<[_]>, _>>()
                })
                .transpose()?,
        }),
    };
    let path = expect.path.as_deref().unwrap_or("/v1/messages");
    let result = explain(
        config,
        path,
        credential,
        expect.session.as_deref(),
        &body,
        now,
        &[],
    );
    let got = match &result.refused {
        Some(_) => Decision::Refuse,
        None => Decision::Forward,
    };
    let explain_tail = || result.steps.last().cloned().unwrap_or_default();
    if got != expect.decision {
        return Err(format!(
            "expected {:?}, got {}",
            expect.decision,
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
        let dir = std::env::temp_dir().join(format!("wye-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\n\
             api_key_env = \"K\"\n[auth]\nissuer = \"wye\"\naudience = \"wye\"\nmodels = [\"claude-*\"]\n\
             [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"",
        )
        .unwrap();
        let case = |name: &str, body: &str, expect: &str| {
            std::fs::write(dir.join(format!("{name}.json")), body).unwrap();
            std::fs::write(dir.join(format!("{name}.expect.toml")), expect).unwrap();
        };
        let haiku_only = "[credential]\nsubject = \"ci\"\nmodels = [\"claude-haiku-*\"]";
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
            "decision = \"forward\"\n[credential]\nsubject = \"ci\"",
        );
        let (report, passed) = policy_test(&config, &dir, 0).unwrap();
        assert!(!passed, "{report}");
        assert!(report.contains("ok    a-haiku"), "{report}");
        assert!(report.contains("ok    b-sonnet"), "{report}");
        assert!(report.contains("ok    c-anonymous"), "{report}");
        assert!(
            report.contains("FAIL  d-wrong: expected Forward, got decision: refuse 403"),
            "{report}"
        );
        assert!(
            report.contains("FAIL  e-model: expected model claude-opus-5-5, got claude-haiku-4-5"),
            "{report}"
        );
        assert!(report.ends_with("3 passed, 2 failed\n"), "{report}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
