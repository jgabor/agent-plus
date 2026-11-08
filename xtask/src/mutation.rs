//! The nightly mutation step: changes the code that changed since the last
//! report, and files an issue for each crate where a check didn't notice.

use crate::forge::{Gh, OpenIssue};
use crate::workspace::{self, Package};
use crate::{Problem, Verdict, base, limit, report};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

const SECONDS: u64 = 3600;
const SECONDS_OVERRIDE: &str = "XTASK_MUTATION_SECONDS";
const OUTPUT: &str = "target/mutants";
const MAX_LISTED: usize = 100;
/// `cargo mutants` ran to the end: nothing missed, missed, or timed out.
const COMPLETED: [i32; 3] = [0, 2, 3];
/// The harness's own packages, which fakes join by their `fake-` prefix.
const NOT_PRODUCT: [&str; 4] = ["fuzz", "t1", "t2", "xtask"];

/// `None` when nothing it covers has changed. Survivors become issues, so they
/// don't fail the night.
pub fn run(root: &Path) -> Result<Option<Verdict>, Problem> {
    let gh = Gh::new(root);
    let issues = gh.open_issues()?;
    let changed = changed_packages(root, &gh, &issues)?;
    if changed.is_empty() {
        return Ok(None);
    }
    let output = root.join(OUTPUT);
    if let Err(error) = fs::remove_dir_all(&output)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(Problem(format!("couldn't clear {OUTPUT}: {error}")));
    }
    // A killed run can't clean up its copies of the tree. Here, the next run does.
    let temp = output.join("tmp");
    fs::create_dir_all(&temp).map_err(|e| Problem(format!("couldn't create {OUTPUT}: {e}")))?;
    let limit = limit::seconds(SECONDS_OVERRIDE, SECONDS)?;
    let finished = limit::run(
        root,
        Path::new("cargo"),
        &arguments(&changed),
        &[("TMPDIR", temp.as_os_str())],
        limit,
    )?;
    if finished.timed_out {
        println!("mutation: stopped at the {limit} second limit");
    } else if !finished
        .status
        .code()
        .is_some_and(|code| COMPLETED.contains(&code))
    {
        return Err(Problem(format!(
            "`cargo mutants` didn't finish: {}",
            finished.status
        )));
    }
    let missed = match fs::read_to_string(output.join("mutants.out/missed.txt")) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Problem(format!("couldn't read the survivors: {error}"))),
    };
    for (package, directory) in &changed {
        file_issue(&gh, &issues, &package.name, directory, &missed)?;
    }
    Ok(Some(Verdict::Pass))
}

/// The product's crates touched since the last report, sorted by name, each
/// with its directory relative to `root`.
fn changed_packages(
    root: &Path,
    gh: &Gh,
    issues: &[OpenIssue],
) -> Result<Vec<(Package, String)>, Problem> {
    let start = report::mutation_start(root, gh, issues)?;
    let files = base::git_text(root, &["log", &start, "--name-only", "--format=", "HEAD"])?;
    let mut changed = Vec::new();
    for package in workspace::packages(root)? {
        let Ok(directory) = package.directory.strip_prefix(root) else {
            continue;
        };
        let directory = directory.to_string_lossy().into_owned();
        let prefix = format!("{directory}/");
        if !package.name.starts_with("fake-")
            && !NOT_PRODUCT.contains(&package.name.as_str())
            && files.lines().any(|file| file.starts_with(&prefix))
        {
            changed.push((package, directory));
        }
    }
    changed.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
    Ok(changed)
}

fn arguments(changed: &[(Package, String)]) -> Vec<String> {
    let mut arguments: Vec<String> = ["mutants", "--colors", "never", "--no-shuffle", "-o", OUTPUT]
        .map(String::from)
        .to_vec();
    for (package, _) in changed {
        arguments.push("-p".to_string());
        arguments.push(package.name.clone());
    }
    arguments
}

fn file_issue(
    gh: &Gh,
    issues: &[OpenIssue],
    name: &str,
    directory: &str,
    missed: &str,
) -> Result<(), Problem> {
    let prefix = format!("{directory}/");
    let survivors: Vec<&str> = missed
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    let title = format!("Mutants survive in {name}");
    if survivors.is_empty() || issues.iter().any(|issue| issue.title == title) {
        return Ok(());
    }
    let mut list = String::new();
    for survivor in survivors.iter().take(MAX_LISTED) {
        list.push_str(&format!("- `{survivor}`\n"));
    }
    if survivors.len() > MAX_LISTED {
        list.push_str(&format!("- and {} more\n", survivors.len() - MAX_LISTED));
    }
    let body = format!(
        "## Goal\nA check notices each of these changes to `{name}`, or the code they change is gone.\n\n## Done when\n`cargo mutants -p {name}` no longer reports these as missed:\n\n{list}\n## Touches\n`{directory}`\n"
    );
    gh.create_issue(&title, &body)?;
    Ok(())
}
