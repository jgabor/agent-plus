//! `cargo xtask bench`: interleaved runs of the base and candidate builds.

use crate::ledger::{self, Measured};
use crate::toml::{self, Table, Value};
use crate::{Problem, Verdict, base, journey, lock, workspace};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

pub const BUDGETS_FILE: &str = "harness/budgets.toml";
const PAIRS: usize = 15;
const BOOT_TIMEOUT_SECONDS: &str = "30";
/// What every build of `ap` exits with when it doesn't know its arguments. A
/// boot that fails exits with 124 or 125.
const USAGE_STATUS: i32 = 2;
const FIELDS: &[&str] = &["key", "name", "target", "band_pct", "band_abs"];
/// The harness that the no-op build's lockfile pins, and its SHA-256. Any
/// fixed bytes will do, since `ap build` only stores them.
const NOOP_HARNESS: &str = "fake harness\n";
const NOOP_HARNESS_SHA256: &str =
    "1a00fe01e85b2882b602d46ed2ad4531bf806b3ba5b2a5a194219587c398606c";
/// The fixed directory that `noop_build_ms` rebuilds: the Agentfile of
/// journey step 5, with its files, and a lockfile that pins `NOOP_HARNESS`.
const NOOP_FILES: &[(&str, &str)] = &[
    (
        "agent/Agentfile",
        "AGENT claude\nOS minimal\nSYSTEM ./system.md\nPROMPT Review the notes.\nMODEL opus\nADD ./data\nADD ./src /work\nSHARE ./out\n",
    ),
    (
        "agent/Agentfile.lock",
        "HARNESS claude 0.0.0 1a00fe01e85b2882b602d46ed2ad4531bf806b3ba5b2a5a194219587c398606c\n",
    ),
    ("agent/system.md", "Be careful.\n"),
    ("agent/data", "one\n"),
    ("agent/src/main.rs", "fn main() {}\n"),
    ("agent/out/.keep", ""),
    ("grants", "NETWORK +\nPOLICY +\n"),
];
/// The agent that `run_ms` runs: the fake harness, as journey step 7 builds
/// it, with no network, so nothing outside the machine is timed.
const RUN_AGENTFILE: &str = "AGENT claude:0.0.0\nOS minimal\nNETWORK -\n";
const RUN_GRANTS: &str = "NETWORK -\nPOLICY +\n";
const RUN_PROMPT: &str = "Say what you can see.";
/// `timeout`'s arguments for every `ap run`. It stops a run that hangs, and
/// with it the VM that its `ap run` holds.
const RUN_TIMEOUT: [&str; 3] = ["-k", "10", "120"];

pub struct Budget {
    key: String,
    name: String,
    target: Option<f64>,
    band: Option<Band>,
}

#[derive(Clone, Copy)]
struct Band {
    pct: f64,
    abs: f64,
}

#[derive(Clone, Copy)]
enum Measure {
    Time,
    Size,
    Boot,
    Build,
    Run,
    NotYet,
}

/// How a budget is measured. A key that isn't listed is a mistake in the
/// budget file, which would otherwise switch that budget off without a word.
fn measure_of(key: &str) -> Option<Measure> {
    match key {
        "version_ms" => Some(Measure::Time),
        "binary_bytes" => Some(Measure::Size),
        "boot_ms" => Some(Measure::Boot),
        "noop_build_ms" => Some(Measure::Build),
        "run_ms" => Some(Measure::Run),
        "idle_mib" => Some(Measure::NotYet),
        _ => None,
    }
}

pub fn read_budgets(root: &Path) -> Result<Vec<Budget>, Problem> {
    let text = fs::read_to_string(root.join(BUDGETS_FILE))
        .map_err(|e| Problem(format!("couldn't read {BUDGETS_FILE}: {e}")))?;
    parse_budgets(&text)
}

/// The budgets as a commit had them. A commit with no budget file has none.
pub fn budgets_at(root: &Path, commit: &str) -> Result<Vec<Budget>, Problem> {
    let object = format!("{commit}:{BUDGETS_FILE}");
    if !base::git(root, &["cat-file", "-e", &object])?
        .status
        .success()
    {
        return Ok(Vec::new());
    }
    parse_budgets(&base::git_text(root, &["show", &object])?)
}

