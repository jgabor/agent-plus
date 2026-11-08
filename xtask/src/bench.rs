//! `cargo xtask bench`: interleaved runs of the base and candidate builds.

use crate::ledger::{self, Measured};
use crate::toml::{self, Table, Value};
use crate::{Problem, Verdict, base, journey, lock, workspace};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub const BUDGETS_FILE: &str = "harness/budgets.toml";
const PAIRS: usize = 15;
const FIELDS: &[&str] = &["key", "name", "target", "band_pct", "band_abs"];

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
    NotYet,
}

/// How a budget is measured. A key that isn't listed is a mistake in the
/// budget file, which would otherwise switch that budget off without a word.
fn measure_of(key: &str) -> Option<Measure> {
    match key {
        "version_ms" => Some(Measure::Time),
        "binary_bytes" => Some(Measure::Size),
        "boot_ms" | "run_ms" | "idle_mib" | "noop_build_ms" => Some(Measure::NotYet),
        _ => None,
    }
}

pub fn read_budgets(root: &Path) -> Result<Vec<Budget>, Problem> {
    let bad = |e: String| Problem(format!("{BUDGETS_FILE}: {e}"));
    let text = fs::read_to_string(root.join(BUDGETS_FILE))
        .map_err(|e| Problem(format!("couldn't read {BUDGETS_FILE}: {e}")))?;
    let tables = toml::array_of_tables(&text, "budget").map_err(bad)?;
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
    drop(hold);

    let measuring = lock::exclusive(root)?;
    let rows = measure(&budgets, &baseline, &candidate)?;
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
        println!(
            "{word} {} base {} candidate {}",
            row.key, row.base, row.candidate
        );
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
    base: f64,
    candidate: f64,
    has_band: bool,
    over: bool,
}

fn measure(budgets: &[Budget], base: &Path, candidate: &Path) -> Result<Vec<Row>, Problem> {
    let mut rows = Vec::new();
    for budget in budgets {
        let (before, after) = match measure_of(&budget.key) {
            Some(Measure::Time) => interleaved(base, candidate)?,
            Some(Measure::Size) => (size(base)?, size(candidate)?),
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
                .is_some_and(|band| over_band(before, after, band)),
        });
    }
    Ok(rows)
}

/// The medians of the pairs, in milliseconds. Each pair swaps which build goes
/// first, so that neither build always runs on a machine the other just warmed.
fn interleaved(base: &Path, candidate: &Path) -> Result<(f64, f64), Problem> {
    let mut before = Vec::with_capacity(PAIRS);
    let mut after = Vec::with_capacity(PAIRS);
    for pair in 0..PAIRS {
        if pair % 2 == 0 {
            before.push(run_once(base)?);
            after.push(run_once(candidate)?);
        } else {
            after.push(run_once(candidate)?);
            before.push(run_once(base)?);
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
    use super::{Band, median, over_band};

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
