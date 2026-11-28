//! `ap rm`, from SPEC.md's `ap rm` and Build sections: it removes an agent
//! from the store, and frees the layers no other agent uses. The approval
//! stays, because it belongs to the Agentfile's path.
//!
//! It refuses while a run of the agent has pending changes, as the SHARE
//! section says. An agent has no persisted state or kept repositories yet,
//! so there's nothing else to remove.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

    /// Starts `ap` as [`Scratch::ap`] runs it, with its output piped.
    fn spawn(&self, args: &[&str]) -> Started {
        let child = Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Started(Some(child))
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

/// An `ap` the test started, killed if the test ends before it does.
struct Started(Option<Child>);

impl Started {
    fn id(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }

    fn exited(&mut self) -> bool {
        self.0.as_mut().unwrap().try_wait().unwrap().is_some()
    }

    fn output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Waits until `done` is true, and fails the test after a minute.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what} took over a minute");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether the process `pid` waits for a lock on a file, as `/proc/locks`
/// lists a waiter: `1: -> FLOCK  ADVISORY  WRITE <pid> ...`.
fn waits_for_a_lock(pid: u32) -> bool {
    let locks = fs::read_to_string("/proc/locks").unwrap();
    let pid = pid.to_string();
    locks.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(1) == Some(&"->") && fields.get(5) == Some(&pid.as_str())
    })
}

