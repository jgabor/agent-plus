//! Runs `merge` against a bare origin. Wrappers on `PATH` for `git` and `cargo`
//! and a fake `gh` log every call, and can kill `xtask` at a chosen call, so
//! that each step of the merge is tested for recovery. No test reaches GitHub.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod common;

const SPEC: &str = "# A\n\nOne. Two.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";
const NO_CHECK: &str = "pub fn f() {}\n";
const FAST: &str = "fn main() {}\n";
const SLOW: &str =
    "fn main() {\n    std::thread::sleep(std::time::Duration::from_millis(150));\n}\n";
const JOURNEY: &str = "[[milestone]]\nname = \"m\"\nsummary = \"s\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"s\"\nmilestone = \"m\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const BUDGETS_EDITED: &str =
    "# edited\n[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";
const BUDGETS: &str =
    "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";
const BRANCH: &str = "7-x";
const HILL_NONE: &str = "Closes #7.\n\nHill: none, tooling.\n";
const NO_REBASE: &str = "the branch doesn't rebase onto `main`. Rebase it by hand, and push it";

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

/// The program on `PATH`, not a shell builtin of the same name.
fn external(program: &str) -> String {
    let path = env::var("PATH").expect("PATH is set");
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|file| file.is_file())
        .unwrap_or_else(|| panic!("no `{program}` on PATH"))
        .display()
        .to_string()
}

struct Pr<'a> {
    state: &'a str,
    body: &'a str,
    labels: &'a [&'a str],
    base: &'a str,
    closes: &'a [u64],
    elsewhere: &'a [u64],
    reviews: &'a [(&'a str, &'a str)],
    fork: bool,
}

impl Default for Pr<'_> {
    fn default() -> Self {
        Pr {
            state: "OPEN",
            body: HILL_NONE,
            labels: &[],
            base: "main",
            closes: &[7],
            elsewhere: &[],
            reviews: &[],
            fork: false,
        }
    }
}

fn pr_json(pr: &Pr, state: &str) -> String {
    let issue = |repository: &str, number: &u64| {
        format!(
            "{{\"number\":{number},\"url\":\"https://github.com/{repository}/issues/{number}\"}}"
        )
    };
    let here = pr.closes.iter().map(|number| issue("o/r", number));
    let there = pr.elsewhere.iter().map(|number| issue("x/y", number));
    let closes: Vec<String> = here.chain(there).collect();
    let reviews: Vec<String> = pr
        .reviews
        .iter()
        .map(|(author, state)| {
            format!(
                "{{\"author\":{{\"login\":{}}},\"state\":{}}}",
                quoted(author),
                quoted(state)
            )
        })
        .collect();
    format!(
        "{{\"number\":9,\"state\":{},\"url\":\"https://github.com/o/r/pull/9\",\"isCrossRepository\":{},\"headRefName\":{},\"baseRefName\":{},\"body\":{},\"labels\":{},\"closingIssuesReferences\":[{}],\"reviews\":[{}]}}\n",
        quoted(state),
        pr.fork,
        quoted(BRANCH),
        quoted(pr.base),
        quoted(pr.body),
        labels_json(pr.labels),
        closes.join(","),
        reviews.join(",")
    )
}

struct Repo {
    root: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
    temp: PathBuf,
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
            temp: parent.join("tmp"),
        };
        fs::create_dir_all(&repo.root).unwrap();
        fs::create_dir_all(&repo.bin).unwrap();
        fs::create_dir_all(&repo.temp).unwrap();
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
    /// call sleep, and `RUN_ON` runs `RUN_COMMAND` just before the first
    /// matching call. The fake `gh` keeps the newest state posted on each
    /// commit, and gives it back when asked for that commit's status.
    fn write_tools(&self) {
        let bin = self.bin.display();
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
if [ -n "$RUN_ON" ] && [ ! -f '{bin}/ran' ]; then
  case "$call" in *"$RUN_ON"*) : > '{bin}/ran'; eval "$RUN_COMMAND" ;; esac
fi
maybe_kill before
"#
        );
        for tool in ["git", "cargo"] {
            let path = real(tool);
            let script =
                format!("{prelude}'{path}' \"$@\"\ncode=$?\nmaybe_kill after\nexit $code\n");
            let file = self.bin.join(tool);
            common::write_tool(&file, &script);
        }
        let gh = format!(
            r#"{prelude}case "$1 $2" in
  "repo view") echo '{{"nameWithOwner":"o/r","owner":{{"login":"o"}}}}' ;;
  "pr view") if [ -f "{bin}/pr-$3.json" ]; then cat "{bin}/pr-$3.json"; elif [ -f '{bin}/merged' ]; then cat '{bin}/pr-merged.json'; else cat '{bin}/pr.json'; fi ;;
  "pr list") if [ -f '{bin}/prs.json' ]; then cat '{bin}/prs.json'; else echo '[]'; fi ;;
  "issue list") cat '{bin}/issues.json' ;;
  "pr comment") cat >> '{bin}/pr-comments'; echo >> '{bin}/pr-comments'; echo 'https://github.com/o/r/pull/9#issuecomment-5' ;;
  "pr merge") if [ -f '{bin}/merge-fails' ]; then echo 'no merge' >&2; exit 1; fi
    echo merged >> '{bin}/merges'; echo "$9" > '{bin}/merged' ;;
  "issue view") cat '{bin}/issue.json' ;;
  "api -X") if [ -f '{bin}/status-fails' ]; then echo 'no status' >&2; exit 1; fi
    if [ -f '{bin}/pending-fails' ] && [ "$6" = state=pending ]; then echo 'no pending' >&2; exit 1; fi
    printf '{{"statuses":[{{"context":"xtask/gate","state":"%s","description":"%s"}}]}}\n' "${{6#state=}}" "${{10#description=}}" > "{bin}/status-${{4##*/}}" ;;
  "api repos/"*) posted="${{2#*/commits/}}"; posted="{bin}/status-${{posted%/status}}"
    if [ -f "$posted" ]; then cat "$posted"; else echo '{{"statuses":[]}}'; fi ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