fn parse_budgets(text: &str) -> Result<Vec<Budget>, Problem> {
    let bad = |e: String| Problem(format!("{BUDGETS_FILE}: {e}"));
    let tables = toml::array_of_tables(text, "budget").map_err(bad)?;
    let mut seen = HashSet::new();
    let mut budgets = Vec::new();
    for table in tables {
        let budget = read_budget(&table).map_err(bad)?;
        if !seen.insert(budget.key.clone()) {
            return Err(bad(format!("`{}` appears twice", budget.key)));
        }
        budgets.push(budget);
    }
    Ok(budgets)
}

fn read_budget(table: &Table) -> Result<Budget, String> {
    if let Some((field, _)) = table.entries().find(|(k, _)| !FIELDS.contains(k)) {
        return Err(format!("a budget has an unknown field `{field}`"));
    }
    let key = table.str("key").ok_or("a budget has no `key`")?;
    if measure_of(key).is_none() {
        return Err(format!("`{key}` isn't a budget this program can measure"));
    }
    let name = table
        .str("name")
        .ok_or_else(|| format!("`{key}` has no `name`"))?;
    let pct = number(table, "band_pct", key)?;
    let abs = number(table, "band_abs", key)?;
    Ok(Budget {
        key: key.to_string(),
        name: name.to_string(),
        target: number(table, "target", key)?,
        band: (pct.is_some() || abs.is_some()).then(|| Band {
            pct: pct.unwrap_or(0.0),
            abs: abs.unwrap_or(0.0),
        }),
    })
}

fn number(table: &Table, field: &str, key: &str) -> Result<Option<f64>, String> {
    let value = match table.get(field) {
        None => return Ok(None),
        Some(Value::Int(number)) => *number as f64,
        Some(Value::Float(number)) => *number,
        Some(_) => return Err(format!("`{field}` of `{key}` isn't a number")),
    };
    if value < 0.0 {
        return Err(format!("`{field}` of `{key}` is negative"));
    }
    Ok(Some(value))
}

/// How many budgets with a target meet it, and how many have one. A budget
/// with no value yet isn't met.
pub fn met(budgets: &[Budget], values: &BTreeMap<String, f64>) -> (usize, usize) {
    let targets = budgets.iter().filter(|b| b.target.is_some()).count();
    (targets - missed(budgets, values).len(), targets)
}

/// A budget with a target that isn't met.
pub struct Missed<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub target: f64,
    pub value: Option<f64>,
}

/// The budgets that miss their targets, in the order of the budget file.
pub fn missed<'a>(budgets: &'a [Budget], values: &BTreeMap<String, f64>) -> Vec<Missed<'a>> {
    budgets
        .iter()
        .filter_map(|budget| {
            let target = budget.target?;
            let value = values.get(&budget.key).copied();
            (!value.is_some_and(|value| value < target)).then_some(Missed {
                key: &budget.key,
                name: &budget.name,
                target,
                value,
            })
        })
        .collect()
}

pub fn run() -> Result<Verdict, Problem> {
    compare(&workspace::root()?, None)
}

/// Measures HEAD against its base. A merge passes the PR body it read, which
/// says why any slowdown is worth it. Otherwise that comes from this branch's
/// open PR.
pub fn compare(root: &Path, pr_body: Option<&str>) -> Result<Verdict, Problem> {
    let budgets = read_budgets(root)?;
    let Some(commit) = base::find(root)? else {
        return Err(Problem(
            "there is no origin/main to compare with. Run `git fetch origin main`".to_string(),
        ));
    };
    ledger::clear_measured(root)?;
    let head = clean_head(root)?;

    let directory = root.join("target/bench");
    let hold = lock::shared(root)?;
    let candidate = build_candidate(root, &directory)?;
    let baseline = build_base(root, &directory, &commit)?;
    let harness = if budgets
        .iter()
        .any(|budget| matches!(measure_of(&budget.key), Some(Measure::Run)))
    {
        Some(build_harness(root)?)
    } else {
        None
    };
    drop(hold);

    let measuring = lock::exclusive(root)?;
    let rows = measure(
        &budgets,
        &baseline,
        &candidate,
        harness.as_deref(),
        &directory,
    )?;
    drop(measuring);

    let any_over = rows.iter().any(|row| row.over);
    let reason = match (any_over, pr_body) {
        (false, _) => None,
        (true, Some(body)) => base::reason_in(body),
        (true, None) => base::budget_reason(root)?,
    };
    let mut verdict = Verdict::Pass;
    let mut values = BTreeMap::new();
    for row in &rows {
        let word = match (row.has_band, row.over, &reason) {
            (false, _, _) => "recorded",
            (true, false, _) => "pass",
            (true, true, Some(_)) => "over",
            (true, true, None) => {
                verdict = Verdict::Fail;
                eprintln!(
                    "bench: {} worsened past its band. Undo the slowdown, or add a `Budget reason: <text>` line to the PR body",
                    row.name
                );
                "fail"
            }
        };
        match row.base {
            Some(base) => println!("{word} {} base {base} candidate {}", row.key, row.candidate),
            None => println!("no-comparison {} candidate {}", row.key, row.candidate),
        }
        values.insert(row.key.clone(), row.candidate);
    }
    for budget in &budgets {
        if matches!(measure_of(&budget.key), Some(Measure::NotYet)) {
            println!("skip {}", budget.key);
        }
    }
    if verdict == Verdict::Pass {
        if let Some(commit) = head
            && clean_head(root)?.as_ref() == Some(&commit)
        {
            let reason = reason.unwrap_or_default();
            let budgets = values;
            ledger::write_measured(
                root,
                &Measured {
                    commit,
                    reason,
                    budgets,
                },
            )?;
        }
        println!("bench: pass");
    } else {
        println!("bench: fail");
    }
    Ok(verdict)
}