fn mkfifo(path: &Path) {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `name` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
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
const SIMPLE_GRANTS: &str = "AGENT claude\nNETWORK -\nPOLICY -\n";

/// An Agentfile that asks for a host variable too, and its grants.
const NOTES: &str = "AGENT claude\nOS minimal\nENV NAME $USER\nNETWORK -\nPOLICY -\n";
const NOTES_GRANTS: &str = "AGENT claude\nVARIABLE USER\nNETWORK -\nPOLICY -\n";

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
    // `ap rm` freed the stand-in for the OS layer, so put it back, and the
    // rebuild downloads nothing. With no terminal to ask on, the build goes
    // ahead only because the grants are still approved.
    common::seed_os(&scratch.store());
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

// spec(Build): `ap rm` and rebuilds free a layer only when no agent or active run still needs it, and no repository an agent kept still depends on it (see [REPO](#repo)).
#[test]
fn rm_waits_for_a_starting_run_to_open_its_layers() {
    let scratch = Scratch::new("starting");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let digests = scratch.digests("notes");
    // The run stops at its first layer, whose uncompressed copy is a FIFO
    // that opens once the test opens it to write. A second link keeps it
    // within the test's reach after `ap rm` frees the layer.
    let plain = scratch.store().join("plain-layers");
    fs::create_dir_all(&plain).unwrap();
    mkfifo(&plain.join(&digests[0]));
    let gate = scratch.root.join("gate");
    fs::hard_link(plain.join(&digests[0]), &gate).unwrap();
    let runs = Runs::new(scratch.store());
    let mut run = scratch.spawn(&["run", "notes", "-p", "hi"]);
    wait_until("the run's start", || {
        let listed = runs.list().unwrap();
        run.exited() || listed.iter().any(|run| runs.active(&run.name).unwrap())
    });

    let mut rm = scratch.spawn(&["rm", "notes"]);
    wait_until("ap rm", || rm.exited() || waits_for_a_lock(rm.id()));
    // Opened once the run waits at the layer, or never if it ended first.
    let mut opened = None;
    wait_until("the run's first layer", || {
        let mut options = OpenOptions::new();
        options.write(true).custom_flags(libc::O_NONBLOCK);
        match options.open(&gate) {
            Ok(file) => opened = Some(file),
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {}
            Err(error) => panic!("{error}"),
        }
        opened.is_some() || run.exited()
    });
    drop(opened);

    let run = run.output();
    let rm = rm.output();
    assert_eq!(rm.status.code(), Some(0), "{}", stderr(&rm));
    // The run read every layer, so it got as far as its boot, which a
    // dynamically linked `ap` refuses.
    assert_eq!(run.status.code(), Some(125), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("dynamically linked"),
        "{}",
        stderr(&run)
    );
    // `ap rm` frees the layers once the run has them open.
    assert_eq!(scratch.ok_in(&scratch.root, &["ls"]), "");
    for digest in &digests {
        assert!(!scratch.has_layer(digest), "{digest}");
    }
}

#[test]
fn a_run_whose_agent_is_removed_while_it_starts_fails_with_no_record() {
    let scratch = Scratch::new("removed-while-starting");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    // The test stands in for an `ap rm` that holds the lock on the store's
    // agents, which writers take.
    let lock = File::open(scratch.store().join("agents/.lock")).unwrap();
    lock.lock().unwrap();
    let mut run = scratch.spawn(&["run", "notes", "-p", "hi"]);
    wait_until("the run's start", || {
        run.exited() || waits_for_a_lock(run.id())
    });
    fs::remove_file(scratch.store().join("agents/notes")).unwrap();
    drop(lock);

    let run = run.output();
    assert_eq!(run.status.code(), Some(125), "{}", stderr(&run));
    assert_eq!(
        stderr(&run),
        "ap: agent notes was removed or rebuilt while the run started\n"
    );
    let runs = Runs::new(scratch.store());
    assert_eq!(runs.list().unwrap(), Vec::new());
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

// spec(SHARE): `ap rm` and `ap reset` delete pending changes too, so they refuse while an agent has any, unless you pass `--force`.
#[test]
fn rm_keeps_a_run_that_gains_pending_changes_while_it_removes_the_agent() {
    let scratch = Scratch::new("pending-meanwhile");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let runs = Runs::new(scratch.store());
    let (run, pending) = run_sharing(&scratch, "notes", 1);
    runs.create_log(&run).unwrap();
    let held = runs.hold(&run).unwrap();
    // The test holds the lock on the store's agents, so `ap rm` waits for it
    // after it found no pending changes, and before it removes the runs.
    let lock = File::open(scratch.store().join("agents/.lock")).unwrap();
    lock.lock().unwrap();
    let mut rm = scratch.spawn(&["rm", "notes"]);
    wait_until("ap rm", || rm.exited() || waits_for_a_lock(rm.id()));

    // Meanwhile the run changes a file, and finishes.
    fs::write(pending.join("records"), "DELETE \"data\"\n").unwrap();
    drop(held);
    drop(lock);

    let rm = rm.output();
    assert_eq!(rm.status.code(), Some(0), "{}", stderr(&rm));
    assert_eq!(scratch.ok_in(&scratch.root, &["ls"]), "");
    assert!(runs.get(&run).unwrap().is_some());
    assert!(runs.log(&run).unwrap().is_some());
    assert!(runs.has_pending(&run).unwrap());
}

// spec(SHARE): `ap rm` and `ap reset` delete pending changes too, so they refuse while an agent has any, unless you pass `--force`.
// spec(ap rm): `ap rm --force` keeps an active run's pending changes, as it keeps the run's log.
#[test]
fn rm_force_deletes_the_pending_changes_of_the_agent_s_finished_runs() {
    let scratch = Scratch::new("force");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let runs = Runs::new(scratch.store());
    let (finished, finished_pending) = run_sharing(&scratch, "notes", 1);
    fs::write(finished_pending.join("records"), "DELETE \"data\"\n").unwrap();
    runs.create_log(&finished).unwrap();
    let (active, active_pending) = run_sharing(&scratch, "notes", 2);
    fs::write(active_pending.join("records"), "DELETE \"data\"\n").unwrap();
    runs.create_log(&active).unwrap();
    let held = runs.hold(&active).unwrap();
    let (other, other_pending) = run_sharing(&scratch, "other", 3);
    fs::write(other_pending.join("records"), "DELETE \"data\"\n").unwrap();

    let output = scratch.ap(&["rm", "--force", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    assert_eq!(scratch.ok_in(&scratch.root, &["ls"]), "");
    assert_eq!(runs.get(&finished).unwrap(), None);
    assert!(runs.log(&finished).unwrap().is_none());
    assert!(!runs.has_pending(&finished).unwrap());
    assert!(!runs.list_pending().unwrap().contains(&finished));
    // The active run's share server may still be writing, so its changes
    // and its log stay, and so do another agent's.
    assert!(runs.has_pending(&active).unwrap());
    assert!(runs.log(&active).unwrap().is_some());
    assert!(runs.has_pending(&other).unwrap());

    // Once the run ends, `ap rm --force` deletes its changes too.
    drop(held);
    scratch.ok_in(&scratch.root, &["rm", "--force", "notes"]);
    assert_eq!(runs.get(&active).unwrap(), None);
    assert!(!runs.list_pending().unwrap().contains(&active));
    assert!(runs.has_pending(&other).unwrap());
}

// spec(SHARE): `ap rm` and `ap reset` delete pending changes too, so they refuse while an agent has any, unless you pass `--force`.
#[test]
fn a_run_stays_active_while_its_share_server_answers_a_request() {
    let scratch = Scratch::new("server-answers");
    let dir = scratch.project("notes", SIMPLE, "notes");
    scratch.build(&dir, SIMPLE_GRANTS);
    let runs = Runs::new(scratch.store());
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    let pending = runs.create_pending_share(&run, &dir).unwrap();
    runs.create_log(&run).unwrap();
    // The test is `ap run` and its run process: it locks the record, and
    // starts the share server with a copy, as `ap run` does.
    let record = runs.lock(&run).unwrap();
    let root = File::open(&dir).unwrap();
    let kept = File::open(&pending).unwrap();
    let roots = [share::Root::new(
        root.as_fd(),
        share::Mode::CopyOnWrite(kept.as_fd()),
    )];
    let ap = Path::new(env!("CARGO_BIN_EXE_ap"));
    let share::Servers { mut started, .. } =
        share::start(ap, &roots, &[], Some(record.as_fd())).unwrap();
    let share::Started { process, socket } = started.remove(0);
    let mut process = Killed(process);
    drop((root, kept));
    // The server holds still while the guest's request to make a directory
    // waits in its socket, and the run process exits.
    let pid = process.0.id();
    // SAFETY: this call takes plain integers, and the process is the
    // server this test started.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
    wait_until("the share server to stop", || {
        stopped(pid) || process.0.try_wait().unwrap().is_some()
    });
    assert!(stopped(pid), "the share server exited");
    let mut socket = File::from(socket);
    socket.write_all(&mkdir_frame(b"made")).unwrap();
    drop(socket);
    drop(record);

    assert!(runs.active(&run).unwrap());
    scratch.ok_in(&scratch.root, &["rm", "notes"]);
    assert_eq!(runs.pending_shares(&run).unwrap().len(), 1);

    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGCONT) }, 0);
    // Its answer finds the run process gone.
    process.0.wait().unwrap();
    assert!(!runs.active(&run).unwrap());
    assert!(runs.has_pending(&run).unwrap());
    assert!(pending.join("tree/made").is_dir());
    let output = scratch.ap(&["rm", "notes"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[test]
fn only_the_server_that_keeps_pending_changes_holds_the_record() {
    let scratch = Scratch::new("server-keeps");
    let runs = Runs::new(scratch.store());
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    let pending = runs.create_pending_share(&run, &scratch.root).unwrap();
    let record = runs.lock(&run).unwrap();
    let root = File::open(&scratch.root).unwrap();
    let kept = File::open(&pending).unwrap();
    let roots = [
        share::Root::new(root.as_fd(), share::Mode::ReadWrite),
        share::Root::new(root.as_fd(), share::Mode::CopyOnWrite(kept.as_fd())),
    ];
    let ap = Path::new(env!("CARGO_BIN_EXE_ap"));
    let servers = share::start(ap, &roots, &[], Some(record.as_fd())).unwrap();
    drop((root, kept, record));
    let mut held = Vec::new();
    for server in servers.started {
        let pid = server.process.id();
        let process = Killed(server.process);
        // `start` can return before the process is the server, with the
        // arguments it starts with. The server answers an empty frame with
        // one once it runs, and it has its own arguments by then.
        let mut socket = File::from(server.socket);
        socket.write_all(&[0; 8]).unwrap();
        let mut answer = [1; 4];
        socket.read_exact(&mut answer).unwrap();
        assert_eq!(answer, [0; 4]);
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        held.push((args[1].to_vec(), args.contains(&&b"keep"[..])));
        drop(socket);
        drop(process);
    }

    // The `rw` share's server changes modes and times, so it holds nothing
    // but its shares.
    assert_eq!(held, [(b"rw".to_vec(), false), (b"keep".to_vec(), true)]);
}

/// A process that is killed if the test fails before it ends.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether the process `pid` is stopped, as `/proc/<pid>/stat` says.
fn stopped(pid: u32) -> bool {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(") ")
        .is_some_and(|(_, state)| state.starts_with('T'))
}

/// A frame for the share server's socket: share 0's FUSE request to make
/// the directory `name` at the share's root.
fn mkdir_frame(name: &[u8]) -> Vec<u8> {
    const MKDIR: u32 = 9;
    const ROOT: u64 = 1;
    let mut body = 0o755_u32.to_le_bytes().to_vec();
    body.extend(0o022_u32.to_le_bytes());
    body.extend(name);
    body.push(0);
    let mut request = ((40 + body.len()) as u32).to_le_bytes().to_vec();
    request.extend(MKDIR.to_le_bytes());
    request.extend(2_u64.to_le_bytes());
    request.extend(ROOT.to_le_bytes());
    request.extend([0; 16]);
    request.extend(body);
    let mut frame = (request.len() as u32).to_le_bytes().to_vec();
    frame.extend(0_u32.to_le_bytes());
    frame.extend(request);
    frame
}

#[test]
fn rm_takes_one_agent() {
    let scratch = Scratch::new("arguments");
    for args in [
        &["rm"][..],
        &["rm", "a", "b"],
        &["rm", "--force"],
        &["rm", "--force", "a", "b"],
        &["rm", "--force", "--force", "a"],
        &["rm", "-f", "a"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("ap rm [--force] agent\n"),
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
        stderr(&output).contains("ap rm [--force] agent\n"),
        "{}",
        stderr(&output)
    );
}
