//! What a merge accepts: whether a PR may merge, which checks its proof runs,
//! which counts fell, and what the owner's approval covers. `merge.rs` runs
//! the sequence around it: the timeout, the slot, the trees, and the statuses.
//! A change to this file waits for the owner, as `waits.rs` says.

use crate::forge::{Forge, Gh, PullRequest};
use crate::gate::{self, Failed, Merging};
use crate::ledger::{self, Record, Scores};
use crate::lock::Hold;
use crate::merge::{GOING, SLOT, WAITING};
use crate::{Problem, Verdict, base, bench, journey, lock, waits, work};
use std::collections::BTreeMap;
use std::path::Path;

const AUTO_MERGE: &str = "auto:merge";

pub enum Decision {
    Merged,
    Refused(String),
    Ready {
        pr: PullRequest,
        issue: u64,
        /// The owner actions that the PR's body names, each as a reason that
        /// it waits for the owner.
        actions: Vec<String>,
    },
}

/// Whether this PR may merge, from what GitHub says now. It writes nothing.
/// `merge` and `prepare` refuse a thread that doesn't hold the PR's issue.
pub fn decide(gh: &Gh, number: u64, mode: Mode, thread: Option<&str>) -> Result<Decision, Problem> {
    let prepare = mode != Mode::Merge;
    let pr = gh.pull_request(number)?;
    match pr.state.as_str() {
        "MERGED" => return Ok(Decision::Merged),
        "OPEN" => {}
        other => {
            return Err(Problem(format!(
                "PR {number} is {other}, so there is nothing to merge"
            )));
        }
    }
    let refused = |why: String| Ok(Decision::Refused(why));
    if pr.from_fork {
        return refused(format!(
            "PR {number} comes from a fork, and its branch isn't one this merge can rebase and push"
        ));
    }
    if pr.base_ref != "main" {
        return refused(format!(
            "PR {number} targets `{}`, and only `main` is merged here",
            pr.base_ref
        ));
    }
    let [issue] = pr.closing_issues[..] else {
        return refused(format!(
            "the body of PR {number} needs one `Closes #<issue>` line, because the ledger records one issue per merge"
        ));
    };
    if pr.changes_requested {
        return refused(format!(
            "a review of PR {number} requested changes. Address it, or dismiss it on GitHub"
        ));
    }
    if mode != Mode::Land
        && let Some(why) = work::refusal(
            &gh.issue(issue)?,
            &gh.owner()?,
            work::now_seconds()?,
            thread,
        )
    {
        return refused(why);
    }
    if !prepare && !gh.issue(issue)?.labels.iter().any(|l| l == AUTO_MERGE) {
        return refused(format!(
            "issue {issue} has no `{AUTO_MERGE}` label, so the owner merges PR {number}. Run `cargo xtask prepare {number}` instead"
        ));
    }
    let actions = match waits::owner_actions(number, &pr.body, || gh.open_issues())? {
        Ok(actions) => actions,
        Err(why) => return refused(why),
    };
    Ok(Decision::Ready { pr, issue, actions })
}

/// Whose rules the sequence follows, and how far it goes.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// An agent's rules, through the squash.
    Merge,
    /// The owner's rules, up to the squash.
    Prepare,
    /// The owner's rules, through the squash.
    Land,
}

impl Mode {
    pub fn command(self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::Prepare => "prepare",
            Mode::Land => "land",
        }
    }
}

/// How the comment that lists what merging accepts begins.
pub const ACCEPTS: &str = "Merging this PR at `";
/// What marks a reason that only `main`'s xtask accepted.
const BY_MAINS: &str = "by `main`'s xtask, ";

pub fn acceptance(head: &str, accepted: &[String]) -> String {
    let lines: Vec<String> = accepted.iter().map(|item| format!("- {item}")).collect();
    format!("{ACCEPTS}{head}` accepts:\n\n{}\n", lines.join("\n"))
}

