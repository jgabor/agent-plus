//! `cargo xtask journey`: real use of `ap`, step by step, from a release build.

use crate::bench::Copies;
use crate::ledger::Downloads;
use crate::toml::{self, Value};
use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, lock, score};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

pub const JOURNEY_FILE: &str = "harness/journey.toml";
const STATIC_TARGET: &str = "x86_64-unknown-linux-musl";
const FAKE_HARNESS: &str = "fake-harness";
/// The server on your host that step 10 reaches from the guest.
const FAKE_UPSTREAM: &str = "fake-upstream";
/// The step that proves `ap build` fetches the OS layer, so its store starts
/// empty.
const FETCHES_OS: i64 = 5;

/// The model providers a step can spend on, each with the variable that
/// carries its paid-check key into the step, as a user's `AUTH key` run gets
/// it. The first is the one a step spends on when it names none.
pub const PROVIDERS: [(&str, &str); 2] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "CODEX_API_KEY"),
];

/// Each provider's paid-check key, by the provider's name.
pub type Keys = BTreeMap<&'static str, String>;

struct Step {
    id: i64,
    name: String,
    milestone: String,
    needs: Vec<i64>,
    script: String,
    floor: Option<String>,
    /// The provider whose key the step needs, and the variable it comes in.
    spend: Option<(&'static str, &'static str)>,
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

/// What a journey timed and fetched: each step that ran, by its id, with its
/// seconds, and what its stores and its download cache gained.
pub type Walked = (Vec<(i64, f64)>, Downloads);

/// What this process's last journey timed and fetched, for the ledger.
static WALKED: Mutex<Option<Walked>> = Mutex::new(None);

/// What the last journey that this process walked timed and fetched.
pub fn walked() -> Option<Walked> {
    WALKED.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Prints each step and the count. It exits 0 whenever it could run, because
/// what fails a merge is a count lower than the base's. Given a step, it runs
/// only that step and the steps it needs, prints no count, and fails unless
/// that step passes.
pub fn run(named: Option<&str>) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let only = named
        .map(|step| {
            step.parse::<i64>()
                .map_err(|_| Problem(format!("`journey` takes a step id, but got `{step}`")))
        })
        .transpose()?;
    let _hold = lock::shared(&root)?;
    let outcomes = walk(&root, &Keys::new(), only, |line| println!("{line}"))?;
    if let Some(id) = only {
        let passed = outcomes.iter().any(|o| o.id == id && o.state == "pass");
        return Ok(if passed { Verdict::Pass } else { Verdict::Fail });
    }
    let (passing, steps) = tally(&outcomes);
    println!("journey {passing} of {steps}");
    Ok(Verdict::Pass)
}

/// How many steps pass, and how many there are, for a caller whose stdout is
/// its result. The caller holds the activity lock.
pub fn count(root: &Path) -> Result<(usize, usize), Problem> {
    Ok(tally(&outcomes(root, &Keys::new())?))
}

/// Each step's result in journey order, for a caller whose stdout is its
/// result. The caller holds the activity lock. A step that spends money runs
/// only with its provider's key in `keys`, and only it sees that key, under
/// the provider's variable in [`PROVIDERS`].
pub fn outcomes(root: &Path, keys: &Keys) -> Result<Vec<Outcome>, Problem> {
    walk(root, keys, None, |line| eprintln!("{line}"))
}

/// The milestones, in roadmap order.
pub fn milestones(root: &Path) -> Result<Vec<Milestone>, Problem> {
    Ok(read(root)?.0)
}

/// How many steps spend money, or need a step that does. None of them run.
pub fn left_out(root: &Path) -> Result<usize, Problem> {
    let mut out = HashSet::new();
    for step in read_steps(root)? {
        if step.spend.is_some() || step.needs.iter().any(|need| out.contains(need)) {
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

/// Runs every step, or with `only`, that step and the steps it needs.
fn walk(
    root: &Path,
    keys: &Keys,
    only: Option<i64>,
    report: impl Fn(&str),
) -> Result<Vec<Outcome>, Problem> {
    let mut steps = read_steps(root)?;
    if let Some(id) = only {
        steps = needed(steps, id)?;
    }
    let journey = root.join("target/journey");
    let bin = install(root, &journey)?;
    // Checked once, because each step's store shares the tree's copies.
    let copies = Copies::find(root)?;

    let cache = journey.join("cache");
    let cached = files_under(&cache)?;
    let mut timed = Vec::new();
    let mut passed = HashSet::new();
    let mut spending = HashSet::new();
    let mut outcomes = Vec::new();
    for step in &steps {
        let spends = step.spend.is_some() || step.needs.iter().any(|need| spending.contains(need));
        if spends {
            spending.insert(step.id);
        }
        let given = step
            .spend
            .and_then(|(provider, name)| Some((name, keys.get(provider)?.as_str())));
        let word = if (step.spend.is_some() && given.is_none())
            || !step.needs.iter().all(|need| passed.contains(need))
        {
            "skip"
        } else {
            let start = Instant::now();
            let ran = run_step(root, &journey, &bin, &copies, step, given)?;
            timed.push((step.id, start.elapsed().as_secs_f64()));
            if ran {
                passed.insert(step.id);
                "pass"
            } else {
                "fail"
            }
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
    let (layers, files) = copies.fetched();
    let fetched = Downloads {
        layers,
        files: files + files_under(&cache)?.difference(&cached).count(),
    };
    *WALKED.lock().unwrap_or_else(PoisonError::into_inner) = Some((timed, fetched));
    Ok(outcomes)
}

/// The step `id`, and each step it needs, directly or through another step,
/// in journey order.
fn needed(steps: Vec<Step>, id: i64) -> Result<Vec<Step>, Problem> {
    if !steps.iter().any(|step| step.id == id) {
        return Err(Problem(format!("{JOURNEY_FILE} has no step {id}")));
    }
    let mut wanted = HashSet::from([id]);
    // A step needs only earlier steps, so one pass from the last step finds
    // them all.
    for step in steps.iter().rev() {
        if wanted.contains(&step.id) {
            wanted.extend(&step.needs);
        }
    }
    Ok(steps
        .into_iter()
        .filter(|step| wanted.contains(&step.id))
        .collect())
}

/// Every file under `directory`, which may not exist.
fn files_under(directory: &Path) -> Result<HashSet<PathBuf>, Problem> {
    let mut files = HashSet::new();
    let mut left = vec![directory.to_path_buf()];
    while let Some(directory) = left.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Problem(format!(
                    "couldn't read {}: {e}",
                    directory.display()
                )));
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|e| Problem(format!("couldn't read {}: {e}", directory.display())))?;
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => left.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    files.insert(entry.path());
                }
                _ => {}
            }
        }
    }
    Ok(files)
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
            spend: read_spend(&table).map_err(bad)?,
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

/// The provider whose key a step that spends gets, and its variable. A step
/// that sets `spend = true` spends on Anthropic unless its `provider` names
/// another, so a journey file written before `provider` reads the same.
fn read_spend(table: &toml::Table) -> Result<Option<(&'static str, &'static str)>, &'static str> {
    let spends = match table.get("spend") {
        None => false,
        Some(Value::Bool(spend)) => *spend,
        Some(_) => return Err("has a `spend` that isn't true or false"),
    };
    let provider = match table.get("provider") {
        None => PROVIDERS[0].0,
        Some(_) if !spends => return Err("has a `provider` but doesn't spend"),
        Some(Value::Str(provider)) => provider,
        Some(_) => return Err("has a `provider` that isn't a string"),
    };
    if !spends {
        return Ok(None);
    }
    PROVIDERS
        .into_iter()
        .find(|(known, _)| *known == provider)
        .map(Some)
        .ok_or("has a `provider` that is neither \"anthropic\" nor \"openai\"")
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
        // In a merge, our input is the pipe to the supervisor.
        .stdin(Stdio::null())
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

/// Builds the fake harness for release, statically linked, or `None` if the
/// workspace has no such package.
pub fn build_fake_harness(root: &Path, target: &Path) -> Result<Option<PathBuf>, Problem> {
    build_if_present(root, target, FAKE_HARNESS)
}

/// Builds `package` as [`build_static`] does, or `None` if the workspace has
/// no such package.
fn build_if_present(root: &Path, target: &Path, package: &str) -> Result<Option<PathBuf>, Problem> {
    if !workspace::packages(root)?
        .iter()
        .any(|known| known.name == package)
    {
        return Ok(None);
    }
    build_static(root, target, package).map(Some)
}

/// Builds `ap` for release, and the fake harness and the fake upstream if the
/// workspace has them, and copies them where the steps will find them. A step
/// puts the fake harness in an agent itself, and starts the fake upstream on
/// your host.
fn install(root: &Path, journey: &Path) -> Result<PathBuf, Problem> {
    let built = build_ap(root, &root.join("target"))?;
    let mut fakes = Vec::new();
    for package in [FAKE_HARNESS, FAKE_UPSTREAM] {
        if let Some(fake) = build_if_present(root, &root.join("target"), package)? {
            fakes.push((fake, package));
        }
    }
    let problem = |e: io::Error| Problem(format!("couldn't prepare {}: {e}", journey.display()));
    if journey.exists() {
        fs::remove_dir_all(journey).map_err(problem)?;
    }
    let bin = journey.join("bin");
    fs::create_dir_all(&bin).map_err(problem)?;
    fs::copy(built, bin.join("ap")).map_err(problem)?;
    for (fake, package) in fakes {
        fs::copy(fake, bin.join(package)).map_err(problem)?;
    }
    Ok(bin)
}

fn run_step(
    root: &Path,
    journey: &Path,
    bin: &Path,
    copies: &Copies,
    step: &Step,
    key: Option<(&str, &str)>,
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
    // Downloads outlast a step, so a harness is downloaded once per journey.
    // The store lives under each step's own home, so it starts empty, but for
    // the tree's copies of the guest files.
    let cache = journey.join("cache");
    for path in [&home, &work, &cache] {
        fs::create_dir_all(path)
            .map_err(|e| Problem(format!("couldn't create {}: {e}", path.display())))?;
    }
    if step.id == FETCHES_OS {
        copies.watch(&home);
    } else {
        copies.put(&home)?;
    }
    let mut command = Command::new("/bin/sh");
    command
        .arg(&script)
        .current_dir(&work)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    if let Some((name, key)) = key {
        command.env(name, key);
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