/// HEAD, if the tree is what HEAD holds. Otherwise what was built says nothing
/// about any commit.
fn clean_head(root: &Path) -> Result<Option<String>, Problem> {
    if !ledger::changes(root)?.is_empty() {
        return Ok(None);
    }
    let head = base::git_text(root, &["rev-parse", "HEAD"])?;
    Ok(Some(head.trim().to_string()))
}

/// The candidate's fake harness, which both builds of `ap` run for `run_ms`.
fn build_harness(root: &Path) -> Result<PathBuf, Problem> {
    journey::build_fake_harness(root, &root.join("target"))
        .map_err(|Problem(e)| Problem(format!("the fake harness build: {e}")))?
        .ok_or_else(|| {
            Problem(
                "`run_ms` runs the fake harness, and the workspace has no `fake-harness` package"
                    .to_string(),
            )
        })
}

fn build_candidate(root: &Path, directory: &Path) -> Result<PathBuf, Problem> {
    let built = journey::build_ap(root, &root.join("target"))
        .map_err(|Problem(e)| Problem(format!("the candidate build: {e}")))?;
    keep(&built, &directory.join("candidate"))
}

/// Builds `ap` from a copy of the base commit's tree. Cargo reads
/// `.cargo/config.toml` from every parent directory, so the copy sits outside
/// the checkout, or the candidate's settings would apply to the base too. The
/// output has one directory per commit, because git dates an exported file by
/// its commit, and a base committed before an earlier build looks unchanged.
fn build_base(root: &Path, directory: &Path, commit: &str) -> Result<PathBuf, Problem> {
    let directory = directory.join("base").join(commit);
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let tree = temp_outside(root)?.join(format!("xtask-base-{:016x}-{commit}", hasher.finish()));
    export(root, commit, &tree)?;
    let built = journey::build_ap(&tree, &directory.join("target"));
    let removed = fs::remove_dir_all(&tree);
    let built = built.map_err(|Problem(e)| Problem(format!("the base build: {e}")))?;
    removed.map_err(|e| Problem(format!("couldn't remove {}: {e}", tree.display())))?;
    keep(&built, &directory)
}

/// The system's temp directory, which must be outside the checkout.
pub fn temp_outside(root: &Path) -> Result<PathBuf, Problem> {
    let resolve = |path: &Path| {
        fs::canonicalize(path)
            .map_err(|e| Problem(format!("couldn't resolve {}: {e}", path.display())))
    };
    let temp = resolve(&env::temp_dir())?;
    if temp.starts_with(resolve(root)?) {
        return Err(Problem(format!(
            "TMPDIR is {}, inside the checkout, so Cargo would apply the candidate's settings to the base build. Point it outside the checkout",
            temp.display()
        )));
    }
    Ok(temp)
}

fn keep(built: &Path, directory: &Path) -> Result<PathBuf, Problem> {
    let problem = |e: std::io::Error| Problem(format!("couldn't keep {}: {e}", built.display()));
    fs::create_dir_all(directory).map_err(problem)?;
    let kept = directory.join("ap");
    fs::copy(built, &kept).map_err(problem)?;
    Ok(kept)
}

