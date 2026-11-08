//! Runs `merge` against a bare origin. Wrappers on `PATH` for `git` and `cargo`
//! and a fake `gh` log every call, and can kill `xtask` at a chosen call, so
//! that each step of the merge is tested for recovery. No test reaches GitHub.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

const SPEC: &str = "# A\n\nOne. Two.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";
const NO_CHECK: &str = "pub fn f() {}\n";
const FAST: &str = "fn main() {}\n";
const SLOW: &str =
    "fn main() {\n    std::thread::sleep(std::time::Duration::from_millis(150));\n}\n";
const JOURNEY: &str =
    "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const BUDGETS: &str =
    "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";
const BRANCH: &str = "7-x";
const HILL_NONE: &str = "Closes #7.\n\nHill: none, tooling.\n";

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn quoted(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn labels_json(labels: &[&str]) -> String {
    let items: Vec<String> = labels
        .iter()
        .map(|label| format!("{{\"name\":{}}}", quoted(label)))
        .collect();
    format!("[{}]", items.join(","))
}

fn record(proven: u32, passing: u32, budgets: &str) -> String {
    format!(
        "issue = 3\npr = 2\ncommit = \"c\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = {proven}, promises = 2 }}\njourney = {{ passing = {passing}, steps = 1 }}\nreason = \"\"\n\n[budgets]\n{budgets}"
    )
}

fn real(program: &str) -> String {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .unwrap();
    stdout(&output).trim().to_string()
}

struct Pr<'a> {
    state: &'a str,
    body: &'a str,
    labels: &'a [&'a str],
    decision: &'a str,
    base: &'a str,
}

impl Default for Pr<'_> {
    fn default() -> Self {
        Pr {
            state: "OPEN",
            body: HILL_NONE,
            labels: &[],
            decision: "",
            base: "main",
        }
    }
}

struct Repo {
    root: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
}

impl Repo {
    /// `main` holds a base record with no proven promises and one passing
    /// step. Branch `7-x` adds `docs.txt`, and is pushed and checked out.
    fn new(name: &str) -> Repo {
        let base = record(0, 1, "");
        Repo::with(
            name,
            &[("harness/ledger/3.toml", base.as_str())],
            &[("docs.txt", "x\n")],
        )
    }

    fn with(name: &str, on_main: &[(&str, &str)], on_branch: &[(&str, &str)]) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("merge")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            origin: parent.join("origin.git"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.root).unwrap();
        fs::create_dir_all(&repo.bin).unwrap();
        repo.write_tools();
        repo.set_pr(&Pr::default());
        repo.set_issue(&["auto:merge"]);
        let ap = "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let demo = "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", SPEC),
            ("crates/ap/Cargo.toml", ap),
            ("crates/ap/src/main.rs", FAST),
            ("crates/demo/Cargo.toml", demo),
            ("crates/demo/src/lib.rs", NO_CHECK),
            ("harness/budgets.toml", BUDGETS),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/one.sh", "#!/bin/sh\nexit 0\n"),
        ];
        for (path, content) in files.iter().chain(on_main) {
            repo.write(path, content);
        }
        let output = repo
            .command("cargo")
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let origin = repo.origin.display().to_string();
        let output = repo
            .command("git")
            .args(["init", "-q", "--bare", "-b", "main", &origin])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["remote", "add", "origin", &origin]);
        repo.commit();
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", BRANCH]);
        for (path, content) in on_branch {
            repo.write(path, content);
        }
        repo.commit();
        repo.git(&["push", "-q", "origin", BRANCH]);
        fs::write(repo.bin.join("calls"), "").unwrap();
        repo
    }

    /// Wrappers for `git` and `cargo`, and the fake `gh`. Each logs the call.
    /// `KILL_ON` kills `xtask` at the first call containing it, `KILL_WHEN`
    /// says whether before or after doing the call, `HANG_ON` makes a matching
    /// call sleep, and `MOVE_BRANCH_ON` moves the branch on GitHub just before
    /// a matching call.
    fn write_tools(&self) {
        let bin = self.bin.display();
        let origin = self.origin.display();
        let git = real("git");
        let prelude = format!(
            r#"#!/bin/sh
call="$(basename "$0") $*"
echo "$call" >> '{bin}/calls'
maybe_kill() {{
  if [ -n "$KILL_ON" ] && [ ! -f '{bin}/killed' ] && [ "$KILL_WHEN" = "$1" ]; then
    case "$call" in *"$KILL_ON"*) : > '{bin}/killed'; kill -9 "$PPID"; exit 9 ;; esac
  fi
}}
if [ -n "$HANG_ON" ]; then
  case "$call" in *"$HANG_ON"*) exec sleep 3 ;; esac
