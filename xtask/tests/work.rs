//! Runs `next` and `claim` against a fake `gh` on `PATH`, so no test reaches
//! GitHub. Each fixture is what `gh issue list` would print.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const LIVE: &str = "2099-01-01T00:00Z";
const EXPIRED: &str = "2001-01-01T00:00Z";
const BEFORE_EXPIRY: &str = "2000-06-01T00:00:00Z";
const AFTER_EXPIRY: &str = "2002-06-01T00:00:00Z";
const OWNER: &str = "jgabor";

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

fn body(touches: &str, extra: &str) -> String {
    format!("## Goal\nx\n\n## Done when\ny\n\n## Touches\n{touches}\n\n{extra}")
}

fn status(id: u64, author: &str, state: &str, branch: &str, lease: &str) -> String {
    let text = format!(
        "<!-- status -->\n**State:** {state}\n**Thread:** t\n**Branch:** `{branch}`\n**Lease until:** {lease}\n**Done:** d\n**Next:** n\n**Decisions:** none\n"
    );
    format!(
        "{{\"id\":\"IC_x\",\"author\":{{\"login\":{}}},\"body\":{},\"url\":\"https://github.com/o/r/issues/1#issuecomment-{id}\"}}",
        quoted(author),
        quoted(&text)
    )
}

fn issue(number: u64, state: &str, labels: &[&str], text: &str, comments: &[String]) -> String {
    let labels: Vec<String> = labels
        .iter()
        .map(|label| format!("{{\"id\":\"L\",\"name\":{}}}", quoted(label)))
        .collect();
    format!(
        "{{\"number\":{number},\"state\":{},\"body\":{},\"labels\":[{}],\"comments\":[{}]}}",
        quoted(state),
        quoted(text),
        labels.join(","),
        comments.join(",")
    )
}

fn open(number: u64, labels: &[&str], touches: &str) -> String {
    issue(number, "OPEN", labels, &body(touches, ""), &[])
}