/// The head and the reasons of each comment that `acceptance` wrote, oldest
/// first, from comment bodies one after another.
pub fn acceptances(comments: &str) -> Vec<(String, Vec<String>)> {
    let mut lists: Vec<(String, Vec<String>)> = Vec::new();
    let mut open = false;
    for line in comments.lines() {
        let head = line
            .strip_prefix(ACCEPTS)
            .and_then(|rest| rest.strip_suffix("` accepts:"));
        if let Some(head) = head {
            lists.push((head.to_string(), Vec::new()));
            open = true;
            continue;
        }
        let Some((_, reasons)) = lists.last_mut().filter(|_| open) else {
            continue;
        };
        match line.strip_prefix("- ") {
            Some(reason) => reasons.push(reason.to_string()),
            // The blank line under the heading.
            None if line.is_empty() && reasons.is_empty() => {}
            None => open = false,
        }
    }
    lists
}

/// What `accepted` has that the `approved` list doesn't cover, or all of it
/// when nothing was approved. A count that fell is covered by an approval of
/// a fall at least as big, measured on any `main`. Which xtask found a reason
/// doesn't matter.
pub fn unapproved(approved: Option<&[String]>, accepted: &[String]) -> Vec<String> {
    let approved = approved.unwrap_or_default();
    accepted
        .iter()
        .filter(|reason| {
            !approved.iter().any(|given| {
                plain(given) == plain(reason)
                    || matches!((fall(given), fall(reason)),
                        (Some((name, allowed)), Some((named, fell))) if name == named && fell <= allowed)
            })
        })
        .cloned()
        .collect()
}

fn plain(reason: &str) -> &str {
    reason.strip_prefix(BY_MAINS).unwrap_or(reason)
}

/// The count and how far it fell, from a reason that `compare_counts` wrote.
fn fall(reason: &str) -> Option<(&str, u64)> {
    let (name, counts) = plain(reason).split_once(" fell from ")?;
    let (from, to) = counts.split_once(" to ")?;
    let (from, to): (u64, u64) = (from.parse().ok()?, to.parse().ok()?);
    Some((name, from.checked_sub(to)?))
}

/// An owner who arms auto-merge approves what the PR's newest comment said
/// merging accepts, and GitHub merges an armed PR once it is green. So a proof
/// that accepts more turns auto-merge off before its green, and arming it
/// again approves the new comment. `earlier` is what was there before it.
pub fn hold_back(
    gh: &Gh,
    number: u64,
    command: &str,
    earlier: &str,
    accepted: &[String],
) -> Result<(), Problem> {
    let lists = acceptances(earlier);
    let new = unapproved(lists.last().map(|(_, reasons)| &reasons[..]), accepted);
    // Read after the comment, so no approval given before it stands. One
    // given just after it is taken back too, which keeps the PR open.
    if new.is_empty() || !gh.armed(number)?.auto_merge {
        return Ok(());
    }
    gh.disable_auto_merge(number)?;
    println!(
        "{command}: PR {number}'s auto-merge is off, because what the owner approved doesn't cover what merging it now accepts:"
    );
    for item in &new {
        println!("- {item}");
    }
    Ok(())
}

pub struct Scoring<'a> {
    pub number: u64,
    pub issue: u64,
    pub mode: Mode,
    pub thread: Option<&'a str>,
    pub pr: &'a PullRequest,
    /// What `Decision::Ready` says of the PR's owner actions.
    pub actions: &'a [String],
    pub gated: &'a str,
    /// The record whose budgets stand in for a bench.
    pub kept: Option<&'a Record>,
    /// The merge slot, when this process holds it, so that the proof's place
    /// comes ahead of every run that waits for one.
    pub slot: Option<&'a Hold>,
}

/// What the proof earned, and what merging it accepts.
pub struct Proof {
    pub record: Record,
    pub accepted: Vec<String>,
    /// The place the proof ran in, which a proof of the PR's own xtask keeps,
    /// unless a bench gave it up.
    pub place: Option<Hold>,
}

