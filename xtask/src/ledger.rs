//! `harness/ledger/`: the scores and budgets recorded for each merge.

use crate::toml::{self, Table, Value};
use crate::{Problem, Verdict, base, bench, journey, lock, score, testrun, workspace};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

pub const DIRECTORY: &str = "harness/ledger";
const MEASURED: &str = "target/bench/result.toml";

#[derive(Debug, PartialEq)]
pub struct Record {
    pub issue: usize,
    pub pr: usize,
    commit: String,
    /// The `main` commit this was measured on. Older records have none.
    base: Option<String>,
    date: String,
    host: String,
    pub proven: usize,
    pub promises: usize,
    pub passing: usize,
    pub steps: usize,
    pub reason: String,
    pub budgets: BTreeMap<String, f64>,
    /// Where the proof's time went. Older records have none.
    pub seconds: Option<Seconds>,
    /// What the proof fetched. Older records have none.
    pub downloads: Option<Downloads>,
}

/// Where a proof's time went, in seconds. It decides nothing.
#[derive(Debug, PartialEq)]
pub struct Seconds {
    /// Each wait, then each phase that ran, as `lint` or `own_xtask`.
    pub spent: Vec<(String, f64)>,
    /// Each journey step that ran, by its id.
    pub steps: Vec<(i64, f64)>,
    /// The test programs that took longest, longest first.
    pub programs: Vec<Program>,
}

/// One test program that a package's `cargo test` ran, named as cargo names
/// it, such as `tests/ledger.rs`.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub package: String,
    pub name: String,
    pub seconds: f64,
}

/// What the journey and the bench fetched: the pinned OS layers that their
/// stores gained, and the other files that they and the journey's download
/// cache gained.
#[derive(Debug, PartialEq)]
pub struct Downloads {
    pub layers: usize,
    pub files: usize,
}

impl Seconds {
    fn render(&self) -> String {
        let mut text = String::new();
        for (name, seconds) in &self.spent {
            text.push_str(&format!("{name} = {}\n", tenths(*seconds)));
        }
        let steps: Vec<String> = self
            .steps
            .iter()
            .map(|(id, seconds)| format!("{id} = {}", tenths(*seconds)))
            .collect();
        text.push_str(&format!("steps = {{ {} }}\n", steps.join(", ")));
        let programs: Vec<String> = self
            .programs
            .iter()
            .map(|program| {
                format!(
                    "{{ package = {}, name = {}, seconds = {} }}",
                    toml::quoted(&program.package),
                    toml::quoted(&program.name),
                    tenths(program.seconds)
                )
            })
            .collect();
        text.push_str(&format!("programs = [{}]\n", programs.join(", ")));
        text
    }

    fn parse(table: &Table) -> Result<Seconds, String> {
        let mut seconds = Seconds {
            spent: Vec::new(),
            steps: Vec::new(),
            programs: Vec::new(),
        };
        for (name, value) in table.entries() {
            match (name, value) {
                ("steps", Value::Table(steps)) => {
                    for (key, value) in steps.entries() {
                        let id = key
                            .parse()
                            .map_err(|_| format!("step `{key}` isn't an id"))?;
                        seconds.steps.push((id, number(value, key)?));
                    }
                }
                ("programs", Value::Arr(programs)) => {
                    for program in programs {
                        let Value::Table(program) = program else {
                            return Err("a program isn't a table".to_string());
                        };
                        seconds.programs.push(Program {
                            package: string(program, "package")?,
                            name: string(program, "name")?,
                            seconds: match program.get("seconds") {
                                Some(value) => number(value, "seconds")?,
                                None => return Err("a program has no `seconds`".to_string()),
                            },
                        });
                    }
                }
                (name, value) => seconds.spent.push((name.to_string(), number(value, name)?)),
            }
        }
        Ok(seconds)
    }

    /// The lines that `merge` prints once a proof ends.
    pub fn show(&self) -> String {
        let mut text = "where the proof's time went, in seconds:\n".to_string();
        let mut line = |what: String, seconds: f64| {
            text.push_str(&format!("  {what} {}\n", tenths(seconds)));
        };
        for (name, seconds) in &self.spent {
            line(name.replace('_', " "), *seconds);
        }
        for (id, seconds) in &self.steps {
            line(format!("step {id}"), *seconds);
        }
        for program in &self.programs {
            line(
                format!("{} {}", program.package, program.name),
                program.seconds,
            );
        }
        text
    }
}

