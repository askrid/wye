//! `wye-bench gate`: one commit judged against another from alternating
//! rounds, under both commits' limits, with one rerun to tell a regression
//! from noise.

use std::{
    collections::BTreeSet,
    fs::File,
    path::Path,
    process::{Command, ExitCode, Stdio},
};

use crate::{
    rounds::{Alternate, Side},
    stats,
};

/// `wye-bench gate`: the commit at `current` against the one at `reference`
/// (two checkouts, built as [`Side::of_checkout`] says), three alternating
/// rounds each. The merged results are compared under both commits' limits,
/// so a commit cannot pass by loosening the gate it is judged by. On a
/// regression the rounds run again, and the gate fails only when the same
/// metric is past its limit again: load pushes some metric past its limit now
/// and then, a different one each time, while a real regression repeats. A
/// failed request or stream is never noise: it fails at once. `flags` go to
/// `wye-bench compare`; `<out>/compare.txt` holds the last comparison.
pub(crate) fn gate(reference: &Path, current: &Path, out: &Path, flags: &[&str]) -> ExitCode {
    let plan = Alternate {
        sides: vec![
            Side::of_checkout("reference", reference),
            Side::of_checkout("current", current),
        ],
        rounds: 3,
        repeat: 1,
        out: out.to_owned(),
    };
    plan.run();
    println!("==> compare");
    let first = compare(&plan, flags);
    let rerun = match decide(&first, None) {
        Verdict::Rerun => {
            println!("==> regression reported; re-running the benchmark to rule out noise");
            plan.run();
            println!("==> compare");
            compare(&plan, flags)
        }
        verdict => return conclude(verdict),
    };
    conclude(decide(&first, Some(&rerun)))
}

fn conclude(verdict: Verdict) -> ExitCode {
    match verdict {
        Verdict::Pass => ExitCode::SUCCESS,
        Verdict::Noise => {
            println!("==> no metric failed both runs: noise, not a regression");
            ExitCode::SUCCESS
        }
        Verdict::Fail(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
        Verdict::Rerun => unreachable!("a rerun is decided only after the first comparison"),
    }
}

/// One comparison of `reference.json` and `current.json` under both
/// commits' limits.
#[derive(Debug)]
struct Comparison {
    /// Whether it passed under both.
    passed: bool,
    /// The metrics past either commit's limit, or missing.
    failed: BTreeSet<String>,
}

/// Compares with the current commit's `wye-bench` (printed, and written to
/// `compare.txt`), then with the reference's for its own limits: only what
/// those add is printed. A reference too old for `flags` fails without
/// naming a metric, and its limits are skipped.
fn compare(plan: &Alternate, flags: &[&str]) -> Comparison {
    let [reference, current] = [&plan.sides[0], &plan.sides[1]];
    let results = [
        plan.out.join(format!("{}.json", reference.name)),
        plan.out.join(format!("{}.json", current.name)),
    ];
    let output = Command::new(&current.bench)
        .arg("compare")
        .args(&results)
        .args(flags)
        .stderr(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| panic!("cannot start {}: {e}", current.bench.display()));
    let table = String::from_utf8_lossy(&output.stdout);
    print!("{table}");
    std::fs::write(plan.out.join("compare.txt"), table.as_bytes()).unwrap();
    let mut passed = output.status.success();
    let mut failed = failed_metrics(&table);

    let log = plan.out.join("compare-reference-limits.txt");
    let file = File::create(&log).unwrap();
    let ran = Command::new(&reference.bench)
        .arg("compare")
        .args(&results)
        .args(flags)
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .status();
    let stricter = failed_metrics(&std::fs::read_to_string(&log).unwrap());
    if !ran.is_ok_and(|status| status.success()) {
        if stricter.is_empty() {
            eprintln!("note: the reference's wye-bench could not compare; its limits are skipped");
        } else {
            println!("past the reference's own limits: {}", join(&stricter));
            passed = false;
        }
    }
    failed.extend(stricter);
    Comparison { passed, failed }
}

/// The metrics a comparison table marks `REGRESSED` or `missing` (in its
/// last column), read from its text, so the table of another commit's
/// `wye-bench` reads alike.
fn failed_metrics(table: &str) -> BTreeSet<String> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [name, .., "REGRESSED" | "missing"] | [name @ ("REGRESSED" | "missing")] => {
                    Some((*name).to_owned())
                }
                _ => None,
            }
        })
        .collect()
}