fn export(root: &Path, commit: &str, tree: &Path) -> Result<(), Problem> {
    let problem = |e: std::io::Error| Problem(format!("couldn't export {commit}: {e}"));
    if tree.exists() {
        fs::remove_dir_all(tree).map_err(problem)?;
    }
    fs::create_dir_all(tree).map_err(problem)?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", commit])
        .current_dir(root)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(problem)?;
    let stream = archive.stdout.take().expect("stdout is piped");
    let unpacked = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(tree)
        .stdin(stream)
        .status()
        .map_err(problem)?;
    let archived = archive.wait().map_err(problem)?;
    if !archived.success() || !unpacked.success() {
        return Err(Problem(format!(
            "couldn't export {commit}: {archived}, {unpacked}"
        )));
    }
    Ok(())
}

struct Row {
    key: String,
    name: String,
    /// `None` when the base can't be measured this way at all.
    base: Option<f64>,
    candidate: f64,
    has_band: bool,
    over: bool,
}

fn measure(
    budgets: &[Budget],
    base: &Path,
    candidate: &Path,
    harness: Option<&Path>,
    directory: &Path,
) -> Result<Vec<Row>, Problem> {
    let mut rows = Vec::new();
    for budget in budgets {
        let (before, after) = match measure_of(&budget.key) {
            Some(Measure::Time) => {
                let (before, after) = interleaved(base, candidate, run_once)?;
                (Some(before), after)
            }
            Some(Measure::Size) => (Some(size(base)?), size(candidate)?),
            Some(Measure::Boot) => boots(base, candidate, &directory.join("home"))?,
            Some(Measure::Build) => noop_builds(base, candidate, &directory.join("noop"))?,
            Some(Measure::Run) => {
                let harness = harness.expect("the fake harness is built for `run_ms`");
                runs(base, candidate, harness, &directory.join("run"))?
            }
            Some(Measure::NotYet) | None => continue,
        };
        rows.push(Row {
            key: budget.key.clone(),
            name: budget.name.clone(),
            base: before,
            candidate: after,
            has_band: budget.band.is_some(),
            over: budget
                .band
                .zip(before)
                .is_some_and(|(band, before)| over_band(before, after, band)),
        });
    }
    Ok(rows)
}

/// The medians of the pairs, in milliseconds. Each pair swaps which build goes
/// first, so that neither build always runs on a machine the other just warmed.
fn interleaved(
    base: &Path,
    candidate: &Path,
    run: impl Fn(&Path) -> Result<f64, Problem>,
) -> Result<(f64, f64), Problem> {
    let mut before = Vec::with_capacity(PAIRS);
    let mut after = Vec::with_capacity(PAIRS);
    for pair in 0..PAIRS {
        if pair % 2 == 0 {
            before.push(run(base)?);
            after.push(run(candidate)?);
        } else {
            after.push(run(candidate)?);
            before.push(run(base)?);
        }
    }
    Ok((median(before), median(after)))
}

fn run_once(binary: &Path) -> Result<f64, Problem> {
    let start = Instant::now();
    let status = Command::new(binary)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Problem(format!("couldn't run {}: {e}", binary.display())))?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    if !status.success() {
        return Err(Problem(format!(
            "`{} --version` failed: {status}",
            binary.display()
        )));
    }
    Ok(elapsed)
}

/// The boot medians of both builds, or of the candidate alone when the base
/// has no `ap boot`. One warm-up run each comes first, and every run shares
/// `home`, so no download is timed.
fn boots(base: &Path, candidate: &Path, home: &Path) -> Result<(Option<f64>, f64), Problem> {
    fs::create_dir_all(home)
        .map_err(|e| Problem(format!("couldn't create {}: {e}", home.display())))?;
    let timed = |binary: &Path| boot_once(binary, home)?.ok_or_else(|| no_boot(binary));
    let base_boots = boot_once(base, home)?.is_some();
    if !base_boots && has_boot(base, home)? {
        return Err(Problem(format!(
            "`{} boot` rejects `--timeout {BOOT_TIMEOUT_SECONDS}`",
            base.display()
        )));
    }
    timed(candidate)?;
    if base_boots {
        let (before, after) = interleaved(base, candidate, timed)?;
        return Ok((Some(before), after));
    }
    let after = (0..PAIRS)
        .map(|_| timed(candidate))
        .collect::<Result<_, _>>()?;
    Ok((None, median(after)))
}