code=$?
maybe_kill after
exit $code
"#
        );
        let file = self.bin.join("gh");
        common::write_tool(&file, &gh);
    }

    fn set_pr(&self, pr: &Pr) {
        fs::write(self.bin.join("pr.json"), pr_json(pr, pr.state)).unwrap();
        fs::write(self.bin.join("pr-merged.json"), pr_json(pr, "MERGED")).unwrap();
    }

    fn set_next_pr(&self, pr: &Pr) {
        fs::write(self.bin.join("pr-next.json"), pr_json(pr, pr.state)).unwrap();
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

    /// An issue that `thread` claimed, with a lease that ends at `lease`.
    fn set_held_issue(&self, labels: &[&str], thread: &str, lease: &str) {
        let body = format!(
            "<!-- status -->\n**State:** working\n**Thread:** {thread}\n**Branch:** `7-x`\n**Lease until:** {lease}\n"
        );
        fs::write(
            self.bin.join("issue.json"),
            format!(
                "{{\"number\":7,\"state\":\"OPEN\",\"body\":\"\",\"labels\":{},\"comments\":[{{\"url\":\"https://github.com/o/r/issues/7#issuecomment-40\",\"author\":{{\"login\":\"o\"}},\"body\":{}}}]}}\n",
                labels_json(labels),
                quoted(&body)
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
            .env("TMPDIR", &self.temp)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD");
        // The gate runs these tests inside a GitHub Actions run, whose page a
        // status would otherwise link to.
        for name in [
            "GITHUB_ACTIONS",
            "GITHUB_SERVER_URL",
            "GITHUB_REPOSITORY",
            "GITHUB_RUN_ID",
            "GITHUB_STEP_SUMMARY",
        ] {
            command.env_remove(name);
        }
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

    /// A shell command that moves a ref on the origin.
    fn update_ref(&self, name: &str, value: &str) -> String {
        format!(
            "'{}' --git-dir '{}' update-ref {name} {value}",
            real("git"),
            self.origin.display()
        )
    }

    /// The branch's commits, at the head that was squash-merged.
    fn commits_merged(&self) -> usize {
        let merged = fs::read_to_string(self.bin.join("merged")).unwrap();
        self.origin(&[
            "rev-list",
            "--count",
            &format!("refs/heads/main..{}", merged.trim()),
        ])
        .parse()
        .unwrap()
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

    fn merge(&self) -> Output {
        self.xtask(&["merge", "9"])
    }

    fn prepare(&self) -> Output {
        self.xtask(&["prepare", "9"])
    }

    fn land(&self) -> Output {
        self.xtask(&["land", "9"])
    }

    /// Where `land` runs from: a checkout of the newest `main`.
    fn on_main(self) -> Repo {
        self.git(&["checkout", "-q", "main"]);
        self.forget_calls();
        self
    }

    fn land_running(&self, on: &str, command: &str) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["land", "9"])
            .env("RUN_ON", on)
            .env("RUN_COMMAND", command)
            .output()
            .unwrap()
    }

    /// The comments `prepare` posted on the PR, each ending in a blank line.
    fn pr_comments(&self) -> String {
        fs::read_to_string(self.bin.join("pr-comments")).unwrap_or_default()
    }

    /// What merges have left in the temporary directory.
    fn merge_trees(&self) -> Vec<String> {
        fs::read_dir(&self.temp)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("xtask-merge"))
            .collect()
    }

    fn merge_running(&self, on: &str, command: &str) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge", "9"])
            .env("RUN_ON", on)
            .env("RUN_COMMAND", command)
            .output()
            .unwrap()
    }

    fn prepare_running(&self, on: &str, command: &str) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["prepare", "9"])
            .env("RUN_ON", on)
            .env("RUN_COMMAND", command)
            .output()
            .unwrap()
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
fn assert_prepared(repo: &Repo, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert_left_prepared(repo);
}

/// The record is pushed and green, and nothing was squashed.
#[track_caller]
fn assert_left_prepared(repo: &Repo) {
    let head = repo.head();
    assert_eq!(
        repo.origin(&["diff-tree", "--no-commit-id", "--name-only", "-r", &head]),
        "harness/ledger/7.toml"
    );
    let calls = repo.calls();
    assert!(
        calls.contains(&format!(
            "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/gate"
        )),
        "{calls}"
    );
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!repo.bin.join("merged").exists());
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
    let output = repo.merge();
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
    assert_merged(&repo, &repo.merge());
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_merge_works_from_a_checkout_of_main() {
    let repo = Repo::new("from-main");
    repo.git(&["checkout", "-q", "main"]);
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_merge_rebases_onto_the_newest_main() {
    let repo = Repo::new("rebase");
    repo.advance_main("other.txt", "y\n");
    assert_merged(&repo, &repo.merge());
    let main = repo.origin(&["rev-parse", "refs/heads/main"]);
    assert_eq!(repo.origin(&["merge-base", &main, &repo.head()]), main);
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_conflict_stops_the_merge_and_leaves_the_branch() {
    let repo = Repo::new("conflict");
    repo.advance_main("docs.txt", "from main\n");
    let before = repo.head();
    let output = repo.merge();
    assert_refused(&repo, &output, "rebase");
    assert_eq!(repo.head(), before);
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_record_left_at_the_tip_is_replaced() {
    let repo = Repo::new("left-record");
    let old = record(0, 1, "");
    repo.change_branch("harness/ledger/7.toml", &old);
    assert_merged(&repo, &repo.merge());
    assert_eq!(repo.commits_on_branch(), 2);
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains("pr = 9\n"), "{record}");
}

#[test]
fn a_pr_without_auto_merge_needs_the_owner() {
    let repo = Repo::new("auto-pr");
    repo.set_issue(&["auto:pr"]);
    assert_refused(&repo, &repo.merge(), "prepare");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_spec_change_pr_needs_the_owner() {
    let repo = Repo::new("spec-change");
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(), "prepare");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_protected_path_needs_the_label_even_for_the_owner() {
    let repo = Repo::new("protected");
    let changed = format!("{BUDGETS}\n# changed\n");
    repo.change_branch("harness/budgets.toml", &changed);
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail labels"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"));

    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_change_to_the_harness_itself_is_only_for_the_owner() {
    let repo = Repo::new("protected-xtask");
    repo.change_branch("xtask/notes.txt", "A way to weaken the gate.\n");
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail labels"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"));

    repo.set_pr(&Pr {
        labels: &["auto:merge", "spec-change"],
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(), "prepare");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_pr_must_close_exactly_one_issue_here() {
    let repo = Repo::new("no-issue");
    for (closes, elsewhere) in [
        (&[][..], &[][..]),
        (&[7, 8][..], &[][..]),
        (&[][..], &[7][..]),
    ] {
        repo.set_pr(&Pr {
            closes,
            elsewhere,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.merge(), "Closes #");
    }
}

#[test]
fn a_standing_change_request_is_refused() {
    let repo = Repo::new("changes");
    for reviews in [
        &[("a", "CHANGES_REQUESTED")][..],
        &[("a", "CHANGES_REQUESTED"), ("a", "COMMENTED")][..],
        &[("a", "APPROVED"), ("a", "CHANGES_REQUESTED")][..],
        &[("a", "CHANGES_REQUESTED"), ("b", "APPROVED")][..],
    ] {
        repo.set_pr(&Pr {
            reviews,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.prepare(), "changes");
    }
}

#[test]
fn a_change_request_that_was_answered_does_not_block() {
    for (index, reviews) in [
        &[("a", "CHANGES_REQUESTED"), ("a", "APPROVED")][..],
        &[("a", "DISMISSED")][..],
    ]
    .into_iter()
    .enumerate()
    {
        let repo = Repo::new(&format!("answered-{index}"));
        repo.set_pr(&Pr {
            reviews,
            ..Pr::default()
        });
        assert_merged(&repo, &repo.merge());
    }
}

#[test]
fn a_pr_from_a_fork_is_refused() {
    let repo = Repo::new("fork");
    repo.set_pr(&Pr {
        fork: true,
        ..Pr::default()
    });
    assert_refused(&repo, &repo.prepare(), "fork");
}

#[test]
fn a_pr_against_another_branch_is_refused() {
    let repo = Repo::new("other-base");
    repo.set_pr(&Pr {
        base: "release",
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(), "main");
}

#[test]
fn an_already_merged_pr_succeeds_without_merging_again() {
    let repo = Repo::new("merged");
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = repo.merge();
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
    assert_eq!(repo.merge().status.code(), Some(2));
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
    let output = repo.merge();
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
    assert_refused(&repo, &repo.merge(), "budgets");
    assert_prepared(&repo, &repo.prepare());
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

/// These paths are protected, so only `prepare` can take them.
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
        repo.set_pr(&Pr {
            labels: &["spec-change"],
            ..Pr::default()
        });
        let output = repo.prepare();
        assert_prepared(&repo, &output);
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
        ("pending", "state=pending", "before"),
        ("status", "state=success", "before"),
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

                let output = repo.merge();
                assert_merged(&repo, &output);
                assert_eq!(repo.commits_merged(), 2, "{name}");
                assert_eq!(
                    fs::read_to_string(repo.bin.join("merges")).unwrap(),
                    "merged\n",
                    "{name}: {}",
                    repo.calls()
                );
                assert!(repo.merge_trees().is_empty(), "{name}");
                assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1, "{name}");
            });
        }
    });
}