/// Runs the gate, the journey, and any bench on the rebased head, saying which
/// it is on, and returns the record they earn. If any check failed, the merge
/// must stop, and the reasons have been printed. `telling` hears what the
/// supervisor does: when the wait for a place and a shared hold of the
/// activity lock starts and ends, and when the bench's wait to hold the lock
/// alone does.
pub fn prove(
    tree: &Path,
    scoring: &Scoring,
    starting: &dyn Fn(&str),
    telling: &dyn Fn(u8) -> Result<(), Problem>,
) -> Result<Result<Proof, Failed>, Problem> {
    let Scoring {
        number, mode, pr, ..
    } = *scoring;
    let prepare = mode != Mode::Merge;
    let commit = base::find(tree)?
        .ok_or_else(|| Problem("there is no origin/main after the fetch".to_string()))?;
    let waiting = waits::reasons(tree, &commit, "HEAD", scoring.actions)?;
    if let Some(failed) = fails_early(tree, &commit, "HEAD", &waiting, number, &pr.body, mode)? {
        return Ok(Err(failed));
    }
    telling(WAITING)?;
    let place = lock::place(tree, scoring.slot)?;
    let mut hold = place.share()?;
    telling(SLOT)?;
    let merging = Merging { prepare, starting };
    let score = match gate::check(tree, &mut hold, Some(&merging))? {
        (failed, Some(score)) if failed.is_empty() => score,
        (failed, _) => return Ok(Err(failed)),
    };
    starting("journey");
    let (passing, steps) = journey::count(tree)?;
    let mut place = Some(hold.keep_place());

    // The base of a revert may be what doesn't build, and a revert puts back a
    // tree that was already measured.
    let revert = prepare && base::is_exact_revert(tree, &commit)?;
    let benched = !revert && scoring.kept.is_none() && budget_can_change(tree, &commit)?;
    if benched {
        // The bench holds the lock alone, and other runs can use the place
        // until then.
        place = None;
        starting("bench");
        // Having the lock started the timeout again, for this proof. The
        // bench's wait only stops it, so that the proof gets it once.
        let waiting = |waiting| telling(if waiting { WAITING } else { GOING });
        if bench::compare(tree, Some(&pr.body), &waiting)? == Verdict::Fail {
            return Ok(Err(vec!["bench"]));
        }
    }
    let scores = Scores {
        proven: score.proven,
        promises: score.promises.len(),
        passing,
        steps,
    };
    let mut record = ledger::compose(
        tree,
        scoring.issue as usize,
        scoring.number as usize,
        scoring.gated,
        Some(&commit),
        scores,
    )?;
    if let Some(kept) = scoring.kept {
        // A pinned size is the same on every machine, so the rebased tree's
        // stands, and the bench's results are kept.
        let measured = kept
            .budgets
            .iter()
            .filter(|(key, _)| !bench::is_pinned(key));
        record
            .budgets
            .extend(measured.map(|(key, value)| (key.clone(), *value)));
        record.reason = kept.reason.clone();
    }
    let mut accepted = waiting;
    match compare_counts(tree, &commit, &record, scoring, benched)? {
        Ok(fell) => accepted.extend(fell),
        Err(failed) => return Ok(Err(failed)),
    }
    if revert {
        accepted
            .push("an exact revert, so it can remove checks, and the bench didn't run".to_string());
    }
    Ok(Ok(Proof {
        record,
        accepted,
        place,
    }))
}

