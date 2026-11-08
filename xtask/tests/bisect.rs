//! Runs `nightly` on a broken `main`, with a real bare origin and a fake `gh`
//! on `PATH`, so that no test reaches GitHub.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAST: &str = "fn main() {}\n";
const BROKEN: &str = "fn main() {}\n\n#[test]\nfn broken() {\n    panic!();\n}\n";
const JOURNEY: &str =
    "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const BUDGETS: &str = "[[budget]]\nkey = \"binary_bytes\"\nname = \"Size\"\n";
const RECORD: &str = "issue = 5\npr = 9\ncommit = \"c\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = { proven = 0, promises = 1 }\njourney = { passing = 1, steps = 1 }\nreason = \"\"\n\n[budgets]\n";
const NO_ISSUES: &str = "[]";
const REPORT_ISSUE: &str =
    "{\"number\":12,\"title\":\"Nightly report\",\"labels\":[],\"isPinned\":true}";

const GH: &str = r#"d="$(dirname "$0")"
printf '%s\n' "$*" >> "$d/calls"
case "$1 $2" in
  "issue list") cat "$d/issues.json" ;;
  "issue view") cat "$d/issue.json" ;;
  "repo view") echo '{"owner":{"login":"o"}}' ;;
  "issue create")
    n="$(cat "$d/next")"
    echo $((n + 1)) > "$d/next"
    echo "https://github.com/o/r/issues/$n" ;;
  "issue pin") ;;
  "issue comment")
    cat > "$d/comment-$3"
    echo "https://github.com/o/r/issues/$3#issuecomment-5" ;;
  "pr create") echo "https://github.com/o/r/pull/90" ;;
  "api -X") ;;
  "api repos/"*)
    sha="$(echo "$2" | cut -d/ -f5)"
    if grep -q "^$sha\$" "$d/passing"; then
      echo '{"statuses":[{"context":"xtask/gate","state":"failure"},{"context":"xtask/nightly","state":"success"}]}'
    else
      echo '{"statuses":[{"context":"xtask/gate","state":"success"},{"context":"xtask/nightly","state":"failure"}]}'
    fi ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#;

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct Repo {
    root: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
    temp: PathBuf,
}

impl Repo {
    fn new(name: &str, ap: &str) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("bisect")
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
        let gh = repo.bin.join("gh");
        fs::write(&gh, format!("#!/bin/sh\n{GH}")).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(repo.bin.join("next"), "77\n").unwrap();
        fs::write(repo.bin.join("passing"), "").unwrap();
        repo.set_issues(NO_ISSUES);
        repo.set_issue_comment(None);
        let manifest = "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        for (path, content) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", "# A\n\nOne.\n"),
            ("crates/ap/Cargo.toml", manifest),
            ("crates/ap/src/main.rs", ap),
            ("harness/budgets.toml", BUDGETS),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/one.sh", "#!/bin/sh\nexit 0\n"),
        ] {
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
        repo.commit("Start");
        repo.push();
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
            .env("TMPDIR", &self.temp)
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
        stdout(&output).trim().to_string()
    }

    fn commit(&self, subject: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", subject]);
    }

    fn push(&self) {
        self.git(&["push", "-q", "origin", "main"]);
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Gives this commit a passing `xtask/nightly`.
    fn mark_passing(&self, commit: &str) {
        let mut text = fs::read_to_string(self.bin.join("passing")).unwrap();
        text.push_str(commit);
        text.push('\n');
        fs::write(self.bin.join("passing"), text).unwrap();
    }

    fn set_issues(&self, json: &str) {
        fs::write(self.bin.join("issues.json"), json).unwrap();
    }

    /// Gives the issue that `gh issue view` shows this one comment, by the owner.
    fn set_issue_comment(&self, body: Option<&str>) {
        self.set_issue_comment_by("o", body);
    }

    fn set_issue_comment_by(&self, author: &str, body: Option<&str>) {
        let comments = body.map_or(String::new(), |body| {
            format!(
                "{{\"url\":\"https://github.com/o/r/issues/50#issuecomment-5\",\"author\":{{\"login\":\"{author}\"}},\"body\":{body:?}}}"
            )
        });
        fs::write(
            self.bin.join("issue.json"),
            format!("{{\"number\":50,\"state\":\"OPEN\",\"body\":\"\",\"labels\":[],\"comments\":[{comments}]}}"),
        )
        .unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }

    fn comment(&self, issue: u64) -> String {
        fs::read_to_string(self.bin.join(format!("comment-{issue}"))).unwrap()
    }

    fn origin_git(&self, args: &[&str]) -> String {
        let output = self
            .command("git")
            .current_dir(&self.origin)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output).trim().to_string()
    }

    fn leftovers(&self) -> Vec<String> {
        fs::read_dir(&self.temp)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("xtask-merge"))
            .collect()
    }

    fn nightly(&self) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .arg("nightly")
            .output()
            .unwrap()
    }

    /// Commits `count` changes after a passing start, with the break in the
    /// one at `position`, counting from 0, and its ledger record beside it.
    /// Returns that commit.
    fn plant(&self, count: usize, position: usize) -> String {
        self.mark_passing(&self.head());
        let mut bad = String::new();
        for index in 0..count {
            if index == position {
                self.write("crates/ap/src/main.rs", BROKEN);
                self.write("harness/ledger/5.toml", RECORD);
                self.commit("Plant the break");
                bad = self.head();
            } else {
                self.write(&format!("notes/{index}.txt"), "more\n");
                self.commit(&format!("Later work {index}"));
            }
        }
        self.push();
        bad
    }
}

