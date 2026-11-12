//! `cargo xtask journey`: real use of `ap`, step by step, from a release build.

use crate::toml::{self, Value};
use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, lock, score};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const JOURNEY_FILE: &str = "harness/journey.toml";
const STATIC_TARGET: &str = "x86_64-unknown-linux-musl";

struct Step {
    id: i64,
    name: String,
    milestone: String,
    needs: Vec<i64>,
    script: String,
    floor: Option<String>,
    spend: bool,
}

/// A named group of steps, and the `ap` commands it delivers.
pub struct Milestone {
    pub name: String,
    /// What a user can do when it is done, in plain words.
    pub summary: String,
    pub commands: Vec<String>,
}

/// What happened to one step: `pass`, `fail`, or `skip`.
pub struct Outcome {
    pub id: i64,
    pub name: String,
    pub milestone: String,
    pub state: &'static str,
    /// The step spends money, or needs a step that does, so the merge sequence
    /// leaves it out.
    pub spends: bool,
}

/// Prints each step and the count. It exits 0 whenever it could run, because
/// what fails a merge is a count lower than the base's.
pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let _hold = lock::shared(&root)?;
    let (passing, steps) = tally(&walk(&root, None, |line| println!("{line}"))?);
    println!("journey {passing} of {steps}");
    Ok(Verdict::Pass)
}

/// How many steps pass, and how many there are, for a caller whose stdout is
/// its result. The caller holds the activity lock.
pub fn count(root: &Path) -> Result<(usize, usize), Problem> {
    Ok(tally(&outcomes(root, None)?))
}

/// Each step's result in journey order, for a caller whose stdout is its
/// result. The caller holds the activity lock. With a model provider's key,
/// the steps that spend money run too, and only they see it, as
/// `ANTHROPIC_API_KEY`.
pub fn outcomes(root: &Path, key: Option<&str>) -> Result<Vec<Outcome>, Problem> {
    walk(root, key, |line| eprintln!("{line}"))
}

/// The milestones, in roadmap order.
pub fn milestones(root: &Path) -> Result<Vec<Milestone>, Problem> {
    Ok(read(root)?.0)
}

/// How many steps spend money, or need a step that does. None of them run.
pub fn left_out(root: &Path) -> Result<usize, Problem> {
    let mut out = HashSet::new();
    for step in read_steps(root)? {
        if step.spend || step.needs.iter().any(|need| out.contains(need)) {
            out.insert(step.id);
        }
    }
    Ok(out.len())
}

/// How many of these steps pass, and how many there are.
pub fn tally(outcomes: &[Outcome]) -> (usize, usize) {
    let passing = outcomes.iter().filter(|o| o.state == "pass").count();
    (passing, outcomes.len())
}

fn walk(root: &Path, key: Option<&str>, report: impl Fn(&str)) -> Result<Vec<Outcome>, Problem> {
    let steps = read_steps(root)?;
    let journey = root.join("target/journey");
    let bin = install(root, &journey)?;

    let mut passed = HashSet::new();
    let mut spending = HashSet::new();
    let mut outcomes = Vec::new();
    for step in &steps {
        let spends = step.spend || step.needs.iter().any(|need| spending.contains(need));
        if spends {
            spending.insert(step.id);
        }
        let given = if step.spend { key } else { None };
        let word = if (step.spend && key.is_none())
            || !step.needs.iter().all(|need| passed.contains(need))
        {
            "skip"
        } else if run_step(root, &journey, &bin, step, given)? {
            passed.insert(step.id);
            "pass"
        } else {
            "fail"
        };
        report(&format!("{word} step {}", step.id));
        outcomes.push(Outcome {
            id: step.id,
            name: step.name.clone(),
            milestone: step.milestone.clone(),
            state: word,
            spends,
        });
    }
    Ok(outcomes)
}

fn read_steps(root: &Path) -> Result<Vec<Step>, Problem> {
    Ok(read(root)?.1)
}