#[test]
fn a_tool_just_written_can_run_while_other_threads_start_processes() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("merge/fresh-tool");
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    fs::create_dir_all(&directory).unwrap();
    thread::scope(|scope| {
        for worker in 0..16 {
            let directory = &directory;
            scope.spawn(move || {
                for round in 0..200 {
                    let tool = directory.join(format!("tool-{worker}-{round}"));
                    common::write_tool(&tool, "#!/bin/sh\nexit 0\n");
                    let status = Command::new(&tool).status().unwrap();
                    assert!(status.success());
                }
            });
        }
    });
}

#[test]
fn a_worktree_locked_by_a_killed_attempt_is_cleared() {
    let repo = Repo::new("locked-worktree");
    assert_eq!(repo.merge_killed("cargo fmt", "before").status.code(), None);
    let listing = repo.git(&["worktree", "list", "--porcelain"]);
    let path = listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .nth(1)
        .expect("the killed merge left its worktree")
        .to_string();
    repo.git(&["worktree", "lock", &path]);
    assert_merged(&repo, &repo.merge());
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
    let deadline = Instant::now() + Duration::from_secs(60);
    while !repo.calls().contains("gh issue view") {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "prepare");
}

#[test]
fn a_branch_that_moved_during_the_merge_is_not_overwritten() {
    let repo = Repo::new("moved");
    let output = repo.merge_running(
        "git push",
        &repo.update_ref("refs/heads/7-x", "refs/heads/main"),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert_eq!(repo.head(), repo.origin(&["rev-parse", "refs/heads/main"]));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn a_main_that_moved_during_the_proof_stops_the_merge() {
    let repo = Repo::new("main-moved");
    let output = repo.merge_running(
        "cargo build --release",
        &repo.update_ref("refs/heads/main", "refs/heads/7-x"),
    );
    assert_refused(&repo, &output, "`main` moved");
    assert_eq!(
        last_post(&repo),
        "failure `main` moved during the merge, so this head wasn't tested on it. Run it again"
    );
}

#[test]
fn a_pr_that_changed_during_the_proof_is_refused() {
    let cases = [
        (
            "label-refused",
            Pr {
                labels: &["spec-change"],
                ..Pr::default()
            },
            "prepare",
        ),
        (
            "body-changed",
            Pr {
                body: "Closes #7.\n\nHill: none, something else.\n",
                ..Pr::default()
            },
            "changed",
        ),
    ];
    for (name, next, message) in cases {
        let repo = Repo::new(&format!("changed-{name}"));
        repo.set_next_pr(&next);
        let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
        let output = repo.merge_running("cargo build --release", &swap);
        assert_refused(&repo, &output, message);
    }
}

#[test]
fn a_long_reason_is_cut_to_what_github_keeps() {
    let repo = Repo::new("long-reason");
    let base = "b".repeat(150);
    repo.set_next_pr(&Pr {
        base: &base,
        ..Pr::default()
    });
    let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
    let output = repo.merge_running("cargo build --release", &swap);
    let why = format!("PR 9 targets `{base}`, and only `main` is merged here");
    assert_refused(&repo, &output, &why);
    let cut: String = why.chars().take(140).collect();
    assert_eq!(last_post(&repo), format!("failure {cut}"));
}

/// The checks read only `spec-change`, and a person can park or unpark the
/// issue at any moment.
#[test]
fn a_label_the_checks_do_not_read_can_change_during_the_proof() {
    for label in ["land-needed", "answer-needed", "other"] {
        let repo = Repo::new(&format!("unread-{label}"));
        repo.set_next_pr(&Pr {
            labels: &[label],
            ..Pr::default()
        });
        let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
        assert_merged(&repo, &repo.merge_running("cargo build --release", &swap));
    }
}

#[test]
fn spec_change_appearing_during_a_prepare_is_refused() {
    let repo = Repo::new("prepare-gains-spec-change");
    repo.set_next_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
    let output = repo.prepare_running("cargo build --release", &swap);
    assert_refused(&repo, &output, "changed");
}

#[test]
fn the_callers_cargo_settings_do_not_reach_the_merge() {
    let repo = Repo::new("caller-config");
    repo.write(
        ".cargo/config.toml",
        "[build]\nrustc-wrapper = \"/bin/false\"\n",
    );
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_killed_merge_keeps_the_slot_until_its_squash_finishes() {
    let repo = Repo::new("squash-outlives");
    let killed = repo.merge_running("gh pr merge", "kill -9 $PPID; sleep 2");
    assert_eq!(killed.status.code(), None);
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("already merged"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        fs::read_to_string(repo.bin.join("merges")).unwrap(),
        "merged\n"
    );
}

#[test]
fn a_merge_resumes_from_another_worktree_and_clears_the_first() {
    let repo = Repo::new("other-worktree");
    let killed = repo.merge_killed("cargo fmt", "before");
    assert_eq!(killed.status.code(), None);
    let other = repo.temp.parent().unwrap().join("other");
    let name = other.display().to_string();
    repo.git(&["worktree", "add", "-q", "--detach", &name, "main"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .current_dir(&other)
        .output()
        .unwrap();
    assert_merged(&repo, &output);
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 2);
}

#[test]
fn a_record_commit_that_changes_more_is_not_merged() {
    let repo = Repo::new("sneaky");
    let hook = repo.root.join(".git/hooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    common::write_tool(
        &hook,
        "#!/bin/sh\necho x > sneaky.txt\ngit add sneaky.txt\n",
    );
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("sneaky.txt"),
        "{}",
        stderr(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (repo.head(), "failure Stopped by an error".to_string())
    );
}

#[test]
fn a_merge_past_its_timeout_stops_and_can_restart() {
    let repo = Repo::new("timeout");
    // On a loaded machine, whatever stops the merge at its timeout can be slow
    // to go on after the kill lands. This `kill` is that slow, once. It runs
    // the real one, because dash's builtin refuses `--`.
    let bin = repo.bin.display();
    let kill = external("kill");
    common::write_tool(
        &repo.bin.join("kill"),
        &format!(
            "#!/bin/sh\n'{kill}' \"$@\"\ncode=$?\nif [ \"$1\" = -KILL ] && [ ! -f '{bin}/slowed' ]; then : > '{bin}/slowed'; sleep 2; fi\nexit $code\n"
        ),
    );
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
    assert_merged(&repo, &repo.merge());
}

/// A command that a merge starts, and that outlives its parent: it ignores
/// hangups, and nothing waits for it. It writes its process id to `stray`.
fn stray(repo: &Repo) -> String {
    format!(
        "(trap '' HUP; exec sleep 30) </dev/null >/dev/null 2>&1 & echo $! > '{}/stray'",
        repo.bin.display()
    )
}

fn stray_pid(repo: &Repo) -> Option<String> {
    let text = fs::read_to_string(repo.bin.join("stray")).ok()?;
    text.ends_with('\n').then(|| text.trim().to_string())
}

/// A zombie counts as stopped, because only its reaping is left.
fn running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// A killed process needs a moment to die, so this waits a little for it.
#[track_caller]
fn assert_stray_stopped(repo: &Repo) {
    let pid = stray_pid(repo).expect("the stray command never started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(&pid) {
        if Instant::now() > deadline {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
            panic!("the stray command, process {pid}, kept running");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_merge_past_its_timeout_stops_what_it_started() {
    let repo = Repo::new("timeout-stray");
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("RUN_ON", "cargo fmt")
        .env("RUN_COMMAND", format!("{}; sleep 3", stray(&repo)))
        .env("XTASK_MERGE_TIMEOUT_SECONDS", "1")
        // As a merge that a merge's own tests start inherits it.
        .env(
            "XTASK_MERGE_WORKER_OF",
            std::os::unix::process::parent_id().to_string(),
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
    assert_stray_stopped(&repo);
}

#[test]
fn a_merge_that_ends_stops_what_it_started() {
    let repo = Repo::new("ended-stray");
    let output = repo.merge_running("cargo fmt", &stray(&repo));
    assert_merged(&repo, &output);
    assert_stray_stopped(&repo);
}

/// As on Ctrl-C, which reaches the caller's process group but not the merge's.
#[test]
fn a_merge_whose_caller_is_killed_stops_what_it_started() {
    let repo = Repo::new("caller-killed");
    let mut merge = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("RUN_ON", "cargo fmt")
        .env("RUN_COMMAND", format!("{}; sleep 30", stray(&repo)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while stray_pid(&repo).is_none() {
        assert!(
            Instant::now() < deadline,
            "the merge never reached `cargo fmt`"
        );
        thread::sleep(Duration::from_millis(50));
    }
    merge.kill().unwrap();
    merge.wait().unwrap();
    assert_stray_stopped(&repo);
    assert_merged(&repo, &repo.merge());
}

/// Each status that was posted, in order: its commit, and then its state and
/// description as `failure Failed: format`.
fn posts(repo: &Repo) -> Vec<(String, String)> {
    let prefix = "gh api -X POST repos/{owner}/{repo}/statuses/";
    repo.calls()
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .map(|post| {
            let (commit, fields) = post.split_once(" -f state=").expect(post);
            let (state, rest) = fields.split_once(" -f context=xtask/gate").expect(post);
            let said = rest.strip_prefix(" -f description=").unwrap_or(rest);
            let said = said.split(" -f target_url=").next().unwrap();
            (commit.to_string(), format!("{state} {said}"))
        })
        .collect()
}

/// What the newest status says.
fn last_post(repo: &Repo) -> String {
    posts(repo).pop().expect("a status was posted").1
}

#[test]
fn the_gate_posts_no_status() {
    let repo = Repo::new("gate-status");
    let output = repo.xtask(&["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(posts(&repo), []);

    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    assert_eq!(repo.xtask(&["gate"]).status.code(), Some(1));
    assert_eq!(posts(&repo), []);
}

#[test]
fn prepare_records_pushes_posts_and_never_squashes() {
    let repo = Repo::new("prepare");
    repo.advance_main("other.txt", "y\n");
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    assert_eq!(repo.commits_on_branch(), 2);
    assert!(stdout(&output).contains("ready"), "{}", stdout(&output));
    assert_eq!(repo.pr_comments(), "");
}

/// The phases go on the head that GitHub shows, and the score on the new one.
#[test]
fn a_proof_says_which_phase_it_is_in_and_then_the_score() {
    for command in ["merge", "prepare", "land"] {
        let repo = Repo::new(&format!("phases-{command}"));
        let repo = if command == "land" {
            repo.on_main()
        } else {
            repo
        };
        let before = repo.head();
        let output = repo.xtask(&[command, "9"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let after = repo.head();
        assert_ne!(before, after, "{command}");
        let said = |commit: &String, text: &str| (commit.clone(), text.to_string());
        assert_eq!(
            posts(&repo),
            [
                said(&before, "pending Proving: lint"),
                said(&before, "pending Proving: tests"),
                said(&before, "pending Proving: journey"),
                said(&after, "success spec 0 of 2, journey 1 of 1"),
            ],
            "{command}"
        );
    }
}

#[test]
fn a_proof_names_the_bench_only_when_it_runs() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "phases-bench",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n")],
    );
    assert_prepared(&repo, &repo.prepare());
    let said: Vec<String> = posts(&repo).into_iter().map(|(_, said)| said).collect();
    assert_eq!(
        said[..4],
        [
            "pending Proving: lint",
            "pending Proving: tests",
            "pending Proving: journey",
            "pending Proving: bench"
        ]
    );
    assert_eq!(said.len(), 5, "{said:?}");

    // Each is posted just before the work it names. The journey and the bench
    // both build a release.
    let calls = repo.calls();
    let order = [
        calls.find("Proving: lint"),
        calls.find("cargo fmt --all"),
        calls.find("cargo clippy --workspace"),
        calls.find("Proving: tests"),
        calls.find("cargo test"),
        calls.find("Proving: journey"),
        calls.find("cargo build --release"),
        calls.find("Proving: bench"),
        calls.rfind("cargo build --release"),
        calls.find("state=success"),
    ];
    assert!(order.iter().all(Option::is_some), "{calls}");
    assert!(order.is_sorted(), "{order:?}: {calls}");
}

/// The proof is complete once it is green, whatever the squash does.
#[test]
fn a_squash_that_fails_leaves_the_proof_green() {
    let repo = Repo::new("squash-fails");
    fs::write(repo.bin.join("merge-fails"), "").unwrap();
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("no merge"), "{}", stderr(&output));
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            repo.head(),
            "success spec 0 of 2, journey 1 of 1".to_string()
        )
    );
}

#[test]
fn a_status_links_to_its_run_only_inside_one() {
    let run = [
        ("GITHUB_SERVER_URL", "https://github.com"),
        ("GITHUB_REPOSITORY", "o/r"),
        ("GITHUB_RUN_ID", "5"),
    ];
    let link = " -f target_url=https://github.com/o/r/actions/runs/5";
    for (name, known, linked) in [("linked", &run[..], 4), ("unlinked", &run[..2], 0)] {
        let repo = Repo::new(&format!("status-{name}"));
        let output = repo
            .command(env!("CARGO_BIN_EXE_xtask"))
            .args(["prepare", "9"])
            .envs(known.iter().copied())
            .output()
            .unwrap();
        assert_prepared(&repo, &output);
        let calls = repo.calls();
        let ending = calls.lines().filter(|line| line.ends_with(link)).count();
        assert_eq!(ending, linked, "{name}: {calls}");
        assert_eq!(calls.matches("target_url").count(), linked, "{name}");
    }
}

/// A `pending` only says what is happening, so losing one loses nothing else.
#[test]
fn a_pending_that_can_not_be_posted_changes_nothing() {
    let repo = Repo::new("prepare-no-pending");
    fs::write(repo.bin.join("pending-fails"), "").unwrap();
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    assert!(
        stderr(&output).contains("no pending"),
        "{}",
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 1 of 1");
}

const LIVE: &str = "2999-01-01T00:00Z";

#[test]
fn a_thread_that_does_not_hold_the_issue_can_not_merge_or_prepare() {
    for args in [
        &["merge", "9"][..],
        &["merge", "9", "--thread", "other"],
        &["prepare", "9"],
        &["prepare", "9", "--thread", "other"],
        &["prepare", "9", "--keep-budgets", "--thread", "other"],
    ] {
        let repo = Repo::new("not-holder");
        repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
        let output = repo.xtask(args);
        assert_refused(&repo, &output, "held by thread `holder`");
        assert!(
            !repo.calls().contains("api -X"),
            "{args:?}: {}",
            repo.calls()
        );
    }
}

#[test]
fn the_thread_that_holds_the_issue_merges_and_prepares() {
    let repo = Repo::new("holder-merges");
    repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
    let output = repo.xtask(&["merge", "9", "--thread", "holder"]);
    assert_merged(&repo, &output);

    let repo = Repo::new("holder-prepares");
    repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
    let output = repo.xtask(&["prepare", "9", "--thread", "holder"]);
    assert_prepared(&repo, &output);
}

#[test]
fn with_no_live_claim_any_thread_prepares() {
    for (labels, lease) in [
        (&["auto:pr", "land-needed", "in-progress"][..], LIVE),
        (&["auto:pr", "in-progress"], "2000-01-01T00:00Z"),
        (&["auto:pr"], LIVE),
    ] {
        let repo = Repo::new("no-claim");
        repo.set_held_issue(labels, "holder", lease);
        let output = repo.prepare();
        assert_prepared(&repo, &output);
    }
}

#[test]
fn prepare_needs_no_auto_merge_and_takes_a_spec_change_pr() {
    let repo = Repo::new("prepare-labels");
    repo.set_issue(&[]);
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn merge_has_no_owner_flag_and_prepare_takes_one_pr() {
    let repo = Repo::new("no-owner-flag");
    let output = repo.xtask(&["merge", "9", "--owner"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("`merge` takes"));
    assert!(!repo.calls().contains("gh "), "{}", repo.calls());
    for args in [&["prepare"][..], &["prepare", "x"], &["prepare", "9", "x"]] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`prepare` takes"), "{args:?}");
    }
}

#[test]
fn a_prepare_can_run_again() {
    let repo = Repo::new("prepare-again");
    assert_prepared(&repo, &repo.prepare());
    repo.advance_main("other.txt", "y\n");
    assert_prepared(&repo, &repo.prepare());
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_killed_prepare_is_not_green_and_resumes() {
    let repo = Repo::new("prepare-killed");
    let killed = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["prepare", "9"])
        .env("KILL_ON", "state=success")
        .env("KILL_WHEN", "before")
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), None);
    repo.forget_calls();
    assert_prepared(&repo, &repo.prepare());
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_lowered_count_is_accepted_in_one_comment_and_merge_refuses_it() {
    let repo = with_base("accept-spec", 1, 1);
    assert_prepared(&repo, &repo.prepare());
    let comments = repo.pr_comments();
    assert!(comments.contains("spec fell from 1 to 0"), "{comments}");
    assert!(comments.contains(&repo.head()), "{comments}");
    assert_eq!(repo.calls().matches("gh pr comment").count(), 1);

    let repo = with_base("accept-journey", 0, 2);
    assert_prepared(&repo, &repo.prepare());
    let comments = repo.pr_comments();
    assert!(comments.contains("journey fell from 2 to 1"), "{comments}");
}

#[test]
fn a_prepared_branch_reads_its_own_record() {
    let repo = with_base("own-record", 0, 2);
    assert_prepared(&repo, &repo.prepare());
    repo.git(&["fetch", "-q", "origin", BRANCH]);
    repo.git(&["checkout", "-q", "--detach", "FETCH_HEAD"]);
    let output = repo.xtask(&["score"]);
    assert!(
        stdout(&output).contains("journey 1 of 1"),
        "{}",
        stdout(&output)
    );
}

/// The branch's head, which nothing replaced, is red and says why.
#[track_caller]
fn assert_failure_posted(repo: &Repo, output: &Output, head: &str, why: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert_eq!(
        posts(repo).pop().unwrap(),
        (head.to_string(), format!("failure {why}"))
    );
    assert_eq!(repo.head(), head);
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn a_failed_proof_posts_failure_on_the_current_head() {
    let repo = Repo::new("prepare-fails");
    repo.advance_main("other.txt", "y\n");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let head = repo.head();
    assert_failure_posted(&repo, &repo.prepare(), &head, "Failed: format");
}

#[test]
fn a_branch_that_does_not_rebase_posts_failure() {
    let repo = Repo::new("prepare-conflict");
    repo.advance_main("docs.txt", "from main\n");
    let head = repo.head();
    assert_failure_posted(&repo, &repo.prepare(), &head, NO_REBASE);
}

#[test]
fn a_failure_that_can_not_be_posted_is_still_a_failure() {
    let repo = Repo::new("prepare-no-post");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    fs::write(repo.bin.join("status-fails"), "").unwrap();
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("no status"), "{}", stderr(&output));
}

/// An agent's `merge` leaves the reason on the PR too, with every check that
/// failed, in the order they ran.
#[test]
fn a_failed_merge_posts_failure() {
    let repo = Repo::new("merge-failure");
    let unused = "pub  fn f( ) {\n    let x = 1;\n}\n";
    repo.change_branch("crates/demo/src/lib.rs", unused);
    let head = repo.head();
    assert_failure_posted(&repo, &repo.merge(), &head, "Failed: format, lint");
}

fn based(proven: u32, base: &str) -> String {
    record(proven, 1, "").replace("date =", &format!("base = \"{base}\"\ndate ="))
}

fn main_commit(repo: &Repo) -> String {
    repo.origin(&["rev-parse", "refs/heads/main"])
}

#[test]
fn a_record_names_the_main_it_was_measured_on() {
    let repo = Repo::new("record-base");
    repo.advance_main("other.txt", "y\n");
    let main = main_commit(&repo);
    assert_prepared(&repo, &repo.prepare());
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains(&format!("base = \"{main}\"\n")), "{record}");
}

#[test]
fn a_record_measured_on_an_older_main_is_not_the_floor() {
    let repo = Repo::new("stale-record");
    repo.advance_main("harness/ledger/8.toml", &based(1, &"0".repeat(40)));
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_record_measured_on_its_parent_is_the_floor() {
    let repo = Repo::new("fresh-record");
    let parent = main_commit(&repo);
    repo.advance_main("harness/ledger/8.toml", &based(1, &parent));
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail hills"),
        "{}",
        stdout(&output)
    );
}

/// `main` gains a commit that changes `path` from `before` to `after`, and adds
/// a ledger record and a repeated line. The branch undoes it, with a commit
/// message naming `claimed`, or that commit if there is none.
fn revert_of(
    name: &str,
    (path, before, after): (&str, &str, &str),
    claimed: Option<&str>,
) -> (Repo, String) {
    let repo = Repo::new(name);
    repo.git(&["checkout", "-q", "main"]);
    repo.write("order.txt", "a\nb\n");
    repo.commit();
    repo.git(&["push", "-q", "origin", "main"]);
    let parent = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.write(path, after);
    repo.write("order.txt", "b\na\nb\n");
    repo.write("harness/ledger/8.toml", &record(0, 1, ""));
    repo.commit();
    repo.git(&["push", "-q", "origin", "main"]);
    let added = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.git(&["checkout", "-q", "-B", BRANCH, "main"]);
    repo.write(path, before);
    repo.write("order.txt", "a\nb\n");
    repo.git(&["add", "-A"]);
    let message = format!(
        "Undo it\n\nThis reverts commit {}.",
        claimed.unwrap_or(&added)
    );
    repo.git(&["commit", "-q", "-m", &message]);
    repo.git(&["push", "-q", "-f", "origin", BRANCH]);
    (repo, parent)
}

fn revert_of_a_check(name: &str, claimed: Option<&str>) -> (Repo, String) {
    revert_of(
        name,
        ("crates/demo/src/lib.rs", NO_CHECK, CHECK_ONE),
        claimed,
    )
}

#[track_caller]
fn assert_floor_held(repo: &Repo, output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(
        stdout(output).contains("fail checks-kept"),
        "{}",
        stdout(output)
    );
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn the_owner_can_merge_an_exact_revert_that_removes_a_check() {
    let (repo, _) = revert_of_a_check("exact-revert", None);
    let output = repo.prepare();
    assert!(
        stdout(&output).contains("pass checks-kept"),
        "{}",
        stdout(&output)
    );
    assert_prepared(&repo, &output);
    let comments = repo.pr_comments();
    assert!(comments.contains("exact revert"), "{comments}");
    assert_eq!(repo.calls().matches("gh pr comment").count(), 1);
}

#[test]
fn an_exact_revert_still_needs_the_owner() {
    let (repo, _) = revert_of_a_check("revert-no-owner", None);
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_revert_with_anything_else_in_it_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-plus", None);
    repo.change_branch("docs.txt", "and something else\n");
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_revert_of_a_commit_outside_the_history_keeps_the_floor() {
    let (repo, parent) = revert_of_a_check("revert-elsewhere", Some(&"0".repeat(40)));
    repo.git(&["checkout", "-q", "--detach", &parent]);
    repo.write("crates/demo/src/lib.rs", CHECK_ONE);
    repo.write("order.txt", "b\na\nb\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "The same change, on no branch"]);
    let twin = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.git(&["checkout", "-q", BRANCH]);
    repo.git(&[
        "commit",
        "-q",
        "--amend",
        "-m",
        &format!("Undo it\n\nThis reverts commit {twin}."),
    ]);
    repo.git(&["push", "-q", "-f", "origin", BRANCH]);
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_message_that_names_no_commit_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-unnamed", Some(&"0".repeat(40)));
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn undoing_the_same_lines_somewhere_else_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-elsewhere-in-file", None);
    repo.change_branch("order.txt", "b\na\n");
    assert_floor_held(&repo, &repo.prepare());
}

#[test]
fn a_revert_that_edits_the_ledger_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-ledger", None);
    repo.change_branch("harness/ledger/3.toml", &record(0, 1, "version_ms = 1\n"));
    assert_floor_held(&repo, &repo.prepare());
}

#[test]
fn the_owner_can_merge_a_revert_of_a_commit_that_broke_the_build() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "revert-build",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_change_that_only_looks_like_a_revert_is_benched() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "fake-revert-build",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    repo.change_branch("docs.txt", "and something else\n");
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("build"), "{}", stderr(&output));
    assert_eq!(last_post(&repo), "failure Stopped by an error");
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
}

#[test]
fn a_revert_without_the_owner_is_benched() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "revert-no-owner-bench",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("build"), "{}", stderr(&output));
}

#[test]
fn land_squashes_what_only_the_owner_may_merge() {
    let repo = Repo::new("land-owner").on_main();
    repo.set_issue(&[]);
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    let output = repo.land();
    assert_merged(&repo, &output);

    let head = repo.head();
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
    assert_eq!(repo.commits_merged(), 2);
}

#[test]
fn land_waits_for_the_merge_slot() {
    let repo = Repo::new("land-slot").on_main();
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["land", "9"])
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

/// A merge that lands during the wait can change the checks themselves.
#[test]
fn land_restarts_if_main_moves_while_it_waits_for_the_slot() {
    let repo = Repo::new("land-slot-moved").on_main();
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["land", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let newest = repo.head();
    repo.origin(&["update-ref", "refs/heads/main", &newest]);
    held.unlock().unwrap();
    child.wait_with_output().unwrap();

    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), newest);
    let calls = repo.calls();
    assert!(calls.contains("cargo xtask land 9"), "{calls}");
    assert!(!calls.contains("gh pr merge"), "{calls}");
}

#[test]
fn land_keeps_the_slot_until_its_squash_finishes() {
    let repo = Repo::new("land-outlives").on_main();
    let killed = repo.land_running("gh pr merge", "kill -9 $PPID; sleep 2");
    assert_eq!(killed.status.code(), None);
    let output = repo.land();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("already merged"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        fs::read_to_string(repo.bin.join("merges")).unwrap(),
        "merged\n"
    );
}

/// The PR is ready for a merge button, and `land` said what that accepts.
#[track_caller]
fn assert_stopped_to_accept(repo: &Repo, output: &Output, accepted: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(stderr(output).contains(accepted), "{}", stderr(output));
    assert_left_prepared(repo);
    let comments = repo.pr_comments();
    assert!(comments.contains(accepted), "{comments}");
}

#[test]
fn land_stops_before_accepting_a_lowered_count() {
    let repo = with_base("land-lower", 1, 1).on_main();
    let output = repo.land();
    assert_stopped_to_accept(&repo, &output, "spec fell from 1 to 0");
}

#[test]
fn land_stops_before_accepting_a_revert() {
    let (repo, _) = revert_of_a_check("land-revert", None);
    let repo = repo.on_main();
    let output = repo.land();
    assert_stopped_to_accept(&repo, &output, "exact revert");
}

#[test]
fn land_merges_nothing_when_the_gate_fails() {
    let repo = Repo::new("land-gate");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let repo = repo.on_main();
    let head = repo.head();
    assert_failure_posted(&repo, &repo.land(), &head, "Failed: format");
}

#[test]
fn land_merges_nothing_when_the_branch_does_not_rebase() {
    let repo = Repo::new("land-conflict");
    repo.advance_main("docs.txt", "from main\n");
    let repo = repo.on_main();
    let head = repo.head();
    assert_failure_posted(&repo, &repo.land(), &head, NO_REBASE);
}

#[test]
fn land_merges_nothing_when_main_moves_during_the_proof() {
    let repo = Repo::new("land-moved").on_main();
    let elsewhere = repo.head();
    let moved = repo.update_ref("refs/heads/main", &elsewhere);
    let output = repo.land_running("cargo build --release", &moved);
    assert_refused(&repo, &output, "moved");
}

#[test]
fn land_succeeds_on_an_already_merged_pr_without_merging_again() {
    let repo = Repo::new("land-merged").on_main();
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = repo.land();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repo.calls().contains("gh pr merge"));
}

#[test]
fn land_updates_a_checkout_behind_main_and_restarts() {
    let repo = Repo::new("land-behind");
    repo.advance_main("other.txt", "y\n");
    let repo = repo.on_main();
    repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
    repo.forget_calls();
    let newest = repo.origin(&["rev-parse", "refs/heads/main"]);
    let seen = repo.bin.join("restart");
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["land", "9"])
        .env(
            "RUSTUP_TOOLCHAIN",
            env::var("RUSTUP_TOOLCHAIN").unwrap_or_default(),
        )
        .env("RUN_ON", "cargo xtask land 9")
        .env(
            "RUN_COMMAND",
            // The fixture has no xtask to build, so the test's binary stands
            // in for the rebuilt one.
            format!(
                "echo \"toolchain=$RUSTUP_TOOLCHAIN\" > '{}'; exec '{}' land 9",
                seen.display(),
                env!("CARGO_BIN_EXE_xtask")
            ),
        )
        .output()
        .unwrap();

    assert_merged(&repo, &output);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), newest);
    let calls = repo.calls();
    let restart = calls.find("cargo xtask land 9").expect(&calls);
    let first_gh = calls.find("gh ").expect(&calls);
    assert!(restart < first_gh, "{calls}");
    // Rustup sets this for its children, and it would outrank a toolchain
    // file that the update just changed.
    assert_eq!(fs::read_to_string(seen).unwrap(), "toolchain=\n");
}

#[track_caller]
fn assert_checkout_refused(repo: &Repo, output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(stderr(output).contains(message), "{}", stderr(output));
    let calls = repo.calls();
    for forbidden in ["gh ", "--ff-only", "cargo xtask"] {
        assert!(!calls.contains(forbidden), "{forbidden}: {calls}");
    }
}

#[test]
fn land_refuses_local_commits_on_main() {
    let repo = Repo::new("land-ahead").on_main();
    repo.write("local.txt", "x\n");
    repo.commit();
    repo.forget_calls();
    assert_checkout_refused(&repo, &repo.land(), "local commits");
}

#[test]
fn land_refuses_changes_to_tracked_files() {
    let repo = Repo::new("land-changed").on_main();
    repo.write("SPEC.md", "# Changed\n");
    assert_checkout_refused(&repo, &repo.land(), "tracked files");
}

#[test]
fn land_refuses_another_branch_that_is_not_at_main() {
    let repo = Repo::new("land-branch");
    repo.forget_calls();
    assert_checkout_refused(&repo, &repo.land(), "isn't at `origin/main`");
}

#[test]
fn land_runs_from_anything_at_the_newest_main() {
    let repo = Repo::new("land-detached").on_main();
    repo.git(&["checkout", "-q", "--detach"]);
    repo.forget_calls();
    assert_merged(&repo, &repo.land());
}

#[test]
fn land_allows_untracked_files() {
    let repo = Repo::new("land-untracked").on_main();
    repo.write("notes.txt", "x\n");
    assert_merged(&repo, &repo.land());
}

#[test]
fn land_takes_one_pr() {
    let repo = Repo::new("land-usage").on_main();
    for args in [&["land"][..], &["land", "x"], &["land", "9", "x"]] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`land` takes"), "{args:?}");
    }
    assert!(!repo.calls().contains("gh "), "{}", repo.calls());
}

/// One row of `gh pr list`.
fn listed(number: u64, branch: &str, head: &str, closes: u64, fork: bool) -> String {
    format!(
        "{{\"number\":{number},\"title\":\"t\",\"labels\":[],\"url\":\"https://github.com/o/r/pull/{number}\",\"isCrossRepository\":{fork},\"headRefName\":{},\"headRefOid\":{},\"closingIssuesReferences\":[{{\"number\":{closes},\"url\":\"https://github.com/o/r/issues/{closes}\"}}]}}",
        quoted(branch),
        quoted(head)
    )
}

const KEPT: &str = "version_ms = 12.5\n";

fn status(head: &str, state: &str) -> String {
    format!(
        "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state={state} -f context=xtask/gate"
    )
}

impl Repo {
    fn set_open_prs(&self, rows: &[String]) {
        fs::write(self.bin.join("prs.json"), format!("[{}]\n", rows.join(","))).unwrap();
    }

    /// Which open issues have `land-needed`. Issue 2 has another label.
    fn set_land_needed(&self, issues: &[u64]) {
        let mut rows = vec![
            "{\"number\":2,\"title\":\"t\",\"labels\":[{\"name\":\"auto:pr\"}],\"isPinned\":false}"
                .to_string(),
        ];
        rows.extend(issues.iter().map(|number| {
            format!("{{\"number\":{number},\"title\":\"t\",\"labels\":[{{\"name\":\"land-needed\"}}],\"isPinned\":false}}")
        }));
        fs::write(
            self.bin.join("issues.json"),
            format!("[{}]\n", rows.join(",")),
        )
        .unwrap();
    }

    /// A branch for another PR, cut from `main`. `pr` is what `gh pr view` says.
    fn other_pr(&self, number: u64, branch: &str, pr: &Pr) {
        self.git(&["checkout", "-q", "main"]);
        self.git(&["checkout", "-q", "-b", branch]);
        self.write(&format!("{branch}.txt"), "x\n");
        self.commit();
        self.git(&["push", "-q", "origin", branch]);
        self.git(&["checkout", "-q", BRANCH]);
        let json = pr_json(pr, pr.state).replace(BRANCH, branch);
        fs::write(self.bin.join(format!("pr-{number}.json")), json).unwrap();
    }

    /// Adds the record that an earlier `prepare` leaves at the tip of a branch.
    fn prepared(&self, branch: &str, issue: u64, reason: &str) {
        self.git(&["checkout", "-q", branch]);
        let record =
            record(0, 1, KEPT).replace("reason = \"\"", &format!("reason = {}", quoted(reason)));
        self.write(&format!("harness/ledger/{issue}.toml"), &record);
        self.commit();
        self.git(&["push", "-q", "origin", branch]);
        self.git(&["checkout", "-q", BRANCH]);
    }

    fn head_of(&self, branch: &str) -> String {
        self.origin(&["rev-parse", &format!("refs/heads/{branch}")])
    }

    /// From a checkout of `main`, as the workflow runs it.
    fn refresh(&self) -> Output {
        self.git(&["checkout", "-q", "main"]);
        self.xtask(&["refresh"])
    }
}

#[test]
fn a_refresh_proves_each_stale_land_needed_pr_again_oldest_first() {
    let repo = Repo::new("refresh");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            ..Pr::default()
        },
    );
    repo.other_pr(
        6,
        "2-z",
        &Pr {
            closes: &[2],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    // Cut from the new `main`, so it isn't behind.
    repo.other_pr(
        4,
        "6-up",
        &Pr {
            closes: &[6],
            ..Pr::default()
        },
    );
    repo.prepared("6-up", 6, "");
    let before: Vec<String> = [BRANCH, "8-y", "2-z", "6-up"]
        .iter()
        .map(|branch| repo.head_of(branch))
        .collect();
    repo.set_open_prs(&[
        listed(9, BRANCH, &before[0], 7, false),
        listed(4, "6-up", &before[3], 6, false),
        listed(6, "2-z", &before[2], 2, false),
        listed(5, "8-y", &before[1], 8, false),
    ]);
    repo.set_land_needed(&[6, 7, 8]);
    repo.forget_calls();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    let at = |text: &str| {
        calls
            .find(text)
            .unwrap_or_else(|| panic!("no `{text}` in:\n{calls}"))
    };
    let green_five = at(&status(&repo.head_of("8-y"), "success"));
    let green_nine = at(&status(&repo.head_of(BRANCH), "success"));
    assert!(at(&status(&before[1], "pending")) < at(&status(&before[0], "pending")));
    at(&format!(
        "{} -f description=Queued, next\n",
        status(&before[1], "pending")
    ));
    at(&format!(
        "{} -f description=Queued, 1 PR ahead\n",
        status(&before[0], "pending")
    ));
    assert!(at(&status(&before[0], "pending")) < green_five, "{calls}");
    assert!(green_five < green_nine, "{calls}");
    for (branch, head) in [("2-z", &before[2]), ("6-up", &before[3])] {
        assert_eq!(&repo.head_of(branch), head);
        assert!(!calls.contains(&format!("statuses/{head}")), "{calls}");
    }
    let out = stdout(&output);
    let line = |text: &str| {
        out.find(&format!("{text}\n"))
            .unwrap_or_else(|| panic!("no `{text}` in:\n{out}"))
    };
    assert!(line("refresh: proving PR 5 (1 of 2)") < line("refresh: proving PR 9 (2 of 2)"));
    // Outside Actions, a fold marker is noise.
    assert!(!out.contains("::"), "{out}");
}

/// In Actions, each proof folds under its PR, and the run's page sums them up.
#[test]
fn a_refresh_in_actions_folds_each_proof_and_sums_them_up() {
    let repo = Repo::new("refresh-actions");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    repo.other_pr(
        4,
        "6-up",
        &Pr {
            closes: &[6],
            ..Pr::default()
        },
    );
    let (nine, five, four) = (
        repo.head_of(BRANCH),
        repo.head_of("8-y"),
        repo.head_of("6-up"),
    );
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false).replace("\"title\":\"t\"", "\"title\":\"Nine\""),
        listed(4, "6-up", &four, 6, false),
    ]);
    repo.set_land_needed(&[6, 7, 8]);
    let summary = repo.temp.join("summary.md");
    fs::write(&summary, "earlier\n").unwrap();
    repo.git(&["checkout", "-q", "main"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    let line = |text: &str| {
        out.find(&format!("{text}\n"))
            .unwrap_or_else(|| panic!("no `{text}` in:\n{out}"))
    };
    let proving = line("refresh: proving PR 9 (2 of 2)");
    let opened = line("::group::PR 9 (2 of 2): Nine");
    let proved = out.find("prepare: PR 9 is ready to merge").expect(&out);
    assert!(line("::group::PR 5 (1 of 2): t") < proving, "{out}");
    assert!(proving < opened && opened < proved, "{out}");
    assert!(proved < out.rfind("::endgroup::\n").unwrap(), "{out}");
    assert_eq!(out.matches("::group::").count(), 2, "{out}");
    assert_eq!(out.matches("::endgroup::").count(), 2, "{out}");

    let summary = fs::read_to_string(&summary).unwrap();
    let rows: Vec<&str> = summary.lines().collect();
    assert_eq!(
        rows[..4],
        [
            "earlier",
            "",
            "| PR | Verdict | Status | Took |",
            "|---|---|---|---|"
        ]
    );
    assert_eq!(rows[4], "| #4 | up to date | | |", "{summary}");
    assert!(
        rows[5].starts_with("| #5 | not proven | Not proven | "),
        "{summary}"
    );
    assert!(rows[6].starts_with("| #9 | proven | spec "), "{summary}");
    for row in &rows[5..] {
        assert!(row.ends_with("s |"), "{summary}");
    }
    assert_eq!(rows.len(), 7, "{summary}");
}

#[test]
fn a_failed_refresh_turns_that_pr_red_and_goes_on_to_the_next() {
    let repo = Repo::new("refresh-fails");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    let (nine, five) = (repo.head_of(BRANCH), repo.head_of("8-y"));
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false),
    ]);
    repo.set_land_needed(&[7, 8]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    // Its `prepare` refused it before the fetch, and so said nothing.
    let unproven = format!("{} -f description=Not proven\n", status(&five, "failure"));
    assert!(calls.contains(&unproven), "{calls}");
    assert_eq!(repo.head_of("8-y"), five);
    assert!(
        calls.contains(&status(&repo.head_of(BRANCH), "success")),
        "{calls}"
    );
}

