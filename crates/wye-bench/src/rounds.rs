//! Alternating rounds: `alternate` runs several builds in turn on one
//! machine, and `gate` judges one commit against another from such rounds.
//!
//! Clock speed, heat, and background load then weigh on every build alike,
//! while a stored result from another day would carry that day's machine
//! state (`docs/development.md`, "Commit gate"). Each side runs its own
//! `wye-bench` as a child process, so a commit that changes the benchmark
//! still measures the other build as that build's commit did.

use std::{
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Instant,
};

use crate::{gate, stats};

/// One build in alternating rounds.
#[derive(Debug, PartialEq, Eq)]
pub struct Side {
    /// Names its result files: `<name>-<round>.json`, and `<name>.json` merged.
    name: String,
    bench: PathBuf,
    wye: PathBuf,
    /// A `--features alloc-stats` build, for the `alloc_` metrics.
    alloc: Option<PathBuf>,
}

impl Side {
    /// Parses `<name>=<wye-bench>,<wye>[,<alloc-stats wye>]`.
    fn parse(spec: &str) -> Option<Self> {
        let (name, paths) = spec.split_once('=')?;
        let named = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let paths: Vec<&str> = paths.split(',').collect();
        if !named || paths.iter().any(|p| p.is_empty()) {
            return None;
        }
        match paths.as_slice() {
            [bench, wye] | [bench, wye, _] => Some(Self {
                name: name.to_owned(),
                bench: bench.into(),
                wye: wye.into(),
                alloc: paths.get(2).map(PathBuf::from),
            }),
            _ => None,
        }
    }

    /// The builds of the checkout at `dir`, as `scripts/bench.sh` makes
    /// them: `target/release/{wye-bench,wye}` and the allocation-counting
    /// `target/alloc/release/wye`.
    fn of_checkout(name: &str, dir: &Path) -> Self {
        Self {
            name: name.to_owned(),
            bench: dir.join("target/release/wye-bench"),
            wye: dir.join("target/release/wye"),
            alloc: Some(dir.join("target/alloc/release/wye")),
        }
    }

    /// Runs this side's `wye-bench run` once, writing `file`.
    fn run(&self, file: &Path, repeat: usize, label: &str) {
        let start = Instant::now();
        let mut command = Command::new(&self.bench);
        command
            .arg("run")
            .arg("--wye")
            .arg(&self.wye)
            .arg("--out")
            .arg(file)
            .arg("--repeat")
            .arg(repeat.to_string());
        if let Some(alloc) = &self.alloc {
            command.arg("--wye-alloc").arg(alloc);
        }
        // The metric table goes to the result file; the phase durations on
        // stderr stay visible.
        let status = command
            .stdout(Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", self.bench.display()));
        assert!(status.success(), "{label}: wye-bench run failed ({status})");
        println!("    {label}: {}s", start.elapsed().as_secs());
    }
}

/// `wye-bench alternate`: which builds, how many rounds, and where results go.
#[derive(Debug, PartialEq, Eq)]
pub struct Alternate {
    sides: Vec<Side>,
    rounds: usize,
    repeat: usize,
    out: PathBuf,
}

impl Alternate {
    /// Parses `--side <spec>... [--rounds N] [--repeat R] --out <dir>`;
    /// rounds and repeat default to 3.
    pub fn parse(flags: &[&str]) -> Option<Self> {
        let mut plan = Self {
            sides: Vec::new(),
            rounds: 3,
            repeat: 3,
            out: PathBuf::new(),
        };
        let count = |value: &str| value.parse().ok().filter(|&n| n > 0);
        let mut rest = flags;
        while let [flag, value, tail @ ..] = rest {
            match *flag {
                "--side" => plan.sides.push(Side::parse(value)?),
                "--rounds" => plan.rounds = count(value)?,
                "--repeat" => plan.repeat = count(value)?,
                "--out" => plan.out = value.into(),
                _ => return None,
            }
            rest = tail;
        }
        let names: BTreeSet<&str> = plan.sides.iter().map(|s| s.name.as_str()).collect();
        let valid = rest.is_empty()
            && !plan.sides.is_empty()
            && names.len() == plan.sides.len()
            && !plan.out.as_os_str().is_empty();
        valid.then_some(plan)
    }