/// Whether `ap boot` with no arguments is a command. Every build that has
/// `ap boot` also has `--timeout`, so a build that rejects it changed the flag,
/// and its comparison isn't lost without a word. The bare boot stops at its
/// own default timeout.
fn has_boot(binary: &Path, home: &Path) -> Result<bool, Problem> {
    let status = Command::new(binary)
        .arg("boot")
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Problem(format!("couldn't run `{} boot`: {e}", binary.display())))?;
    Ok(status.code() != Some(USAGE_STATUS))
}

fn no_boot(binary: &Path) -> Problem {
    Problem(format!("`{} boot` isn't a command", binary.display()))
}

/// The medians of rebuilding the fixed directory with nothing changed, for
/// both builds, or for the candidate alone when the base has no `ap build`.
/// Each build has a `HOME` of its own, so neither reads the other's store,
/// and its download cache holds the harness the lockfile pins. Each builds
/// the directory once to warm up, so every timed run is a rebuild.
fn noop_builds(
    base: &Path,
    candidate: &Path,
    directory: &Path,
) -> Result<(Option<f64>, f64), Problem> {
    let problem =
        |e: std::io::Error| Problem(format!("couldn't write {}: {e}", directory.display()));
    if directory.exists() {
        fs::remove_dir_all(directory).map_err(problem)?;
    }
    for (path, content) in NOOP_FILES {
        let path = directory.join(path);
        fs::create_dir_all(path.parent().expect("a file has a parent")).map_err(problem)?;
        fs::write(&path, content).map_err(problem)?;
    }
    let homes = [
        directory.join("home/base"),
        directory.join("home/candidate"),
    ];
    for home in &homes {
        let cache = home.join(".cache/ap");
        fs::create_dir_all(&cache).map_err(problem)?;
        fs::write(cache.join(NOOP_HARNESS_SHA256), NOOP_HARNESS).map_err(problem)?;
    }
    let home_of = |binary: &Path| if binary == base { &homes[0] } else { &homes[1] };
    let grants = directory.join("grants");
    let agent = directory.join("agent");
    let timed = |binary: &Path| {
        build_once(binary, &grants, &agent, home_of(binary))?.ok_or_else(|| no_build(binary))
    };
    let base_builds = build_once(base, &grants, &agent, home_of(base))?.is_some();
    timed(candidate)?;
    if base_builds {
        let (before, after) = interleaved(base, candidate, timed)?;
        return Ok((Some(before), after));
    }
    let after = (0..PAIRS)
        .map(|_| timed(candidate))
        .collect::<Result<_, _>>()?;
    Ok((None, median(after)))
}

fn no_build(binary: &Path) -> Problem {
    Problem(format!(
        "`{} build --approve=<file> <dir>` isn't a command",
        binary.display()
    ))
}