/// What fails a proof before its gate runs, comparing `head` with `commit`:
/// under `merge`, a PR that waits for the owner for the reasons in `waiting`,
/// and a body without the `Hill: none, <reason>` line that the PR needs
/// whatever the proof finds. It says why on stderr.
pub fn fails_early(
    root: &Path,
    commit: &str,
    head: &str,
    waiting: &[String],
    number: u64,
    body: &str,
    mode: Mode,
) -> Result<Option<Failed>, Problem> {
    let mut failed = Failed::new();
    if mode == Mode::Merge && !waiting.is_empty() {
        for reason in waiting {
            eprintln!("owner: {reason}");
        }
        eprintln!(
            "owner: so the owner merges PR {number}. Run `cargo xtask prepare {number}` instead"
        );
        gate::record("owner", false, &mut failed);
        return Ok(Some(failed));
    }
    if declares_no_hill(body) {
        return Ok(None);
    }
    if has_hill_line(body) {
        eprintln!(
            "hills: the `Hill:` line of PR {number} gives no reason, so its body needs a `Hill: none, <reason>` line"
        );
    } else if !can_raise_a_count(root, commit, head)? {
        eprintln!(
            "hills: PR {number} changes only paths that no count comes from, so its body needs a `Hill: none, <reason>` line"
        );
    } else {
        return Ok(None);
    }
    gate::record("hill-line", false, &mut failed);
    Ok(Some(failed))
}

/// The spec count is compared by the gate. This compares the two that need the
/// journey and the bench, and then asks whether anything rose. Returns the
/// counts that fell and are allowed to, or the checks that the PR fails here.
///
/// When the bench ran, the base's budgets come from what it measured for the
/// base, so a loaded machine slows both builds alike. Otherwise they come from
/// the base's ledger record, as the candidate's do.
fn compare_counts(
    tree: &Path,
    commit: &str,
    record: &Record,
    scoring: &Scoring,
    benched: bool,
) -> Result<Result<Vec<String>, Failed>, Problem> {
    let budgets = bench::read_budgets(tree)?;
    let before = ledger::newest(tree, commit)?;
    let (proven, passing) = before
        .as_ref()
        .map_or((0, 0), |base| (base.proven, base.passing));
    let measured = if benched {
        ledger::measured_base(tree, scoring.gated, commit)?
    } else {
        None
    };
    let budgets_met = base_values(measured.as_ref(), before.as_ref())
        .map_or(0, |values| bench::met(&budgets, values).0);
    let met_now = bench::met(&budgets, &record.budgets).0;

    let mut failed = Failed::new();
    let mut accepted = Vec::new();
    if record.proven < proven {
        accepted.push(format!("spec fell from {proven} to {}", record.proven));
    }
    for (name, before, after) in [
        ("journey", passing, record.passing),
        ("budgets", budgets_met, met_now),
    ] {
        if after < before {
            eprintln!(
                "{name}: the count fell from {before} to {after}. Restore it, or hand the PR to the owner"
            );
            accepted.push(format!("{name} fell from {before} to {after}"));
        }
        gate::record(
            name,
            after >= before || scoring.mode != Mode::Merge,
            &mut failed,
        );
    }
    let rose = record.proven > proven || record.passing > passing || met_now > budgets_met;
    if !rose && !declares_no_hill(&scoring.pr.body) {
        eprintln!(
            "hills: no count rose, so the PR body needs a `Hill: none, <reason>` line, or a change that raises one"
        );
        gate::record("hill-line", false, &mut failed);
    }
    Ok(if failed.is_empty() {
        Ok(accepted)
    } else {
        Err(failed)
    })
}

/// The base's budget values: what this run's bench measured for it, or else
/// its ledger record. A budget the bench couldn't measure for the base has no
/// value, so it isn't met, whatever the record says.
fn base_values<'a>(
    measured: Option<&'a BTreeMap<String, f64>>,
    before: Option<&'a Record>,
) -> Option<&'a BTreeMap<String, f64>> {
    measured.or(before.map(|base| &base.budgets))
}

fn declares_no_hill(body: &str) -> bool {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("Hill: none,"))
        .any(|reason| !reason.trim().is_empty())
}

/// Whether the body has a `Hill:` line, which `declares_no_hill` may not take.
fn has_hill_line(body: &str) -> bool {
    body.lines().any(|line| line.trim().starts_with("Hill:"))
}

/// What no count is read from: a path, or a directory that ends in `/`.
const NO_COUNT: [&str; 5] = [
    "AGENTS.md",
    "HARNESS.md",
    "harness/tasks/",
    ".github/",
    "xtask/tests/",
];

