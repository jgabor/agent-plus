use super::prepare::last_post;
use super::*;

pub(super) fn with_base(name: &str, proven: u32, passing: u32) -> Repo {
    let base = record(proven, passing, "");
    Repo::with(
        name,
        &[("harness/ledger/3.toml", base.as_str())],
        &[("docs.txt", "x\n")],
    )
}

#[test]
fn a_lowered_spec_count_needs_the_owner() {
    let repo = with_base("lower-spec", 1, 1);
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail hills"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));
    assert_eq!(last_post(&repo), "failure Failed: hills");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_lowered_journey_count_needs_the_owner() {
    let repo = with_base("lower-journey", 0, 2);
    assert_refused(&repo, &repo.merge(), "journey");
    assert_eq!(last_post(&repo), "failure Failed: journey");
    assert_prepared(&repo, &repo.prepare());
}

/// The base build is fast and the candidate sleeps, in the same run. The
/// sleep is the target, so the candidate always misses it, and the base has
/// all of it to spare.
#[test]
fn a_lowered_budget_count_needs_the_owner() {
    let budgets = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = 150.0\n";
    let base = record(0, 1, "version_ms = 1\n");
    let repo = Repo::with(
        "lower-budget",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets),
        ],
        &[("crates/ap/src/main.rs", SLOW)],
    );
    assert_refused(&repo, &repo.merge(), "budgets");
    assert_prepared(&repo, &repo.prepare());
}

/// Both builds miss a target that the base's record meets, as on a loaded
/// machine, so the base is counted from this run and the count holds.
#[test]
fn a_target_both_builds_miss_in_one_run_lowers_no_count() {
    let budgets = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = 0.000001\n";
    let base = record(0, 1, "version_ms = 0\n");
    let repo = Repo::with(
        "loaded-budget",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets),
        ],
        &[("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n")],
    );
    let output = repo.merge();
    assert!(
        stdout(&output).contains("bench: pass"),
        "{}",
        stdout(&output)
    );
    assert_merged(&repo, &output);
}

/// The base can't boot, so this run has no boot value for it, and its record's
/// value doesn't stand in. The candidate meets the target, so the count rises
/// and the PR needs no `Hill: none` line.
#[test]
fn a_budget_the_base_cant_be_measured_for_isnt_met_for_it() {
    let budgets = "[[budget]]\nkey = \"boot_ms\"\nname = \"boot\"\ntarget = 10000.0\nband_pct = 10.0\nband_abs = 25.0\n";
    let base = record(0, 1, "boot_ms = 1\n");
    let repo = Repo::with(
        "unmeasured-base",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets),
            ("crates/ap/src/main.rs", NO_BOOT),
        ],
        &[("crates/ap/src/main.rs", BOOTS)],
    );
    repo.set_pr(&Pr {
        body: "Closes #7.\n",
        ..Pr::default()
    });
    let output = repo.merge();
    assert!(
        stdout(&output).contains("no-comparison boot_ms"),
        "{}",
        stdout(&output)
    );
    assert_merged(&repo, &output);
}

/// With no bench, both counts come from the ledger, and the candidate's
/// record carries the base's values.
#[test]
fn without_a_bench_both_budget_counts_come_from_the_ledger() {
    let budgets = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = 0.000001\n";
    let base = record(0, 1, "version_ms = 0\n");
    let repo = Repo::with(
        "ledger-budget",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets),
        ],
        &[("docs.txt", "x\n")],
    );
    let output = repo.merge();
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));
    assert_merged(&repo, &output);
}