fi
if [ -n "$MOVE_BRANCH_ON" ]; then
  case "$call" in *"$MOVE_BRANCH_ON"*) '{git}' --git-dir '{origin}' update-ref refs/heads/{BRANCH} refs/heads/main ;; esac
fi
maybe_kill before
"#
        );
        for tool in ["git", "cargo"] {
            let path = real(tool);
            let script =
                format!("{prelude}'{path}' \"$@\"\ncode=$?\nmaybe_kill after\nexit $code\n");
            let file = self.bin.join(tool);
            fs::write(&file, script).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let gh = format!(
            r#"{prelude}case "$1 $2" in
  "repo view") echo '{{"nameWithOwner":"o/r"}}' ;;
  "pr view") if [ -f '{bin}/merged' ]; then cat '{bin}/pr-merged.json'; else cat '{bin}/pr.json'; fi ;;
  "pr list") echo '[]' ;;
  "pr merge") echo merged >> '{bin}/merges'; : > '{bin}/merged' ;;
  "issue view") cat '{bin}/issue.json' ;;
  "api -X") if [ -f '{bin}/status-fails' ]; then echo 'no status' >&2; exit 1; fi ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
code=$?
maybe_kill after
exit $code
"#
        );
        let file = self.bin.join("gh");
        fs::write(&file, gh).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn set_pr(&self, pr: &Pr) {
        let json = |state: &str| {
            format!(
                "{{\"number\":9,\"state\":{},\"headRefName\":{},\"baseRefName\":{},\"body\":{},\"labels\":{},\"reviewDecision\":{}}}\n",
                quoted(state),
                quoted(BRANCH),
                quoted(pr.base),
                quoted(pr.body),
                labels_json(pr.labels),
                quoted(pr.decision)
            )
        };
        fs::write(self.bin.join("pr.json"), json(pr.state)).unwrap();
        fs::write(self.bin.join("pr-merged.json"), json("MERGED")).unwrap();
    }

    fn set_issue(&self, labels: &[&str]) {
        fs::write(
            self.bin.join("issue.json"),
            format!(
                "{{\"number\":7,\"state\":\"OPEN\",\"body\":\"\",\"labels\":{},\"comments\":[]}}\n",
                labels_json(labels)
            ),
        )
        .unwrap();
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            env::var("PATH").expect("PATH is set")
        );
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("PATH", path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD");
        command
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[track_caller]
    fn git(&self, args: &[&str]) -> String {
        let output = self.command("git").args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "change"]);
    }

    fn change_branch(&self, path: &str, content: &str) {
        self.write(path, content);
        self.commit();
        self.git(&["push", "-q", "origin", BRANCH]);
        self.forget_calls();
    }

    fn forget_calls(&self) {
        fs::write(self.bin.join("calls"), "").unwrap();
    }

    fn advance_main(&self, path: &str, content: &str) {
        self.git(&["checkout", "-q", "main"]);
        self.write(path, content);
        self.commit();
        self.git(&["push", "-q", "origin", "main"]);
        self.git(&["checkout", "-q", BRANCH]);
        self.forget_calls();
    }

    fn origin(&self, args: &[&str]) -> String {
        let directory = self.origin.display().to_string();
        let output = Command::new(real("git"))
            .args(["--git-dir", &directory])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output).trim().to_string()
    }

    fn head(&self) -> String {
        self.origin(&["rev-parse", "refs/heads/7-x"])
    }

    fn commits_on_branch(&self) -> usize {
        self.origin(&["rev-list", "--count", "refs/heads/main..refs/heads/7-x"])
            .parse()
            .unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .unwrap()
    }

    fn merge(&self, extra: &[&str]) -> Output {
        let mut args = vec!["merge", "9"];
        args.extend(extra);
        self.xtask(&args)
    }

    fn merge_directory(&self) -> PathBuf {
        self.root.join("target/merge")
    }

    fn merge_killed(&self, on: &str, when: &str) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge", "9"])
            .env("KILL_ON", on)
            .env("KILL_WHEN", when)
            .output()
            .unwrap()
    }
}

