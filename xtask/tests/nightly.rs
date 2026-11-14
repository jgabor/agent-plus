//! Runs `report` and `nightly` in real git repositories, with a fake `gh` on
//! `PATH` standing in for GitHub, so no test reaches it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

const BUDGETS: &str = "[[budget]]\nkey = \"idle_mib\"\nname = \"Boot\"\ntarget = 150.0\n\n[[budget]]\nkey = \"binary_bytes\"\nname = \"Size\"\n";
const JOURNEY: &str = "[[milestone]]\nname = \"m\"\nsummary = \"s\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"first\"\nmilestone = \"m\"\nneeds = []\nscript = \"harness/journey/ok.sh\"\n\n[[step]]\nid = 2\nname = \"model\"\nmilestone = \"m\"\nneeds = [1]\nscript = \"harness/journey/ok.sh\"\nspend = true\n\n[[step]]\nid = 3\nname = \"after the model\"\nmilestone = \"m\"\nneeds = [2]\nscript = \"harness/journey/ok.sh\"\n\n[[step]]\nid = 4\nname = \"last\"\nmilestone = \"m\"\nneeds = [1]\nscript = \"harness/journey/bad.sh\"\n";
const NO_ISSUES: &str = "[]";
const REPORT_ISSUE: &str =
    "[{\"number\":12,\"title\":\"Nightly report\",\"labels\":[],\"isPinned\":true}]";
const WAITING: &str = "[{\"number\":30,\"title\":\"Promises that end in a colon\",\"labels\":[{\"name\":\"land-needed\"}],\"isPinned\":false},{\"number\":31,\"title\":\"Something else\",\"labels\":[{\"name\":\"auto:pr\"}],\"isPinned\":false}]";
const WAITING_FOR_EITHER: &str = "[{\"number\":30,\"title\":\"Ready\",\"labels\":[{\"name\":\"land-needed\"}],\"isPinned\":false},{\"number\":32,\"title\":\"Stuck\",\"labels\":[{\"name\":\"answer-needed\"}],\"isPinned\":false}]";
const CHECKS_PASSED: &str = "## Checks\n\n- pass bench\n- pass gate\n- skip t2\n- skip t3\n- pass journey\n- skip fuzz\n- skip mutation\n";

const GH: &str = r#"printf '%s\n' "$*" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "issue list")
    case "$*" in
      *"--state all"*) cat "$(dirname "$0")/all-issues.json" 2>/dev/null || echo '[]' ;;
      *) cat "$(dirname "$0")/issues.json" ;;
    esac ;;
  "pr list") echo '[]' ;;
  "issue edit")
    if [ -e "$(dirname "$0")/no-edits" ]; then echo "edits are off" >&2; exit 1; fi
    cat > "$(dirname "$0")/edited-$3" ;;
  "issue view") cat "$(dirname "$0")/issue.json" ;;
  "repo view") echo '{"owner":{"login":"o"}}' ;;
  "issue create") echo "https://github.com/o/r/issues/77" ;;
  "api repos/"*) echo '{"statuses":[]}' ;;
  "issue pin") ;;
  "issue comment")
    n="$(cat "$(dirname "$0")/comments" 2>/dev/null || echo 0)"
    echo $((n + 1)) > "$(dirname "$0")/comments"
    cat > "$(dirname "$0")/comment-$((n + 1))"
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
    idle_mib: f64,
    reason: &'a str,
}

const PAID_KEY: &str = ".config/agent-plus/paid-checks/anthropic-api-key";

/// A journey step that records the key it was given, and passes.
const KEY_SCRIPT: &str =
    "#!/bin/sh\nprintf '%s' \"${ANTHROPIC_API_KEY-none}\" > \"$HOME/../key\"\n";

const CLEAN: Recorded = Recorded {
    issue: 5,
    pr: 9,
    proven: 1,
    passing: 1,
    idle_mib: 100.0,
    reason: "",
};

/// Logs its arguments, and enforces the time limit as a real scope would.
/// A scope kills every process in the job. A process group is the simplest
/// stand-in that covers these fake jobs, though a process that left the group
/// would escape it. `timeout` can't be trusted with the group: on
/// `ubuntu-latest` it leaves the job's descendants running. The job starts its
/// own timer once it has a group, so the group always exists at the limit, and
/// one kill reaches every process at once. Two kills could be split by the
/// `wait` below waking between them.
const SYSTEMD_RUN: &str = r#"#!/bin/sh
d="$(dirname "$0")"
printf '%s\n' "$*" >> "$d/systemd-calls"
limit=
while [ "$1" != "--" ]; do
    case "$1" in RuntimeMaxSec=*) limit="${1#RuntimeMaxSec=}";; esac
    shift