/// A fork's branch isn't in this repository, and `prepare` refuses it.
#[test]
fn a_refresh_runs_nothing_from_a_fork() {
    let repo = Repo::new("refresh-fork");
    repo.set_pr(&Pr {
        fork: true,
        ..Pr::default()
    });
    let head = "a".repeat(40);
    repo.set_open_prs(&[listed(9, "patch", &head, 7, true)]);
    repo.set_land_needed(&[7]);
    repo.forget_calls();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(calls.contains(&status(&head, "pending")), "{calls}");
    let unproven = format!("{} -f description=Not proven\n", status(&head, "failure"));
    assert!(calls.contains(&unproven), "{calls}");
    assert!(!calls.contains("worktree add"), "{calls}");
}

#[test]
fn a_refresh_keeps_the_branchs_budgets_and_runs_no_bench() {
    let base = record(0, 1, "version_ms = 99\n");
    let repo = Repo::with(
        "refresh-budgets",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", SLOW)],
    );
    repo.prepared(BRANCH, 7, "worth it");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains(KEPT.trim_end()), "{record}");
    assert!(record.contains("reason = \"worth it\"\n"), "{record}");
    let main = repo.origin(&["rev-parse", "refs/heads/main"]);
    assert!(record.contains(&format!("base = \"{main}\"\n")), "{record}");
}