#[track_caller]
fn assert_merged(repo: &Repo, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert!(repo.bin.join("merged").exists(), "gh pr merge never ran");
}

#[track_caller]
fn assert_refused(repo: &Repo, output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(stderr(output).contains(message), "{}", stderr(output));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
}

#[test]
fn a_merge_records_pushes_verifies_and_squashes() {
    let repo = Repo::new("happy");
    let output = repo.merge(&[]);
    assert_merged(&repo, &output);

    let head = repo.head();
    let gated = repo.origin(&["rev-parse", &format!("{head}~1")]);
    assert_eq!(repo.commits_on_branch(), 2);
    assert_eq!(
        repo.origin(&["diff-tree", "--no-commit-id", "--name-only", "-r", &head]),
        "harness/ledger/7.toml"
    );
    let record = repo.origin(&["show", &format!("{head}:harness/ledger/7.toml")]);
    assert!(record.contains("issue = 7\npr = 9\n"), "{record}");
    assert!(
        record.contains(&format!("commit = \"{gated}\"")),
        "{record}"
    );
    assert!(
        record.contains("journey = { passing = 1, steps = 1 }"),
        "{record}"
    );

    let calls = repo.calls();
    assert!(
        calls.contains(&format!(
            "gh pr merge 9 -R o/r --squash --delete-branch --match-head-commit {head}"
        )),
        "{calls}"
    );
    assert!(
        calls.contains(&format!(
            "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/gate"
        )),
        "{calls}"
    );
}

#[test]
fn a_merge_leaves_the_callers_checkout_alone() {
    let repo = Repo::new("alone");
    let before = repo.git(&["rev-parse", "HEAD"]);
    assert_merged(&repo, &repo.merge(&[]));
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    assert!(!repo.merge_directory().exists());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_merge_works_from_a_checkout_of_main() {
    let repo = Repo::new("from-main");
    repo.git(&["checkout", "-q", "main"]);
    assert_merged(&repo, &repo.merge(&[]));
}

#[test]
fn a_merge_rebases_onto_the_newest_main() {
    let repo = Repo::new("rebase");
    repo.advance_main("other.txt", "y\n");
    assert_merged(&repo, &repo.merge(&[]));
    let main = repo.origin(&["rev-parse", "refs/heads/main"]);
    assert_eq!(repo.origin(&["merge-base", &main, &repo.head()]), main);
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_conflict_stops_the_merge_and_leaves_the_branch() {
    let repo = Repo::new("conflict");
    repo.advance_main("docs.txt", "from main\n");
    let before = repo.head();
    let output = repo.merge(&[]);
    assert_refused(&repo, &output, "rebase");
    assert_eq!(repo.head(), before);
    assert!(!repo.merge_directory().exists());
}

#[test]
fn a_record_left_at_the_tip_is_replaced() {
    let repo = Repo::new("left-record");
    let old = record(0, 1, "");
    repo.change_branch("harness/ledger/7.toml", &old);
    assert_merged(&repo, &repo.merge(&[]));
    assert_eq!(repo.commits_on_branch(), 2);
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains("pr = 9\n"), "{record}");
}

#[test]
fn a_pr_without_auto_merge_needs_the_owner() {
    let repo = Repo::new("auto-pr");
    repo.set_issue(&["auto:pr"]);
    assert_refused(&repo, &repo.merge(&[]), "--owner");
    assert_merged(&repo, &repo.merge(&["--owner"]));
}

#[test]
fn a_spec_change_pr_needs_the_owner() {
    let repo = Repo::new("spec-change");
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(&[]), "--owner");
    assert_merged(&repo, &repo.merge(&["--owner"]));
}

#[test]
fn a_protected_path_needs_the_label_even_for_the_owner() {
    let repo = Repo::new("protected");
    let changed = format!("{BUDGETS}\n# changed\n");
    repo.change_branch("harness/budgets.toml", &changed);
    let output = repo.merge(&["--owner"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail labels"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"));
}