#[test]
fn a_change_that_raises_nothing_needs_a_hill_none_line() {
    let repo = Repo::new("no-hill");
    for body in ["Closes #7.\n", "Closes #7.\n\nHill: none,  \n"] {
        repo.set_pr(&Pr {
            body,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.merge(), "Hill: none");
        assert_eq!(last_post(&repo), "failure Failed: hill-line");
    }
    // A count can come from `docs.txt`, so the proof ran to find out.
    assert!(repo.calls().contains("cargo fmt --all"), "{}", repo.calls());
}

/// No count comes from these paths, so a change only to them can't raise one,
/// and learns that it needs the line before the gate runs.
#[test]
fn a_change_no_count_comes_from_needs_a_hill_none_line_before_the_gate() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "no-hill-early",
        &[("harness/ledger/3.toml", base.as_str())],
        &[
            ("AGENTS.md", "# Rules\n"),
            ("HARNESS.md", "# Harness\n"),
            ("harness/tasks/worker.md", "# Worker\n"),
            (".github/workflows/x.yml", "# comment\n"),
            ("xtask/tests/x.rs", "// a helper\n"),
        ],
    );
    repo.set_pr(&Pr {
        body: "Closes #7.\n",
        ..Pr::default()
    });
    let output = repo.prepare();
    assert_refused(&repo, &output, "Hill: none");
    assert!(!repo.calls().contains("cargo fmt"), "{}", repo.calls());
    assert_eq!(last_post(&repo), "failure Failed: hill-line");
}

#[test]
fn a_change_that_raises_a_count_needs_no_hill_none_line() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "raises",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/demo/src/lib.rs", CHECK_ONE)],
    );
    repo.set_pr(&Pr {
        body: "Closes #7.\n",
        ..Pr::default()
    });
    assert_merged(&repo, &repo.merge());
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(
        record.contains("spec = { proven = 1, promises = 2 }"),
        "{record}"
    );
}

#[test]
fn the_bench_runs_only_when_a_budget_can_be_affected() {
    let repo = Repo::new("no-bench");
    let output = repo.merge();
    assert_merged(&repo, &output);
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));

    let affecting = [
        ("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n"),
        (".cargo/config.toml", "[build]\n"),
        ("guest/kernel/pin.toml", "version = \"1\"\n"),
    ];
    for (index, (path, content)) in affecting.into_iter().enumerate() {
        let base = record(0, 1, "");
        let repo = Repo::with(
            &format!("bench-{index}"),
            &[("harness/ledger/3.toml", base.as_str())],
            &[(path, content)],
        );
        let output = repo.merge();
        assert_merged(&repo, &output);
        assert!(
            stdout(&output).contains("bench: pass"),
            "{path}: {}",
            stdout(&output)
        );
        let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
        assert!(record.contains("version_ms = "), "{path}: {record}");
    }
}

/// An integration test isn't part of the `ap` that the bench measures.
#[test]
fn a_change_only_to_the_tests_of_a_crate_runs_no_bench() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "bench-tests-only",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/tests/a.rs", "#[test]\nfn a() {}\n")],
    );
    let output = repo.merge();
    assert_merged(&repo, &output);
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));
    assert!(!repo.calls().contains("Proving: bench"), "{}", repo.calls());
}

/// Neither change loosens a budget, so each merges.
#[test]
fn a_change_to_how_budgets_are_measured_runs_the_bench() {
    for (index, (path, content)) in [
        ("harness/budgets.toml", BUDGETS_EDITED),
        ("xtask/src/bench.rs", "// the boot measurer\n"),
    ]
    .into_iter()
    .enumerate()
    {
        let base = record(0, 1, "");
        let repo = Repo::with(
            &format!("bench-protected-{index}"),
            &[("harness/ledger/3.toml", base.as_str())],
            &[(path, content)],
        );
        let output = repo.merge();
        assert_merged(&repo, &output);
        assert!(
            stdout(&output).contains("bench: pass"),
            "{path}: {}",
            stdout(&output)
        );
    }
}

#[test]
fn a_slowdown_needs_a_reason_in_the_pr_body() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "slow",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", SLOW)],
    );
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("bench: fail"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));
    assert_eq!(last_post(&repo), "failure Failed: bench");

    repo.set_pr(&Pr {
        body: "Closes #7.\n\nHill: none, tooling.\n\nBudget reason: worth it\n",
        ..Pr::default()
    });
    assert_merged(&repo, &repo.merge());
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains("reason = \"worth it\"\n"), "{record}");
}
