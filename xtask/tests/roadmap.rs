//! Runs `cargo xtask roadmap` in throwaway repositories, with a fake `gh` on
//! `PATH` standing in for GitHub, so no test reaches it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

const JOURNEY: &str = r#"[[milestone]]
name = "Boot"
summary = "Start a machine"
commands = ["ap --version", "ap boot"]

[[milestone]]
name = "Author"
summary = "Describe an agent"
commands = ["ap init"]

[[milestone]]
name = "Run"
summary = "Run <agent> | watch it"
commands = []

[[step]]
id = 1
name = "first"
milestone = "Boot"
needs = []
script = "harness/journey/ok.sh"

[[step]]
id = 2
name = "second"
milestone = "Author"
needs = [1]
script = "harness/journey/bad.sh"

[[step]]
id = 3
name = "third"
milestone = "Author"
needs = [2]
script = "harness/journey/ok.sh"

[[step]]
id = 4
name = "fourth"
milestone = "Run"
needs = [1]
script = "harness/journey/ok.sh"
"#;

/// `ap init` has two sentences of its own, and Build's two through its
/// pointer. The pointer to Other only "says", so Other's don't count.
const SPEC: &str = "# Reference

## Build

`ap build` writes an image. A rebuild is a hit.

## Commands

### ap --version

`ap --version` prints the version.

### ap boot

`ap boot` boots a machine. It powers off.

### ap init

[Build](#build)
specifies `ap init`. [Other](#other) says more.

### ap push

**Planned.** `ap push` shares an agent.

### ap mcp

*Proposed:* `ap mcp` drives a desktop.

## Other

Unrelated.
";

/// `ap mcp` has no item under Core commands, and `ap pull` isn't in SPEC.md.
const README: &str = "# Demo

## Core commands

#### About

- `ap --version`: print the version

#### Agents

- `ap boot [--timeout N]`: boot a bare machine,
  then power off
- `ap init [dir]`: create an Agentfile from <dir>
- `ap push` and `ap pull`: share agents (after v0.1.0)

## Why

- `ap mcp`: outside Core commands, so it describes nothing
";

const CHECKS: &str = "// spec(ap --version): `ap --version` prints the version.
#[test]
fn the_version_prints() {}

// spec(Build): A rebuild is a hit.
#[test]
fn a_rebuild_is_a_hit() {}
";

const GH: &str = r#"#!/bin/sh
d="$(dirname "$0")"
printf '%s\n' "$*" >> "$d/calls"
case "$1 $2" in
  "issue list") cat "$d/issues.json" ;;
  "pr list") cat "$d/prs.json" ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#;

const HEADER: &str = "This page shows what works in Agent+ today, and what comes next. The nightly job writes it from the project's checks, so don't edit it.\n";
const DETAILS_HEADER: &str = "The nightly job writes this issue from `cargo xtask roadmap --details`, so don't edit it. To change it, change `harness/journey.toml` or the issues it lists.\n";

fn quoted(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn labels(names: &[&str]) -> String {
    let labels: Vec<String> = names
        .iter()
        .map(|name| format!("{{\"name\":{}}}", quoted(name)))
        .collect();
    labels.join(",")
}

struct Issue<'a> {
    number: u64,
    state: &'a str,
    title: &'a str,
    labels: &'a [&'a str],
    body: &'a str,
    parent: Option<u64>,
    created: &'a str,
}

const PLAIN: Issue = Issue {
    number: 0,
    state: "OPEN",
    title: "t",
    labels: &[],
    body: "",
    parent: None,
    created: "2026-01-01T00:00:00Z",
};

impl Issue<'_> {
    fn json(&self) -> String {
        let parent = self.parent.map_or("null".to_string(), |parent| {
            format!("{{\"number\":{parent},\"url\":\"https://github.com/o/r/issues/{parent}\"}}")
        });
        format!(
            "{{\"number\":{0},\"state\":{1},\"title\":{2},\"body\":{3},\"labels\":[{4}],\"url\":\"https://github.com/o/r/issues/{0}\",\"parent\":{parent},\"createdAt\":{5}}}",
            self.number,
            quoted(self.state),
            quoted(self.title),
            quoted(self.body),
            labels(self.labels),
            quoted(self.created),
        )
    }
}

