//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. The
//! init must call it from its own loop, and not reap while it runs, because
//! `waitpid(-1)` would take the status this module is waiting for.

use crate::protocol::{MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Duration;

const NOT_EXECUTABLE: u8 = 126;
const NOT_FOUND: u8 = 127;
const SIGNALED: u8 = 128;

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;
/// How long after the program ends the pipes may stay quiet. A background job
/// can hold them open for as long as it runs.
const QUIET: Duration = Duration::from_millis(100);

/// Serves requests until the host hangs up.
///
/// A refused frame ends the service. The caller powers off, and a program
/// still running goes with the machine.
pub fn serve(channel: &mut (impl Read + Write)) -> Result<(), ProtocolError> {
    while let Some(ToGuest::Run { argv }) = ToGuest::read(channel)? {
        run(&argv, channel)?;
    }
    Ok(())
}

enum Update {
    Output(ToHost),
    Ended(io::Result<ExitStatus>),
}

fn run(argv: &[Vec<u8>], channel: &mut impl Write) -> Result<(), ProtocolError> {
    let (program, args) = argv.split_first().ok_or(ProtocolError::Malformed)?;
    let spawned = Command::new(OsStr::from_bytes(program))
        .args(args.iter().map(|arg| OsStr::from_bytes(arg)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return ToHost::Exited(status_of_failure(&error)).write(channel),
    };

    let (updates, received) = sync_channel(QUEUE);
    if let Some(stdout) = child.stdout.take() {
        forward(stdout, ToHost::Stdout, updates.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, ToHost::Stderr, updates.clone());
    }
    wait(child, updates);

    let mut ended = None;
    loop {
        let update = match ended {
            None => received.recv().map_err(RecvTimeoutError::from),
            Some(_) => received.recv_timeout(QUIET),
        };
        match update {
            Ok(Update::Output(output)) => output.write(channel)?,
            Ok(Update::Ended(status)) => ended = Some(status?),
            Err(_) => break,
        }
    }
    let status = ended.ok_or_else(|| io::Error::other("the program's waiter stopped"))?;
    ToHost::Exited(status_of_exit(status)).write(channel)
}

/// Sends a pipe's output until it closes. The thread outlives the program if
/// a background job keeps the pipe open, and ends with the pipe or the
/// machine.
fn forward(
    mut pipe: impl Read + Send + 'static,
    wrap: fn(Vec<u8>) -> ToHost,
    updates: SyncSender<Update>,
) {
    thread::spawn(move || {
        let mut buffer = vec![0; MAX_OUTPUT];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    let output = wrap(buffer[..count].to_vec());
                    if updates.send(Update::Output(output)).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
}

fn wait(mut child: Child, updates: SyncSender<Update>) {
    thread::spawn(move || {
        let _ = updates.send(Update::Ended(child.wait()));
    });
}

fn status_of_failure(error: &io::Error) -> u8 {
    if error.kind() == io::ErrorKind::NotFound {
        NOT_FOUND
    } else {
        NOT_EXECUTABLE
    }
}

fn status_of_exit(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code as u8,
        (None, Some(signal)) => SIGNALED + signal as u8,
        (None, None) => SIGNALED,
    }
}
