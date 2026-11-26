use super::floor::revert_of;
use super::prepare::last_post;
use super::waits::changing;
use super::*;

/// How many times a merge built an xtask.
pub(super) fn builds(repo: &Repo) -> usize {
    repo.calls()
        .matches("cargo build --quiet --package xtask")
        .count()
}

impl Repo {
    /// Runs another xtask from this checkout, as a checkout whose xtask isn't
    /// `main`'s would.
    fn run_as(&self, binary: &Path, args: &[&str]) -> Output {
        self.command(binary).args(args).output().unwrap()
    }

    /// The record at the head of the PR's branch.
    fn record_at_head(&self) -> String {
        self.origin(&["show", &format!("{}:harness/ledger/7.toml", self.head())])
    }
}

const ONE_PASSING: &str = "journey = { passing = 1, steps = 1 }";

/// The checkout's xtask runs a journey runner that fails every step, which
/// `main`'s doesn't. Each command proves the PR with `main`'s.
#[track_caller]
fn assert_proven_by_mains_xtask(name: &str, checkout: impl Fn(&Repo)) {
    let failing = failing();
    for command in ["merge", "prepare", "land"] {
        let repo = Repo::new(&format!("{name}-{command}"));
        checkout(&repo);
        repo.forget_calls();
        let output = repo.run_as(failing, &[command, "9"]);
        if command == "prepare" {
            assert_prepared(&repo, &output);
        } else {
            assert_merged(&repo, &output);
        }
        let record = repo.record_at_head();
        assert!(record.contains(ONE_PASSING), "{command}: {record}");
        assert_eq!(repo.pr_comments(), "", "{command}");
        assert_eq!(builds(&repo), 1, "{command}: {}", repo.calls());
    }
}

#[test]
fn a_checkout_behind_main_proves_a_pr_with_the_xtask_of_main() {
    assert_proven_by_mains_xtask("checks-behind", |repo| {
        repo.advance_main("other.txt", "y\n");
    });
}

#[test]
fn a_checkout_with_changes_under_xtask_proves_a_pr_with_the_xtask_of_main() {
    assert_proven_by_mains_xtask("checks-changed", |repo| {
        repo.git(&["checkout", "-q", "main"]);
        repo.write("xtask/src/journey.rs", "// variant: failing\n");
    });
}

#[test]
fn a_checkout_of_main_proves_a_pr_with_the_xtask_of_main() {
    assert_proven_by_mains_xtask("checks-main", |repo| {
        repo.git(&["checkout", "-q", "main"]);
    });
}

/// The PR's own xtask lets `.github/` through, but `main`'s decides.
#[test]
fn a_pr_that_lets_itself_through_still_waits() {
    let repo = changing(
        "checks-unprotect",
        &[],
        &[
            ("xtask/src/merge.rs", "// variant: unprotected\n"),
            (".github/workflows/x.yml", "name: x\n"),
        ],
    );
    let output = repo.run_as(unprotected(), &["merge", "9"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("fail owner"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("it changes `.github/workflows/x.yml`"),
        "{}",
        stderr(&output)
    );
    assert!(!repo.bin.join("merged").exists());
    assert_eq!(last_post(&repo), "failure Failed: owner");
}

/// What `main` measures after the merge is what its own runner says.
#[test]
fn a_pr_that_changes_the_journey_runner_is_recorded_by_its_own() {
    failing();
    let base = record(0, 1, "");
    let repo = Repo::with(
        "checks-own-runner",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("xtask/src/journey.rs", "// variant: failing\n")],
    );
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    let record = repo.record_at_head();
    assert!(
        record.contains("journey = { passing = 0, steps = 1 }"),
        "{record}"
    );
    let comments = repo.pr_comments();
    assert!(comments.contains("journey fell from 1 to 0"), "{comments}");
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 0 of 1");
    assert_eq!(builds(&repo), 2, "{}", repo.calls());
}

/// A merge that lands while this one builds the checks changes them.
#[test]
fn a_merge_builds_the_xtask_of_main_again_if_main_moves() {
    let repo = Repo::new("checks-moved");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("later.txt", "x\n");
    repo.commit();
    let later = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.git(&["push", "-q", "origin", "HEAD:refs/heads/spare"]);
    repo.git(&["checkout", "-q", BRANCH]);
    repo.forget_calls();
    let moved = repo.update_ref("refs/heads/main", &later);
    let output = repo.merge_running("cargo build --quiet --package xtask", &moved);

    assert_merged(&repo, &output);
    assert_eq!(builds(&repo), 2, "{}", repo.calls());
    repo.origin(&["merge-base", "--is-ancestor", &later, &repo.head()]);
}

/// Another merge builds the xtask of `main` in the same place while this one
/// runs its checks, and this one goes on running its own.
#[test]
fn a_merge_keeps_its_xtask_of_main_while_another_builds_one() {
    let repo = Repo::new("checks-shared");
    repo.forget_calls();
    let other =
        "cargo build --quiet --package xtask --target-dir \"$TMPDIR\"/xtask-checks-*/target";
    let output = repo.merge_running("cargo tree", other);

    assert_merged(&repo, &output);
    assert_eq!(builds(&repo), 2, "{}", repo.calls());
    let kept: Vec<_> = fs::read_dir(&repo.temp)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("target/debug/xtask").is_file() && path.join("bin").is_dir())
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
}

/// The xtask that a revert puts back was measured already, and may not know
/// how to `prove`.
#[test]
fn an_exact_revert_of_a_change_to_xtask_is_proven_by_mains_alone() {
    let (repo, _) = revert_of(
        "checks-revert",
        ("xtask/src/journey.rs", "// before\n", "// after\n"),
        None,
    );
    repo.forget_calls();
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    assert_eq!(builds(&repo), 1, "{}", repo.calls());
}