    /// Runs each side's `wye-bench run --repeat <repeat>` once per round,
    /// reversing the order every other round so a trend within the run
    /// weighs on every side alike, then writes `<out>/<name>.json`, the
    /// merge of its rounds ([`gate::median`]).
    pub fn run(&self) {
        let out = &self.out;
        std::fs::create_dir_all(out).unwrap();
        for side in &self.sides {
            remove_rounds(out, &side.name);
        }
        let mut files = vec![Vec::new(); self.sides.len()];
        for round in 1..=self.rounds {
            for i in order(self.sides.len(), round) {
                let side = &self.sides[i];
                let file = out.join(format!("{}-{round}.json", side.name));
                let label = format!("{} {round}/{}", side.name, self.rounds);
                side.run(&file, self.repeat, &label);
                files[i].push(file.display().to_string());
            }
        }
        for (side, files) in self.sides.iter().zip(&files) {
            let runs: Vec<&str> = files.iter().map(String::as_str).collect();
            let merged = out.join(format!("{}.json", side.name));
            gate::median(&merged.display().to_string(), &runs);
        }
    }
}

/// The sides of `round` (from 1) in running order: as given in odd rounds,
/// reversed in even ones.
fn order(sides: usize, round: usize) -> Vec<usize> {
    if round % 2 == 1 {
        (0..sides).collect()
    } else {
        (0..sides).rev().collect()
    }
}

/// Removes `<name>-<round>.json` files an earlier run left in `out`, so the
/// directory holds only this run's rounds.
fn remove_rounds(out: &Path, name: &str) {
    for entry in std::fs::read_dir(out).unwrap().flatten() {
        let file_name = entry.file_name();
        let round = file_name.to_str().and_then(|f| {
            f.strip_prefix(name)?
                .strip_prefix('-')?
                .strip_suffix(".json")
        });
        if round.is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())) {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
}

/// `wye-bench gate`: the commit at `current` against the one at `reference`
/// (two checkouts, built as [`Side::of_checkout`] says), three alternating
/// rounds each. The merged results are compared under both commits' limits,
/// so a commit cannot pass by loosening the gate it is judged by. On a
/// regression the rounds run again, and the gate fails only when the same
/// metric is past its limit again: load pushes some metric past its limit now
/// and then, a different one each time, while a real regression repeats. A
/// failed request or stream is never noise: it fails at once. `flags` go to
/// `wye-bench compare`; `<out>/compare.txt` holds the last comparison.
pub fn gate(reference: &Path, current: &Path, out: &Path, flags: &[&str]) -> ExitCode {
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
    use std::os::unix::fs::PermissionsExt;

    use wye_testkit::TempDir;

    use super::*;

    fn comparison(passed: bool, failed: &[&str]) -> Comparison {
        Comparison {
            passed,
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn a_side_names_its_wye_bench_wye_and_optional_alloc_build() {
        let side = Side::parse("a=bench/wye-bench,bin/wye,alloc/wye").unwrap();
        assert_eq!(side.name, "a");
        assert_eq!(side.bench, PathBuf::from("bench/wye-bench"));
        assert_eq!(side.wye, PathBuf::from("bin/wye"));
        assert_eq!(side.alloc, Some(PathBuf::from("alloc/wye")));
        assert_eq!(Side::parse("b=x,y").unwrap().alloc, None);
        for bad in ["x,y", "=x,y", "a=x", "a=x,", "a=x,y,z,w", "a b=x,y", "a=,y"] {
            assert_eq!(Side::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn alternate_needs_distinct_sides_and_an_out_directory() {
        let plan = Alternate::parse(&["--side", "a=x,y", "--side", "b=x,z", "--out", "o"]).unwrap();
        assert_eq!((plan.rounds, plan.repeat, plan.sides.len()), (3, 3, 2));
        let plan = Alternate::parse(&[
            "--side", "a=x,y", "--rounds", "5", "--repeat", "1", "--out", "o",
        ]);
        assert_eq!(plan.map(|p| (p.rounds, p.repeat)), Some((5, 1)));
        for bad in [
            &["--out", "o"][..],
            &["--side", "a=x,y"],
            &["--side", "a=x,y", "--side", "a=x,z", "--out", "o"],
            &["--side", "a=x,y", "--rounds", "0", "--out", "o"],
            &["--side", "a=x,y", "--out", "o", "--extra"],
        ] {
            assert_eq!(Alternate::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn rounds_alternate_which_side_goes_first() {
        assert_eq!(order(2, 1), [0, 1]);
        assert_eq!(order(2, 2), [1, 0]);
        assert_eq!(order(2, 3), [0, 1]);
        assert_eq!(order(3, 2), [2, 1, 0]);
    }

    /// Runs real child processes: a stand-in `wye-bench` that logs which
    /// side ran and reports that side's round count as its metric.
    #[test]
    fn alternate_runs_each_side_per_round_and_merges_its_rounds() {
        let dir = TempDir::new("bench-alternate");
        let log = dir.join("order.log");
        let bench = dir.join("fake-wye-bench");
        // Arguments: run --wye <wye> --out <file> --repeat <n>.
        let script = format!(
            "#!/bin/sh\necho \"$3\" >> '{log}'\n\
             n=$(grep -c \"^$3\\$\" '{log}')\n\
             printf '{{\"machine\":{{}},\"repeat\":1,\"metrics\":{{\"m\":%s}}}}' \"$n\" > \"$5\"\n",
            log = log.display()
        );
        std::fs::write(&bench, script).unwrap();
        std::fs::set_permissions(&bench, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a-7.json"), "stale").unwrap();
        let bench = bench.display().to_string();
        let (a, b) = (format!("a={bench},wye-a"), format!("b={bench},wye-b"));
        let out_flag = out.display().to_string();
        Alternate::parse(&[
            "--side", &a, "--side", &b, "--out", &out_flag, "--repeat", "1",
        ])
        .unwrap()
        .run();
        let ran = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            ran.lines().collect::<Vec<_>>(),
            ["wye-a", "wye-b", "wye-b", "wye-a", "wye-a", "wye-b"]
        );
        assert!(!out.join("a-7.json").exists());
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("b.json")).unwrap()).unwrap();
        // Rounds 1, 2, 3: the median is 2.
        assert_eq!(merged["metrics"]["m"], 2.0);
        assert_eq!(merged["repeat"], 3);
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