#[test]
fn a_pr_must_close_exactly_one_issue() {
    let repo = Repo::new("no-issue");
    for body in [
        "Hill: none, tooling.\n",
        "Closes #7.\nCloses #8.\n\nHill: none, tooling.\n",
    ] {
        repo.set_pr(&Pr {
            body,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.merge(&[]), "Closes #");
    }
}

#[test]
fn a_pr_with_changes_requested_is_refused() {
    let repo = Repo::new("changes");
    repo.set_pr(&Pr {
        decision: "CHANGES_REQUESTED",
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(&["--owner"]), "changes");
}

#[test]
fn a_pr_against_another_branch_is_refused() {
    let repo = Repo::new("other-base");
    repo.set_pr(&Pr {
        base: "release",
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(&[]), "main");
}

#[test]
fn an_already_merged_pr_succeeds_without_merging_again() {
    let repo = Repo::new("merged");
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = repo.merge(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repo.calls().contains("gh pr merge"));
}

#[test]
fn a_closed_pr_is_a_problem() {
    let repo = Repo::new("closed");
    repo.set_pr(&Pr {
        state: "CLOSED",
        ..Pr::default()
    });
    assert_eq!(repo.merge(&[]).status.code(), Some(2));
}

#[test]
fn bad_usage_is_a_problem() {
    let repo = Repo::new("usage");
    for args in [
        &["merge"][..],
        &["merge", "x"][..],
        &["merge", "1", "extra"][..],
    ] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`merge` takes"), "{args:?}");
    }
}

#[test]
fn a_failing_gate_stops_the_merge() {
    let repo = Repo::new("gate-fails");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let output = repo.merge(&[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail format"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));
}

fn with_base(name: &str, proven: u32, passing: u32) -> Repo {
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
    let output = repo.merge(&[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail hills"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));
    assert_merged(&repo, &repo.merge(&["--owner"]));
}

#[test]
fn a_lowered_journey_count_needs_the_owner() {
    let repo = with_base("lower-journey", 0, 2);
    assert_refused(&repo, &repo.merge(&[]), "journey");
    assert_merged(&repo, &repo.merge(&["--owner"]));
}

#[test]
fn a_lowered_budget_count_needs_the_owner() {
    let budgets = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = 0.000001\n";
    let base = record(0, 1, "version_ms = 0\n");
    let repo = Repo::with(
        "lower-budget",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets),
        ],
        &[("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n")],
    );
    assert_refused(&repo, &repo.merge(&[]), "budgets");
    assert_merged(&repo, &repo.merge(&["--owner"]));
}

#[test]
fn a_change_that_raises_nothing_needs_a_hill_none_line() {
    let repo = Repo::new("no-hill");
    for body in ["Closes #7.\n", "Closes #7.\n\nHill: none,  \n"] {
        repo.set_pr(&Pr {
            body,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.merge(&[]), "Hill: none");
    }
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
    assert_merged(&repo, &repo.merge(&[]));
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(
        record.contains("spec = { proven = 1, promises = 2 }"),
        "{record}"
    );
}

#[test]
fn the_bench_runs_only_when_a_budget_can_be_affected() {
    let repo = Repo::new("no-bench");
    let output = repo.merge(&[]);
    assert_merged(&repo, &output);
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));

    let affecting = [
        ("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n"),
        (".cargo/config.toml", "[build]\n"),
    ];
    for (index, (path, content)) in affecting.into_iter().enumerate() {
        let base = record(0, 1, "");
        let repo = Repo::with(
            &format!("bench-{index}"),
            &[("harness/ledger/3.toml", base.as_str())],
            &[(path, content)],
        );
        let output = repo.merge(&[]);
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

#[test]
fn a_slowdown_needs_a_reason_in_the_pr_body() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "slow",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", SLOW)],
    );
    let output = repo.merge(&[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("bench: fail"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));

    repo.set_pr(&Pr {
        body: "Closes #7.\n\nHill: none, tooling.\n\nBudget reason: worth it\n",
        ..Pr::default()
    });
    assert_merged(&repo, &repo.merge(&[]));
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains("reason = \"worth it\"\n"), "{record}");
}

#[test]
fn a_kill_at_each_step_resumes() {
    let cases = [
        ("fetch", "git fetch", "before"),
        ("worktree", "git worktree add", "after"),
        ("rebase-before", "git rebase", "before"),
        ("rebase-after", "git rebase", "after"),
        ("gate", "cargo fmt", "before"),
        ("journey", "cargo build --release", "before"),
        ("commit", "git commit", "after"),
        ("push-before", "git push", "before"),
        ("push-after", "git push", "after"),
        ("status", "gh api -X POST", "before"),
        ("merge-before", "gh pr merge", "before"),
        ("merge-after", "gh pr merge", "after"),
    ];
    thread::scope(|scope| {
        for (name, on, when) in cases {
            scope.spawn(move || {
                let repo = Repo::new(&format!("kill-{name}"));
                let killed = repo.merge_killed(on, when);
                assert_eq!(killed.status.code(), None, "{name} wasn't killed");
                assert!(repo.bin.join("killed").exists(), "{name}");

                let output = repo.merge(&[]);
                assert_merged(&repo, &output);
                assert_eq!(repo.commits_on_branch(), 2, "{name}");
                assert_eq!(
                    fs::read_to_string(repo.bin.join("merges")).unwrap(),
                    "merged\n",
                    "{name}: {}",
                    repo.calls()
                );
                assert!(!repo.merge_directory().exists(), "{name}");
            });
        }
    });
}

#[test]
fn a_worktree_locked_by_a_killed_attempt_is_cleared() {
    let repo = Repo::new("locked-worktree");
    let args = ["worktree", "add", "-q", "--lock", "--detach"];
    let path = repo.merge_directory().join("9");
    fs::create_dir_all(repo.merge_directory()).unwrap();
    let mut command = repo.command("git");
    let output = command.args(args).arg(&path).arg("main").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_merged(&repo, &repo.merge(&[]));
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_merge_waits_for_the_merge_slot() {
    let repo = Repo::new("slot");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(!repo.calls().contains("git fetch"), "{}", repo.calls());
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(repo.bin.join("merged").exists());
}

#[test]
fn the_pr_is_read_again_after_the_slot() {
    let repo = Repo::new("read-again");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    while !repo.calls().contains("gh issue view") {
        thread::sleep(Duration::from_millis(10));
    }
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "--owner");
}

#[test]
fn a_branch_that_moved_during_the_merge_is_not_overwritten() {
    let repo = Repo::new("moved");
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("MOVE_BRANCH_ON", "git push")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert_eq!(repo.head(), repo.origin(&["rev-parse", "refs/heads/main"]));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn a_record_commit_that_changes_more_is_not_merged() {
    let repo = Repo::new("sneaky");
    let hook = repo.root.join(".git/hooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        "#!/bin/sh\necho x > sneaky.txt\ngit add sneaky.txt\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let output = repo.merge(&[]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("sneaky.txt"),
        "{}",
        stderr(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
    assert!(!repo.calls().contains("statuses"), "{}", repo.calls());
}

#[test]
fn a_merge_past_its_timeout_stops_and_can_restart() {
    let repo = Repo::new("timeout");
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("HANG_ON", "cargo fmt")
        .env("XTASK_MERGE_TIMEOUT_SECONDS", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
    assert!(!repo.bin.join("merged").exists());
    assert_merged(&repo, &repo.merge(&[]));
}

fn status_call(repo: &Repo) -> Option<String> {
    repo.calls()
        .lines()
        .find(|line| line.starts_with("gh api -X POST"))
        .map(str::to_string)
}

#[test]
fn the_gate_posts_its_status_on_a_clean_pushed_head() {
    let repo = Repo::new("gate-status");
    let output = repo.xtask(&["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let head = repo.head();
    assert_eq!(
        status_call(&repo).unwrap(),
        format!(
            "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/gate"
        )
    );
}

#[test]
fn the_gate_posts_a_failure_too() {
    let repo = Repo::new("gate-status-fail");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    assert_eq!(repo.xtask(&["gate"]).status.code(), Some(1));
    assert!(status_call(&repo).unwrap().contains("state=failure"));
}

#[test]
fn the_gate_posts_nothing_for_a_dirty_tree() {
    let repo = Repo::new("gate-dirty");
    repo.write("notes.txt", "uncommitted\n");
    assert_eq!(repo.xtask(&["gate"]).status.code(), Some(0));
    assert_eq!(status_call(&repo), None);
}

#[test]
fn the_gate_posts_nothing_for_an_unpushed_head() {
    let repo = Repo::new("gate-unpushed");
    repo.write("notes.txt", "committed only\n");
    repo.commit();
    assert_eq!(repo.xtask(&["gate"]).status.code(), Some(0));
    assert_eq!(status_call(&repo), None);
}

#[test]
fn a_status_that_can_not_be_posted_does_not_change_the_verdict() {
    let repo = Repo::new("gate-no-post");
    fs::write(repo.bin.join("status-fails"), "").unwrap();
    let output = repo.xtask(&["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("no status"), "{}", stderr(&output));
}
