//! Alternating rounds: `alternate` runs several builds in turn on one
//! machine, as `gate` does for two commits.
//!
//! Clock speed, heat, and background load then weigh on every build alike,
//! while a stored result from another day would carry that day's machine
//! state (`docs/development.md`, "Commit gate"). Each side runs its own
//! `wye-bench` as a child process, so a commit that changes the benchmark
//! still measures the other build as that build's commit did.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use crate::compare;

/// One build in alternating rounds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Side {
    /// Names its result files: `<name>-<round>.json`, and `<name>.json` merged.
    pub(crate) name: String,
    pub(crate) bench: PathBuf,
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
    pub(crate) fn of_checkout(name: &str, dir: &Path) -> Self {
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
pub(crate) struct Alternate {
    pub(crate) sides: Vec<Side>,
    pub(crate) rounds: usize,
    pub(crate) repeat: usize,
    pub(crate) out: PathBuf,
}

impl Alternate {
    /// Parses `--side <spec>... [--rounds N] [--repeat R] --out <dir>`;
    /// rounds and repeat default to 3.
    pub(crate) fn parse(flags: &[&str]) -> Option<Self> {
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
    /// merge of its rounds ([`compare::median`]).
    pub(crate) fn run(&self) {
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
            compare::median(&merged.display().to_string(), &runs);
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use wye_testkit::TempDir;

    use super::*;

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
}
