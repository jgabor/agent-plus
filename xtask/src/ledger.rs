//! `harness/ledger/`: the scores and budgets recorded for each merge.

use crate::toml::{self, Table, Value};
use crate::{Problem, Verdict, base, journey, lock, score, testrun, workspace};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

const DIRECTORY: &str = "harness/ledger";
const MEASURED: &str = "target/bench/result.toml";

#[derive(Debug, PartialEq)]
pub struct Record {
    issue: usize,
    pr: usize,
    commit: String,
    date: String,
    host: String,
    pub proven: usize,
    promises: usize,
    pub passing: usize,
    pub steps: usize,
    reason: String,
    pub budgets: BTreeMap<String, f64>,
}

impl Record {
    fn render(&self) -> String {
        let mut text = format!(
            "issue = {}\npr = {}\ncommit = {}\ndate = {}\nhost = {}\nspec = {{ proven = {}, promises = {} }}\njourney = {{ passing = {}, steps = {} }}\nreason = {}\n\n[budgets]\n",
            self.issue,
            self.pr,
            toml::quoted(&self.commit),
            toml::quoted(&self.date),
            toml::quoted(&self.host),
            self.proven,
            self.promises,
            self.passing,
            self.steps,
            toml::quoted(&self.reason),
        );
        push_budgets(&mut text, &self.budgets);
        text
    }

    fn parse(text: &str) -> Result<Record, String> {
        let table = toml::document(text)?;
        let spec = section(&table, "spec")?;
        let journey = section(&table, "journey")?;
        Ok(Record {
            issue: count(&table, "issue")?,
            pr: count(&table, "pr")?,
            commit: string(&table, "commit")?,
            date: string(&table, "date")?,
            host: string(&table, "host")?,
            proven: count(spec, "proven")?,
            promises: count(spec, "promises")?,
            passing: count(journey, "passing")?,
            steps: count(journey, "steps")?,
            reason: string(&table, "reason")?,
            budgets: read_budgets(&table)?,
        })
    }
}

/// What a passing bench measured, kept for `record`.
pub struct Measured {
    pub commit: String,
    pub reason: String,
    pub budgets: BTreeMap<String, f64>,
}

impl Measured {
    fn render(&self) -> String {
        let mut text = format!(
            "commit = {}\nreason = {}\n\n[budgets]\n",
            toml::quoted(&self.commit),
            toml::quoted(&self.reason),
        );
        push_budgets(&mut text, &self.budgets);
        text
    }

    fn parse(text: &str) -> Result<Measured, String> {
        let table = toml::document(text)?;
        Ok(Measured {
            commit: string(&table, "commit")?,
            reason: string(&table, "reason")?,
            budgets: read_budgets(&table)?,
        })
    }
}

pub fn clear_measured(root: &Path) -> Result<(), Problem> {
    match fs::remove_file(root.join(MEASURED)) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(Problem(format!("couldn't remove {MEASURED}: {e}")))
        }
        _ => Ok(()),
    }
}

pub fn write_measured(root: &Path, measured: &Measured) -> Result<(), Problem> {
    fs::write(root.join(MEASURED), measured.render())
        .map_err(|e| Problem(format!("couldn't write {MEASURED}: {e}")))
}

/// What the bench measured, if the last bench was of this commit.
fn read_measured(root: &Path, commit: &str) -> Result<Option<Measured>, Problem> {
    let text = match fs::read_to_string(root.join(MEASURED)) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Problem(format!("couldn't read {MEASURED}: {e}"))),
    };
    let measured = Measured::parse(&text).map_err(|e| Problem(format!("{MEASURED}: {e}")))?;
    Ok((measured.commit == commit).then_some(measured))
}

fn push_budgets(text: &mut String, budgets: &BTreeMap<String, f64>) {
    for (name, value) in budgets {
        text.push_str(&format!("{name} = {value}\n"));
    }
}

fn read_budgets(table: &Table) -> Result<BTreeMap<String, f64>, String> {
    let mut budgets = BTreeMap::new();
    for (name, value) in section(table, "budgets")?.entries() {
        let value = match value {
            Value::Int(number) => *number as f64,
            Value::Float(number) => *number,
            _ => return Err(format!("budget `{name}` isn't a number")),
        };
        budgets.insert(name.to_string(), value);
    }
    Ok(budgets)
}

fn section<'a>(table: &'a Table, key: &str) -> Result<&'a Table, String> {
    table
        .table(key)
        .ok_or_else(|| format!("`{key}` isn't a table"))
}

fn count(table: &Table, key: &str) -> Result<usize, String> {
    table
        .int(key)
        .and_then(|number| usize::try_from(number).ok())
        .ok_or_else(|| format!("`{key}` isn't a count"))
}

fn string(table: &Table, key: &str) -> Result<String, String> {
    table
        .str(key)
        .map(str::to_string)
        .ok_or_else(|| format!("`{key}` isn't a string"))
}

