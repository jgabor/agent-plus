//! `ap rm`, from SPEC.md's `ap rm` and Build sections: it removes an agent
//! from the store, and frees the layers no other agent uses. The approval
//! stays, because it belongs to the Agentfile's path.
//!
//! It refuses while a run of the agent has pending changes, as the SHARE
//! section says. An agent has no persisted state or kept repositories yet,
//! so there's nothing else to remove.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use store::{Agents, Approvals, Runs};

/// A test's own directory, with a home directory in it.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rm").join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        common::seed_os(&home.join(".local/share/ap"));
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    /// Runs `ap` in `dir`, with only `HOME` in its environment and no
    /// terminal.
    fn ap_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ap(&self, args: &[&str]) -> Output {
        self.ap_in(&self.root, args)
    }

    /// Makes the directory `name` with this Agentfile, and a file `data`
    /// holding `data`, and returns its path.
    fn project(&self, name: &str, agentfile: &str, data: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        common::pin_harness(&self.home, &dir);
        fs::write(dir.join("data"), data).unwrap();
        dir
    }

    /// Runs `ap` with these arguments in `dir`, and fails the test unless it
    /// exits 0.
    fn ok_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.ap_in(dir, args);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    }

    /// Builds `dir`, approving `grants`, and fails the test unless it
    /// builds.
    fn build(&self, dir: &Path, grants: &str) {
        let file = self.root.join("grants");
        fs::write(&file, grants).unwrap();
        let approve = format!("--approve={}", file.display());
        self.ok_in(dir, &["build", &approve]);
    }

    fn store(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    /// The digests of the agent's layers, in order.
    fn digests(&self, name: &str) -> Vec<String> {
        let agent = Agents::new(self.store()).get(name).unwrap().unwrap();
        agent.layers.into_iter().map(|layer| layer.digest).collect()
    }

    fn has_layer(&self, digest: &str) -> bool {
        self.store().join("zstd-layers").join(digest).exists()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The OS and the locked harness, which every agent here shares, one layer
/// from `ADD`, and the harness configuration, which agents with the same
/// harness settings share.
const SIMPLE: &str = "AGENT claude\nOS minimal\nADD ./data\nNETWORK -\nPOLICY -\n";

/// The grants that `SIMPLE` asks for.
const SIMPLE_GRANTS: &str = "NETWORK -\nPOLICY -\n";

/// An Agentfile that asks for a host variable too, and its grants.
const NOTES: &str = "AGENT claude\nOS minimal\nENV NAME $USER\nNETWORK -\nPOLICY -\n";
const NOTES_GRANTS: &str = "VARIABLE USER\nNETWORK -\nPOLICY -\n";

// spec(ap rm): `ap rm <agent>` removes an agent and its persisted state.
// spec(Build): `ap rm` and rebuilds free a layer only when no agent or active run still needs it, and no repository an agent kept still depends on it (see [REPO](#repo)).
#[test]
fn rm_removes_the_agent_and_frees_only_its_own_layers() {
    let scratch = Scratch::new("removes");
    let one = scratch.project("one", SIMPLE, "one");
    let two = scratch.project("two", SIMPLE, "two");
    scratch.build(&one, SIMPLE_GRANTS);
    scratch.build(&two, SIMPLE_GRANTS);
    let ones = scratch.digests("one");
    let twos = scratch.digests("two");
    // The OS, the harness, the `ADD` layer, and the harness configuration.
    assert_eq!(ones.len(), 4);
    assert_eq!(ones[0], twos[0]);
    assert_eq!(ones[1], twos[1]);
    assert_ne!(ones[2], twos[2]);
    assert_eq!(ones[3], twos[3]);

    let output = scratch.ap(&["rm", "one"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    let listed = scratch.ok_in(&scratch.root, &["ls"]);
    assert_eq!(listed, format!("two {}\n", two.display()));
    // The layer only it used is freed, and the ones it shared stay.
    assert!(scratch.has_layer(&ones[0]));
    assert!(scratch.has_layer(&ones[1]));
    assert!(!scratch.has_layer(&ones[2]));
    assert!(scratch.has_layer(&ones[3]));
    assert!(scratch.has_layer(&twos[2]));
    assert_eq!(scratch.digests("two"), twos);
}

#[test]
fn rm_keeps_the_approval_of_the_agentfile() {
    let scratch = Scratch::new("approval");
    let dir = scratch.project("notes", NOTES, "");
    scratch.build(&dir, NOTES_GRANTS);

    scratch.ok_in(&scratch.root, &["rm", "notes"]);

    assert_eq!(scratch.ok_in(&scratch.root, &["ls"]), "");
    let approvals = Approvals::new(scratch.store());
    let kept = approvals.get(&dir.join("Agentfile")).unwrap();
    let kept = kept.map(|grants| grants.to_string());
    assert_eq!(kept.as_deref(), Some(NOTES_GRANTS));
    // With no terminal to ask on, the build goes ahead only because the
    // grants are still approved.
    scratch.ok_in(&dir, &["build"]);
    assert_eq!(
        scratch.ok_in(&scratch.root, &["ls"]),
        format!("notes {}\n", dir.display())
    );
}

#[test]
fn rm_of_an_unknown_agent_fails_and_names_it() {
    let scratch = Scratch::new("unknown");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);

    for name in ["other", "Not-A-Name"] {
        let output = scratch.ap(&["rm", name]);

        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), format!("ap: no agent named {name}\n"));
    }
    assert_eq!(
        scratch.ok_in(&scratch.root, &["ls"]),
        format!("notes {}\n", dir.display())
    );
}

// spec(ap logs): A finished run's log is kept until `ap rm <agent>` or `ap reset <agent>` deletes it.
#[test]
fn rm_deletes_the_logs_of_finished_runs_and_keeps_an_active_run_s() {
    let scratch = Scratch::new("logs");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let runs = Runs::new(scratch.store());
    let finished = runs.create("notes", 1).unwrap();
    runs.create_log(&finished.name).unwrap();
    let active = runs.create("notes", 2).unwrap();
    runs.create_log(&active.name).unwrap();
    let held = runs.hold(&active.name).unwrap();
    let other = runs.create("other", 3).unwrap();
    runs.create_log(&other.name).unwrap();

    scratch.ok_in(&scratch.root, &["rm", "notes"]);
    assert!(runs.log(&finished.name).unwrap().is_none());
    assert_eq!(runs.get(&finished.name).unwrap(), None);
    assert!(runs.log(&active.name).unwrap().is_some());
    assert!(runs.log(&other.name).unwrap().is_some());

    // Once the run ends, `ap rm` deletes its log, though the agent is gone.
    drop(held);
    scratch.ok_in(&scratch.root, &["rm", "notes"]);
    assert!(runs.log(&active.name).unwrap().is_none());
    let output = scratch.ap(&["rm", "notes"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(runs.log(&other.name).unwrap().is_some());
}

/// Records a finished run of `agent` with one copy-on-write share, and
/// returns its name and the share's pending directory. The share has no
/// changes yet.
fn run_sharing(scratch: &Scratch, agent: &str, started: u128) -> (String, PathBuf) {
    let runs = Runs::new(scratch.store());
    let run = runs.create(agent, started).unwrap();
    runs.create_pending(&run.name).unwrap();
    let pending = runs.create_pending_share(&run.name, &scratch.root).unwrap();
    fs::create_dir(pending.join("tree")).unwrap();
    fs::write(pending.join("records"), "").unwrap();
    (run.name, pending)
}

// spec(SHARE): `ap rm` and `ap reset` delete pending changes too, so they refuse while an agent has any, unless you pass `--force`.
// spec(ap rm): When it refuses, it lists the runs with pending changes, newest first.
#[test]
fn rm_refuses_while_a_run_of_the_agent_has_pending_changes() {
    let scratch = Scratch::new("pending");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let runs = Runs::new(scratch.store());
    let (oldest, oldest_pending) = run_sharing(&scratch, "notes", 1);
    fs::write(oldest_pending.join("records"), "DELETE \"data\"\n").unwrap();
    let (clean, _) = run_sharing(&scratch, "notes", 2);
    let (newest, newest_pending) = run_sharing(&scratch, "notes", 3);
    fs::write(newest_pending.join("records"), "DELETE \"data\"\n").unwrap();
    let (other, other_pending) = run_sharing(&scratch, "other", 4);
    fs::write(other_pending.join("records"), "DELETE \"data\"\n").unwrap();

    let output = scratch.ap(&["rm", "notes"]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "ap: agent notes has pending changes, so nothing is removed. Apply or discard them first:\n{newest}\n{oldest}\n"
        )
    );
    // Nothing is removed: not the agent, its runs, or their changes.
    assert_eq!(
        scratch.ok_in(&scratch.root, &["ls"]),
        format!("notes {}\n", dir.display())
    );
    for run in [&oldest, &clean, &newest] {
        assert!(runs.get(run).unwrap().is_some(), "{run}");
    }
    assert!(runs.has_pending(&oldest).unwrap());
    assert!(runs.has_pending(&newest).unwrap());

    // Once the changes are gone, and another agent's don't count, it goes.
    scratch.ok_in(&scratch.root, &["discard", &oldest]);
    scratch.ok_in(&scratch.root, &["discard", &newest]);
    scratch.ok_in(&scratch.root, &["rm", "notes"]);
    assert_eq!(scratch.ok_in(&scratch.root, &["ls"]), "");
    assert!(runs.has_pending(&other).unwrap());
}

// spec(SHARE): `ap rm` and `ap reset` delete pending changes too, so they refuse while an agent has any, unless you pass `--force`.
#[test]
fn rm_refuses_for_pending_changes_after_the_agent_is_gone() {
    let scratch = Scratch::new("pending-gone");
    let runs = Runs::new(scratch.store());
    let (run, pending) = run_sharing(&scratch, "notes", 1);
    fs::write(pending.join("records"), "DELETE \"data\"\n").unwrap();

    let output = scratch.ap(&["rm", "notes"]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        format!(
            "ap: agent notes has pending changes, so nothing is removed. Apply or discard them first:\n{run}\n"
        )
    );
    assert!(runs.get(&run).unwrap().is_some());
    assert!(runs.has_pending(&run).unwrap());
}

#[test]
fn rm_takes_one_agent() {
    let scratch = Scratch::new("arguments");
    for args in [
        &["rm"][..],
        &["rm", "a", "b"],
        &["rm", "--force", "a"],
        &["rm", "-f", "a"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("ap rm agent\n"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn usage_lists_rm() {
    let scratch = Scratch::new("usage");
    let output = scratch.ap(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("ap rm agent\n"),
        "{}",
        stderr(&output)
    );
}