/// What the gate does after a comparison.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// Both comparisons failed, but on different metrics.
    Noise,
    /// The first comparison failed on metrics a rerun may clear.
    Rerun,
    Fail(String),
}

/// The gate's rule, given the first comparison and, once it ran, the rerun's.
fn decide(first: &Comparison, rerun: Option<&Comparison>) -> Verdict {
    let last = rerun.unwrap_or(first);
    if last.passed {
        return Verdict::Pass;
    }
    // A panic or a bad report: there is no run to rerun against.
    if last.failed.is_empty() {
        return Verdict::Fail("the comparison failed without naming a metric".to_owned());
    }
    let failures: BTreeSet<String> = last
        .failed
        .iter()
        .filter(|name| stats::counts_failures(name))
        .cloned()
        .collect();
    if !failures.is_empty() {
        return Verdict::Fail(format!("requests or streams failed: {}", join(&failures)));
    }
    let Some(rerun) = rerun else {
        return Verdict::Rerun;
    };
    let both: BTreeSet<String> = first.failed.intersection(&rerun.failed).cloned().collect();
    if both.is_empty() {
        Verdict::Noise
    } else {
        Verdict::Fail(format!("past the limit in both runs: {}", join(&both)))
    }
}

fn join(names: &BTreeSet<String>) -> String {
    names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comparison(passed: bool, failed: &[&str]) -> Comparison {
        Comparison {
            passed,
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn failed_metrics_are_the_rows_marked_regressed_or_missing() {
        let table = "metric  reference  current  limit  result\n\
                     unary_added_p50_us  100.00  900.00  160.00  REGRESSED\n\
                     sse_failed_streams  missing\n\
                     tls_unary_added_p50_us  100.00  120.00  350.00  ok\n\
                     auth_unary_added_p50_us  absent\n\
                     otlp_unary_added_p50_us  100.00  900.00  160.00  report\n\
                     --only alloc_x: matches no gated metric\n";
        let failed: Vec<String> = failed_metrics(table).into_iter().collect();
        assert_eq!(failed, ["sse_failed_streams", "unary_added_p50_us"]);
    }

    #[test]
    fn a_passing_comparison_passes_at_once() {
        assert_eq!(decide(&comparison(true, &[]), None), Verdict::Pass);
    }

    #[test]
    fn a_regression_reruns_and_fails_only_when_the_same_metric_fails_again() {
        let first = comparison(false, &["unary_added_p50_us", "tls_upload_added_p50_us"]);
        assert_eq!(decide(&first, None), Verdict::Rerun);
        let cleared = comparison(true, &[]);
        assert_eq!(decide(&first, Some(&cleared)), Verdict::Pass);
        let other = comparison(false, &["upload_cpu_ms_per_mb"]);
        assert_eq!(decide(&first, Some(&other)), Verdict::Noise);
        let same = comparison(false, &["upload_cpu_ms_per_mb", "unary_added_p50_us"]);
        assert_eq!(
            decide(&first, Some(&same)),
            Verdict::Fail("past the limit in both runs: unary_added_p50_us".to_owned())
        );
    }

    #[test]
    fn failed_requests_or_streams_are_never_noise() {
        let failing = comparison(false, &["unary_added_p50_us", "tls_paced_failed_streams"]);
        let reason = "requests or streams failed: tls_paced_failed_streams".to_owned();
        assert_eq!(decide(&failing, None), Verdict::Fail(reason.clone()));
        // In the rerun too, even when the first run failed on something else.
        let first = comparison(false, &["unary_added_p50_us"]);
        assert_eq!(decide(&first, Some(&failing)), Verdict::Fail(reason));
    }

    #[test]
    fn a_failed_comparison_that_names_no_metric_fails() {
        let unnamed = comparison(false, &[]);
        let reason = "the comparison failed without naming a metric".to_owned();
        assert_eq!(decide(&unnamed, None), Verdict::Fail(reason.clone()));
        let first = comparison(false, &["unary_added_p50_us"]);
        assert_eq!(decide(&first, Some(&unnamed)), Verdict::Fail(reason));
    }
}