impl Downloads {
    pub fn show(&self) -> String {
        let plural = |count: usize, word: &str| match count {
            1 => format!("1 {word}"),
            _ => format!("{count} {word}s"),
        };
        format!(
            "the proof fetched {} and {}",
            plural(self.layers, "layer"),
            plural(self.files, "file")
        )
    }
}

/// Seconds to a tenth, which is all a slow step needs.
fn tenths(seconds: f64) -> String {
    format!("{seconds:.1}")
}

fn number(value: &Value, name: &str) -> Result<f64, String> {
    match value {
        Value::Int(number) => Ok(*number as f64),
        Value::Float(number) => Ok(*number),
        _ => Err(format!("`{name}` isn't a number")),
    }
}

impl Record {
    fn render(&self) -> String {
        let base = self.base.as_deref().map_or(String::new(), |base| {
            format!("base = {}\n", toml::quoted(base))
        });
        let mut text = format!(
            "issue = {}\npr = {}\ncommit = {}\n{base}date = {}\nhost = {}\nspec = {{ proven = {}, promises = {} }}\njourney = {{ passing = {}, steps = {} }}\nreason = {}\n\n[budgets]\n",
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
        if let Some(seconds) = &self.seconds {
            text.push_str("\n[seconds]\n");
            text.push_str(&seconds.render());
        }
        if let Some(Downloads { layers, files }) = &self.downloads {
            text.push_str(&format!(
                "\n[downloads]\nlayers = {layers}\nfiles = {files}\n"
            ));
        }
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
            base: match table.get("base") {
                Some(_) => Some(string(&table, "base")?),
                None => None,
            },
            date: string(&table, "date")?,
            host: string(&table, "host")?,
            proven: count(spec, "proven")?,
            promises: count(spec, "promises")?,
            passing: count(journey, "passing")?,
            steps: count(journey, "steps")?,
            reason: string(&table, "reason")?,
            budgets: read_values(&table, "budgets")?,
            seconds: optional(&table, "seconds", Seconds::parse)?,
            downloads: optional(&table, "downloads", |downloads| {
                Ok(Downloads {
                    layers: count(downloads, "layers")?,
                    files: count(downloads, "files")?,
                })
            })?,
        })
    }
}

/// What a passing bench measured, kept for `record`, and for the counts that
/// `merge` compares.
#[derive(Debug, PartialEq)]
pub struct Measured {
    pub commit: String,
    /// The `main` commit whose build was the base.
    pub base: String,
    pub reason: String,
    /// The candidate's values.
    pub budgets: BTreeMap<String, f64>,
    /// The base's values from the same run. A budget the base can't be
    /// measured for has none.
    pub base_budgets: BTreeMap<String, f64>,
}

impl Measured {
    fn render(&self) -> String {
        let mut text = format!(
            "commit = {}\nbase = {}\nreason = {}\n\n[budgets]\n",
            toml::quoted(&self.commit),
            toml::quoted(&self.base),
            toml::quoted(&self.reason),
        );
        push_budgets(&mut text, &self.budgets);
        text.push_str("\n[base_budgets]\n");
        push_budgets(&mut text, &self.base_budgets);
        text
    }