/// The milestones in the file's order, and the steps in the order of their ids.
fn read(root: &Path) -> Result<(Vec<Milestone>, Vec<Step>), Problem> {
    let text = fs::read_to_string(root.join(JOURNEY_FILE))
        .map_err(|e| Problem(format!("couldn't read {JOURNEY_FILE}: {e}")))?;
    let tables = toml::arrays_of_tables(&text, &["milestone", "step"])
        .map_err(|e| Problem(format!("{JOURNEY_FILE}: {e}")))?;
    let mut milestones: Vec<Milestone> = Vec::new();
    let mut commands = HashSet::new();
    let mut steps = BTreeMap::new();
    for (kind, table) in tables {
        if kind == "milestone" {
            milestones.push(read_milestone(&table, &milestones, &mut commands)?);
            continue;
        }
        let bad = |what: &str| Problem(format!("{JOURNEY_FILE}: a step {what}"));
        let id = table.int("id").ok_or_else(|| bad("has no integer `id`"))?;
        let step = Step {
            id,
            name: table
                .str("name")
                .ok_or_else(|| bad("has no `name`"))?
                .to_string(),
            milestone: table
                .str("milestone")
                .ok_or_else(|| Problem(format!("{JOURNEY_FILE}: step {id} has no `milestone`")))?
                .to_string(),
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
        if !milestones.iter().any(|known| known.name == step.milestone) {
            return Err(Problem(format!(
                "{JOURNEY_FILE}: step {} is in the milestone `{}`, which has no `[[milestone]]` table",
                step.id, step.milestone
            )));
        }
    }
    for Milestone { name, .. } in &milestones {
        if !steps.values().any(|step| step.milestone == *name) {
            return Err(Problem(format!(
                "{JOURNEY_FILE}: the milestone `{name}` has no step"
            )));
        }
    }
    Ok((milestones, steps.into_values().collect()))
}

/// No milestone repeats a name, no command is listed twice in the file, and
/// every milestone says what it is for.
fn read_milestone(
    table: &toml::Table,
    milestones: &[Milestone],
    commands: &mut HashSet<String>,
) -> Result<Milestone, Problem> {
    let bad = |what: String| Problem(format!("{JOURNEY_FILE}: {what}"));
    let name = table
        .str("name")
        .ok_or_else(|| bad("a milestone has no `name`".to_string()))?;
    if milestones.iter().any(|known| known.name == name) {
        return Err(bad(format!("the milestone `{name}` appears twice")));
    }
    let listed = table.strs("commands").ok_or_else(|| {
        bad(format!(
            "the milestone `{name}` has no string array `commands`"
        ))
    })?;
    for command in &listed {
        if !commands.insert(command.to_string()) {
            return Err(bad(format!("the command `{command}` is listed twice")));
        }
    }
    let summary = table
        .str("summary")
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .ok_or_else(|| bad(format!("the milestone `{name}` has no `summary`")))?;
    Ok(Milestone {
        name: name.to_string(),
        summary: summary.to_string(),
        commands: listed.into_iter().map(str::to_string).collect(),
    })
}

/// Builds a statically linked `ap` for release from `root`, so the same file
/// can run as the guest's `/init`, and returns where the binary is.
pub fn build_ap(root: &Path, target: &Path) -> Result<PathBuf, Problem> {
    build_static(root, target, "ap")
}

/// Builds the binary of `package` for release, statically linked, so it can
/// run in the guest.
fn build_static(root: &Path, target: &Path, package: &str) -> Result<PathBuf, Problem> {
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked", "--package", package])
        .args(["--target", STATIC_TARGET])
        .arg("--target-dir")
        .arg(target)
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(no_cargo)?;
    if !status.success() {
        return Err(Problem(format!(
            "`cargo build --release --package {package}` failed"
        )));
    }
    Ok(target.join(STATIC_TARGET).join("release").join(package))
}

/// Builds `ap` and the fake harness for release, and copies them where the
/// steps will find them. A step puts the fake harness in an agent itself.
fn install(root: &Path, journey: &Path) -> Result<PathBuf, Problem> {
    let built = build_ap(root, &root.join("target"))?;
    let fake = build_static(root, &root.join("target"), "fake-harness")?;
    let problem = |e: io::Error| Problem(format!("couldn't prepare {}: {e}", journey.display()));
    if journey.exists() {
        fs::remove_dir_all(journey).map_err(problem)?;
    }
    let bin = journey.join("bin");
    fs::create_dir_all(&bin).map_err(problem)?;
    fs::copy(built, bin.join("ap")).map_err(problem)?;
    fs::copy(fake, bin.join("fake-harness")).map_err(problem)?;
    Ok(bin)
}

fn run_step(
    root: &Path,
    journey: &Path,
    bin: &Path,
    step: &Step,
    key: Option<&str>,
) -> Result<bool, Problem> {
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
    let mut command = Command::new("/bin/sh");
    command
        .arg(&script)
        .current_dir(&work)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    if let Some(key) = key {
        command.env("ANTHROPIC_API_KEY", key);
    }
    let status = command
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