/// Writes the record for the current head, which must be what the tree holds,
/// because the record says these are that commit's scores.
pub fn record(issue: &str, pr: &str) -> Result<Verdict, Problem> {
    let number = |flag: &str, word: &str| {
        word.parse::<usize>()
            .map_err(|_| Problem(format!("{flag} takes a number, but got `{word}`")))
    };
    let (issue, pr) = (number("--issue", issue)?, number("--pr", pr)?);
    let root = workspace::root()?;
    if !root.join(".git").exists() {
        return Err(Problem(
            "`record` describes a commit, so run it at the top of a git repository".to_string(),
        ));
    }
    let hold = lock::shared(&root)?;
    let changed = changes(&root)?;
    if !changed.is_empty() {
        return Err(Problem(format!(
            "the tree differs from HEAD, so its scores aren't that commit's. Commit or discard these first:\n{}",
            changed.trim_end()
        )));
    }
    let commit = base::git_text(&root, &["rev-parse", "HEAD"])?;
    let date = base::git_text(&root, &["show", "-s", "--format=%cs", "HEAD"])?;

    let results = testrun::run(&root, &hold)?;
    let score = score::compute(&root, &results)?;
    let (passing, steps) = journey::count(&root)?;
    let mut budgets = at_head(&root)?.map_or_else(BTreeMap::new, |previous| previous.budgets);
    let mut reason = String::new();
    if let Some(measured) = read_measured(&root, commit.trim())? {
        budgets.extend(measured.budgets);
        reason = measured.reason;
    }
    let record = Record {
        issue,
        pr,
        commit: commit.trim().to_string(),
        date: date.trim().to_string(),
        host: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        proven: score.proven,
        promises: score.promises.len(),
        passing,
        steps,
        reason,
        budgets,
    };

    let path = format!("{DIRECTORY}/{issue}.toml");
    let problem = |e: std::io::Error| Problem(format!("couldn't write {path}: {e}"));
    fs::create_dir_all(root.join(DIRECTORY)).map_err(problem)?;
    fs::write(root.join(&path), record.render()).map_err(problem)?;
    println!("{path}");
    Ok(Verdict::Pass)
}

/// How the tree differs from HEAD, leaving out the ledger, where an earlier
/// attempt at a record may be. Empty means it doesn't.
pub fn changes(root: &Path) -> Result<String, Problem> {
    let exclude = format!(":(exclude){DIRECTORY}");
    base::git_text(
        root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            ".",
            &exclude,
        ],
    )
}

/// The newest record in the history of HEAD. `None` outside a repository, or
/// before the first commit.
pub fn at_head(root: &Path) -> Result<Option<Record>, Problem> {
    if !root.join(".git").exists() || !base::commit_exists(root, "HEAD")? {
        return Ok(None);
    }
    newest(root, "HEAD")
}

/// The newest record in the history of a commit that the commit still holds.
pub fn newest(root: &Path, commit: &str) -> Result<Option<Record>, Problem> {
    let pattern = format!(":(glob){DIRECTORY}/*.toml");
    let listing = base::git_text(
        root,
        &[
            "log",
            "--name-only",
            "--format=",
            "-z",
            commit,
            "--",
            &pattern,
        ],
    )?;
    for path in listing.split('\0').map(str::trim).filter(|p| !p.is_empty()) {
        let object = format!("{commit}:{path}");
        if !base::git(root, &["cat-file", "-e", &object])?
            .status
            .success()
        {
            continue;
        }
        let text = base::git_text(root, &["show", &object])?;
        let record =
            Record::parse(&text).map_err(|e| Problem(format!("{path} at {commit}: {e}")))?;
        return Ok(Some(record));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::Record;
    use std::collections::BTreeMap;

    fn sample(reason: &str) -> Record {
        Record {
            issue: 123,
            pr: 130,
            commit: "abc1234".to_string(),
            date: "2026-11-20".to_string(),
            host: "linux-x86_64".to_string(),
            proven: 417,
            promises: 812,
            passing: 7,
            steps: 19,
            reason: reason.to_string(),
            budgets: BTreeMap::from([
                ("binary_bytes".to_string(), 9312000.0),
                ("boot_ms".to_string(), 141.2),
                ("run_ms".to_string(), 930.0),
            ]),
        }
    }

    #[test]
    fn a_record_round_trips() {
        let record = sample("");
        assert_eq!(Record::parse(&record.render()).unwrap(), record);
    }

    #[test]
    fn a_reason_with_quotes_round_trips() {
        let record = sample("the \"new\" path\\here\nand a second line");
        assert_eq!(Record::parse(&record.render()).unwrap(), record);
    }

    #[test]
    fn the_format_matches_the_contract() {
        let mut record = sample("");
        record.budgets.remove("binary_bytes");
        assert_eq!(
            record.render(),
            "issue = 123\npr = 130\ncommit = \"abc1234\"\ndate = \"2026-11-20\"\nhost = \"linux-x86_64\"\nspec = { proven = 417, promises = 812 }\njourney = { passing = 7, steps = 19 }\nreason = \"\"\n\n[budgets]\nboot_ms = 141.2\nrun_ms = 930\n"
        );
    }

    #[test]
    fn a_missing_or_mistyped_field_is_an_error() {
        let good = sample("").render();
        for (from, to) in [
            ("issue = 123\n", ""),
            ("commit = \"abc1234\"\n", "commit = 5\n"),
            ("proven = 417", "proven = -1"),
            ("steps = 19", "steps = \"19\""),
            ("[budgets]\n", ""),
            ("boot_ms = 141.2\n", "boot_ms = \"fast\"\n"),
        ] {
            assert!(good.contains(from), "{from}");
            let text = good.replace(from, to);
            assert!(Record::parse(&text).is_err(), "{text}");
        }
    }
}
