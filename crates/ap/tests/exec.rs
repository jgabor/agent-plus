//! `ap exec`, from SPEC.md's `ap exec` section, against a fake run. Nothing
//! here boots: a test makes a run active the way a run process does, by
//! holding the lock on its record, and plays the run's `ap run` on its
//! socket. What the guest does with the command is checked in `t1`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};

use ap::runs::{End, FromExec, FromRun, read_from_exec, send_to_exec};
use guest::linux::{open_terminal, take_terminal};
use guest::protocol::{Exec, Size};
use store::Runs;

/// The status `ap exec` exits with when `ap` fails.
const FAILED: i32 = 125;

/// A test's own home, and the runtime directory `ap` uses without
/// `XDG_RUNTIME_DIR`.
struct Scratch {
    home: PathBuf,
    store: PathBuf,
    runtime: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("exec")
            .join(name);
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ap"));
        command.args(args).env_clear().env("HOME", &self.home);
        command
    }

    /// Runs `ap` with only `HOME` in its environment, and `input` as its
    /// standard input.
    fn ap(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Records a run, and makes it active, with its socket. The run is
    /// active for as long as the returned file is open.
    fn run(&self) -> (String, File, UnixListener) {
        let runs = Runs::new(&self.store);
        let run = runs.create("notes", 1_794_567_729_000_000_000).unwrap();
        let record = runs.hold(&run.name).unwrap();
        let dir = File::open(&self.runtime).unwrap();
        let socket = ap::runs::short_path(&dir, &ap::runs::socket_name(&run.name));
        (run.name, record, UnixListener::bind(socket).unwrap())
    }
}

/// Plays `ap run` for one `ap exec`: takes its connection, and hands it to
/// `serve`, which returns what it saw.
fn fake_run<T: Send + 'static>(
    listener: UnixListener,
    serve: impl FnOnce(UnixStream) -> T + Send + 'static,
) -> JoinHandle<T> {
    thread::spawn(move || serve(listener.accept().unwrap().0))
}

/// Reads the command `ap exec` sends first.
fn command(stream: &mut UnixStream) -> Exec {
    match read_from_exec(stream).unwrap() {
        Some(FromExec::Exec(exec)) => exec,
        other => panic!("{other:?}"),
    }
}

// spec(ap exec): `ap exec <run> <command> [<arg>...]` runs a command inside an active run, and exits with the command's status.
// spec(ap exec): Its standard input, output, and error are those of `ap exec`.
#[test]
fn ap_exec_passes_its_input_and_output_through_and_exits_with_the_command_s_status() {
    let scratch = Scratch::new("status");
    let (name, _record, listener) = scratch.run();
    let run = fake_run(listener, |mut stream| {
        let exec = command(&mut stream);
        let mut input = Vec::new();
        loop {
            match read_from_exec(&mut stream).unwrap() {
                Some(FromExec::Input(bytes)) => input.extend(bytes),
                Some(FromExec::EndOfInput) => break,
                other => panic!("{other:?}"),
            }
        }
        for message in [
            FromRun::Stdout(input),
            FromRun::Stderr(b"to stderr".to_vec()),
            FromRun::End(End::Exited(7)),
        ] {
            send_to_exec(&mut stream, &message).unwrap();
        }
        exec
    });

    let output = scratch.ap(&["exec", &name, "cat", "-", "--", "-n"], b"typed");
    let exec = run.join().unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(output.stdout, b"typed", "{output:?}");
    assert_eq!(output.stderr, b"to stderr", "{output:?}");
    // Every argument after the run is the command's, even one that looks
    // like a flag, and without a terminal on both sides there is none.
    assert_eq!(
        exec,
        Exec {
            argv: ["cat", "-", "--", "-n"]
                .map(|arg| arg.as_bytes().to_vec())
                .to_vec(),
            terminal: None,
        }
    );
}