/// Commits after the record mean that it doesn't describe this branch.
#[test]
fn a_refresh_with_no_record_at_the_tip_fails_and_pushes_nothing() {
    let repo = Repo::new("refresh-no-record");
    repo.prepared(BRANCH, 7, "");
    repo.change_branch("more.txt", "x\n");
    repo.advance_main("later.txt", "x\n");
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("no ledger record"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.head(), head);
    // `prepare` said why, and `refresh` leaves that standing.
    let failures: Vec<(String, String)> = posts(&repo)
        .into_iter()
        .filter(|(_, said)| said.starts_with("failure"))
        .collect();
    let why = "failure the tip of PR 9 has no ledger record, so there are no budgets to keep. Run `cargo xtask prepare 9`";
    assert_eq!(failures, [(head, why.to_string())]);
}

/// A `prepare` that was stopped after it posted the green did prove the PR.
#[test]
fn a_refresh_leaves_a_green_that_its_prepare_posted() {
    let repo = Repo::new("refresh-green");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);
    let summary = repo.temp.join("summary.md");

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("KILL_ON", "state=success")
        .env("KILL_WHEN", "after")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(repo.bin.join("killed").exists());
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            repo.head(),
            "success spec 0 of 2, journey 1 of 1".to_string()
        )
    );
    let summary = fs::read_to_string(&summary).unwrap();
    let row = "| #9 | proven | spec 0 of 2, journey 1 of 1 | ";
    assert!(summary.contains(row), "{summary}");
}