/// Whether `head` changes a path that a count can come from.
fn can_raise_a_count(root: &Path, commit: &str, head: &str) -> Result<bool, Problem> {
    let args = ["diff", "--name-only", "--no-renames", "-z", commit, head];
    let changed = base::git_text(root, &args)?;
    Ok(changed
        .split('\0')
        .filter(|path| !path.is_empty())
        .any(|path| {
            !NO_COUNT
                .iter()
                .any(|quiet| path == *quiet || (quiet.ends_with('/') && path.starts_with(quiet)))
        }))
}

/// Whether the change can move a budget: the product's code, what it is built
/// from, how it is built, what it boots, or how a budget is measured.
fn budget_can_change(tree: &Path, commit: &str) -> Result<bool, Problem> {
    let args = ["diff", "--name-only", "--no-renames", "-z", commit, "HEAD"];
    let changed = base::git_text(tree, &args)?;
    Ok(changed.split('\0').any(affects_a_budget))
}

/// The packages under `crates/` that `ap` and `fake-harness` are built from,
/// which the bench builds. A test fails while a package is on neither list.
#[cfg(test)]
const BENCHED: [&str; 11] = [
    "agentfile",
    "ap",
    "erofs",
    "fake-harness",
    "git",
    "guest",
    "net",
    "netstack",
    "share",
    "store",
    "vmm",
];
/// The packages under `crates/` that neither is built from.
const NOT_BENCHED: [&str; 5] = ["fake-upstream", "fake-vm", "fuzz", "t1", "t2"];

/// A path under `crates/` affects a budget unless it is in a package's
/// `tests/`, or in a package that `NOT_BENCHED` names. A package on neither
/// list does, until a test puts it on one.
fn affects_a_budget(path: &str) -> bool {
    if let Some(inside) = path.strip_prefix("crates/") {
        let (package, rest) = inside.split_once('/').unwrap_or((inside, ""));
        return !NOT_BENCHED.contains(&package) && !rest.starts_with("tests/");
    }
    path.starts_with(".cargo/")
        || path.starts_with("guest/kernel/")
        || [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "harness/budgets.toml",
            "xtask/src/bench.rs",
        ]
        .contains(&path)
}

/// Whether the PR changes the xtask that proves it. Its tests aren't part of
/// it, and the gate runs them either way.
fn changes_xtask(tree: &Path, main: &str) -> Result<bool, Problem> {
    let args = [
        "diff",
        "--name-only",
        main,
        "HEAD",
        "--",
        "xtask/",
        ":(exclude)xtask/tests/",
    ];
    Ok(!base::git_text(tree, &args)?.trim().is_empty())
}

impl Proof {
    /// This proof, by the PR's own xtask, after `main`'s. Its record is the
    /// one kept, and it names what `main`'s xtask accepted that it didn't.
    pub fn after(mut self, mains: Proof) -> Proof {
        for item in mains.accepted {
            if !self.accepted.contains(&item) {
                self.accepted.push(format!("{BY_MAINS}{item}"));
            }
        }
        self
    }
}

/// Whether the PR's own xtask proves it too, after `main`'s. The xtask of the
/// rebased tree is what measures `main` after the merge. An exact revert puts
/// back an xtask that was already measured, and that may not know how to
/// `prove`.
pub fn proves_with_its_own(tree: &Path, main: &str, mode: Mode) -> Result<bool, Problem> {
    Ok(changes_xtask(tree, main)? && !(mode != Mode::Merge && base::is_exact_revert(tree, main)?))
}

#[cfg(test)]
mod tests {
    use super::{BENCHED, NOT_BENCHED, acceptance, acceptances, affects_a_budget, unapproved};

