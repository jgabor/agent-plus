//! Runs `report` and `nightly` in real git repositories, with a fake `gh` on
//! `PATH` standing in for GitHub, so no test reaches it.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const BUDGETS: &str = "[[budget]]\nkey = \"boot_ms\"\nname = \"Boot\"\ntarget = 150.0\n\n[[budget]]\nkey = \"binary_bytes\"\nname = \"Size\"\n";
const JOURNEY: &str = "[[step]]\nid = 1\nname = \"first\"\nneeds = []\nscript = \"harness/journey/ok.sh\"\n\n[[step]]\nid = 2\nname = \"model\"\nneeds = [1]\nscript = \"harness/journey/ok.sh\"\nspend = true\n\n[[step]]\nid = 3\nname = \"after the model\"\nneeds = [2]\nscript = \"harness/journey/ok.sh\"\n\n[[step]]\nid = 4\nname = \"last\"\nneeds = [1]\nscript = \"harness/journey/bad.sh\"\n";
const NO_ISSUES: &str = "[]";
const REPORT_ISSUE: &str =
    "[{\"number\":12,\"title\":\"Nightly report\",\"labels\":[],\"isPinned\":true}]";
const WAITING: &str = "[{\"number\":30,\"title\":\"Promises that end in a colon\",\"labels\":[{\"name\":\"needs-owner\"}],\"isPinned\":false},{\"number\":31,\"title\":\"Something else\",\"labels\":[{\"name\":\"auto:pr\"}],\"isPinned\":false}]";
const CHECKS_PASSED: &str = "## Checks\n\n- pass bench\n- pass gate\n- skip t2\n- skip t3\n- pass journey\n- skip fuzz\n- skip mutation\n";

const GH: &str = r#"printf '%s\n' "$*" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "issue list") cat "$(dirname "$0")/issues.json" ;;
  "issue view") cat "$(dirname "$0")/issue.json" ;;
  "repo view") echo '{"owner":{"login":"o"}}' ;;
  "issue create") echo "https://github.com/o/r/issues/77" ;;
  "issue pin") ;;
  "issue comment")
    cat > "$(dirname "$0")/comment"
    if [ -e "$(dirname "$0")/no-comments" ]; then echo "comments are off" >&2; exit 1; fi
    echo "https://github.com/o/r/issues/$3#issuecomment-5" ;;
  "api -X") ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#;

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct Recorded<'a> {
    issue: usize,
    pr: usize,
    proven: usize,
    passing: usize,
    boot_ms: f64,
    reason: &'a str,
}

