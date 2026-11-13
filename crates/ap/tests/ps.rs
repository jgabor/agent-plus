//! `ap ps`, from SPEC.md's `ap ps` section. Nothing here boots: a test makes
//! a run active the way `ap run` does, by holding the lock on its record, and
//! gives it a socket in the runtime directory. The boots are in `t1`.

use std::fs::{self, File};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use store::Runs;

/// A second, in the nanoseconds a run's record keeps.
const SECOND: u128 = 1_000_000_000;

/// A test's own home, and the runtime directory `ap` uses without
/// `XDG_RUNTIME_DIR`.
struct Scratch {
    home: PathBuf,
    store: PathBuf,
    runtime: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ps").join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let store = home.join(".local/share/ap");
        let runtime = store.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        Scratch {
            home,
            store,
            runtime,
        }
    }

    /// Runs `ap` with only `HOME` in its environment.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Records a run of `agent`, and gives it a socket. The run is active
    /// for as long as the returned file is open.
    fn run(&self, agent: &str, started: u128) -> (String, File, UnixListener) {
        let (run, record) = Runs::new(&self.store).create(agent, started).unwrap();
        record.lock().unwrap();
        let dir = File::open(&self.runtime).unwrap();
        let socket = ap::runs::short_path(&dir, &ap::runs::socket_name(&run.name));
        (run.name, record, UnixListener::bind(socket).unwrap())
    }
}

// spec(ap ps): It exits 0, even when no run is active.
#[test]
fn with_no_active_run_ap_ps_prints_its_header_and_exits_0() {
    let scratch = Scratch::new("none");
    // Nothing has made the runtime directory yet.
    fs::remove_dir(&scratch.runtime).unwrap();
    for _ in 0..2 {
        let output = scratch.ap(&["ps"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(output.stdout, b"NAME  AGENT  STARTED\n", "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        fs::create_dir_all(&scratch.runtime).unwrap();
    }
}

// spec(ap ps): `ap ps` lists the active runs.
// spec(ap ps): It prints a header row, then one row per run, newest first, with the run's name, its agent, and when it started.
#[test]
fn ap_ps_lists_the_active_runs_newest_first() {
    let scratch = Scratch::new("active");
    let (older, _older_record, _older_socket) = scratch.run("notes", 1_794_567_729 * SECOND);
    let (newer, _newer_record, _newer_socket) =
        scratch.run("review-bot", 1_794_567_790 * SECOND + 5);
    // A run whose `ap run` is gone, though its socket is still here.
    let (ended, ended_record, _ended_socket) = scratch.run("notes", 1_794_567_800 * SECOND);
    drop(ended_record);
    // An active run with no socket, and a file that isn't a run's.
    let (_, _hidden_record) = Runs::new(&scratch.store)
        .create("notes", 1_794_567_810 * SECOND)
        .unwrap();
    fs::write(scratch.runtime.join("notes.txt"), "").unwrap();

    let output = scratch.ap(&["ps"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        format!(
            "NAME      AGENT       STARTED\n\
             {newer}  review-bot  2026-11-13T11:03:10Z\n\
             {older}  notes       2026-11-13T11:02:09Z\n"
        ),
        "{ended} is listed"
    );
}

#[test]
fn ap_ps_a_is_named_as_not_built_and_other_arguments_print_usage() {
    let scratch = Scratch::new("arguments");
    let output = scratch.ap(&["ps", "-a"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("`ap ps -a` is not built yet"),
        "{output:?}"
    );
    for args in [&["ps", "-x"][..], &["ps", "notes"], &["ps", "-a", "-a"]] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ap ps"),
            "{output:?}"
        );
    }
}