    fn parse(text: &str) -> Result<Measured, String> {
        let table = toml::document(text)?;
        Ok(Measured {
            commit: string(&table, "commit")?,
            base: string(&table, "base")?,
            reason: string(&table, "reason")?,
            budgets: read_values(&table, "budgets")?,
            base_budgets: read_values(&table, "base_budgets")?,
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

/// What the bench measured for the base, if the last bench was of `commit`
/// against `base`.
pub fn measured_base(
    root: &Path,
    commit: &str,
    base: &str,
) -> Result<Option<BTreeMap<String, f64>>, Problem> {
    Ok(read_measured(root, commit)?
        .filter(|measured| measured.base == base)
        .map(|measured| measured.base_budgets))
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

fn read_values(table: &Table, key: &str) -> Result<BTreeMap<String, f64>, String> {
    let mut budgets = BTreeMap::new();
    for (name, value) in section(table, key)?.entries() {
        let value = number(value, name).map_err(|e| format!("budget {e}"))?;
        budgets.insert(name.to_string(), value);
    }
    Ok(budgets)
}

/// The table under `key`, read with `parse`, or `None` if there is none.
fn optional<T>(
    table: &Table,
    key: &str,
    parse: impl Fn(&Table) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match table.get(key) {
        Some(_) => section(table, key).and_then(parse).map(Some),
        None => Ok(None),
    }
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
    let mut hold = lock::testing(&root)?;
    let changed = changes(&root)?;
    if !changed.is_empty() {
        return Err(Problem(format!(
            "the tree differs from HEAD, so its scores aren't that commit's. Commit or discard these first:\n{}",
            changed.trim_end()
        )));
    }
    let commit = base::git_text(&root, &["rev-parse", "HEAD"])?;

    let results = testrun::run(&root, &mut hold)?;
    let score = score::compute(&root, &results)?;
    let (passing, steps) = journey::count(&root)?;
    let scores = Scores {
        proven: score.proven,
        promises: score.promises.len(),
        passing,
        steps,
    };
    let measured_on = base::find(&root)?;
    let record = compose(
        &root,
        issue,
        pr,
        commit.trim(),
        measured_on.as_deref(),
        scores,
    )?;
    println!("{}", store(&root, &record)?);
    Ok(Verdict::Pass)
}

/// What a commit earned, from runs that have already happened.
pub struct Scores {
    pub proven: usize,
    pub promises: usize,
    pub passing: usize,
    pub steps: usize,
}

/// The record for `commit`. Budgets that weren't measured keep their last
/// values, and the bench's result is used only if it was for this commit. The
/// sizes of the pinned guest files are read every time.
pub fn compose(
    root: &Path,
    issue: usize,
    pr: usize,
    commit: &str,
    base: Option<&str>,
    scores: Scores,
) -> Result<Record, Problem> {
    let date = base::git_text(root, &["show", "-s", "--format=%cs", commit])?;
    let mut budgets = at_head(root)?.map_or_else(BTreeMap::new, |previous| previous.budgets);
    let mut reason = String::new();
    if let Some(measured) = read_measured(root, commit)? {
        budgets.extend(measured.budgets);
        reason = measured.reason;
    }
    budgets.extend(bench::pinned_sizes(root, commit)?);
    Ok(Record {
        issue,
        pr,
        commit: commit.to_string(),
        base: base.map(str::to_string),
        date: date.trim().to_string(),
        host: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        proven: scores.proven,
        promises: scores.promises,
        passing: scores.passing,
        steps: scores.steps,
        reason,
        budgets,
        seconds: None,
        downloads: None,
    })
}

pub fn path(issue: usize) -> String {
    format!("{DIRECTORY}/{issue}.toml")
}

/// Writes the record, and returns its path from the root.
pub fn store(root: &Path, record: &Record) -> Result<String, Problem> {
    let path = path(record.issue);
    let problem = |e: std::io::Error| Problem(format!("couldn't write {path}: {e}"));
    fs::create_dir_all(root.join(DIRECTORY)).map_err(problem)?;
    fs::write(root.join(&path), record.render()).map_err(problem)?;
    Ok(path)
}

/// The record for this issue that the tree holds.
pub fn read(root: &Path, issue: usize) -> Result<Record, Problem> {
    read_file(&root.join(path(issue)))
}

pub fn read_file(path: &Path) -> Result<Record, Problem> {
    let shown = path.display();
    let text =
        fs::read_to_string(path).map_err(|e| Problem(format!("couldn't read {shown}: {e}")))?;
    Record::parse(&text).map_err(|e| Problem(format!("{shown}: {e}")))
}

/// Writes a record to a file of its own, outside the ledger.
pub fn write_file(path: &Path, record: &Record) -> Result<(), Problem> {
    fs::write(path, record.render())
        .map_err(|e| Problem(format!("couldn't write {}: {e}", path.display())))
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
        let record = read_at(root, commit, path)?;
        let args = ["log", "-1", "--format=%H", commit, "--", path];
        let written = base::git_text(root, &args)?;
        if !merged_after_main_moved(root, written.trim(), &record)? {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

/// Whether a record was measured on another `main` than the one `commit`, its
/// squash, landed on. What it says then isn't what that `main` plus the PR
/// scores, so it can't be anyone's floor. A record not yet squashed sits right
/// on the head it measured, and is what its own branch should read.
pub fn merged_after_main_moved(
    root: &Path,
    commit: &str,
    record: &Record,
) -> Result<bool, Problem> {
    let Some(measured) = &record.base else {
        return Ok(false);
    };
    let parent = format!("{commit}~1");
    if !base::commit_exists(root, &parent)? {
        return Ok(true);
    }
    let parent = base::git_text(root, &["rev-parse", &parent])?;
    Ok(parent.trim() != measured && parent.trim() != record.commit)
}

/// The record this commit wrote, if it wrote one.
pub fn of_commit(root: &Path, commit: &str) -> Result<Option<Record>, Problem> {
    let pattern = format!(":(glob){DIRECTORY}/*.toml");
    let listing = base::git_text(
        root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            "--diff-filter=AM",
            commit,
            "--",
            &pattern,
        ],
    )?;
    match listing.split('\0').find(|path| !path.is_empty()) {
        Some(path) => read_at(root, commit, path).map(Some),
        None => Ok(None),
    }
}

fn read_at(root: &Path, commit: &str, path: &str) -> Result<Record, Problem> {
    let text = base::git_text(root, &["show", &format!("{commit}:{path}")])?;
    Record::parse(&text).map_err(|e| Problem(format!("{path} at {commit}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{Downloads, Measured, Program, Record, Seconds};
    use std::collections::BTreeMap;

    fn sample(reason: &str) -> Record {
        Record {
            issue: 123,
            pr: 130,
            commit: "abc1234".to_string(),
            base: None,
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
            seconds: None,
            downloads: None,
        }
    }

    #[test]
    fn a_bench_result_keeps_the_bases_values() {
        let measured = Measured {
            commit: "abc1234".to_string(),
            base: "def5678".to_string(),
            reason: String::new(),
            budgets: BTreeMap::from([
                ("boot_ms".to_string(), 277.0),
                ("version_ms".to_string(), 1.5),
            ]),
            base_budgets: BTreeMap::from([("version_ms".to_string(), 1.25)]),
        };
        assert_eq!(Measured::parse(&measured.render()).unwrap(), measured);
    }

    #[test]
    fn a_record_round_trips() {
        let record = sample("");
        assert_eq!(Record::parse(&record.render()).unwrap(), record);
    }

    #[test]
    fn a_base_round_trips_after_the_commit() {
        let mut record = sample("");
        record.base = Some("def5678".to_string());
        let text = record.render();
        assert!(
            text.contains("commit = \"abc1234\"\nbase = \"def5678\"\ndate ="),
            "{text}"
        );
        assert_eq!(Record::parse(&text).unwrap(), record);
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

    fn spent() -> (Seconds, Downloads) {
        let seconds = Seconds {
            spent: vec![
                ("slot".to_string(), 12.3),
                ("place".to_string(), 0.0),
                ("lint".to_string(), 41.24),
            ],
            steps: vec![(1, 0.3), (5, 30.0)],
            programs: vec![Program {
                package: "xtask".to_string(),
                name: "tests/nightly.rs".to_string(),
                seconds: 41.2,
            }],
        };
        (
            seconds,
            Downloads {
                layers: 1,
                files: 3,
            },
        )
    }

    #[test]
    fn where_the_time_went_round_trips_after_the_budgets() {
        let mut record = sample("");
        let (seconds, downloads) = spent();
        record.seconds = Some(seconds);
        record.downloads = Some(downloads);
        let text = record.render();
        assert!(
            text.ends_with(
                "run_ms = 930\n\n[seconds]\nslot = 12.3\nplace = 0.0\nlint = 41.2\nsteps = { 1 = 0.3, 5 = 30.0 }\nprograms = [{ package = \"xtask\", name = \"tests/nightly.rs\", seconds = 41.2 }]\n\n[downloads]\nlayers = 1\nfiles = 3\n"
            ),
            "{text}"
        );
        let read = Record::parse(&text).unwrap();
        record.seconds.as_mut().unwrap().spent[2].1 = 41.2;
        assert_eq!(read, record);
    }

    #[test]
    fn a_record_without_the_time_or_the_downloads_reads() {
        let text = sample("").render();
        assert!(!text.contains("[seconds]") && !text.contains("[downloads]"));
        let record = Record::parse(&text).unwrap();
        assert_eq!((record.seconds, record.downloads), (None, None));
    }

    #[test]
    fn a_missing_or_mistyped_field_is_an_error() {
        let good = sample("").render();
        for (from, to) in [
            ("issue = 123\n", ""),
            ("date =", "base = 5\ndate ="),
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
