//! Runs `record` and `score` in real git repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SPEC: &str = "# A\n\nOne. Two.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";
const AP: &str = "fn main() {}\n";
const JOURNEY: &str = "[[milestone]]\nname = \"m\"\nsummary = \"s\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"s\"\nmilestone = \"m\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const STEP: &str = "#!/bin/sh\nexit 0\n";

fn record_text(issue: u32, budgets: &str) -> String {
    format!(
        "issue = {issue}\npr = 9\ncommit = \"old\"\ndate = \"2026-01-01\"\nhost = \"linux-x86_64\"\nspec = {{ proven = 0, promises = 2 }}\njourney = {{ passing = 0, steps = 1 }}\nreason = \"\"\n\n[budgets]\n{budgets}"
    )
}

struct Repo {
    root: PathBuf,
}

fn command(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("XTASK_ACTIVITY_HELD");
    command
}

impl Repo {
    fn new(name: &str, extra: &[(&str, &str)]) -> Repo {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("ledger")
            .join(name);
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        let repo = Repo { root };
        let ap = "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let demo = "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let base = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", SPEC),
            ("crates/ap/Cargo.toml", ap),
            ("crates/ap/src/main.rs", AP),
            ("crates/demo/Cargo.toml", demo),
            ("crates/demo/src/lib.rs", CHECK_ONE),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/one.sh", STEP),
        ];
        for (path, content) in base.iter().chain(extra) {
            repo.write(path, content);
        }
        let output = command("cargo", &repo.root)
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success());
        repo.git(&["init", "-q", "-b", "main"]);
        repo.commit();
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[track_caller]
    fn git(&self, args: &[&str]) -> String {
        let output = command("git", &self.root).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "change",
        ]);
    }

    fn xtask(&self, args: &[&str]) -> Output {
        command(env!("CARGO_BIN_EXE_xtask"), &self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn record(&self, issue: &str) -> Output {
        self.xtask(&["record", "--issue", issue, "--pr", "130"])
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn record_writes_the_scores_of_the_current_head() {
    let repo = Repo::new("writes", &[]);
    let output = repo.record("123");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "harness/ledger/123.toml\n");
    let text = fs::read_to_string(repo.root.join("harness/ledger/123.toml")).unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let date = repo.git(&["show", "-s", "--format=%cs", "HEAD"]);
    let host = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    assert_eq!(
        text,
        format!(
            "issue = 123\npr = 130\ncommit = \"{0}\"\nbase = \"{0}\"\ndate = \"{1}\"\nhost = \"{host}\"\nspec = {{ proven = 1, promises = 2 }}\njourney = {{ passing = 1, steps = 1 }}\nreason = \"\"\n\n[budgets]\n",
            head.trim(),
            date.trim(),
        )
    );
}

#[test]
fn record_carries_budgets_over_from_the_previous_record() {
    let previous = record_text(5, "boot_ms = 141.2\nbinary_bytes = 9312000\n");
    let repo = Repo::new("carries", &[("harness/ledger/5.toml", previous.as_str())]);
    repo.record("6");
    let text = fs::read_to_string(repo.root.join("harness/ledger/6.toml")).unwrap();
    assert!(
        text.ends_with("[budgets]\nbinary_bytes = 9312000\nboot_ms = 141.2\n"),
        "{text}"
    );
}

#[test]
fn record_can_write_over_an_earlier_attempt() {
    let repo = Repo::new("again", &[]);
    assert_eq!(repo.record("7").status.code(), Some(0));
    assert_eq!(repo.record("7").status.code(), Some(0));
}

#[test]
fn record_needs_both_numbers() {
    let repo = Repo::new("usage", &[]);
    for args in [
        &["record"][..],
        &["record", "--issue", "1"][..],
        &["record", "--pr", "1", "--issue", "x"][..],
        &["record", "--issue", "x", "--pr", "1"][..],
        &["record", "--issue", "1", "--pr", "1", "extra"][..],
    ] {
        assert_eq!(repo.xtask(args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn record_refuses_a_tree_that_differs_from_head() {
    let repo = Repo::new("dirty", &[]);
    repo.write("notes.txt", "uncommitted\n");
    let output = repo.record("1");
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("notes.txt"), "{}", stderr(&output));
    assert!(!repo.root.join("harness/ledger/1.toml").exists());
}

#[test]
fn record_sees_untracked_files_that_git_config_hides() {
    let repo = Repo::new("dirty-hidden", &[]);
    repo.git(&["config", "status.showUntrackedFiles", "no"]);
    repo.write("notes.txt", "uncommitted\n");
    let output = repo.record("1");
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("notes.txt"), "{}", stderr(&output));
    assert!(!repo.root.join("harness/ledger/1.toml").exists());
}

#[test]
fn record_outside_a_repository_is_a_problem() {
    let repo = Repo::new("no-repo", &[]);
    fs::remove_dir_all(repo.root.join(".git")).unwrap();
    let output = repo.record("1");
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(!repo.root.join("harness/ledger/1.toml").exists());
}

#[test]
fn score_prints_the_journey_from_the_newest_record() {
    let previous = record_text(5, "").replace("passing = 0, steps = 1", "passing = 4, steps = 19");
    let repo = Repo::new("score", &[("harness/ledger/5.toml", previous.as_str())]);
    let output = repo.xtask(&["score"]);
    assert_eq!(stdout(&output), "spec 1 of 2\njourney 4 of 19\n");
    let output = repo.xtask(&["score", "--json"]);
    assert_eq!(
        stdout(&output),
        "{\"spec\":{\"proven\":1,\"promises\":2},\"journey\":{\"passing\":4,\"steps\":19}}\n"
    );
}

#[test]
fn score_before_the_first_commit_prints_only_the_spec() {
    let repo = Repo::new("score-unborn", &[]);
    fs::remove_dir_all(repo.root.join(".git")).unwrap();
    repo.git(&["init", "-q", "-b", "main"]);
    assert_eq!(stdout(&repo.xtask(&["score"])), "spec 1 of 2\n");
}

#[test]
fn score_without_a_record_prints_only_the_spec() {
    let repo = Repo::new("score-none", &[]);
    assert_eq!(stdout(&repo.xtask(&["score"])), "spec 1 of 2\n");
}
