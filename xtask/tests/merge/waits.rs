use super::prepare::last_post;
use super::*;

/// A repository whose `main` also holds `on_main`, and whose branch holds
/// `on_branch`.
pub(super) fn changing(name: &str, on_main: &[(&str, &str)], on_branch: &[(&str, &str)]) -> Repo {
    let base = record(0, 1, "");
    let mut main = vec![("harness/ledger/3.toml", base.as_str())];
    main.extend_from_slice(on_main);
    Repo::with(name, &main, on_branch)
}

/// `merge` refuses the PR and names why, and `prepare` takes it and says
/// that merging accepts it.
#[track_caller]
fn assert_waits(repo: &Repo, reason: &str) {
    let output = repo.merge();
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
    assert!(stderr(&output).contains(reason), "{}", stderr(&output));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
    assert_eq!(last_post(repo), "failure Failed: owner");
    assert_prepared(repo, &repo.prepare());
    let comments = repo.pr_comments();
    let heading = format!("Merging this PR at `{}` accepts:", repo.head());
    assert!(comments.contains(&heading), "{comments}");
    assert!(comments.contains(reason), "{comments}");
}

#[test]
fn a_pr_that_rewrites_a_spec_sentence_waits() {
    let repo = changing("waits-spec", &[], &[("SPEC.md", "# A\n\nOne. Three.\n")]);
    assert_waits(
        &repo,
        "it changes or removes this sentence of SPEC.md under `A`: Two.",
    );
}

#[test]
fn a_pr_that_adds_a_waiver_waits() {
    let repo = changing(
        "waits-waiver",
        &[],
        &[("harness/waived.txt", "A\tTwo.\tClaims nothing.\n")],
    );
    assert_waits(
        &repo,
        "it adds a waiver to harness/waived.txt under `A`: Two.",
    );
}

#[test]
fn a_pr_that_widens_a_band_waits() {
    let wider = BUDGETS.replace("band_pct = 20.0", "band_pct = 30.0");
    let repo = changing("waits-band", &[], &[("harness/budgets.toml", &wider)]);
    assert_waits(
        &repo,
        "it widens the band of `version_ms` from 20% and 25 to 30% and 25",
    );
}

#[test]
fn a_pr_that_raises_a_target_waits() {
    let tight = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = 1000.0\n";
    let loose = tight.replace("1000.0", "2000.0");
    let repo = changing(
        "waits-target",
        &[("harness/budgets.toml", tight)],
        &[("harness/budgets.toml", &loose)],
    );
    assert_waits(
        &repo,
        "it raises the target of `version_ms` from 1000 to 2000",
    );
}

#[test]
fn a_pr_that_removes_a_budget_waits() {
    let repo = changing("waits-budget", &[], &[("harness/budgets.toml", "")]);
    assert_waits(&repo, "it removes the budget `version_ms`");
}

#[test]
fn a_pr_that_audits_a_build_script_waits() {
    let repo = changing(
        "waits-build-script",
        &[],
        &[(
            "harness/build-scripts.txt",
            "# Audited.\nfoo 1.0.0 Sets one cfg.\n",
        )],
    );
    assert_waits(&repo, "it adds `foo 1.0.0` to harness/build-scripts.txt");
}

pub(super) const TWO_GATE_TESTS: &str = "#[test]\nfn one() {}\n\n#[test]\nfn two() {}\n";

#[test]
fn a_pr_that_lowers_the_number_of_gate_tests_waits() {
    let repo = changing(
        "waits-gate-tests",
        &[("xtask/tests/a.rs", TWO_GATE_TESTS)],
        &[("xtask/tests/a.rs", "#[test]\nfn one() {}\n")],
    );
    assert_waits(&repo, "the number of tests under `xtask/` fell from 2 to 1");
}

#[test]
fn a_pr_that_changes_github_waits() {
    let repo = changing(
        "waits-github",
        &[],
        &[(".github/workflows/x.yml", "name: x\n")],
    );
    assert_waits(&repo, "it changes `.github/workflows/x.yml`");
}

#[test]
fn a_pr_that_changes_what_waits_waits() {
    let repo = changing(
        "waits-defines",
        &[],
        &[("xtask/src/waits.rs", "// fewer\n")],
    );
    assert_waits(
        &repo,
        "it changes `xtask/src/waits.rs`, which decides what waits for the owner",
    );
}

/// How a fallen count is found decides what a merge accepts.
#[test]
fn a_pr_that_changes_how_a_fallen_count_is_found_waits() {
    let repo = changing(
        "waits-counts",
        &[],
        &[("xtask/src/accepts.rs", "// a count that fell\n")],
    );
    assert_waits(
        &repo,
        "it changes `xtask/src/accepts.rs`, which decides what waits for the owner",
    );
}

/// The timeout is how a merge runs, not what it accepts.
#[test]
fn a_pr_that_changes_the_timeout_merges() {
    let repo = changing(
        "free-timeout",
        &[],
        &[("xtask/src/merge.rs", "// a longer timeout\n")],
    );
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_pr_that_adds_a_spec_sentence_with_its_check_merges() {
    let check = "// spec(A): Three.\n#[test]\nfn three() {}\n";
    let repo = changing(
        "free-spec",
        &[],
        &[
            ("SPEC.md", "# A\n\nOne. Two. Three.\n"),
            ("crates/demo/src/lib.rs", check),
        ],
    );
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_pr_that_reflows_a_spec_paragraph_merges() {
    let repo = changing("free-reflow", &[], &[("SPEC.md", "# A\n\nOne.\nTwo.\n")]);
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_pr_that_edits_the_rules_merges() {
    let repo = changing(
        "free-rules",
        &[("AGENTS.md", "# Rules\n"), ("HARNESS.md", "# Harness\n")],
        &[
            ("AGENTS.md", "# Rules\n\nOne more.\n"),
            ("HARNESS.md", "# Harness\n\nOne more.\n"),
            ("harness/tasks/worker.md", "# Worker\n"),
        ],
    );
    assert_merged(&repo, &repo.merge());
}

/// Tests are counted, not named, so one can be renamed or moved.
#[test]
fn a_pr_that_changes_xtask_and_keeps_its_tests_merges() {
    let repo = changing(
        "free-xtask",
        &[("xtask/tests/a.rs", TWO_GATE_TESTS)],
        &[
            ("xtask/tests/a.rs", "#[test]\nfn two() {}\n"),
            (
                "xtask/src/b.rs",
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn first() {}\n}\n",
            ),
            ("xtask/src/bench.rs", "// a better measurer\n"),
        ],
    );
    assert_merged(&repo, &repo.merge());
}