/// The queue position only says what is happening, as any `pending` does.
#[test]
fn a_refresh_proves_a_pr_whose_queue_position_can_not_be_posted() {
    let repo = Repo::new("refresh-no-pending");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    fs::write(repo.bin.join("pending-fails"), "").unwrap();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 1 of 1");
}

#[test]
fn a_change_to_github_workflows_needs_the_label() {
    let repo = Repo::new("protected-github");
    repo.change_branch(".github/workflows/x.yml", "name: x\n");
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail labels"),
        "{}",
        stdout(&output)
    );

    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_prepared(&repo, &repo.prepare());
}

/// The checks compile from the checkout, so older checks can't prove a newer
/// `main`. The run that the newer push started proves this PR instead.
#[test]
fn a_refresh_built_from_an_older_main_proves_nothing() {
    let repo = Repo::new("refresh-older-checks");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("older `main`"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.head(), head);
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            head,
            "failure these checks were built from an older `main`, so they can't prove PR 9 on the newest one"
                .to_string()
        )
    );
}

const BUDGETS_NOT_YET: &str = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n\n[[budget]]\nkey = \"run_ms\"\nname = \"r\"\nband_pct = 20.0\nband_abs = 25.0\n";

/// `land` runs `main`'s xtask, which can't measure what only the PR teaches
/// it to measure.
#[test]
fn land_sends_a_pr_that_may_add_a_measurer_to_prepare_on_its_branch() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "land-measurer",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", BUDGETS_NOT_YET),
        ],
        &[("xtask/src/bench.rs", "// the run measurer\n")],
    )
    .on_main();
    repo.set_pr(&Pr {
        body: "Closes #7.\n",
        labels: &["spec-change"],
        ..Pr::default()
    });
    let output = repo.land();
    assert_refused(&repo, &output, "doesn't measure `run_ms` yet");
    let said = stderr(&output);
    assert!(
        said.contains("`cargo xtask prepare 9` from a checkout of its branch"),
        "{said}"
    );
    assert!(!said.contains("Hill: none"), "{said}");
    assert_eq!(posts(&repo), [], "{}", repo.calls());
}

#[test]
fn land_takes_a_change_to_a_measurer_when_every_budget_is_measured() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "land-measurer-known",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("xtask/src/bench.rs", "// the boot measurer\n")],
    )
    .on_main();
    repo.set_pr(&Pr {
        labels: &["spec-change"],
        ..Pr::default()
    });
    assert_merged(&repo, &repo.land());
}