/// One row of `gh pr list`.
fn pr(number: u64, title: &str, names: &[&str], closes: &[u64]) -> String {
    let closes: Vec<String> = closes
        .iter()
        .map(|issue| {
            format!("{{\"number\":{issue},\"url\":\"https://github.com/o/r/issues/{issue}\"}}")
        })
        .collect();
    format!(
        "{{\"number\":{number},\"title\":{},\"labels\":[{}],\"url\":\"https://github.com/o/r/pull/{number}\",\"isCrossRepository\":false,\"headRefName\":\"b\",\"headRefOid\":\"c\",\"closingIssuesReferences\":[{}]}}",
        quoted(title),
        labels(names),
        closes.join(",")
    )
}

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("roadmap")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.bin).unwrap();
        common::write_tool(&repo.bin.join("gh"), GH);
        repo.set(&[], &[]);
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            (
                "crates/ap/Cargo.toml",
                "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
            ),
            ("crates/ap/src/main.rs", "fn main() {}\n"),
            ("crates/ap/tests/checks.rs", CHECKS),
            ("SPEC.md", SPEC),
            ("README.md", README),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/ok.sh", "exit 0\n"),
            ("harness/journey/bad.sh", "exit 1\n"),
        ];
        for (path, content) in files {
            let path = repo.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let lockfile = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&repo.root)
            .output()
            .unwrap();
        assert!(lockfile.status.success());
        let init = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo.root)
            .output()
            .unwrap();
        assert!(init.status.success());
        repo
    }

    fn set(&self, issues: &[Issue], prs: &[String]) {
        let issues: Vec<String> = issues.iter().map(Issue::json).collect();
        self.set_json(&issues, prs);
    }

    fn set_json(&self, issues: &[String], prs: &[String]) {
        let write = |name: &str, rows: &[String]| {
            fs::write(self.bin.join(name), format!("[{}]\n", rows.join(","))).unwrap();
        };
        write("issues.json", issues);
        write("prs.json", prs);
    }

    fn calls(&self) -> Vec<String> {
        let calls = fs::read_to_string(self.bin.join("calls")).unwrap_or_default();
        calls.lines().map(str::to_string).collect()
    }

    fn xtask(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .current_dir(&self.root)
            .env("PATH", common::path(&[&self.bin]))
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD")
            .output()
            .unwrap()
    }

    fn roadmap(&self) -> String {
        self.printed(&["roadmap"])
    }

    fn details(&self) -> String {
        self.printed(&["roadmap", "--details"])
    }

    fn printed(&self, args: &[&str]) -> String {
        let output = self.xtask(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

/// Everything the roadmap shows, in one repository.
fn full() -> (Vec<Issue<'static>>, Vec<String>) {
    let issues = vec![
        Issue {
            number: 1,
            title: "Roadmap",
            body: "typed by hand",
            ..PLAIN
        },
        Issue {
            number: 2,
            title: "Nightly report",
            ..PLAIN
        },
        Issue {
            number: 3,
            title: "Roadmap details",
            body: "written last night",
            ..PLAIN
        },
        Issue {
            number: 10,
            title: "Boot the machine",
            body: "## Goal\nIt boots.\n\nMilestone: Boot.\n\n## Touches\n`crates/ap`\n",
            ..PLAIN
        },
        Issue {
            number: 11,
            state: "CLOSED",
            parent: Some(10),
            ..PLAIN
        },
        Issue {
            number: 12,
            state: "CLOSED",
            parent: Some(10),
            ..PLAIN
        },
        Issue {
            number: 20,
            title: "Author an agent",
            labels: &["auto:merge"],
            body: "Milestone: Author.",
            ..PLAIN
        },
        Issue {
            number: 21,
            state: "CLOSED",
            parent: Some(20),
            ..PLAIN
        },
        Issue {
            number: 22,
            labels: &["auto:merge"],
            parent: Some(20),
            ..PLAIN
        },
        Issue {
            number: 30,
            title: "`ap lint` flags bloated instructions",
            parent: Some(1),
            ..PLAIN
        },
        Issue {
            number: 31,
            state: "CLOSED",
            title: "A gap that was closed",
            parent: Some(1),
            ..PLAIN
        },
        Issue {
            number: 40,
            title: "Milestones are in the journey file",
            labels: &["auto:pr", "land-needed"],
            ..PLAIN
        },
        Issue {
            number: 42,
            title: "Stuck on a question",
            labels: &["auto:merge", "answer-needed"],
            ..PLAIN
        },
        Issue {
            number: 43,
            title: "Share <src> with `ap run <agent>`",
            ..PLAIN
        },
        Issue {
            number: 44,
            title: "Labeled, so it waits on nobody",
            labels: &["auto:pr"],
            ..PLAIN
        },
        Issue {
            number: 46,
            title: "Ready, with no PR",
            labels: &["land-needed"],
            ..PLAIN
        },
        Issue {
            number: 49,
            title: "Its PR is ready, and the label isn't copied yet",
            ..PLAIN
        },
        Issue {
            number: 51,
            title: "Asked, and then finished",
            labels: &["auto:merge", "answer-needed"],
            ..PLAIN
        },
        Issue {
            number: 47,
            state: "CLOSED",
            title: "Closed, so it waits on nobody",
            labels: &["answer-needed"],
            ..PLAIN
        },
    ];
    let prs = vec![
        pr(
            41,
            "Group the journey's steps",
            &["spec-change", "land-needed"],
            &[49, 40],
        ),
        pr(52, "Finish what was asked about", &["land-needed"], &[51]),
        pr(45, "A PR that closes nothing", &["land-needed"], &[]),
        pr(48, "A PR that waits on nobody", &[], &[43]),
    ];
    (issues, prs)
}

/// What anyone can read: no parents, no steps, and no counts of promises.
const FULL: &str = "
## First release (v0.1.0)

Milestones done: 2 of 3.

| | Milestone | What you can do | Status |
|---|---|---|---|
| 1 | Boot | Start a machine | Done |
| 2 | Author | Describe an agent | In progress |
| 3 | Run | Run \\<agent> \\| watch it | Done |

## Commands

### 1. Boot (done)

- [x] `ap --version`: print the version
- [x] `ap boot`: boot a bare machine, then power off

### 2. Author (in progress)

- [ ] `ap init`: create an Agentfile from \\<dir>

## After the first release

- `ap push` and `ap pull`: share agents (after v0.1.0)
- `ap mcp`
- #30 `ap lint` flags bloated instructions

For each milestone's issue and checks, and for what waits on the owner, see #3.
";

const FULL_DETAILS: &str = "
## v0.1.0

journey 2 of 4

### 1. Boot

Parent: #10, open, with 2 of 2 sub-issues closed. Every step passes, so it is ready to close.

Commands:

- [x] `ap --version`: proven 1 of 1
- [ ] `ap boot`: proven 0 of 2

Steps:

- [x] Step 1: first

### 2. Author

Parent: #20, open, with 1 of 2 sub-issues closed.

Commands:

- [ ] `ap init`: proven 1 of 4

Steps:

- [ ] Step 2: second (fails)
- [ ] Step 3: third (skipped)

### 3. Run

Parent: none.

Commands: none.

Steps:

- [x] Step 4: fourth

## After v0.1.0

- `ap push`: later
- `ap mcp`: a proposal
- #30 `ap lint` flags bloated instructions

## Waiting on the owner

### To merge

- #41 Group the journey's steps (closes #40, #49)
- #45 A PR that closes nothing
- #46 Ready, with no PR
- #52 Finish what was asked about (closes #51)

### To answer

- #42 Stuck on a question

### To label or close

- #10 Boot the machine
- #43 Share \\<src> with `ap run <agent>`

## Busy and large files

Nothing.
";

#[test]
fn the_roadmap_shows_each_milestone_and_command_in_plain_words() {
    let repo = Repo::new("full");
    let (issues, prs) = full();
    repo.set(&issues, &prs);
    assert_eq!(repo.roadmap(), format!("{HEADER}{FULL}"));
}

#[test]
fn the_details_show_parents_steps_proof_and_what_waits_on_the_owner() {
    let repo = Repo::new("full-details");
    let (issues, prs) = full();
    repo.set(&issues, &prs);
    assert_eq!(repo.details(), format!("{DETAILS_HEADER}{FULL_DETAILS}"));
}

/// The words a reader of the roadmap would have to look up stay in the details.
#[test]
fn the_roadmap_leaves_the_harness_out() {
    for word in [
        "Parent",
        "Step",
        "proven",
        "sub-issue",
        "journey",
        "Waiting",
    ] {
        assert!(!FULL.contains(word), "{word}");
        assert!(FULL_DETAILS.contains(word), "{word}");
    }
}

/// A milestone is done when every one of its steps passes, and the first one
/// that isn't done is the one in progress.
#[test]
fn a_roadmap_with_every_step_passing_has_nothing_in_progress() {
    let repo = Repo::new("all-done");
    fs::write(repo.root.join("harness/journey/bad.sh"), "exit 0\n").unwrap();
    let text = repo.roadmap();
    assert!(text.contains("Milestones done: 3 of 3.\n"), "{text}");
    assert!(
        text.contains("### 2. Author (done)\n\n- [x] `ap init`"),
        "{text}"
    );
    assert!(!text.contains("In progress"), "{text}");
    assert!(!text.contains("Planned"), "{text}");
}

#[test]
fn a_milestone_after_the_one_in_progress_is_planned() {
    let repo = Repo::new("planned");
    fs::write(repo.root.join("harness/journey/ok.sh"), "exit 1\n").unwrap();
    let text = repo.roadmap();
    assert!(text.contains("Milestones done: 0 of 3.\n"), "{text}");
    assert!(
        text.contains("| 1 | Boot | Start a machine | In progress |\n"),
        "{text}"
    );
    assert!(
        text.contains("| 2 | Author | Describe an agent | Planned |\n"),
        "{text}"
    );
    assert!(
        text.contains("### 1. Boot (in progress)\n\n- [ ] `ap --version`"),
        "{text}"
    );
    assert!(
        text.contains("### 2. Author (planned)\n\n- [ ] `ap init`"),
        "{text}"
    );
}

/// The gate's `spec` check is what asks for the item. The roadmap still prints.
#[test]
fn a_command_the_readme_doesnt_describe_is_shown_by_name() {
    let repo = Repo::new("no-readme");
    fs::remove_file(repo.root.join("README.md")).unwrap();
    let text = repo.roadmap();
    assert!(
        text.contains("- [x] `ap --version`\n- [x] `ap boot`\n"),
        "{text}"
    );
    assert!(
        text.contains("## After the first release\n\n- `ap push`\n- `ap mcp`\n"),
        "{text}"
    );
}

/// The command only prints. Writing the issue is the nightly job's.
#[test]
fn the_roadmap_reads_github_and_writes_nothing() {
    let repo = Repo::new("reads");
    repo.roadmap();
    assert_eq!(
        repo.calls(),
        [
            "issue list --state all --limit 2000 --json number,state,title,body,labels,parent,url,createdAt",
            "pr list --state open --limit 1000 --json number,title,labels,url,isCrossRepository,headRefName,headRefOid,closingIssuesReferences,autoMergeRequest",
        ]
    );
}

/// GitHub lists the newest first, and its order isn't the roadmap's.
#[test]
fn the_same_inputs_give_the_same_bytes_in_any_order() {
    let repo = Repo::new("order");
    let (mut issues, mut prs) = full();
    issues.reverse();
    prs.reverse();
    repo.set(&issues, &prs);
    assert_eq!(repo.roadmap(), format!("{HEADER}{FULL}"));
    assert_eq!(repo.roadmap(), format!("{HEADER}{FULL}"));
    assert_eq!(repo.details(), format!("{DETAILS_HEADER}{FULL_DETAILS}"));
}

#[test]
fn a_repository_with_no_issues_still_has_a_roadmap() {
    let repo = Repo::new("empty");
    let text = repo.roadmap();
    assert!(text.starts_with(HEADER), "{text}");
    // With no details issue, there is nothing to point at.
    assert!(
        text.ends_with(
            "## After the first release\n\n- `ap push` and `ap pull`: share agents (after v0.1.0)\n- `ap mcp`\n"
        ),
        "{text}"
    );
    let text = repo.details();
    assert!(text.starts_with(DETAILS_HEADER), "{text}");
    assert_eq!(text.matches("Parent: none.\n").count(), 3, "{text}");
    assert!(
        text.ends_with(
            "## After v0.1.0\n\n- `ap push`: later\n- `ap mcp`: a proposal\n\n## Waiting on the owner\n\n### To merge\n\nNothing.\n\n### To answer\n\nNothing.\n\n### To label or close\n\nNothing.\n\n## Busy and large files\n\nNothing.\n"
        ),
        "{text}"
    );
}

/// GitHub's form of the time this many days ago, and half a day more, so a
/// run that takes hours still counts the same days.
fn days_ago(days: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let seconds = now.as_secs() - days * DAY - DAY / 2;
    // Howard Hinnant's algorithm, from days since 1970 to a civil date.
    let z = seconds / DAY + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let rest = seconds % DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// An action only the owner can take comes before anything else that waits
/// on the owner, the oldest first, and isn't also an issue to label.
#[test]
fn the_owners_actions_come_first_and_oldest_first() {
    let repo = Repo::new("actions");
    let (one, two, nine) = (days_ago(1), days_ago(2), days_ago(9));
    let action = &["action-needed"];
    let issues = [
        Issue {
            number: 60,
            title: "Turn the timer on, on the Linux machine",
            labels: action,
            created: &two,
            ..PLAIN
        },
        Issue {
            number: 61,
            title: "Set the `<token>` secret",
            labels: action,
            created: &nine,
            ..PLAIN
        },
        Issue {
            number: 62,
            title: "Done, so it waits on nobody",
            state: "CLOSED",
            labels: action,
            created: &nine,
            ..PLAIN
        },
        Issue {
            number: 63,
            title: "Stuck on a question",
            labels: &["auto:merge", "answer-needed"],
            ..PLAIN
        },
        Issue {
            number: 64,
            title: "Enable the setting",
            labels: action,
            created: &one,
            ..PLAIN
        },
    ];
    repo.set(&issues, &[]);
    let text = repo.details();
    let waiting = "## Waiting on the owner\n\n### To do\n\n- #61 Set the `<token>` secret (open 9 days)\n- #60 Turn the timer on, on the Linux machine (open 2 days)\n- #64 Enable the setting (open 1 day)\n\n### To merge\n\nNothing.\n\n### To answer\n\n- #63 Stuck on a question\n\n### To label or close\n\nNothing.\n\n## Busy";
    assert!(text.contains(waiting), "{text}");
}

fn parent_line(repo: &Repo, issues: &[Issue]) -> String {
    repo.set(issues, &[]);
    let text = repo.details();
    let line = text.lines().find(|line| line.starts_with("Parent: "));
    line.unwrap().to_string()
}

/// A milestone's parent is the issue with the milestone's line in its body.
#[test]
fn a_milestone_line_names_the_parent() {
    let repo = Repo::new("parent");
    let boot = |number: u64, state: &'static str, body: &'static str| Issue {
        number,
        state,
        body,
        labels: &["auto:merge"],
        ..PLAIN
    };
    let named = "Milestone: Boot.";
    assert_eq!(
        parent_line(&repo, &[boot(7, "CLOSED", named)]),
        "Parent: #7, closed, with 0 of 0 sub-issues closed."
    );
    // An open parent is the one being worked on, whatever came before it.
    assert_eq!(
        parent_line(
            &repo,
            &[
                boot(9, "OPEN", named),
                boot(8, "OPEN", named),
                boot(7, "CLOSED", named)
            ]
        ),
        "Parent: #8, open, with 0 of 0 sub-issues closed. Every step passes, so it is ready to close."
    );
    for body in [
        "Milestone: Boot",
        "Milestone: Bootstrap.",
        "See Milestone: Boot. for more",
        "Milestone: Author.",
    ] {
        let line = parent_line(&repo, &[boot(7, "OPEN", body)]);
        assert_eq!(line, "Parent: none.", "{body}");
    }
    assert_eq!(
        parent_line(
            &repo,
            &[boot(7, "OPEN", "## Goal\r\n  Milestone: Boot.  \r\n")]
        ),
        "Parent: #7, open, with 0 of 0 sub-issues closed. Every step passes, so it is ready to close."
    );
}

/// A sub-issue of the roadmap in another repository isn't listed, and neither
/// is an issue in this one whose parent only shares the roadmap's number.
#[test]
fn only_this_repositorys_sub_issues_count() {
    let repo = Repo::new("elsewhere");
    let roadmap = Issue {
        number: 1,
        title: "Roadmap",
        ..PLAIN
    };
    let foreign = Issue {
        number: 5,
        title: "Belongs elsewhere",
        labels: &["auto:pr"],
        ..PLAIN
    };
    let foreign = foreign.json().replace(
        "\"parent\":null",
        "\"parent\":{\"number\":1,\"url\":\"https://github.com/o/other/issues/1\"}",
    );
    repo.set_json(&[roadmap.json(), foreign], &[]);
    let text = repo.details();
    let after = "## After v0.1.0\n\n- `ap push`: later\n- `ap mcp`: a proposal\n\n## Waiting";
    assert!(text.contains(after), "{text}");
    let text = repo.roadmap();
    assert!(text.ends_with("- `ap mcp`\n"), "{text}");
}

#[test]
fn the_roadmap_takes_only_details() {
    let repo = Repo::new("arguments");
    let output = repo.xtask(&["roadmap", "--write"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`roadmap` takes only `--details`"),
        "{stderr}"
    );
    assert_eq!(repo.calls(), Vec::<String>::new());
}

#[test]
fn a_roadmap_that_cant_reach_github_is_an_error() {
    let repo = Repo::new("unreachable");
    common::write_tool(&repo.bin.join("gh"), "#!/bin/sh\necho nope >&2\nexit 1\n");
    let output = repo.xtask(&["roadmap"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

/// The time of the last commit in the history that `busy` writes. It is in
/// the past, so a count that started from now would find no commit.
const HEAD_TIME: u64 = 1_790_000_000;
const DAY: u64 = 86_400;

impl Repo {
    /// Writes a file of this many lines.
    fn lines(&self, path: &str, count: usize) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "//\n".repeat(count)).unwrap();
    }

    /// Adds a line to a file.
    fn append(&self, path: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        text.push_str("//\n");
        fs::write(path, text).unwrap();
    }

    /// Commits everything, at this time.
    fn commit_at(&self, time: u64) {
        let date = format!("{time} +0000");
        for args in [
            &["add", "-A"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "change",
            ],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.root)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    }
}

/// A history where `lib/fNN.rs` changed in NN commits in the week before
/// the last one, and `lib/gone.rs` changed in more, then went.
fn busy() -> Repo {
    let repo = Repo::new("busy");
    repo.lines("lib/f12.rs", 2088);
    repo.lines("lib/gone.rs", 1);
    repo.lines("tools/old.rs", 1);
    repo.lines("tools/under.rs", 1999);
    repo.lines("tools/big1.rs", 2000);
    repo.lines("tools/big2.rs", 2999);
    repo.commit_at(HEAD_TIME - 30 * DAY);
    // An hour before the week, so it doesn't count.
    repo.append("tools/old.rs");
    repo.commit_at(HEAD_TIME - 7 * DAY - 3600);
    for commit in 1..=12 {
        for file in commit..=12 {
            repo.append(&format!("lib/f{file:02}.rs"));
        }
        repo.append("lib/gone.rs");
        repo.append("notes.md");
        repo.commit_at(HEAD_TIME - 6 * DAY + commit as u64 * 3600);
    }
    fs::remove_file(repo.root.join("lib/gone.rs")).unwrap();
    repo.append("tools/big2.rs");
    repo.commit_at(HEAD_TIME);
    repo
}

/// The 10 busiest Rust files come first, and then every other one with
/// 2,000 lines or more. A file that is gone, a Markdown file, and a change
/// from before the week aren't counted.
#[test]
fn the_details_list_the_busy_and_large_rust_files() {
    let repo = busy();
    let issue = |number: u64, state: &'static str, body: &'static str| Issue {
        number,
        state,
        body,
        labels: &["auto:merge"],
        ..PLAIN
    };
    repo.set(
        &[
            // A directory counts for each file in it.
            issue(5, "OPEN", "## Touches\n`lib`, `HARNESS.md#roadmap`\n"),
            issue(6, "OPEN", "## Touches\n`lib/f12.rs`\n\n## Notes\n`tools`\n"),
            issue(7, "CLOSED", "## Touches\n`lib/f12.rs`, `tools`\n"),
            // A path counts only in whole parts.
            issue(8, "OPEN", "## Touches\n`lib/f1`, `tools/big`\n"),
        ],
        &[],
    );
    let mut rows = String::new();
    for file in (3..=11).rev() {
        rows.push_str(&format!("| `lib/f{file:02}.rs` | {file} | {file} | 1 |\n"));
    }
    let expected = format!(
        "\n## Busy and large files\n\n| File | PRs in 7 days | Lines | Open issues that name it |\n|---|---|---|---|\n| `lib/f12.rs` | 12 | 2100 | 2 |\n{rows}| `tools/big2.rs` | 1 | 3000 | 0 |\n| `tools/big1.rs` | 0 | 2000 | 0 |\n"
    );
    let text = repo.details();
    assert!(text.ends_with(&expected), "{text}");
    assert_eq!(text.matches("## Busy and large files").count(), 1, "{text}");
}

/// A workspace that isn't a repository has no history to list. The test's
/// workspace is inside this repository, so git is kept from looking above it.
#[test]
fn a_workspace_outside_git_lists_no_files() {
    let repo = Repo::new("no-git");
    fs::remove_dir_all(repo.root.join(".git")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["roadmap", "--details"])
        .current_dir(&repo.root)
        .env("PATH", common::path(&[&repo.bin]))
        .env("GIT_CEILING_DIRECTORIES", repo.root.parent().unwrap())
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("XTASK_ACTIVITY_HELD")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.ends_with("\n## Busy and large files\n\nNothing.\n"),
        "{text}"
    );
}