/// Runs `ap exec` with a terminal of this size as its standard input, and
/// its standard output a terminal as well only if `both`. Returns the
/// command the fake run got.
fn exec_in_terminal(name: &str, size: Size, both: bool) -> Exec {
    let scratch = Scratch::new(name);
    let (run_name, _record, listener) = scratch.run();
    let run = fake_run(listener, |mut stream| {
        let exec = command(&mut stream);
        send_to_exec(&mut stream, &FromRun::End(End::Exited(0))).unwrap();
        exec
    });
    let (controller, terminal) = open_terminal(size).unwrap();
    let mut ap = scratch.command(&["exec", &run_name, "sh"]);
    ap.stdin(terminal.try_clone().unwrap())
        .stderr(Stdio::null())
        .stdout(match both {
            true => Stdio::from(terminal),
            false => Stdio::null(),
        });
    // SAFETY: `take_terminal` only makes system calls.
    unsafe { ap.pre_exec(take_terminal) };
    let mut child = ap.spawn().unwrap();
    drop(ap);
    // The terminal must be read, or a write to it could block.
    thread::spawn(move || {
        let mut controller = controller;
        let mut buffer = [0; 4096];
        while let Ok(1..) = controller.read(&mut buffer) {}
    });
    assert_eq!(child.wait().unwrap().code(), Some(0));
    run.join().unwrap()
}

// spec(ap exec): When both standard input and standard output of `ap exec` are terminals, the command gets a terminal too.
#[test]
fn the_command_gets_a_terminal_of_your_terminal_s_size_only_when_both_ends_are_terminals() {
    let size = Size {
        rows: 33,
        columns: 101,
    };
    assert_eq!(
        exec_in_terminal("terminal", size, true).terminal,
        Some(size)
    );
    assert_eq!(exec_in_terminal("half-terminal", size, false).terminal, None);
}

// spec(ap exec): If the run has finished or doesn't exist, `ap exec` says so on standard error and exits with status 125.
#[test]
fn a_run_that_has_finished_or_does_not_exist_fails_with_125() {
    let scratch = Scratch::new("gone");
    let (finished, record, listener) = scratch.run();
    drop(record);
    drop(listener);
    for (run, says) in [
        (finished.as_str(), format!("run {finished} has finished")),
        ("nosuchrun", "no run is named nosuchrun".to_owned()),
        ("not/a/run", "no run is named not/a/run".to_owned()),
    ] {
        let output = scratch.ap(&["exec", run, "true"], b"");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(stderr.contains(&says), "{stderr}");
    }
}

// spec(ap exec): If the run ends while the command runs, `ap exec` exits with status 125.
#[test]
fn a_run_that_ends_while_the_command_runs_fails_with_125() {
    let scratch = Scratch::new("ended");
    let (name, record, listener) = scratch.run();
    let run = fake_run(listener, move |mut stream| {
        command(&mut stream);
        send_to_exec(&mut stream, &FromRun::Stdout(b"partial".to_vec())).unwrap();
        // The run ends, and its `ap run` with it.
        drop(record);
    });
    let output = scratch.ap(&["exec", &name, "sleep", "100"], b"");
    run.join().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
    assert_eq!(output.stdout, b"partial", "{output:?}");
    assert!(
        stderr.contains(&format!("run {name} ended before the command did")),
        "{stderr}"
    );
}

#[test]
fn a_command_the_guest_refused_fails_with_the_status_it_gave() {
    let scratch = Scratch::new("refused");
    let (name, _record, listener) = scratch.run();
    let run = fake_run(listener, |mut stream| {
        command(&mut stream);
        let refused = End::Failed(125, "the guest closed the command's connection".to_owned());
        send_to_exec(&mut stream, &FromRun::End(refused)).unwrap();
    });
    let output = scratch.ap(&["exec", &name, "true"], b"");
    run.join().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
    assert!(
        stderr.contains("ap: the guest closed the command's connection"),
        "{stderr}"
    );
}

#[test]
fn ap_exec_without_a_command_prints_usage() {
    let scratch = Scratch::new("usage");
    for args in [&["exec"][..], &["exec", "k3x9q2mf"]] {
        let output = scratch.ap(args, b"");
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ap exec run command"),
            "{output:?}"
        );
    }
}