fn claimed(number: u64, touches: &str) -> String {
    issue(
        number,
        "OPEN",
        &["auto:merge", "in-progress"],
        &body(touches, ""),
        &[status(number * 10, OWNER, "working", "b", LIVE)],
    )
}

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    fn new(name: &str, issues: &[String]) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("work")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.root).unwrap();
        fs::create_dir_all(&repo.bin).unwrap();
        fs::write(
            repo.root.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\n",
        )
        .unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.set_issues(issues);
        repo.write_gh();
        repo
    }

    fn set_issues(&self, issues: &[String]) {
        fs::write(
            self.bin.join("issues.json"),
            format!("[{}]\n", issues.join(",")),
        )
        .unwrap();
    }

    /// `branch-<name>` holds what the branch API prints. With no such file, the
    /// branch doesn't exist. Every call is written to `calls`.
    fn write_gh(&self) {
        let bin = self.bin.display();
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> '{bin}/calls'
case "$1 $2" in
  "repo view") echo '{{"owner":{{"login":"{OWNER}"}}}}' ;;
  "issue list") cat '{bin}/issues.json' ;;
  "issue view") cat '{bin}/view.json' ;;
  "issue edit") ;;
  "issue comment")
    if [ -f '{bin}/comment-refused' ]; then echo 'gh: comment refused' >&2; exit 1; fi
    echo 'https://github.com/o/r/issues/1#issuecomment-500' ;;
  "api -X") ;;
  api*)
    name="${{2##*/}}"
    if [ -f "{bin}/branch-$name" ]; then cat "{bin}/branch-$name"; else
      echo 'gh: Branch not found (HTTP 404)' >&2; exit 1
    fi ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#
        );
        let path = self.bin.join("gh");
        common::write_tool(&path, &script);
    }

    fn set_branch(&self, name: &str, date: &str) {
        fs::write(
            self.bin.join(format!("branch-{name}")),
            format!("{{\"commit\":{{\"commit\":{{\"committer\":{{\"date\":\"{date}\"}}}}}}}}\n"),
        )
        .unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }

    fn git(&self, args: &[&str]) {
        let output = self.command("git").args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
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

    fn xtask(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .unwrap()
    }

    fn next(&self) -> String {
        let output = self.xtask(&["next"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    }

    fn numbers(&self) -> Vec<u64> {
        let text = self.next();
        text.split("\"issue\":")
            .skip(1)
            .map(|rest| {
                let end = rest.find(',').unwrap();
                rest[..end].parse().unwrap()
            })
            .collect()
    }
}

const T: &str = "`crates/a`";

fn listed(name: &str, issues: &[String]) -> Vec<u64> {
    Repo::new(name, issues).numbers()
}

#[test]
fn an_available_issue_is_printed_in_full() {
    let repo = Repo::new(
        "full",
        &[open(7, &["auto:merge"], "`crates/a` and `xtask/`")],
    );
    assert_eq!(
        repo.next(),
        "[{\"issue\":7,\"touches\":[\"crates/a\",\"xtask/\"],\"autonomy\":\"auto:merge\",\"branch\":null,\"worktree\":null}]\n"
    );
}

#[test]
fn nothing_claimable_prints_an_empty_array() {
    assert_eq!(Repo::new("empty", &[]).next(), "[]\n");
}

#[test]
fn the_autonomy_is_the_label() {
    let repo = Repo::new("pr", &[open(7, &["auto:pr"], T)]);
    assert!(repo.next().contains("\"autonomy\":\"auto:pr\""));
}

#[test]
fn an_issue_with_no_autonomy_label_is_unavailable() {
    assert_eq!(listed("no-auto", &[open(7, &[], T)]), Vec::<u64>::new());
}

#[test]
fn a_blocked_issue_waits_for_an_open_blocker() {
    let blocked = |by: &str| {
        issue(
            8,
            "OPEN",
            &["auto:merge"],
            &body(T, &format!("Blocked by {by}.\n")),
            &[],
        )
    };
    let blocker = |state: &str| issue(3, state, &[], &body("", ""), &[]);
    assert_eq!(
        listed("blocked-open", &[blocker("OPEN"), blocked("#3")]),
        Vec::<u64>::new()
    );
    assert_eq!(
        listed("blocked-closed", &[blocker("CLOSED"), blocked("#3")]),
        vec![8]
    );
    assert_eq!(listed("blocked-unknown", &[blocked("#99")]), vec![8]);
    assert_eq!(
        listed("blocked-second", &[blocker("OPEN"), blocked("#99, #3")]),
        Vec::<u64>::new()
    );
}

#[test]
fn a_parked_issue_waits_for_the_owner() {
    for label in ["land-needed", "answer-needed"] {
        assert_eq!(
            listed(label, &[open(7, &["auto:merge", label], T)]),
            Vec::<u64>::new(),
            "{label}"
        );
    }
}

#[test]
fn a_closed_issue_is_not_listed() {
    let closed = issue(7, "CLOSED", &["auto:merge"], &body(T, ""), &[]);
    assert_eq!(listed("closed", &[closed]), Vec::<u64>::new());
}

#[test]
fn an_issue_with_no_touches_is_not_claimable() {
    let text = "## Goal\nx\n\n## Done when\ny\n";
    let missing = issue(7, "OPEN", &["auto:merge"], text, &[]);
    assert_eq!(listed("no-touches", &[missing]), Vec::<u64>::new());
    let empty = issue(7, "OPEN", &["auto:merge"], &body("nothing", ""), &[]);
    assert_eq!(listed("empty-touches", &[empty]), Vec::<u64>::new());
}

#[test]
fn touches_ends_at_the_next_heading() {
    let text = "## Touches\n`crates/a`\n\n## Notes\n`crates/b`\n";
    let repo = Repo::new("heading", &[issue(7, "OPEN", &["auto:merge"], text, &[])]);
    assert!(repo.next().contains("\"touches\":[\"crates/a\"]"));
}

#[test]
fn a_parent_is_not_claimable() {
    let parent = open(1, &["auto:merge"], T);
    let child = |state: &str| {
        issue(
            5,
            state,
            &["auto:merge"],
            &body("`crates/b`", "Part of #1.\n"),
            &[],
        )
    };
    assert_eq!(
        listed("parent-open", &[parent.clone(), child("OPEN")]),
        vec![5]
    );
    assert_eq!(
        listed("parent-closed", &[parent, child("CLOSED")]),
        Vec::<u64>::new()
    );
}

#[test]
fn a_live_claim_is_not_listed() {
    assert_eq!(listed("live", &[claimed(7, T)]), Vec::<u64>::new());
}

fn abandoned(number: u64, branch: &str) -> String {
    issue(
        number,
        "OPEN",
        &["auto:merge", "in-progress"],
        &body(T, ""),
        &[status(number * 10, OWNER, "working", branch, EXPIRED)],
    )
}

#[test]
fn an_abandoned_issue_is_listed_with_its_branch() {
    let repo = Repo::new("abandoned", &[abandoned(7, "7-x")]);
    repo.set_branch("7-x", BEFORE_EXPIRY);
    assert_eq!(
        repo.next(),
        "[{\"issue\":7,\"touches\":[\"crates/a\"],\"autonomy\":\"auto:merge\",\"branch\":\"7-x\",\"worktree\":null}]\n"
    );
}

#[test]
fn an_abandoned_issue_names_the_worktree_holding_its_branch() {
    let repo = Repo::new("worktree", &[abandoned(7, "7-x")]);
    repo.set_branch("7-x", BEFORE_EXPIRY);
    repo.git(&["checkout", "-q", "-b", "7-x"]);
    let root = fs::canonicalize(&repo.root).unwrap();
    assert!(
        repo.next()
            .contains(&format!("\"worktree\":\"{}\"", root.display())),
        "{}",
        repo.next()
    );
}

#[test]
fn an_abandoned_issue_whose_branch_is_gone_has_no_branch() {
    let repo = Repo::new("gone", &[abandoned(7, "7-x")]);
    assert!(repo.next().contains("\"branch\":null,\"worktree\":null"));
}

#[test]
fn commits_since_the_lease_keep_a_claim_alive() {
    let repo = Repo::new("commits", &[abandoned(7, "7-x")]);
    repo.set_branch("7-x", AFTER_EXPIRY);
    assert_eq!(repo.numbers(), Vec::<u64>::new());
}

#[test]
fn a_commit_in_the_minute_after_expiry_keeps_a_claim_alive() {
    let repo = Repo::new("same-minute", &[abandoned(7, "7-x")]);
    repo.set_branch("7-x", "2001-01-01T00:00:30Z");
    assert_eq!(repo.numbers(), Vec::<u64>::new());
}

#[test]
fn a_parked_status_does_not_hold_an_expired_claim() {
    let issues = [issue(
        7,
        "OPEN",
        &["auto:merge", "in-progress"],
        &body(T, ""),
        &[
            status(70, OWNER, "working", "7-x", EXPIRED),
            status(71, OWNER, "parked", "7-y", LIVE),
        ],
    )];
    let repo = Repo::new("parked-loser", &issues);
    repo.set_branch("7-x", BEFORE_EXPIRY);
    assert_eq!(repo.numbers(), vec![7]);
    assert!(repo.next().contains("\"branch\":\"7-x\""));
}

#[test]
fn in_progress_with_only_parked_statuses_stays_claimed() {
    let issues = [issue(
        7,
        "OPEN",
        &["auto:merge", "in-progress"],
        &body(T, ""),
        &[status(71, OWNER, "parked", "7-y", EXPIRED)],
    )];
    let repo = Repo::new("only-parked", &issues);
    let output = repo.xtask(&["next"]);
    assert_eq!(stdout(&output), "[]\n");
    assert!(stderr(&output).contains("#7"), "{}", stderr(&output));
}

#[test]
fn a_comment_from_another_account_is_ignored() {
    let label = &["auto:merge", "in-progress"][..];
    let theirs_only = |lease: &str| {
        issue(
            7,
            "OPEN",
            label,
            &body(T, ""),
            &[status(70, "stranger", "working", "b", lease)],
        )
    };
    assert_eq!(
        listed("stranger-expired", &[theirs_only(EXPIRED)]),
        Vec::<u64>::new(),
        "no lease of the owner's has expired"
    );
    let both = issue(
        7,
        "OPEN",
        label,
        &body(T, ""),
        &[
            status(70, OWNER, "working", "7-x", EXPIRED),
            status(71, "stranger", "working", "b", LIVE),
        ],
    );
    let repo = Repo::new("stranger-live", &[both]);
    repo.set_branch("7-x", BEFORE_EXPIRY);
    assert_eq!(repo.numbers(), vec![7]);
}

#[test]
fn in_progress_with_no_status_comment_stays_claimed() {
    let repo = Repo::new("no-status", &[open(7, &["auto:merge", "in-progress"], T)]);
    let output = repo.xtask(&["next"]);
    assert_eq!(stdout(&output), "[]\n");
    assert!(stderr(&output).contains("#7"), "{}", stderr(&output));
}

#[test]
fn a_lease_that_can_not_be_read_stays_claimed() {
    let bad = issue(
        7,
        "OPEN",
        &["auto:merge", "in-progress"],
        &body(T, ""),
        &[status(70, OWNER, "working", "b", "tomorrow")],
    );
    let repo = Repo::new("bad-lease", &[bad]);
    let output = repo.xtask(&["next"]);
    assert_eq!(stdout(&output), "[]\n");
    assert!(stderr(&output).contains("#7"), "{}", stderr(&output));
}

#[test]
fn a_live_claim_without_an_autonomy_label_still_counts() {
    let held = issue(
        2,
        "OPEN",
        &["in-progress"],
        &body(T, ""),
        &[status(20, OWNER, "working", "b", LIVE)],
    );
    assert_eq!(
        listed("held", &[held, open(7, &["auto:merge"], T)]),
        Vec::<u64>::new()
    );
}

#[test]
fn overlap_is_a_path_prefix_table() {
    for (case, first, second, overlap) in [
        ("equal", "`crates/a`", "`crates/a`", true),
        ("prefix", "`crates/`", "`crates/a/src/x.rs`", true),
        ("reverse", "`crates/a/src/x.rs`", "`crates/`", true),
        ("slash", "`crates/a/`", "`crates/a`", true),
        ("sibling", "`crates/a`", "`crates/b`", false),
        ("similar", "`crates/ap`", "`crates/apple`", false),
        ("second-word", "`x` `crates/a`", "`crates/a`", true),
    ] {
        let issues = [
            open(4, &["auto:merge"], first),
            open(9, &["auto:merge"], second),
        ];
        let expected = if overlap { vec![4] } else { vec![4, 9] };
        assert_eq!(
            listed(&format!("overlap-{case}"), &issues),
            expected,
            "{case}"
        );
    }
}

#[test]
fn an_issue_overlapping_a_live_claim_is_left_out() {
    assert_eq!(
        listed("over-live", &[claimed(2, T), open(9, &["auto:merge"], T)]),
        Vec::<u64>::new()
    );
}

#[test]
fn an_issue_left_out_does_not_hide_a_later_one() {
    let issues = [
        claimed(2, T),
        open(4, &["auto:merge"], T),
        open(9, &["auto:merge"], "`crates/b`"),
    ];
    assert_eq!(listed("not-hidden", &issues), vec![9]);
}

#[test]
fn an_abandoned_issue_does_not_hold_its_paths() {
    let issues = [abandoned(2, "2-x"), open(9, &["auto:merge"], "`crates/b`")];
    let repo = Repo::new("abandoned-free", &issues);
    repo.set_branch("2-x", BEFORE_EXPIRY);
    assert_eq!(repo.numbers(), vec![2, 9]);
}

#[test]
fn entries_are_in_number_order_up_to_the_free_slots() {
    let paths = ["`a`", "`b`", "`c`", "`d`", "`e`"];
    let issues: Vec<String> = [9, 8, 7, 6, 5]
        .iter()
        .zip(paths)
        .map(|(number, path)| open(*number, &["auto:merge"], path))
        .collect();
    assert_eq!(listed("slots-free", &issues), vec![5, 6, 7]);

    let mut one_held = issues.clone();
    one_held.push(claimed(2, "`z`"));
    assert_eq!(listed("slots-one", &one_held), vec![5, 6]);

    let mut full = issues;
    full.extend([claimed(1, "`x`"), claimed(2, "`y`"), claimed(3, "`z`")]);
    assert_eq!(listed("slots-full", &full), Vec::<u64>::new());
}

#[test]
fn next_takes_no_arguments() {
    let output = Repo::new("args", &[]).xtask(&["next", "extra"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("takes no arguments"));
}

#[test]
fn a_failing_gh_is_a_problem() {
    let repo = Repo::new("gh-fails", &[]);
    fs::write(repo.bin.join("gh"), "#!/bin/sh\necho nope >&2\nexit 1\n").unwrap();
    let output = repo.xtask(&["next"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("nope"), "{}", stderr(&output));
}

#[test]
fn claim_needs_a_number() {
    let repo = Repo::new("claim-usage", &[]);
    for args in [
        &["claim"][..],
        &["claim", "x"][..],
        &["claim", "1", "extra"][..],
        &["claim", "1", "--thread"][..],
    ] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`claim` takes"), "{args:?}");
    }
    assert_eq!(repo.calls(), "");
}

#[test]
fn claim_on_the_default_branch_is_a_problem() {
    let repo = Repo::new("claim-main", &[open(7, &["auto:merge"], T)]);
    let output = repo.xtask(&["claim", "7"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("branch"), "{}", stderr(&output));
    assert!(!repo.calls().contains("issue edit"), "{}", repo.calls());
}

#[test]
fn claim_of_a_claimed_issue_fails_without_writing() {
    let repo = Repo::new("claim-held", &[claimed(7, T)]);
    repo.git(&["checkout", "-q", "-b", "7-x"]);
    let output = repo.xtask(&["claim", "7"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(!calls.contains("issue edit"), "{calls}");
    assert!(!calls.contains("issue comment"), "{calls}");
}

#[test]
fn claim_writes_through_gh_and_loses_when_its_comment_is_gone() {
    let repo = Repo::new("claim-race", &[open(7, &["auto:merge"], T)]);
    repo.git(&["checkout", "-q", "-b", "7-x"]);
    fs::write(
        repo.bin.join("view.json"),
        issue(
            7,
            "OPEN",
            &["auto:merge", "in-progress"],
            &body(T, ""),
            &[status(100, OWNER, "working", "7-y", LIVE)],
        ),
    )
    .unwrap();
    let output = repo.xtask(&["claim", "7"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let calls = repo.calls();
    assert!(
        calls.contains("issue edit 7 --add-label in-progress"),
        "{calls}"
    );
    assert!(calls.contains("issue comment 7 --body-file -"), "{calls}");
}

/// A repository where a claim loses to a live status comment.
fn repo_where_a_claim_loses(name: &str) -> Repo {
    let repo = Repo::new(name, &[open(7, &["auto:merge"], T)]);
    repo.git(&["checkout", "-q", "-b", "7-x"]);
    fs::write(
        repo.bin.join("view.json"),
        issue(
            7,
            "OPEN",
            &["auto:merge", "in-progress"],
            &body(T, ""),
            &[status(100, OWNER, "working", "7-y", LIVE)],
        ),
    )
    .unwrap();
    repo
}

/// Bigger than Linux's usual 64 KiB pipe buffer, so writing it blocks until the
/// fake `gh`, which never reads its input, exits. The write then breaks.
fn unread_thread() -> String {
    "x".repeat(100_000)
}

#[test]
fn a_gh_that_exits_without_reading_its_input_still_succeeds() {
    let repo = repo_where_a_claim_loses("gh-unread");
    let output = repo.xtask(&["claim", "7", "--thread", &unread_thread()]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[test]
fn a_gh_that_fails_without_reading_its_input_reports_its_own_error() {
    let repo = repo_where_a_claim_loses("gh-unread-fails");
    fs::write(repo.bin.join("comment-refused"), "").unwrap();
    let output = repo.xtask(&["claim", "7", "--thread", &unread_thread()]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("gh: comment refused"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_claim_waits_for_another_claim_on_this_machine() {
    let repo = Repo::new("claim-lock", &[claimed(7, T)]);
    repo.git(&["checkout", "-q", "-b", "7-x"]);
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("claim.lock")).unwrap();
    held.lock().unwrap();
    let mut child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["claim", "7"])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(repo.calls(), "");
    held.unlock().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert!(repo.calls().contains("issue list"), "{}", repo.calls());
}

fn filler(count: u64) -> Vec<String> {
    (1..=count)
        .map(|id| {
            format!(
                "{{\"id\":\"IC_x\",\"author\":{{\"login\":\"someone\"}},\"body\":\"hello\",\"url\":\"https://github.com/o/r/issues/1#issuecomment-{id}\"}}"
            )
        })
        .collect()
}

#[test]
fn an_issue_at_the_comment_limit_is_read_in_full() {
    let labels = &["auto:merge", "in-progress"][..];
    let listed = issue(7, "OPEN", labels, &body(T, ""), &filler(100));
    let mut all = filler(100);
    all.push(status(900, OWNER, "working", "7-x", EXPIRED));
    let repo = Repo::new("comment-limit", &[listed]);
    fs::write(
        repo.bin.join("view.json"),
        issue(7, "OPEN", labels, &body(T, ""), &all),
    )
    .unwrap();
    repo.set_branch("7-x", BEFORE_EXPIRY);
    assert_eq!(repo.numbers(), vec![7]);
}

#[test]
fn a_listing_at_the_issue_limit_is_a_problem() {
    let issues: Vec<String> = (1..=2000)
        .map(|number| issue(number, "CLOSED", &[], "", &[]))
        .collect();
    let output = Repo::new("issue-limit", &issues).xtask(&["next"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("2000"), "{}", stderr(&output));
}
