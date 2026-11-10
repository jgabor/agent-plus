//! Runs `cargo xtask roadmap` in throwaway repositories, with a fake `gh` on
//! `PATH` standing in for GitHub, so no test reaches it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const JOURNEY: &str = r#"[[milestone]]
name = "Boot"
commands = ["ap --version", "ap boot"]

[[milestone]]
name = "Author"
commands = ["ap init"]

[[milestone]]
name = "Run"
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

const GH: &str = r#"#!/bin/sh
d="$(dirname "$0")"
printf '%s\n' "$*" >> "$d/calls"
case "$1 $2" in
  "issue list") cat "$d/issues.json" ;;
  "pr list") cat "$d/prs.json" ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#;

const HEADER: &str = "The nightly job writes this issue from `cargo xtask roadmap`, so don't edit it. To change it, change `harness/journey.toml` or the issues it lists.\n";

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
}

const PLAIN: Issue = Issue {
    number: 0,
    state: "OPEN",
    title: "t",
    labels: &[],
    body: "",
    parent: None,
};

impl Issue<'_> {
    fn json(&self) -> String {
        let parent = self.parent.map_or("null".to_string(), |parent| {
            format!("{{\"number\":{parent},\"url\":\"https://github.com/o/r/issues/{parent}\"}}")
        });
        format!(
            "{{\"number\":{0},\"state\":{1},\"title\":{2},\"body\":{3},\"labels\":[{4}],\"url\":\"https://github.com/o/r/issues/{0}\",\"parent\":{parent}}}",
            self.number,
            quoted(self.state),
            quoted(self.title),
            quoted(self.body),
            labels(self.labels),
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
        let path = format!(
            "{}:{}",
            self.bin.display(),
            env::var("PATH").expect("PATH is set")
        );
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD")
            .output()
            .unwrap()
    }

    fn roadmap(&self) -> String {
        let output = self.xtask(&["roadmap"]);
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
            title: "Found along the way",
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
            &[40],
        ),
        pr(45, "A PR that closes nothing", &["land-needed"], &[]),
        pr(48, "A PR that waits on nobody", &[], &[43]),
    ];
    (issues, prs)
}

const FULL: &str = "
## v0.1.0

journey 2 of 4

### 1. Boot

Parent: #10, open, with 2 of 2 sub-issues closed. Every step passes, so it is ready to close.
Commands: `ap --version`, `ap boot`

- [x] Step 1: first

### 2. Author

Parent: #20, open, with 1 of 2 sub-issues closed.
Commands: `ap init`

- [ ] Step 2: second (fails)
- [ ] Step 3: third (skipped)

### 3. Run

Parent: none.
Commands: none.

- [x] Step 4: fourth

## After v0.1.0

- #30 `ap lint` flags bloated instructions

## Waiting on the owner

### To merge

- #41 Group the journey's steps (closes #40)
- #45 A PR that closes nothing
- #46 Ready, with no PR

### To answer

- #42 Stuck on a question

### To label or close

- #10 Boot the machine
- #43 Found along the way
";

#[test]
fn the_roadmap_shows_milestones_gaps_and_what_waits_on_the_owner() {
    let repo = Repo::new("full");
    let (issues, prs) = full();
    repo.set(&issues, &prs);
    assert_eq!(repo.roadmap(), format!("{HEADER}{FULL}"));
}

/// The command only prints. Writing the issue is the nightly job's.
#[test]
fn the_roadmap_reads_github_and_writes_nothing() {
    let repo = Repo::new("reads");
    repo.roadmap();
    assert_eq!(
        repo.calls(),
        [
            "issue list --state all --limit 2000 --json number,state,title,body,labels,parent,url",
            "pr list --state open --limit 1000 --json number,title,labels,url,isCrossRepository,headRefName,headRefOid,closingIssuesReferences",
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
}

#[test]
fn a_repository_with_no_issues_still_has_a_roadmap() {
    let repo = Repo::new("empty");
    let text = repo.roadmap();
    assert!(text.starts_with(HEADER), "{text}");
    assert_eq!(text.matches("Parent: none.\n").count(), 3, "{text}");
    assert!(
        text.ends_with(
            "## After v0.1.0\n\nNothing.\n\n## Waiting on the owner\n\n### To merge\n\nNothing.\n\n### To answer\n\nNothing.\n\n### To label or close\n\nNothing.\n"
        ),
        "{text}"
    );
}

fn parent_line(repo: &Repo, issues: &[Issue]) -> String {
    repo.set(issues, &[]);
    let text = repo.roadmap();
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
    let text = repo.roadmap();
    assert!(text.contains("## After v0.1.0\n\nNothing.\n"), "{text}");
}

#[test]
fn the_roadmap_takes_no_arguments() {
    let repo = Repo::new("arguments");
    let output = repo.xtask(&["roadmap", "--write"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`roadmap` takes no arguments"), "{stderr}");
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