#[test]
fn a_planted_break_is_bisected_and_reverted() {
    let repo = Repo::new("planted", FAST);
    let bad = repo.plant(4, 1);
    let short = repo.git(&["rev-parse", "--short", &bad]);
    let head = repo.head();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).ends_with("nightly: fail\n"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("nightly: filed #78\n"),
        "{}",
        stdout(&output)
    );

    let calls = repo.calls();
    assert!(
        calls.contains("issue create --title `main` fails the gate "),
        "{calls}"
    );
    let branch = format!("78-revert-{short}");
    let create =
        format!("pr create --base main --head {branch} --title Revert \"Plant the break\" ");
    assert!(calls.contains(&create), "{calls}");
    assert!(calls.contains("Closes #78."), "{calls}");
    assert!(calls.contains("Hill: none, "), "{calls}");

    let shown = format!("{branch}:crates/ap/src/main.rs");
    assert_eq!(format!("{}\n", repo.origin_git(&["show", &shown])), FAST);
    repo.origin_git(&["cat-file", "-e", &format!("{branch}:harness/ledger/5.toml")]);
    repo.origin_git(&["cat-file", "-e", &format!("{branch}:notes/2.txt")]);
    assert_eq!(
        repo.origin_git(&["rev-parse", &format!("{branch}~1")]),
        head
    );
    let message = repo.origin_git(&["log", "-1", "--format=%B", &branch]);
    assert!(
        message.contains(&format!("This reverts commit {bad}.")),
        "{message}"
    );

    let comment = repo.comment(78);
    assert!(comment.contains(&format!("`{bad}`")), "{comment}");
    assert!(comment.contains("#90"), "{comment}");
    assert_eq!(repo.leftovers(), Vec::<String>::new());
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
}

#[test]
fn the_break_is_found_wherever_it_is() {
    for (name, count, position) in [
        ("first", 3, 0),
        ("last", 3, 2),
        ("only", 1, 0),
        ("long", 6, 4),
    ] {
        let repo = Repo::new(&format!("where-{name}"), FAST);
        let bad = repo.plant(count, position);
        let output = repo.nightly();
        assert_eq!(output.status.code(), Some(1), "{name}: {}", stderr(&output));
        let comment = repo.comment(78);
        assert!(comment.contains(&format!("`{bad}`")), "{name}: {comment}");
    }
}

#[test]
fn the_bisect_starts_at_the_newest_passing_nightly() {
    let repo = Repo::new("newest", FAST);
    let start = repo.head();
    repo.write("notes/a.txt", "a\n");
    repo.commit("Passes too");
    let newest = repo.head();
    repo.mark_passing(&start);
    repo.mark_passing(&newest);
    repo.write("crates/ap/src/main.rs", BROKEN);
    repo.commit("Plant the break");
    let bad = repo.head();
    repo.write("notes/b.txt", "b\n");
    repo.commit("Later");
    repo.push();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(78).contains(&format!("`{bad}`")),
        "{}",
        repo.comment(78)
    );
    let calls = repo.calls();
    assert!(
        calls.contains(&format!("commits/{newest}/status")),
        "{calls}"
    );
    assert!(
        !calls.contains(&format!("commits/{start}/status")),
        "{calls}"
    );
}

#[test]
fn a_passing_night_files_nothing() {
    let repo = Repo::new("passing", FAST);
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(!calls.contains("fails the gate"), "{calls}");
    assert!(!calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("/status\n"), "{calls}");
}