/// Milliseconds that `ap build` takes on the directory `agent`, with the
/// grants in `grants` approved, or `None` if this build exits with its usage status, because it
/// has no such command, or has no `--approve`. `--approve` came after `ap
/// build`, so a base without it is an old build, as one without `ap build` is,
/// and it can't do the work the candidate is timed on.
fn build_once(
    binary: &Path,
    grants: &Path,
    agent: &Path,
    home: &Path,
) -> Result<Option<f64>, Problem> {
    let shown = binary.display();
    let mut approve = std::ffi::OsString::from("--approve=");
    approve.push(grants);
    let start = Instant::now();
    let output = Command::new(binary)
        .arg("build")
        .arg(approve)
        .arg(agent)
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Problem(format!("couldn't run `{shown} build`: {e}")))?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    if output.status.code() == Some(USAGE_STATUS) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(Problem(format!(
            "`{shown} build` failed: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(Some(elapsed))
}

/// The medians of running a fixed agent headless, for both builds, or for
/// the candidate alone when the base has no `ap run`. Each build of `ap`
/// builds an agent of its own, named for it, so each runs what it built. All
/// of them share one `HOME`, so they share one store, and the download cache
/// holds the fake harness that the lockfiles pin. Each builds its agent and
/// runs it once first, so no build or download is timed.
fn runs(
    base: &Path,
    candidate: &Path,
    harness: &Path,
    directory: &Path,
) -> Result<(Option<f64>, f64), Problem> {
    let problem =
        |e: std::io::Error| Problem(format!("couldn't write {}: {e}", directory.display()));
    if directory.exists() {
        fs::remove_dir_all(directory).map_err(problem)?;
    }
    let digest = sha256(harness)?;
    let home = directory.join("home");
    let cache = home.join(".cache/ap");
    fs::create_dir_all(&cache).map_err(problem)?;
    fs::copy(harness, cache.join(&digest)).map_err(problem)?;
    let grants = directory.join("grants");
    fs::write(&grants, RUN_GRANTS).map_err(problem)?;
    let agents = [directory.join("base"), directory.join("candidate")];
    for agent in &agents {
        fs::create_dir_all(agent).map_err(problem)?;
        fs::write(agent.join("Agentfile"), RUN_AGENTFILE).map_err(problem)?;
        let lock = format!("HARNESS claude 0.0.0 {digest}\n");
        fs::write(agent.join("Agentfile.lock"), lock).map_err(problem)?;
    }
    let agent_of = |binary: &Path| {
        if binary == base {
            &agents[0]
        } else {
            &agents[1]
        }
    };
    let name_of = |binary: &Path| if binary == base { "base" } else { "candidate" };
    let timed = |binary: &Path| {
        run_once_headless(binary, name_of(binary), &home)?.ok_or_else(|| no_run(binary))
    };
    let warm = |binary: &Path| {
        build_once(binary, &grants, agent_of(binary), &home)?.ok_or_else(|| no_build(binary))?;
        timed(binary)
    };
    let base_runs = has_run(base, name_of(base), &home)?;
    if base_runs {
        warm(base)?;
    }
    warm(candidate)?;
    if base_runs {
        let (before, after) = interleaved(base, candidate, timed)?;
        return Ok((Some(before), after));
    }
    let after = (0..PAIRS)
        .map(|_| timed(candidate))
        .collect::<Result<_, _>>()?;
    Ok((None, median(after)))
}

/// `ap run <name> -p <prompt>`, under `timeout`.
fn run_command(binary: &Path, name: &str, home: &Path) -> Command {
    let mut command = Command::new("timeout");
    command
        .args(RUN_TIMEOUT)
        .arg(binary)
        .args(["run", name, "-p", RUN_PROMPT])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null());
    command
}

/// Whether `ap run` is a command, asked before the agent `name` is built. A
/// build that has it fails that run, but not with its usage status, and a
/// build from before `ap run` isn't asked to build an Agentfile it may not
/// read.
fn has_run(binary: &Path, name: &str, home: &Path) -> Result<bool, Problem> {
    let status = run_command(binary, name, home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Problem(format!("couldn't run `{} run`: {e}", binary.display())))?;
    Ok(status.code() != Some(USAGE_STATUS))
}

fn no_run(binary: &Path) -> Problem {
    Problem(format!(
        "`{} run <agent> -p <prompt>` isn't a command",
        binary.display()
    ))
}

/// Milliseconds from spawning `ap run` until the first byte of its output,
/// or `None` if this build exits with its usage status. The rest of the
/// output is read to its end, and the run waited for, so the VM has stopped
/// before the next run starts.
fn run_once_headless(binary: &Path, name: &str, home: &Path) -> Result<Option<f64>, Problem> {
    let shown = binary.display();
    let problem = |e: std::io::Error| Problem(format!("couldn't run `{shown} run`: {e}"));
    let start = Instant::now();
    let mut child = run_command(binary, name, home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(problem)?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let draining = thread::spawn(move || {
        let mut errors = Vec::new();
        stderr.read_to_end(&mut errors).map(|_| errors)
    });
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut first = [0; 1];
    let read = stdout.read(&mut first);
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let rest = io::copy(&mut stdout, &mut io::sink());
    let status = child.wait().map_err(problem)?;
    let printed = read.map_err(problem)?;
    rest.map_err(problem)?;
    let errors = draining.join().expect("draining stderr can't panic");
    let errors = errors.map_err(problem)?;
    if status.code() == Some(USAGE_STATUS) {
        return Ok(None);
    }
    if !status.success() {
        return Err(Problem(format!(
            "`{shown} run` failed: {status}: {}",
            String::from_utf8_lossy(&errors).trim()
        )));
    }
    if printed == 0 {
        return Err(Problem(format!("`{shown} run` printed nothing")));
    }
    Ok(Some(elapsed))
}

/// The SHA-256 digest of the file at `path`, in hex. macOS has `shasum`, and
/// may lack `sha256sum`.
fn sha256(path: &Path) -> Result<String, Problem> {
    let shown = path.display();
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .map_err(|e| Problem(format!("couldn't hash {shown}: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let digest = text.split(' ').next().unwrap_or_default();
    if !output.status.success() || digest.len() != 64 {
        return Err(Problem(format!("couldn't hash {shown}: {}", output.status)));
    }
    Ok(digest.to_string())
}

/// Milliseconds from spawning `ap boot` until its ready line appears, or
/// `None` if this build exits with its usage status, because it has no such
/// command. `ap` itself stops the VM at the timeout.
fn boot_once(binary: &Path, home: &Path) -> Result<Option<f64>, Problem> {
    let shown = binary.display();
    let problem = |e: std::io::Error| Problem(format!("couldn't run `{shown} boot`: {e}"));
    let start = Instant::now();
    let mut child = Command::new(binary)
        .args(["boot", "--timeout", BOOT_TIMEOUT_SECONDS])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(problem)?;
    // Drained apart from stdout, so that a failing boot with a lot to say can't
    // block on a full pipe while this waits for its ready line.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let draining = thread::spawn(move || {
        let mut errors = Vec::new();
        stderr.read_to_end(&mut errors).map(|_| errors)
    });
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut ready = String::new();
    let read = stdout.read_line(&mut ready);
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    child.stdout = Some(stdout.into_inner());
    let output = child.wait_with_output().map_err(problem)?;
    read.map_err(problem)?;
    let errors = draining.join().expect("draining stderr can't panic");
    let errors = errors.map_err(problem)?;
    let errors = String::from_utf8_lossy(&errors);
    if output.status.code() == Some(USAGE_STATUS) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(Problem(format!(
            "`{shown} boot` failed: {}: {}",
            output.status,
            errors.trim()
        )));
    }
    if ready.trim().is_empty() {
        return Err(Problem(format!("`{shown} boot` printed no ready line")));
    }
    Ok(Some(elapsed))
}

fn size(binary: &Path) -> Result<f64, Problem> {
    let metadata = fs::metadata(binary)
        .map_err(|e| Problem(format!("couldn't read {}: {e}", binary.display())))?;
    Ok(metadata.len() as f64)
}

/// The middle value, to the hundredth.
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    (values[values.len() / 2] * 100.0).round() / 100.0
}

fn over_band(base: f64, candidate: f64, band: Band) -> bool {
    candidate - base > band.abs.max(base * band.pct / 100.0)
}

#[cfg(test)]
mod tests {
    use super::{Band, NOOP_FILES, NOOP_HARNESS, NOOP_HARNESS_SHA256, median, over_band};
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn the_noop_lockfile_pins_the_harness_by_its_digest() {
        // macOS has `shasum`, and may lack `sha256sum`, as the journey's
        // step 5 allows.
        let mut hasher = Command::new("sha256sum")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .or_else(|_| {
                Command::new("shasum")
                    .args(["-a", "256"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
            })
            .expect("sha256sum or shasum runs");
        hasher
            .stdin
            .take()
            .unwrap()
            .write_all(NOOP_HARNESS.as_bytes())
            .unwrap();
        let output = hasher.wait_with_output().unwrap();
        let digest = String::from_utf8(output.stdout).unwrap();
        assert_eq!(digest.split(' ').next(), Some(NOOP_HARNESS_SHA256));
        let lock = NOOP_FILES
            .iter()
            .find(|(path, _)| *path == "agent/Agentfile.lock")
            .map(|(_, content)| *content);
        assert_eq!(
            lock,
            Some(format!("HARNESS claude 0.0.0 {NOOP_HARNESS_SHA256}\n").as_str())
        );
    }

    #[test]
    fn the_median_is_the_middle_value() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![1.234, 9.0, 0.5]), 1.23);
    }

    #[test]
    fn the_band_is_whichever_is_larger() {
        let band = Band {
            pct: 20.0,
            abs: 2.0,
        };
        assert!(!over_band(100.0, 120.0, band));
        assert!(over_band(100.0, 120.5, band));
        assert!(!over_band(1.0, 3.0, band));
        assert!(over_band(1.0, 3.5, band));
    }

    #[test]
    fn getting_faster_is_never_over() {
        let band = Band { pct: 0.0, abs: 0.0 };
        assert!(!over_band(10.0, 1.0, band));
        assert!(!over_band(10.0, 10.0, band));
        assert!(over_band(10.0, 10.5, band));
    }
}