    #[test]
    fn only_what_ap_and_the_fake_harness_are_built_from_can_change_a_budget() {
        for path in [
            "crates/ap/src/main.rs",
            "crates/ap/Cargo.toml",
            "crates/ap/build.rs",
            "crates/fake-harness/src/main.rs",
            "crates/git/src/lib.rs",
            "crates/vmm/src/tests.rs",
            "crates/new/src/lib.rs",
            "Cargo.lock",
            "xtask/src/bench.rs",
        ] {
            assert!(affects_a_budget(path), "{path}");
        }
        for path in [
            "crates/ap/tests/run.rs",
            "crates/git/tests/data/pack",
            "crates/t1/src/lib.rs",
            "crates/t2/tests/claude.rs",
            "crates/fuzz/src/lib.rs",
            "crates/fake-upstream/src/main.rs",
            "crates/fake-vm/Cargo.toml",
            "xtask/src/merge.rs",
            "docs.txt",
        ] {
            assert!(!affects_a_budget(path), "{path}");
        }
    }

    /// A new package has to be put on one side of the rule.
    #[test]
    fn every_package_under_crates_is_benched_or_not() {
        let root = crate::workspace::root().unwrap();
        let crates = root.join("crates");
        let mut named = 0;
        for package in crate::workspace::packages(&root).unwrap() {
            let Ok(directory) = package.directory.strip_prefix(&crates) else {
                continue;
            };
            let directory = directory.to_string_lossy();
            let sides = [
                BENCHED.contains(&&*directory),
                NOT_BENCHED.contains(&&*directory),
            ];
            assert_eq!(
                sides.iter().filter(|side| **side).count(),
                1,
                "`crates/{directory}` must be in exactly one of `BENCHED` and `NOT_BENCHED`"
            );
            named += 1;
        }
        assert_eq!(named, BENCHED.len() + NOT_BENCHED.len());
    }

    fn reasons(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn each_acceptance_comment_is_read_with_its_head_and_reasons() {
        let first = acceptance(
            "aaa",
            &reasons(&["it changes `x`", "journey fell from 2 to 1"]),
        );
        let second = acceptance("bbb", &reasons(&["spec fell from 3 to 2"]));
        let other = "The refresh couldn't prove this PR: Failed: lint\n\n- a list\n";
        let quoted = "> Merging this PR at `ccc` accepts:\n";
        let comments = format!("{first}\n{other}\n{second}\n{quoted}\n");
        assert_eq!(
            acceptances(&comments),
            [
                (
                    "aaa".to_string(),
                    reasons(&["it changes `x`", "journey fell from 2 to 1"])
                ),
                ("bbb".to_string(), reasons(&["spec fell from 3 to 2"])),
            ]
        );
        assert!(acceptances("").is_empty());
    }

    #[test]
    fn only_what_the_approval_did_not_cover_is_new() {
        let accepted = reasons(&["it changes `x`", "journey fell from 9 to 8"]);
        assert_eq!(unapproved(None, &accepted), accepted);
        assert!(unapproved(Some(&accepted), &accepted).is_empty());
        let fewer = reasons(&["journey fell from 9 to 8"]);
        let more = reasons(&[
            "journey fell from 9 to 8",
            "it changes `y`",
            "it changes `x`",
        ]);
        assert!(unapproved(Some(&more), &fewer).is_empty());
        assert_eq!(
            unapproved(Some(&fewer), &accepted),
            reasons(&["it changes `x`"])
        );
        // A fall as big on a newer `main` is the same fall, and a bigger one
        // isn't.
        let approved = reasons(&["journey fell from 9 to 8"]);
        assert!(unapproved(Some(&approved), &reasons(&["journey fell from 10 to 9"])).is_empty());
        let further = reasons(&["journey fell from 9 to 7"]);
        assert_eq!(unapproved(Some(&approved), &further), further);
        let other = reasons(&["spec fell from 9 to 8"]);
        assert_eq!(unapproved(Some(&approved), &other), other);
        // Which xtask found it doesn't matter.
        let mains = reasons(&["by `main`'s xtask, journey fell from 9 to 8"]);
        assert!(unapproved(Some(&approved), &mains).is_empty());
        assert!(unapproved(Some(&mains), &approved).is_empty());
    }
}