done
shift
[ -n "$limit" ] || exec "$@"
exec 3<&0
setsid sh -c 'limit="$1"; shift
(sleep "$limit"; kill -KILL 0) </dev/null >/dev/null 2>&1 &
exec "$@"' sh "$limit" "$@" <&3 3<&- &
job=$!
exec 3<&-
wait "$job"
status=$?
kill -KILL "-$job" 2>/dev/null
exit "$status"
"#;

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
        common::write_tool(&gh, &format!("#!/bin/sh\n{GH}"));
        let systemd = repo.bin.join("systemd-run");
        common::write_tool(&systemd, SYSTEMD_RUN);
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
            .env("CARGO_HOME", self.root.parent().unwrap().join("cargo-home"))
            // The nightly reads the paid checks' key from `HOME`, so a test
            // must never see the owner's. The toolchain stays where it is.
            .env("HOME", self.home())
            .env("RUSTUP_HOME", common::rustup_home())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD")
            .env_remove("XTASK_FUZZ_SECONDS")
            .env_remove("XTASK_MUTATION_SECONDS");
        command
    }

    fn home(&self) -> PathBuf {
        self.root.parent().unwrap().join("home")
    }

    /// Puts a key where the nightly looks for the paid checks' key.
    fn set_paid_key(&self, key: &str) {
        let path = self.home().join(PAID_KEY);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, key).unwrap();
    }

    /// What a step running `harness/journey/key.sh` saw of the key.
    fn key_seen(&self, step: u32) -> String {
        fs::read_to_string(self.root.join(format!("target/journey/step-{step}/key"))).unwrap()
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
                "issue = {}\npr = {}\ncommit = \"gated\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = {}, promises = 7 }}\njourney = {{ passing = {}, steps = 4 }}\nreason = \"{}\"\n\n[budgets]\nidle_mib = {}\n",
                record.issue,
                record.pr,
                record.proven,
                record.passing,
                record.reason,
                record.idle_mib,
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

    /// Puts a fake `cargo-mutants` on the path. It logs its arguments and limits,
    /// and then runs `body`, with `$d` as this directory.
    fn set_mutants(&self, body: &str) {
        let path = self.bin.join("cargo-mutants");
        let script = format!(
            "#!/bin/sh\nd=\"$(dirname \"$0\")\"\nprintf '%s\\n' \"$*\" >> \"$d/mutants-calls\"\necho \"$TMPDIR\" > \"$d/mutants-tmp\"\nmkdir -p target/mutants/mutants.out\n{body}"
        );
        common::write_tool(&path, &script);
    }

    /// A fake `cargo-mutants` that reports these survivors.
    fn set_survivors(&self, missed: &str) {
        fs::write(self.bin.join("missed.txt"), missed).unwrap();
        self.set_mutants("cat \"$d/missed.txt\" > target/mutants/mutants.out/missed.txt\nexit 2\n");
    }

    fn read_bin(&self, name: &str) -> String {
        fs::read_to_string(self.bin.join(name))
            .unwrap()
            .trim()
            .to_string()
    }

    /// Commits a change to these crates, recent enough to be mutated.
    fn touch(&self, crates: &[&str]) {
        for name in crates {
            self.write(&format!("crates/{name}/note.txt"), "changed\n");
        }
        self.commit("Touch crates", 1);
        self.sync();
    }

    /// Makes this the one issue with the roadmap's title, open or closed.
    fn set_roadmap_issue(&self, state: &str, body: &str) {
        self.set_roadmap_issues(state, body, None);
    }

    /// The roadmap issue, and the open issue with its details if they have a
    /// body.
    fn set_roadmap_issues(&self, state: &str, body: &str, details: Option<&str>) {
        let issue = |number: u64, state: &str, title: &str, body: &str| {
            format!(
                "{{\"number\":{number},\"state\":\"{state}\",\"title\":\"{title}\",\"body\":{body:?},\"labels\":[],\"url\":\"https://github.com/o/r/issues/{number}\",\"parent\":null}}"
            )
        };
        let mut issues = vec![issue(158, state, "Roadmap", body)];
        if let Some(body) = details {
            issues.push(issue(159, "OPEN", "Roadmap details", body));
        }
        let json = format!("[{}]", issues.join(","));
        fs::write(self.bin.join("all-issues.json"), json).unwrap();
    }

    fn edits(&self) -> Vec<String> {
        let edits = self.calls().into_iter();
        edits
            .filter(|call| call.starts_with("issue edit"))
            .collect()
    }

    fn calls(&self) -> Vec<String> {
        match fs::read_to_string(self.bin.join("calls")) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn comment(&self) -> String {
        fs::read_to_string(self.bin.join("comment-1")).unwrap()
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
            idle_mib: 200.0,
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
fn the_report_lists_an_issue_waiting_for_a_landing_or_an_answer() {
    let repo = Repo::new("waiting-for-either");
    repo.set_issues(WAITING_FOR_EITHER);
    let output = repo.xtask(&["report"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(report.contains("#30 Ready"), "{report}");
    assert!(report.contains("#32 Stuck"), "{report}");
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

/// Each issue's body is what `cargo xtask roadmap` prints, from the same run
/// of the journey that the night's check used.
#[test]
fn nightly_writes_the_roadmap_and_its_details_to_their_issues() {
    let repo = Repo::new("roadmap-written");
    repo.set_roadmap_issues("OPEN", "typed by hand", Some("written last night"));
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The roadmap links to its details, so they are written first.
    assert_eq!(
        repo.edits(),
        [
            "issue edit 159 --body-file -",
            "issue edit 158 --body-file -"
        ]
    );
    let calls = repo.calls();
    assert!(
        !calls.iter().any(|call| call.contains("--title Roadmap")),
        "{calls:?}"
    );

    let written = fs::read_to_string(repo.bin.join("edited-158")).unwrap();
    assert!(
        written.starts_with("This page shows what works in Agent+ today"),
        "{written}"
    );
    assert!(written.contains("Milestones done: 0 of 1.\n"), "{written}");
    assert!(
        written.contains("| 1 | m | s | In progress |\n"),
        "{written}"
    );
    assert!(written.ends_with("see #159.\n"), "{written}");
    assert!(!written.contains("Step"), "{written}");
    assert_eq!(stdout(&repo.xtask(&["roadmap"])), written);

    let details = fs::read_to_string(repo.bin.join("edited-159")).unwrap();
    assert!(
        details
            .starts_with("The nightly job writes this issue from `cargo xtask roadmap --details`"),
        "{details}"
    );
    assert!(details.contains("\njourney 1 of 4\n"), "{details}");
    assert!(
        details.contains(
            "- [x] Step 1: first\n- [ ] Step 2: model (skipped)\n- [ ] Step 3: after the model (skipped)\n- [ ] Step 4: last (fails)\n"
        ),
        "{details}"
    );
    assert_eq!(stdout(&repo.xtask(&["roadmap", "--details"])), details);
}

/// The first night after the roadmap issue is opened, its details have no
/// issue yet.
#[test]
fn nightly_files_the_details_issue_when_there_is_none() {
    let repo = Repo::new("roadmap-details-filed");
    repo.set_roadmap_issue("OPEN", "typed by hand");
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    let filed: Vec<&String> = calls
        .iter()
        .filter(|call| call.starts_with("issue create --title Roadmap details --body "))
        .collect();
    assert_eq!(filed.len(), 1, "{calls:?}");
    assert!(
        filed[0].contains("cargo xtask roadmap --details"),
        "{calls:?}"
    );
    // The fake `gh` numbers every new issue 77.
    assert_eq!(repo.edits(), ["issue edit 158 --body-file -"]);
    let written = fs::read_to_string(repo.bin.join("edited-158")).unwrap();
    assert!(written.ends_with("see #77.\n"), "{written}");
}

/// Writing the same roadmap again would only add an edit to the issue.
#[test]
fn nightly_leaves_an_unchanged_roadmap_alone() {
    let repo = Repo::new("roadmap-unchanged");
    repo.set_roadmap_issues("OPEN", "", Some(""));
    let printed = stdout(&repo.xtask(&["roadmap"]));
    assert!(printed.ends_with("see #159.\n"), "{printed}");
    let details = stdout(&repo.xtask(&["roadmap", "--details"]));
    assert!(details.contains("journey 1 of 4"), "{details}");
    // GitHub's website saves line ends as `\r\n`, and may drop the last one.
    repo.set_roadmap_issues(
        "OPEN",
        printed.replace('\n', "\r\n").trim_end(),
        Some(details.replace('\n', "\r\n").trim_end()),
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(repo.edits(), Vec::<String>::new());
    let calls = repo.calls();
    assert!(
        !calls.iter().any(|call| call.contains("--title Roadmap")),
        "{calls:?}"
    );
}

/// Closing the roadmap issue is how the owner turns the roadmap off.
#[test]
fn nightly_writes_no_roadmap_without_an_open_issue_for_it() {
    let repo = Repo::new("roadmap-closed");
    repo.set_roadmap_issue("CLOSED", "typed by hand");
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    assert_eq!(repo.edits(), Vec::<String>::new());
    assert!(
        !calls.iter().any(|call| call.contains("--title Roadmap")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("pr list")),
        "{calls:?}"
    );
}

#[test]
fn a_roadmap_that_cant_be_written_is_an_error() {
    let repo = Repo::new("roadmap-refused");
    repo.set_roadmap_issue("OPEN", "typed by hand");
    fs::write(repo.bin.join("no-edits"), "").unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("edits are off"),
        "{}",
        stderr(&output)
    );
    // The night's result is still on the commit, and the report still went out.
    let status = format!("statuses/{head} -f state=success -f context=xtask/nightly");
    let calls = repo.calls();
    assert!(
        calls.iter().any(|call| call.ends_with(&status)),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("issue comment 77")),
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

/// The fixture's step 2 spends. With the key, it runs and counts like any
/// other step, and so does step 3, which needs it.
#[test]
fn nightly_runs_the_spend_steps_with_the_paid_checks_key() {
    let repo = Repo::with(
        "paid-key",
        "fn main() {}\n",
        &[("harness/journey/ok.sh", KEY_SCRIPT)],
    );
    repo.set_paid_key("sk-test\n");
    repo.set_roadmap_issues("OPEN", "typed by hand", Some("typed by hand"));
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("journey 3 of 4\n"),
        "{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains("sk-test"), "{}", stdout(&output));
    assert!(!stderr(&output).contains("sk-test"), "{}", stderr(&output));
    // Only a step that spends gets the key.
    assert_eq!(repo.key_seen(1), "none");
    assert_eq!(repo.key_seen(2), "sk-test");
    assert_eq!(repo.key_seen(3), "none");
    let comment = repo.comment();
    assert!(
        comment.contains("Journey steps that spend money, and weren't run: 0\n"),
        "{comment}"
    );
    let details = fs::read_to_string(repo.bin.join("edited-159")).unwrap();
    assert!(
        details
            .contains("- [x] Step 1: first\n- [x] Step 2: model\n- [x] Step 3: after the model\n"),
        "{details}"
    );
}

#[test]
fn nightly_without_the_paid_checks_key_runs_no_spend_step() {
    let repo = Repo::with(
        "no-paid-key",
        "fn main() {}\n",
        &[("harness/journey/ok.sh", KEY_SCRIPT)],
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("journey 1 of 4\n"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.root.join("target/journey/step-2/key").exists());
    assert!(
        repo.comment()
            .contains("Journey steps that spend money, and weren't run: 2\n"),
        "{}",
        repo.comment()
    );
}

/// The ledger counts no step that spends, so a passing one can't hide a step
/// that fell.
#[test]
fn a_passing_spend_step_does_not_hide_a_fall() {
    let repo = Repo::new("paid-hides-fall");
    repo.set_paid_key("sk-test");
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
        stdout(&output).contains("journey 3 of 4\n"),
        "{}",
        stdout(&output)
    );
    assert!(
        repo.comment().contains("- fail journey\n"),
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
    assert!(!repo.bin.join("comment-1").exists());
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
            idle_mib: 140.0,
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
            idle_mib: 200.0,
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
    common::write_tool(
        &shim,
        &format!(
            "#!/bin/sh\nif [ \"$1 $2\" = \"clippy --version\" ]; then echo \"no clippy here\" >&2; exit 1; fi\nexec \"{}\" \"$@\"\n",
            env::var("CARGO").expect("cargo sets CARGO for tests")
        ),
    );
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

const MISSED: &str = "crates/ap/src/main.rs:1:1: replace main with ()\n";
const MANIFEST: &str = "[package]\nname = \"NAME\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";

fn manifest(name: &str) -> String {
    MANIFEST.replace("NAME", name)
}

#[track_caller]
fn assert_mutation_ran(repo: &Repo, output: &Output, code: i32, state: &str) {
    assert_eq!(output.status.code(), Some(code), "{}", stderr(output));
    let comment = repo.comment();
    assert!(
        comment.contains(&format!("- {state} mutation\n")),
        "{comment}"
    );
}

#[test]
fn a_surviving_mutant_opens_an_issue() {
    let repo = Repo::new("survivor");
    repo.set_survivors(MISSED);
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let mutants = repo.read_bin("mutants-calls");
    let runner = repo.root.join("target/mutants/runner");
    assert_eq!(
        mutants,
        format!(
            "mutants --colors never --no-shuffle --cargo-arg=--config=target.'cfg(all())'.runner=[{:?}] -o target/mutants -p ap",
            runner.display().to_string()
        )
    );
    let calls = repo.calls().join("\n");
    assert!(
        calls.contains("issue create --title Mutants survive in ap --body "),
        "{calls}"
    );
    assert!(calls.contains(MISSED.trim()), "{calls}");
    assert!(calls.contains("## Definition of done"), "{calls}");
    assert!(!calls.contains("--label"), "{calls}");
}

#[test]
fn a_mutants_issue_folds_its_list() {
    let repo = Repo::new("folded-survivors");
    repo.set_survivors(&format!(
        "{MISSED}crates/ap/src/main.rs:2:1: replace main with ()\n"
    ));
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.calls().join("\n");
    // The blank line after the summary is what makes GitHub render the list.
    let fold = "no longer reports these as missed:\n\n\
        <details>\n<summary>2 missed mutants</summary>\n\n\
        - `crates/ap/src/main.rs:1:1: replace main with ()`\n\
        - `crates/ap/src/main.rs:2:1: replace main with ()`\n\n\
        </details>\n\n\
        ## Touches\n`crates/ap`";
    assert!(calls.contains(fold), "{calls}");
}

#[test]
fn one_missed_mutant_is_counted_as_one() {
    let repo = Repo::new("one-survivor");
    repo.set_survivors(MISSED);
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.calls().join("\n");
    assert!(
        calls.contains("<summary>1 missed mutant</summary>"),
        "{calls}"
    );
}

#[test]
fn mutants_run_as_one_contained_job() {
    let repo = Repo::new("memory");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.read_bin("systemd-calls");
    for part in [
        flags().as_str(),
        "TimeoutStopSec=30",
        "MemoryMax=8G",
        "MemorySwapMax=0",
        "OOMPolicy=continue",
        "RuntimeMaxSec=3600",
        "-- cargo mutants ",
    ] {
        assert!(calls.contains(part), "{part} in {calls}");
    }
}

/// The flags each job starts with. A job joins its caller's slice where the
/// caller has one, which this test's caller shares.
fn flags() -> String {
    let own = fs::read_to_string("/proc/self/cgroup").unwrap();
    let inherit = if own.contains("/user@") {
        " --slice-inherit"
    } else {
        ""
    };
    format!("--user --scope --quiet --collect --expand-environment=no{inherit} -p ")
}

/// Finds the runner in the fake's arguments, and runs `cargo test` with it. The
/// test runs away only if `RUNAWAY_HERE` is set.
const TEST_WITH_RUNNER: &str = r#"for a in "$@"; do case "$a" in --cargo-arg=*) arg="${a#--cargo-arg=}";; esac; done
if [ -n "$RUNAWAY_HERE" ]; then export RUNAWAY="$d"; fi
cargo test --quiet --package ap --test runaway ${arg:+"$arg"} >/dev/null 2>&1
echo $? > "$d/tested"
"#;

/// Starts a process that only sleeps, and then allocates 2 GiB, but only when
/// `RUNAWAY` names a directory to say so in.
const RUNAWAY_TEST: &str = r#"use std::path::Path;
use std::process::Command;

// The child is meant to outlive this test, unless the job takes it too.
#[allow(clippy::zombie_processes)]
#[test]
fn runaway() {
    let Some(d) = std::env::var_os("RUNAWAY") else {
        return;
    };
    Command::new("sh")
        .arg("-c")
        .arg(CHILD)
        .arg("sh")
        .arg(&d)
        .spawn()
        .unwrap();
    while !Path::new(&d).join("child").exists() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut blocks = Vec::new();
    for _ in 0..2048 {
        blocks.push(vec![1u8; 1 << 20]);
    }
    assert_eq!(blocks.len(), 2048);
}

const CHILD: &str = r"echo $$ $(cut -d')' -f2 /proc/$$/stat | cut -d' ' -f21) > $1/child.new
mv $1/child.new $1/child
exec sleep 3608";
"#;

/// `cargo mutants` gives `cargo` the runner for each test binary.
#[test]
fn each_test_of_mutants_is_a_job_of_its_own() {
    let repo = Repo::with(
        "test-job",
        "fn main() {}\n",
        &[("crates/ap/tests/runaway.rs", RUNAWAY_TEST)],
    );
    repo.set_mutants(TEST_WITH_RUNNER);
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert_eq!(repo.read_bin("tested"), "0");
    let calls = repo.read_bin("systemd-calls");
    let job = calls
        .lines()
        .find(|line| line.contains("/runaway-"))
        .unwrap_or_else(|| panic!("no job ran the test in {calls}"));
    for part in [
        flags().as_str(),
        "MemoryMax=8G",
        "MemorySwapMax=0",
        "OOMPolicy=kill",
        "BindsTo=",
    ] {
        assert!(job.contains(part), "{part} in {job}");
    }
}

/// Mutants crash on purpose, so a test binary's job may write a core of at
/// most 1 byte. When `core_pattern` is a pipe, the kernel skips the dump at
/// that limit, where at 0 it still records the crash.
#[test]
fn a_test_job_dumps_no_core() {
    let repo = Repo::new("test-core");
    repo.set_mutants(
        r#"for a in "$@"; do case "$a" in --cargo-arg=*) runner="${a#*runner=[\"}"; runner="${runner%\"]}";; esac; done
"$runner" sh -c 'grep "^Max core file size" /proc/self/limits > "$1/core"' sh "$d"
"#,
    );
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let core: Vec<String> = repo
        .read_bin("core")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    assert_eq!(core[4..], ["1", "1", "bytes"], "{core:?}");
}

/// When the kernel kills a test over the memory limit, a process it started
/// mustn't live on into the next mutant, and `cargo mutants` must go on. One
/// that did took the rest of a nightly run with it.
#[test]
fn a_mutant_over_the_memory_limit_takes_only_its_own_processes() {
    let repo = Repo::with(
        "test-oom",
        "fn main() {}\n",
        &[("crates/ap/tests/runaway.rs", RUNAWAY_TEST)],
    );
    fs::remove_file(repo.bin.join("systemd-run")).unwrap();
    repo.set_mutants(TEST_WITH_RUNNER);
    repo.touch(&["ap"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("nightly")
        .env("XTASK_MEMORY_MAX", "512M")
        .env("RUNAWAY_HERE", "1")
        .output()
        .unwrap();
    let child = ids(&repo, "child");
    let _reap = Reap(child[0], child[1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_alive(child[0], child[1]) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !is_alive(child[0], child[1]),
        "process {} outlived the test that started it",
        child[0]
    );
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert_eq!(repo.read_bin("tested"), "101", "the test failed");
}

#[test]
fn mutants_build_in_the_target_directory() {
    let repo = Repo::new("mutants-tmp");
    repo.set_mutants("test -d \"$TMPDIR\" || exit 9\nexit 0\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let tmp = repo.read_bin("mutants-tmp");
    assert!(tmp.ends_with("/target/mutants/tmp"), "{tmp}");
}

#[test]
fn no_survivors_open_no_issue() {
    let repo = Repo::new("no-survivors");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(
        !repo.calls().join("\n").contains("Mutants survive"),
        "{:?}",
        repo.calls()
    );
}

#[test]
fn only_changed_crates_of_the_product_are_mutated() {
    let (demo, fake, t1, other) = (
        manifest("demo"),
        manifest("fake-x"),
        manifest("t1"),
        manifest("other"),
    );
    let tasks = manifest("xtask");
    let repo = Repo::with(
        "changed-crates",
        "fn main() {}\n",
        &[
            ("crates/demo/Cargo.toml", &demo),
            ("crates/demo/src/lib.rs", "pub fn f() {}\n"),
            ("crates/fake-x/Cargo.toml", &fake),
            ("crates/fake-x/src/lib.rs", "pub fn f() {}\n"),
            ("crates/xtask/Cargo.toml", &tasks),
            ("crates/xtask/src/lib.rs", "pub fn f() {}\n"),
            ("crates/t1/Cargo.toml", &t1),
            ("crates/t1/src/lib.rs", "pub fn f() {}\n"),
            ("crates/other/Cargo.toml", &other),
            ("crates/other/src/lib.rs", "pub fn f() {}\n"),
        ],
    );
    repo.set_mutants("exit 0\n");
    repo.touch(&["demo", "fake-x", "t1", "xtask"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.read_bin("mutants-calls");
    assert!(calls.ends_with(" -o target/mutants -p demo"), "{calls}");
}

#[test]
fn each_crate_gets_its_own_issue() {
    let demo = manifest("demo");
    let repo = Repo::with(
        "two-crates",
        "fn main() {}\n",
        &[
            ("crates/demo/Cargo.toml", &demo),
            ("crates/demo/src/lib.rs", "pub fn f() {}\n"),
        ],
    );
    repo.set_survivors(&format!(
        "{MISSED}crates/demo/src/lib.rs:1:1: replace f with ()\n"
    ));
    repo.touch(&["ap", "demo"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.calls().join("\n");
    assert!(calls.contains("--title Mutants survive in ap "), "{calls}");
    assert!(
        calls.contains("--title Mutants survive in demo "),
        "{calls}"
    );
    assert!(repo.read_bin("mutants-calls").ends_with(" -p ap -p demo"));
}

#[test]
fn an_open_issue_for_a_crate_isnt_filed_again() {
    let repo = Repo::new("survivor-open");
    repo.set_survivors(MISSED);
    repo.touch(&["ap"]);
    repo.set_issues(
        "[{\"number\":40,\"title\":\"Mutants survive in ap\",\"labels\":[],\"isPinned\":false}]",
    );
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(
        !repo.calls().join("\n").contains("--title Mutants"),
        "{:?}",
        repo.calls()
    );
}

#[test]
fn a_long_list_of_survivors_is_cut_short() {
    let repo = Repo::new("many-survivors");
    let missed: String = (0..150)
        .map(|n| format!("crates/ap/src/main.rs:{n}:1: replace main with ()\n"))
        .collect();
    repo.set_survivors(&missed);
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let calls = repo.calls().join("\n");
    assert!(calls.contains("main.rs:99:1"), "{calls}");
    assert!(!calls.contains("main.rs:100:1"), "{calls}");
    assert!(calls.contains("and 50 more"), "{calls}");
    // The summary counts every survivor, and not only the ones listed.
    assert!(
        calls.contains("<summary>150 missed mutants</summary>"),
        "{calls}"
    );
}

#[test]
fn mutation_stops_at_its_limit_and_keeps_what_it_found() {
    let repo = Repo::new("mutation-limit");
    fs::write(repo.bin.join("missed.txt"), MISSED).unwrap();
    repo.set_mutants("cat \"$d/missed.txt\" > target/mutants/mutants.out/missed.txt\nsleep 30\n");
    repo.touch(&["ap"]);
    let started = Instant::now();
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("nightly")
        .env("XTASK_MUTATION_SECONDS", "1")
        .output()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(
        stdout(&output).contains("second limit"),
        "{}",
        stdout(&output)
    );
    assert!(
        repo.calls().join("\n").contains("Mutants survive in ap"),
        "{:?}",
        repo.calls()
    );
}

/// The ids of a process, as the fake recorded them: its pid, group, session,
/// and start time.
fn ids(repo: &Repo, name: &str) -> Vec<u64> {
    repo.read_bin(name)
        .split_whitespace()
        .map(|id| id.parse().unwrap())
        .collect()
}

/// The state and start time from `/proc/<pid>/stat`, which `exec` keeps.
fn stat(pid: u64) -> Option<(String, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
    Some((fields[0].to_string(), fields[19].parse().ok()?))
}

/// The start time tells the process from a later one given the same pid.
fn is_alive(pid: u64, start: u64) -> bool {
    stat(pid).is_some_and(|(state, started)| started == start && state != "Z")
}

/// Kills the escaped process if the test ends with it still alive.
struct Reap(u64, u64);

impl Drop for Reap {
    fn drop(&mut self) {
        if is_alive(self.0, self.1) {
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .status();
        }
    }
}

/// The fake `systemd-run` kills a process group, which a process can leave.
/// `cargo mutants` starts its workers in groups of their own, so only a real
/// scope contains them. This job starts one in a new session and group.
#[test]
fn a_real_scope_stops_a_process_that_left_the_job_group() {
    let repo = Repo::new("real-scope");
    fs::remove_file(repo.bin.join("systemd-run")).unwrap();
    repo.set_mutants(
        r#"setsid sh -c 'echo $$ $(ps -o pgid=,sid= -p $$) $(cut -d")" -f2 /proc/$$/stat | cut -d" " -f21) > "$1/escaped.new"
mv "$1/escaped.new" "$1/escaped"
exec sleep 3607' sh "$d" </dev/null >/dev/null 2>&1 &
while [ ! -e "$d/escaped" ]; do sleep 0.1; done
echo $$ $(ps -o pgid=,sid= -p $$) > "$d/job"
sleep 30
"#,
    );
    repo.touch(&["ap"]);
    let started = Instant::now();
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("nightly")
        .env("XTASK_MUTATION_SECONDS", "1")
        .output()
        .unwrap();
    let escaped = ids(&repo, "escaped");
    let _reap = Reap(escaped[0], escaped[3]);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_mutation_ran(&repo, &output, 0, "pass");
    let job = ids(&repo, "job");
    assert_ne!(escaped[1], job[1], "it left the job's group");
    assert_ne!(escaped[2], job[2], "it left the job's session");
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_alive(escaped[0], escaped[3]) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !is_alive(escaped[0], escaped[3]),
        "process {} outlived the job's limit",
        escaped[0]
    );
}

/// A slice of the user manager for one test, stopped when the test ends. A
/// slice outlives the scopes in it, and a dash in its name would nest it.
struct Slice(String);

impl Slice {
    fn new(test: &str) -> Self {
        Slice(format!("xtask_{test}_{}.slice", std::process::id()))
    }
}

impl Drop for Slice {
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .status();
    }
}

/// A test binary's job stays in the slice of the job that runs `cargo mutants`,
/// and stops when that job stops. This job starts one with the runner that
/// `cargo` would use.
#[test]
fn a_test_job_stays_in_the_slice_of_its_outer_job() {
    let repo = Repo::new("real-slice");
    fs::remove_file(repo.bin.join("systemd-run")).unwrap();
    repo.set_mutants(
        r#"for a in "$@"; do case "$a" in --cargo-arg=*) runner="${a#*runner=[\"}"; runner="${runner%\"]}";; esac; done
cut -d: -f3 /proc/self/cgroup > "$d/outer"
"$runner" sh -c 'cut -d: -f3 /proc/self/cgroup > "$1/inner.new"
echo $$ $(cut -d")" -f2 /proc/$$/stat | cut -d" " -f21) >> "$1/inner.new"
mv "$1/inner.new" "$1/inner"
exec sleep 3609' sh "$d" </dev/null >/dev/null 2>&1 &
while [ ! -e "$d/inner" ]; do sleep 0.1; done
sleep 30
"#,
    );
    repo.touch(&["ap"]);
    let slice = Slice::new("nightly");
    let output = repo
        .command("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={}", slice.0))
        .args(["--", env!("CARGO_BIN_EXE_xtask"), "nightly"])
        .env("XTASK_MUTATION_SECONDS", "1")
        .output()
        .unwrap();
    let inner = repo.read_bin("inner");
    let (cgroup, ids) = inner.split_once('\n').unwrap();
    let ids: Vec<u64> = ids
        .split_whitespace()
        .map(|id| id.parse().unwrap())
        .collect();
    let _reap = Reap(ids[0], ids[1]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let outer = repo.read_bin("outer");
    let parent = |path: &str| path.trim().rsplit_once('/').unwrap().0.to_string();
    assert!(
        parent(&outer).ends_with(&format!("/{}", slice.0)),
        "{outer}"
    );
    assert_eq!(parent(cgroup), parent(&outer), "{cgroup}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_alive(ids[0], ids[1]) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !is_alive(ids[0], ids[1]),
        "process {} outlived its outer job",
        ids[0]
    );
}

#[test]
fn mutation_that_cant_test_the_baseline_fails() {
    let repo = Repo::new("baseline");
    repo.set_mutants("exit 4\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 1, "fail");
    assert!(
        stderr(&output).contains("cargo mutants"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_earlier_result_isnt_read_again() {
    let repo = Repo::new("stale");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    repo.write("target/mutants/mutants.out/missed.txt", MISSED);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(
        !repo.calls().join("\n").contains("Mutants survive"),
        "{:?}",
        repo.calls()
    );
}

fn with_fuzz(name: &str, main: &str) -> Repo {
    let fuzz = manifest("fuzz");
    Repo::with(
        name,
        "fn main() {}\n",
        &[
            ("crates/fuzz/Cargo.toml", &fuzz),
            ("crates/fuzz/src/main.rs", main),
        ],
    )
}

#[test]
fn fuzzing_runs_for_an_hour_within_the_memory_limit() {
    let repo = with_fuzz(
        "fuzz-runs",
        "fn main() {\n    let seconds = std::env::args().nth(1).unwrap();\n    assert_eq!(seconds, \"3300\");\n}\n",
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- pass fuzz\n"),
        "{}",
        repo.comment()
    );
    let calls = repo.read_bin("systemd-calls");
    assert!(
        calls.lines().all(|line| line.contains("MemoryMax=8G")),
        "{calls}"
    );
    // The gate step runs each package's tests under a limit of its own.
    let lines: Vec<&str> = calls
        .lines()
        .filter(|line| !line.contains("-- cargo test "))
        .collect();
    assert_eq!(lines.len(), 2, "{calls}");
    assert!(lines[0].contains("RuntimeMaxSec=300 "), "{calls}");
    assert!(lines[0].contains("-- cargo build --release"), "{calls}");
    assert!(lines[1].contains("RuntimeMaxSec=3600 "), "{calls}");
    assert!(lines[1].ends_with("/release/fuzz 3300"), "{calls}");
}

#[test]
fn a_fuzz_failure_fails_the_step() {
    let repo = with_fuzz("fuzz-fails", "fn main() {\n    panic!(\"found one\");\n}\n");
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- fail fuzz\n"),
        "{}",
        repo.comment()
    );
}

#[test]
fn a_hung_fuzz_target_is_killed_and_fails() {
    let repo = with_fuzz(
        "fuzz-hangs",
        "fn main() {\n    std::thread::sleep(std::time::Duration::from_secs(60));\n}\n",
    );
    let started = Instant::now();
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("nightly")
        .env("XTASK_FUZZ_SECONDS", "1")
        .output()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- fail fuzz\n"),
        "{}",
        repo.comment()
    );
}

#[test]
fn code_a_report_covered_isnt_mutated_again() {
    let repo = Repo::new("mutation-reported");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    repo.set_last_report(&repo.short("HEAD"));
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "skip");
    assert!(!repo.bin.join("mutants-calls").exists());
}

#[test]
fn a_change_older_than_a_day_is_mutated_if_no_report_covered_it() {
    let repo = Repo::new("mutation-unreported");
    repo.set_mutants("exit 0\n");
    let start = repo.short("HEAD");
    repo.write("crates/ap/note.txt", "changed\n");
    repo.commit("Touch ap", 48);
    repo.sync();
    repo.set_last_report(&start);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(repo.bin.join("mutants-calls").exists());
}

#[test]
fn mutants_run_under_the_activity_lock() {
    let repo = Repo::new("mutation-lock");
    repo.set_mutants(
        "flock -n -x .git/xtask/activity.lock true && echo free > \"$d/lock\" || echo held > \"$d/lock\"\nexit 0\n",
    );
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert_eq!(repo.read_bin("lock"), "held");
}

#[test]
fn the_fuzzer_is_built_to_panic_on_overflow() {
    let repo = with_fuzz(
        "fuzz-checks",
        "fn main() {\n    let args = std::env::args().count();\n    assert!(std::panic::catch_unwind(|| debug_assert_eq!(args, 0)).is_err());\n    assert!(std::panic::catch_unwind(|| u8::MAX + args as u8).is_err());\n}\n",
    );
    let output = repo.xtask(&["nightly"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        repo.comment().contains("- pass fuzz\n"),
        "{}",
        repo.comment()
    );
}

#[test]
fn a_mutation_run_killed_before_its_limit_fails() {
    let repo = Repo::new("mutation-killed");
    repo.set_mutants("kill -KILL $$\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 1, "fail");
}

#[test]
fn a_night_mutation_failed_is_tried_again() {
    let repo = Repo::new("mutation-retried");
    repo.set_mutants("exit 0\n");
    let start = repo.short("HEAD");
    repo.write("crates/ap/note.txt", "changed\n");
    repo.commit("Touch ap", 48);
    repo.sync();
    let failed = repo.short("HEAD");
    repo.set_issues(REPORT_ISSUE);
    repo.set_report_comments(&[
        (
            "o",
            &format!("# Nightly report for `{start}`\n\n- pass mutation\n"),
        ),
        (
            "o",
            &format!("# Nightly report for `{failed}`\n\n- fail mutation\n"),
        ),
    ]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    assert!(repo.bin.join("mutants-calls").exists());
}

#[test]
fn the_job_itself_keeps_the_activity_lock() {
    let repo = Repo::new("mutation-lock-passed");
    repo.set_mutants("readlink /proc/$$/fd/0 > \"$d/stdin\"\nexit 0\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "pass");
    let stdin = repo.read_bin("stdin");
    assert!(stdin.ends_with("/.git/xtask/activity.lock"), "{stdin}");
}

#[test]
fn a_waiting_issue_that_mentions_failure_isnt_a_failed_night() {
    let repo = Repo::new("mutation-title");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    let head = repo.short("HEAD");
    repo.set_issues(REPORT_ISSUE);
    repo.set_report_comments(&[(
        "o",
        &format!("# Nightly report for `{head}`\n\n- #30 Investigate - fail mutation\n"),
    )]);
    let output = repo.xtask(&["nightly"]);
    assert_mutation_ran(&repo, &output, 0, "skip");
}

#[test]
fn a_hold_inherited_from_a_parent_still_runs_the_job() {
    let repo = Repo::new("mutation-inherited");
    repo.set_mutants("exit 0\n");
    repo.touch(&["ap"]);
    let output = repo.xtask(&["run", env!("CARGO_BIN_EXE_xtask"), "nightly"]);
    assert_mutation_ran(&repo, &output, 1, "pass");
    let calls = repo.read_bin("systemd-calls");
    let run = calls.lines().next().unwrap();
    assert!(run.contains("MemoryMax=8G"), "{run}");
    assert!(!run.contains("RuntimeMaxSec"), "{run}");
}

#[test]
fn a_record_merged_after_main_moved_is_listed_as_not_in_the_ledger() {
    let repo = Repo::new("stale-record");
    repo.merge("First (#9)", 3, &CLEAN);
    let text = fs::read_to_string(repo.root.join("harness/ledger/5.toml"))
        .unwrap()
        .replace("proven = 1", "proven = 6")
        .replace("date =", &format!("base = \"{}\"\ndate =", "0".repeat(40)));
    repo.write("harness/ledger/6.toml", &text);
    repo.commit("Late (#10)", 2);
    let head = repo.short("HEAD");
    let report = repo.report();
    assert!(report.starts_with(&format!("# Nightly report for `{head}`\n\nspec 1 of 7")));
    assert!(
        report.contains(&format!(
            "## Not in the ledger\n\n- `{head}` Late (#10), merged after `main` moved\n"
        )),
        "{report}"
    );
    assert!(!report.contains("issue #5\n- `"), "{report}");
}