const CLEAN: Recorded = Recorded {
    issue: 5,
    pr: 9,
    proven: 1,
    passing: 1,
    boot_ms: 100.0,
    reason: "",
};

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Repo {
        Repo::with(name, "fn main() {}\n", &[])
    }

    /// A workspace whose one commit is too old for any report.
    fn with(name: &str, ap: &str, extra: &[(&str, &str)]) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("nightly")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.bin).unwrap();
        let gh = repo.bin.join("gh");
        fs::write(&gh, format!("#!/bin/sh\n{GH}")).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        repo.set_issues(NO_ISSUES);
        repo.set_report_comments(&[]);
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", "# A\n\nOne.\n"),
            (
                "crates/ap/Cargo.toml",
                "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
            ),
            ("crates/ap/src/main.rs", ap),
            ("harness/budgets.toml", BUDGETS),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/ok.sh", "#!/bin/sh\nexit 0\n"),
            ("harness/journey/bad.sh", "#!/bin/sh\nexit 1\n"),
        ];
        for (path, content) in files.iter().chain(extra) {
            repo.write(path, content);
        }
        let output = repo
            .command("cargo")
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        repo.git(&["init", "-q", "-b", "main"]);
        repo.commit("Start", 72);
        repo.sync();
        repo
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
        stdout(&output).trim().to_string()
    }

    fn short(&self, name: &str) -> String {
        self.git(&["rev-parse", "--short", name])
    }

    /// Makes `origin/main` the current commit.
    fn sync(&self) {
        self.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    /// Commits everything, dated this many hours ago.
    fn commit(&self, subject: &str, age_hours: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let date = format!("{} +0000", now - age_hours * 3600);
        self.git(&["add", "-A"]);
        let output = self
            .command("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                subject,
            ])
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_DATE", &date)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }

    /// Commits a ledger record, dated this many hours ago.
    fn merge(&self, subject: &str, age_hours: u64, record: &Recorded) {
        self.write(
            &format!("harness/ledger/{}.toml", record.issue),
            &format!(
                "issue = {}\npr = {}\ncommit = \"gated\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = {}, promises = 7 }}\njourney = {{ passing = {}, steps = 4 }}\nreason = \"{}\"\n\n[budgets]\nboot_ms = {}\n",
                record.issue,
                record.pr,
                record.proven,
                record.passing,
                record.reason,
                record.boot_ms,
            ),
        );
        self.commit(subject, age_hours);
    }

    fn set_issues(&self, json: &str) {
        fs::write(self.bin.join("issues.json"), json).unwrap();
    }

    /// Gives the report issue these comments, oldest first.
    fn set_report_comments(&self, comments: &[(&str, &str)]) {
        let comments: Vec<String> = comments
            .iter()
            .enumerate()
            .map(|(id, (author, body))| {
                format!(
                    "{{\"url\":\"https://github.com/o/r/issues/12#issuecomment-{id}\",\"author\":{{\"login\":\"{author}\"}},\"body\":{body:?}}}"
                )
            })
            .collect();
        fs::write(
            self.bin.join("issue.json"),
            format!(
                "{{\"number\":12,\"state\":\"OPEN\",\"body\":\"b\",\"labels\":[],\"comments\":[{}]}}",
                comments.join(",")
            ),
        )
        .unwrap();
    }

    /// Makes the report issue's newest comment a report on this commit.
    fn set_last_report(&self, commit: &str) {
        self.set_issues(REPORT_ISSUE);
        self.set_report_comments(&[
            ("o", "# Nightly report for `0000000`\n"),
            (
                "o",
                &format!("# Nightly report for `{commit}`\n\nspec 1 of 7\n"),
            ),
            ("o", "A remark from the owner."),
        ]);
    }

    fn calls(&self) -> Vec<String> {
        match fs::read_to_string(self.bin.join("calls")) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn comment(&self) -> String {
        fs::read_to_string(self.bin.join("comment")).unwrap()
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .unwrap()
    }

    #[track_caller]
    fn report(&self) -> String {
        let output = self.xtask(&["report"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    }
}

#[test]
fn the_report_covers_the_last_day_of_main() {
    let repo = Repo::new("day");
    repo.merge(
        "Too old (#2)",
        25,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge("Add a thing (#9)", 23, &CLEAN);
    let merged = repo.short("HEAD");
    repo.commit("Push straight to main", 1);
    let unrecorded = repo.short("HEAD");
    repo.set_issues(WAITING);

    assert_eq!(
        repo.report(),
        format!(
            "# Nightly report for `{unrecorded}`\n\nspec 1 of 7, journey 1 of 4, budgets 1 of 1\n\nJourney steps that spend money, and weren't run: 2\n\n## Merged\n\n- `{merged}` Add a thing (#9), issue #5\n\n## Regressed\n\nNothing.\n\n## Not in the ledger\n\n- `{unrecorded}` Push straight to main\n\n## Waiting on the owner\n\n- #30 Promises that end in a colon\n"
        )
    );
    assert_eq!(
        repo.calls(),
        ["issue list --state open --limit 2000 --json number,title,labels,isPinned"]
    );
}

#[test]
fn a_quiet_day_says_so() {
    let repo = Repo::new("quiet");
    repo.merge("Too old (#9)", 30, &CLEAN);
    let head = repo.short("HEAD");
    assert_eq!(
        repo.report(),
        format!(
            "# Nightly report for `{head}`\n\nspec 1 of 7, journey 1 of 4, budgets 1 of 1\n\nJourney steps that spend money, and weren't run: 2\n\n## Merged\n\nNothing.\n\n## Regressed\n\nNothing.\n\n## Not in the ledger\n\nNothing.\n\n## Waiting on the owner\n\nNothing.\n"
        )
    );
}

#[test]
fn a_repository_with_no_record_has_no_scores() {
    let repo = Repo::new("no-record");
    let report = repo.report();
    assert!(
        report.contains("`\n\nNo record in the ledger yet, so there are no scores.\n\n"),
        "{report}"
    );
    let output = repo.xtask(&["report", "--json"]);
    assert!(
        stdout(&output).contains("\"scores\":null"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_reason_given_is_listed_as_a_regression() {
    let repo = Repo::new("reason");
    repo.merge(
        "Start scoring (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge(
        "Slower for now (#9)",
        2,
        &Recorded {
            reason: "Worth it",
            ..CLEAN
        },
    );
    let head = repo.short("HEAD");
    let report = repo.report();
    assert!(
        report.contains(&format!(
            "## Regressed\n\n- `{head}` a budget went past its band: Worth it\n\n"
        )),
        "{report}"
    );
}

#[test]
fn a_lower_score_than_the_record_before_is_a_regression() {
    let repo = Repo::new("fell");
    let before = Recorded {
        issue: 1,
        pr: 2,
        proven: 3,
        passing: 2,
        ..CLEAN
    };
    repo.merge("Better (#2)", 30, &before);
    repo.merge(
        "Worse (#9)",
        2,
        &Recorded {
            boot_ms: 200.0,
            ..CLEAN
        },
    );
    let head = repo.short("HEAD");
    let report = repo.report();
    assert!(
        report.contains(&format!(
            "## Regressed\n\n- `{head}` spec fell from 3 to 1\n- `{head}` journey fell from 2 to 1\n- `{head}` budgets fell from 1 to 0\n\n"
        )),
        "{report}"
    );
}

#[test]
fn a_level_day_has_no_regression() {
    let repo = Repo::new("level");
    repo.merge(
        "First (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge("Same (#9)", 2, &CLEAN);
    let report = repo.report();
    assert!(report.contains("## Regressed\n\nNothing.\n"), "{report}");
}

#[test]
fn the_first_commit_can_carry_a_record() {
    let repo = Repo::new("first");
    repo.merge("Second (#9)", 2, &CLEAN);
    repo.git(&["checkout", "-q", "--orphan", "alone"]);
    repo.commit("Everything at once (#9)", 2);
    let report = repo.report();
    assert!(
        report.contains("Everything at once (#9), issue #5"),
        "{report}"
    );
    assert!(report.contains("## Regressed\n\nNothing.\n"), "{report}");
}

#[test]
fn the_report_as_json() {
    let repo = Repo::new("json");
    repo.merge(
        "First (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            proven: 2,
            ..CLEAN
        },
    );
    repo.merge("The \"new\" one (#9)", 2, &CLEAN);
    let merged = repo.short("HEAD");
    repo.commit("By hand", 1);
    let head = repo.short("HEAD");
    repo.set_issues(WAITING);
    let output = repo.xtask(&["report", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "{{\"commit\":\"{head}\",\"scores\":{{\"spec\":{{\"proven\":1,\"promises\":7}},\"journey\":{{\"passing\":1,\"steps\":4}},\"budgets\":{{\"met\":1,\"targets\":1}}}},\"left_out\":2,\"merged\":[{{\"commit\":\"{merged}\",\"subject\":\"The \\\"new\\\" one (#9)\",\"issue\":5,\"pr\":9}}],\"regressed\":[{{\"commit\":\"{merged}\",\"issue\":5,\"pr\":9,\"what\":\"spec fell from 2 to 1\"}}],\"unrecorded\":[{{\"commit\":\"{head}\",\"subject\":\"By hand\"}}],\"waiting\":[{{\"issue\":30,\"title\":\"Promises that end in a colon\"}}]}}\n"
        )
    );
}

#[test]
fn the_report_takes_only_json() {
    let repo = Repo::new("usage");
    let output = repo.xtask(&["report", "--html"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("`report` takes only `--json`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_report_that_cant_reach_github_is_an_error() {
    let repo = Repo::new("offline");
    fs::remove_file(repo.bin.join("issues.json")).unwrap();
    let output = repo.xtask(&["report"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert_eq!(stdout(&output), "");
}

#[test]
fn nightly_refuses_anything_but_main() {
    let repo = Repo::new("off-main");
    repo.commit("Ahead of main", 1);
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("origin/main"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.calls(), Vec::<String>::new());
}

#[test]
fn nightly_refuses_a_changed_tree() {
    let repo = Repo::new("dirty");
    repo.write("note.txt", "a change\n");
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("note.txt"), "{}", stderr(&output));
    assert_eq!(repo.calls(), Vec::<String>::new());
}

#[test]
fn nightly_takes_no_arguments() {
    let repo = Repo::new("args");
    let output = repo.xtask(&["nightly", "--t3"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(repo.calls(), Vec::<String>::new());
}

#[test]
fn nightly_files_and_pins_the_report_issue_the_first_time() {
    let repo = Repo::new("first-night");
    let head = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let calls = repo.calls();
    let posted: Vec<&str> = calls
        .iter()
        .map(String::as_str)
        .filter(|call| !call.contains("--json"))
        .collect();
    assert_eq!(posted.len(), 4, "{calls:?}");
    assert!(
        posted[0].starts_with("issue create --title Nightly report "),
        "{calls:?}"
    );
    assert_eq!(posted[1], "issue pin 77");
    assert_eq!(posted[2], "issue comment 77 --body-file -");
    assert_eq!(
        posted[3],
        format!(
            "api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/nightly"
        )
    );
    let comment = repo.comment();
    assert!(comment.starts_with("# Nightly report for `"), "{comment}");
    assert!(comment.contains(CHECKS_PASSED), "{comment}");
    assert!(
        stdout(&output).ends_with("nightly: pass\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn nightly_reuses_the_pinned_report_issue() {
    let repo = Repo::new("pinned");
    repo.set_issues(
        "[{\"number\":12,\"title\":\"Nightly report\",\"labels\":[],\"isPinned\":true}]",
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(
        calls.contains(&"issue comment 12 --body-file -".to_string()),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("issue create")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("issue pin")),
        "{calls:?}"
    );
}

#[test]
fn nightly_pins_a_report_issue_that_lost_its_pin() {
    let repo = Repo::new("unpinned");
    repo.set_issues(
        "[{\"number\":12,\"title\":\"Nightly report\",\"labels\":[],\"isPinned\":false}]",
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(calls.contains(&"issue pin 12".to_string()), "{calls:?}");
    assert!(
        calls.contains(&"issue comment 12 --body-file -".to_string()),
        "{calls:?}"
    );
}

#[test]
fn a_failing_gate_still_posts_the_report() {
    let repo = Repo::with(
        "gate-fails",
        "fn main() {}\n\n#[test]\nfn broken() {\n    panic!();\n}\n",
        &[],
    );
    let head = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let comment = repo.comment();
    assert!(comment.contains("- pass bench\n- fail gate\n"), "{comment}");
    let status = format!("statuses/{head} -f state=failure -f context=xtask/nightly");
    assert!(
        repo.calls().iter().any(|call| call.ends_with(&status)),
        "{:?}",
        repo.calls()
    );
    assert!(
        stdout(&output).ends_with("nightly: fail\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_step_that_cant_run_counts_as_failed() {
    let repo = Repo::with("cant-run", "fn main() { not rust }\n", &[]);
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let comment = repo.comment();
    assert!(comment.contains("- fail bench\n- fail gate\n"), "{comment}");
    assert!(comment.contains("- fail journey\n"), "{comment}");
}

#[test]
fn a_journey_below_the_ledger_fails() {
    let repo = Repo::new("journey-fell");
    repo.merge(
        "Claims two (#9)",
        30,
        &Recorded {
            proven: 0,
            passing: 2,
            ..CLEAN
        },
    );
    repo.sync();
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- pass gate\n"),
        "{}",
        repo.comment()
    );
    assert!(
        repo.comment().contains("- fail journey\n"),
        "{}",
        repo.comment()
    );
}

#[test]
fn a_journey_level_with_the_ledger_passes() {
    let repo = Repo::new("journey-level");
    repo.merge("Claims one (#9)", 30, &Recorded { proven: 0, ..CLEAN });
    repo.sync();
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- pass journey\n"),
        "{}",
        repo.comment()
    );
}

#[test]
fn a_t2_package_runs_and_can_fail() {
    let manifest = "[package]\nname = \"t2\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    let repo = Repo::with(
        "t2",
        "fn main() {}\n",
        &[
            ("crates/t2/Cargo.toml", manifest),
            (
                "crates/t2/src/lib.rs",
                "#[test]\nfn adapter() {\n    panic!();\n}\n",
            ),
        ],
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(repo.comment().contains("- fail t2\n"), "{}", repo.comment());
}

#[test]
fn a_passing_t2_package_is_reported() {
    let manifest = "[package]\nname = \"t2\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    let repo = Repo::with(
        "t2-passes",
        "fn main() {}\n",
        &[
            ("crates/t2/Cargo.toml", manifest),
            ("crates/t2/src/lib.rs", "#[test]\nfn adapter() {}\n"),
        ],
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(repo.comment().contains("- pass t2\n"), "{}", repo.comment());
}

#[test]
fn a_report_that_cant_be_posted_is_an_error() {
    let repo = Repo::new("no-comments");
    fs::write(repo.bin.join("no-comments"), "").unwrap();
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("comments are off"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_commit_that_removes_a_record_wrote_none() {
    let repo = Repo::new("removed");
    repo.merge("First (#9)", 30, &CLEAN);
    fs::remove_file(repo.root.join("harness/ledger/5.toml")).unwrap();
    repo.commit("Drop the record", 2);
    let head = repo.short("HEAD");
    let report = repo.report();
    assert!(
        report.contains(&format!(
            "## Not in the ledger\n\n- `{head}` Drop the record\n"
        )),
        "{report}"
    );
}

#[test]
fn a_night_that_never_ran_loses_nothing() {
    let repo = Repo::new("missed-night");
    repo.merge(
        "Reported (#2)",
        60,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    let reported = repo.short("HEAD");
    repo.merge("Landed the night it was off (#9)", 40, &CLEAN);
    repo.set_last_report(&reported);
    let report = repo.report();
    assert!(
        report.contains("Landed the night it was off (#9), issue #5"),
        "{report}"
    );
    assert!(!report.contains("Reported (#2)"), "{report}");
}

#[test]
fn a_commit_already_reported_isnt_repeated() {
    let repo = Repo::new("already");
    repo.merge(
        "Reported (#2)",
        3,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    let reported = repo.short("HEAD");
    repo.merge("New (#9)", 1, &CLEAN);
    repo.set_last_report(&reported);
    let report = repo.report();
    assert!(report.contains("New (#9), issue #5"), "{report}");
    assert!(!report.contains("Reported (#2)"), "{report}");
}

#[test]
fn a_last_report_that_cant_be_used_falls_back_to_a_day() {
    for (name, commit) in [
        ("gone", "deadbee"),
        ("option", "--output=x"),
        ("name", "HEAD"),
        ("empty", ""),
    ] {
        let repo = Repo::new(&format!("fallback-{name}"));
        repo.merge(
            "Too old (#2)",
            25,
            &Recorded {
                issue: 1,
                pr: 2,
                ..CLEAN
            },
        );
        repo.merge("Today (#9)", 1, &CLEAN);
        repo.set_last_report(commit);
        let report = repo.report();
        assert!(report.contains("Today (#9), issue #5"), "{name}: {report}");
        assert!(!report.contains("Too old (#2)"), "{name}: {report}");
    }
}

#[test]
fn a_last_report_off_this_history_falls_back_to_a_day() {
    let repo = Repo::new("off-history");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.commit("Elsewhere", 1);
    let elsewhere = repo.short("HEAD");
    repo.git(&["checkout", "-q", "main"]);
    repo.merge(
        "Too old (#2)",
        25,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge("Today (#9)", 1, &CLEAN);
    repo.set_last_report(&elsewhere);
    let report = repo.report();
    assert!(report.contains("Today (#9), issue #5"), "{report}");
    assert!(!report.contains("Too old (#2)"), "{report}");
}

#[test]
fn a_report_that_cant_be_built_still_fails_the_commit() {
    let repo = Repo::new("broken-record");
    repo.write("harness/ledger/5.toml", "issue = \"not a number\"\n");
    repo.commit("Bad record (#9)", 2);
    repo.merge(
        "Good record (#10)",
        1,
        &Recorded {
            issue: 6,
            pr: 10,
            proven: 0,
            ..CLEAN
        },
    );
    repo.sync();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.xtask(&["nightly"]);
    assert!(
        stdout(&output).contains("gate: pass\n"),
        "{}",
        stdout(&output)
    );
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("harness/ledger/5.toml"),
        "{}",
        stderr(&output)
    );
    let status = format!("statuses/{head} -f state=failure -f context=xtask/nightly");
    assert!(
        repo.calls().iter().any(|call| call.ends_with(&status)),
        "{:?}",
        repo.calls()
    );
    assert!(!repo.bin.join("comment").exists());
}

#[test]
fn a_report_heading_from_someone_else_is_ignored() {
    let repo = Repo::new("forged");
    repo.merge(
        "Reported (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    let reported = repo.short("HEAD");
    repo.merge("Landed since (#9)", 2, &CLEAN);
    let head = repo.short("HEAD");
    repo.set_issues(REPORT_ISSUE);
    repo.set_report_comments(&[
        ("o", &format!("# Nightly report for `{reported}`\n")),
        ("someone", &format!("# Nightly report for `{head}`\n")),
    ]);
    let report = repo.report();
    assert!(report.contains("Landed since (#9), issue #5"), "{report}");
    assert!(!report.contains("Reported (#2)"), "{report}");
}

#[test]
fn a_tighter_target_later_doesnt_blame_an_earlier_commit() {
    let repo = Repo::new("tighter");
    repo.merge(
        "Before (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge(
        "Slower, inside the target (#9)",
        3,
        &Recorded {
            boot_ms: 140.0,
            ..CLEAN
        },
    );
    repo.write("harness/budgets.toml", &BUDGETS.replace("150.0", "125.0"));
    repo.commit("Tighten the target", 1);
    let report = repo.report();
    assert!(report.contains("## Regressed\n\nNothing.\n"), "{report}");
}

#[test]
fn a_commit_with_no_budget_file_compares_no_budgets() {
    let repo = Repo::new("no-budget-file");
    fs::remove_file(repo.root.join("harness/budgets.toml")).unwrap();
    repo.merge(
        "Before (#2)",
        30,
        &Recorded {
            issue: 1,
            pr: 2,
            ..CLEAN
        },
    );
    repo.merge(
        "Without the file (#9)",
        3,
        &Recorded {
            boot_ms: 200.0,
            ..CLEAN
        },
    );
    repo.write("harness/budgets.toml", BUDGETS);
    repo.commit("Add the file", 1);
    let report = repo.report();
    assert!(
        report.contains("Without the file (#9), issue #5"),
        "{report}"
    );
    assert!(report.contains("## Regressed\n\nNothing.\n"), "{report}");
}

#[test]
fn a_missing_tool_stops_nightly_before_any_check() {
    let repo = Repo::new("no-clippy");
    let shim = repo.bin.join("cargo");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nif [ \"$1 $2\" = \"clippy --version\" ]; then echo \"no clippy here\" >&2; exit 1; fi\nexec \"{}\" \"$@\"\n",
            env::var("CARGO").expect("cargo sets CARGO for tests")
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("no clippy here"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.calls(), Vec::<String>::new());
    assert!(!repo.root.join("target/bench").exists());
}