#[test]
fn an_open_issue_stops_a_second_response() {
    let repo = Repo::new("open-issue", FAST);
    repo.plant(2, 0);
    repo.set_issues(&format!(
        "[{REPORT_ISSUE},{{\"number\":50,\"title\":\"`main` fails the gate\",\"labels\":[],\"isPinned\":false}}]"
    ));
    repo.set_issue_comment(Some("<!-- nightly response -->\nBisected."));
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("#50 already covers"),
        "{}",
        stdout(&output)
    );
    let calls = repo.calls();
    assert!(!calls.contains("issue create"), "{calls}");
    assert!(!calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("/status\n"), "{calls}");
}

#[test]
fn no_passing_nightly_is_said_and_reverts_nothing() {
    let repo = Repo::new("no-passing", BROKEN);
    repo.write("notes/a.txt", "a\n");
    repo.commit("Later");
    repo.push();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(78).contains("no passing"),
        "{}",
        repo.comment(78)
    );
    assert!(!repo.calls().contains("pr create"), "{}", repo.calls());
}

#[test]
fn the_search_for_a_passing_nightly_is_bounded() {
    let repo = Repo::new("bounded", BROKEN);
    repo.mark_passing(&repo.head());
    for index in 0..201 {
        repo.commit(&format!("Empty {index}"));
    }
    repo.push();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(78).contains("no passing"),
        "{}",
        repo.comment(78)
    );
    let calls = repo.calls();
    assert_eq!(calls.matches("/status\n").count(), 200, "{calls}");
}

#[test]
fn a_revert_that_conflicts_is_said_and_opens_nothing() {
    let repo = Repo::new("conflict", FAST);
    repo.mark_passing(&repo.head());
    repo.write("crates/ap/src/main.rs", BROKEN);
    repo.commit("Plant the break");
    repo.write(
        "crates/ap/src/main.rs",
        &BROKEN.replace("broken", "still_broken"),
    );
    repo.commit("Change the same lines");
    repo.push();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(78).contains("Couldn't"),
        "{}",
        repo.comment(78)
    );
    assert!(!repo.calls().contains("pr create"), "{}", repo.calls());
    assert_eq!(repo.leftovers(), Vec::<String>::new());
}

#[test]
fn a_commit_that_passed_on_an_earlier_night_is_still_bisected() {
    let repo = Repo::new("passed-before", FAST);
    let bad = repo.plant(2, 0);
    repo.mark_passing(&repo.head());
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(78).contains(&format!("`{bad}`")),
        "{}",
        repo.comment(78)
    );
}

#[test]
fn an_issue_with_no_response_is_picked_up() {
    let repo = Repo::new("resume", FAST);
    let bad = repo.plant(2, 0);
    let short = repo.git(&["rev-parse", "--short", &bad]);
    repo.set_issues(&format!(
        "[{REPORT_ISSUE},{{\"number\":50,\"title\":\"`main` fails the gate\",\"labels\":[],\"isPinned\":false}}]"
    ));
    repo.set_issue_comment(Some("A remark that isn't a response."));
    repo.git(&["checkout", "-q", "--orphan", "left-behind"]);
    repo.commit("What a killed run pushed");
    repo.git(&[
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/50-revert-{short}"),
    ]);
    repo.git(&["checkout", "-q", "main"]);
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(!calls.contains("issue create"), "{calls}");
    assert!(
        calls.contains(&format!("--head 50-revert-{short} ")),
        "{calls}"
    );
    let comment = repo.comment(50);
    assert!(
        comment.starts_with("<!-- nightly response -->\n"),
        "{comment}"
    );
    assert!(comment.contains(&format!("`{bad}`")), "{comment}");
}

#[test]
fn a_revert_of_a_protected_path_carries_the_label() {
    let repo = Repo::new("protected", FAST);
    repo.mark_passing(&repo.head());
    repo.write("crates/ap/src/main.rs", BROKEN);
    repo.write("harness/journey.toml", &format!("# A note.\n{JOURNEY}"));
    repo.commit("Plant the break");
    repo.push();
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.calls().contains(" --label spec-change"),
        "{}",
        repo.calls()
    );
}

#[test]
fn an_ordinary_revert_carries_no_label() {
    let repo = Repo::new("no-label", FAST);
    repo.plant(1, 0);
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("--label"), "{calls}");
}

#[test]
fn a_response_from_someone_else_doesnt_count() {
    let repo = Repo::new("forged-response", FAST);
    let bad = repo.plant(1, 0);
    repo.set_issues(&format!(
        "[{REPORT_ISSUE},{{\"number\":50,\"title\":\"`main` fails the gate\",\"labels\":[],\"isPinned\":false}}]"
    ));
    repo.set_issue_comment_by("someone", Some("<!-- nightly response -->\nHandled."));
    let output = repo.nightly();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        repo.comment(50).contains(&format!("`{bad}`")),
        "{}",
        repo.comment(50)
    );
}
