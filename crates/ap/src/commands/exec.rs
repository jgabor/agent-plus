//! `ap exec <run> <command> [<arg>...]`, from SPEC.md's `ap exec` section. It
//! connects to the run's socket, which the run's `ap run` serves, as
//! [`ap::runs`] says, and sends the command. It passes your input to the
//! command, writes the command's output to its own, filtered as `ap run`'s
//! is, and exits with the command's status, or with 125 if the run has
//! finished, doesn't exist, or ends first.
//!
//! When both your input and your output are terminals, the command gets a
//! terminal in the guest, and yours is put in raw mode while it runs, as an
//! interactive run's is. Its resizes follow.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use ap::filter::Filter;
use ap::runs::{self, End, FromRun};
use guest::protocol::Exec;
use store::Runs;

use super::run::{RawMode, pass_keys, take_signals, taken_signals, terminal_size};

/// The status `ap exec` exits with when `ap` fails.
const FAILED: u8 = 125;

/// The arguments of `ap exec`.
pub struct Args {
    run: OsString,
    /// The command and its arguments, as given.
    argv: Vec<Vec<u8>>,
}

/// Reads `ap exec`'s arguments: the run, then the command and its arguments,
/// which may look like flags.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let (run, command) = args.split_first()?;
    if command.is_empty() {
        return None;
    }
    Some(Args {
        run: run.clone(),
        argv: command.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
    })
}

/// Runs the command, and returns its status, or `ap`'s own.
pub fn run(args: &Args) -> u8 {
    match exec(args) {
        Ok(status) => status,
        Err(message) => {
            eprintln!("ap: {message}");
            FAILED
        }
    }
}

fn exec(args: &Args) -> Result<u8, String> {
    let name = args.run.to_string_lossy();
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .ok_or("HOME isn't an absolute path, so there's no place for the store")?;
    let runs = Runs::new(&root);
    let gone = || match runs.get(&name) {
        Ok(Some(_)) => format!("run {name} has finished"),
        _ => format!("no run is named {name}"),
    };
    if !store::valid_run_name(&name) || !runs.active(&name).map_err(|error| error.to_string())? {
        return Err(gone());
    }
    let dir = runs::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &root);
    // The run can end at any moment, and take its socket with it.
    let opened = File::open(&dir).map_err(|_| gone())?;
    let socket = runs::short_path(&opened, &runs::socket_name(&name));
    let mut stream = UnixStream::connect(socket).map_err(|_| gone())?;

    let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    let size = terminal.then(|| {
        // Blocked before the size is read and before any thread starts, a
        // resize from here on waits for the thread that takes it, as does a
        // signal that ends `ap exec`.
        // SAFETY: the set is valid, and a null old set is allowed.
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &taken_signals(), std::ptr::null_mut()) };
        terminal_size()
    });
    let exec = Exec {
        argv: args.argv.clone(),
        terminal: size,
    };
    let lost = |error: io::Error| format!("lost run {name}: {error}");
    runs::send_exec(&mut stream, exec).map_err(lost)?;
    let writer = Arc::new(Mutex::new(stream.try_clone().map_err(lost)?));
    let raw = match terminal {
        true => {
            // Raw mode comes first, so a signal taken from here on finds the
            // modes to put back.
            let raw =
                RawMode::start().map_err(|error| format!("can't set up your terminal: {error}"))?;
            pass_keys(Arc::clone(&writer), false);
            take_signals(writer);
            Some(raw)
        }
        false => {
            pass_keys(writer, true);
            None
        }
    };
    let end = relay(&mut stream);
    drop(raw);
    match end {
        Ok(End::Exited(status)) => Ok(status),
        Ok(End::Failed(status, reason)) => {
            eprintln!("ap: {reason}");
            Ok(status)
        }
        Err(_) => Err(format!("run {name} ended before the command did")),
    }
}

/// Writes the command's output to this process's, filtered, until the
/// command ends. Write errors are ignored, as `ap run`'s are.
fn relay(stream: &mut UnixStream) -> io::Result<End> {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    loop {
        match runs::read_from_run(stream)? {
            Some(FromRun::Stdout(bytes)) => {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(&stdout_filter.push(&bytes));
                let _ = stdout.flush();
            }
            Some(FromRun::Stderr(bytes)) => {
                let _ = io::stderr().write_all(&stderr_filter.push(&bytes));
            }
            Some(FromRun::End(end)) => return Ok(end),
            Some(FromRun::ForExec(..)) | None => {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
    }
}
