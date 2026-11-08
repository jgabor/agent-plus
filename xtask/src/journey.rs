//! `cargo xtask journey`: real use of `ap`, step by step, from a release build.

use crate::toml::{self, Value};
use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, lock, score};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const JOURNEY_FILE: &str = "harness/journey.toml";

struct Step {
    id: i64,
    needs: Vec<i64>,
    script: String,
    floor: Option<String>,
    spend: bool,
}

/// Prints each step and the count. It exits 0 whenever it could run, because
/// what fails a merge is a count lower than the base's.
pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let _hold = lock::shared(&root)?;
    let (passing, steps) = walk(&root, |line| println!("{line}"))?;
    println!("journey {passing} of {steps}");
    Ok(Verdict::Pass)
}

/// How many steps pass, and how many there are, for a caller whose stdout is
/// its result. The caller holds the activity lock.
pub fn count(root: &Path) -> Result<(usize, usize), Problem> {
    walk(root, |line| eprintln!("{line}"))
}

fn walk(root: &Path, report: impl Fn(&str)) -> Result<(usize, usize), Problem> {
    let steps = read_steps(root)?;
    let journey = root.join("target/journey");
    let bin = install(root, &journey)?;

    let mut passed = HashSet::new();
    for step in &steps {
        let word = if step.spend || !step.needs.iter().all(|need| passed.contains(need)) {
            "skip"
        } else if run_step(root, &journey, &bin, step)? {
            passed.insert(step.id);
            "pass"
        } else {
            "fail"
        };
        report(&format!("{word} step {}", step.id));
    }
    Ok((passed.len(), steps.len()))
}

fn read_steps(root: &Path) -> Result<Vec<Step>, Problem> {
    let text = fs::read_to_string(root.join(JOURNEY_FILE))
        .map_err(|e| Problem(format!("couldn't read {JOURNEY_FILE}: {e}")))?;
    let tables = toml::array_of_tables(&text, "step")
        .map_err(|e| Problem(format!("{JOURNEY_FILE}: {e}")))?;
    let mut steps = BTreeMap::new();
    for table in tables {
        let bad = |what: &str| Problem(format!("{JOURNEY_FILE}: a step {what}"));
        let id = table.int("id").ok_or_else(|| bad("has no integer `id`"))?;
        let step = Step {
            id,
            needs: table
                .ints("needs")
                .ok_or_else(|| bad("has no integer array `needs`"))?,
            script: table
                .str("script")
                .ok_or_else(|| bad("has no `script`"))?
                .to_string(),
            floor: match table.get("floor") {
                None => None,
                Some(Value::Str(floor)) => Some(floor.clone()),
                Some(_) => return Err(bad("has a `floor` that isn't a string")),
            },
            spend: match table.get("spend") {
                None => false,
                Some(Value::Bool(spend)) => *spend,
                Some(_) => return Err(bad("has a `spend` that isn't true or false")),
            },
        };
        if steps.insert(id, step).is_some() {
            return Err(Problem(format!("{JOURNEY_FILE}: step {id} appears twice")));
        }
    }
    for step in steps.values() {
        for need in &step.needs {
            if *need >= step.id || !steps.contains_key(need) {
                return Err(Problem(format!(
                    "{JOURNEY_FILE}: step {} needs step {need}, which isn't an earlier step",
                    step.id
                )));
            }
        }
    }
    Ok(steps.into_values().collect())
}

/// Builds `ap` for release, and copies it where the steps will find it.
fn install(root: &Path, journey: &Path) -> Result<PathBuf, Problem> {
    let target = root.join("target");
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked", "--package", "ap"])
        .arg("--target-dir")
        .arg(&target)
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(no_cargo)?;
    if !status.success() {
        return Err(Problem("`cargo build --release` failed".to_string()));
    }
    let problem = |e: io::Error| Problem(format!("couldn't prepare {}: {e}", journey.display()));
    if journey.exists() {
        fs::remove_dir_all(journey).map_err(problem)?;
    }
    let bin = journey.join("bin");
    fs::create_dir_all(&bin).map_err(problem)?;
    fs::copy(target.join("release/ap"), bin.join("ap")).map_err(problem)?;
    Ok(bin)
}

fn run_step(root: &Path, journey: &Path, bin: &Path, step: &Step) -> Result<bool, Problem> {
    if let Some(floor) = &step.floor
        && !floor_check_exists(root, floor)?
    {
        eprintln!("step {}: the check `{floor}` doesn't exist yet", step.id);
        return Ok(false);
    }
    let script = root.join(&step.script);
    if !script.exists() {
        eprintln!("step {}: no script at {}", step.id, step.script);
        return Ok(false);
    }
    let directory = journey.join(format!("step-{}", step.id));
    let home = directory.join("home");
    let work = directory.join("work");
    for path in [&home, &work] {
        fs::create_dir_all(path)
            .map_err(|e| Problem(format!("couldn't create {}: {e}", path.display())))?;
    }
    let status = Command::new("/bin/sh")
        .arg(&script)
        .current_dir(&work)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(|e| Problem(format!("couldn't run {}: {e}", step.script)))?;
    Ok(status.success())
}

/// Escape checks are tests in the package `t1`.
fn floor_check_exists(root: &Path, floor: &str) -> Result<bool, Problem> {
    for package in workspace::packages(root)? {
        if package.name != "t1" {
            continue;
        }
        for file in score::rust_files(&package.directory)? {
            let source = fs::read_to_string(&file)
                .map_err(|e| Problem(format!("couldn't read {}: {e}", file.display())))?;
            if score::test_functions(&source)
                .iter()
                .any(|name| name == floor)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
