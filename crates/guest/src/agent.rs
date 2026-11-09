//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. The
//! init must call it from its own loop, and not reap while it runs, because
//! `waitpid(-1)` would take the status this module is waiting for. Orphans
//! wait until it returns.

use crate::protocol::{MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

const NOT_EXECUTABLE: u8 = 126;
const NOT_FOUND: u8 = 127;
const SIGNALED: u8 = 128;

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;
/// How long to wait for output in total once the program has ended.
const PATIENCE: Duration = Duration::from_millis(100);
/// The most output sent once the program has ended: its two full pipes, what
/// is queued, and what the forwarding threads hold.
const ALLOWANCE: usize = 2 * 64 * 1024 + (QUEUE + 2) * MAX_OUTPUT;

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

    let status = loop {
        match received.recv() {
            Ok(Update::Output(output)) => output.write(channel)?,
            Ok(Update::Ended(status)) => break status?,
            Err(_) => return Err(io::Error::other("the program's waiter stopped").into()),
        }
    };
    drain(&received, channel)?;
    ToHost::Exited(status_of_exit(status)).write(channel)
}

/// Sends what the program left behind, until its pipes close or a background
/// job holding them has had its allowance. Time spent writing doesn't count,
/// so a slow host loses nothing.
fn drain(received: &Receiver<Update>, channel: &mut impl Write) -> Result<(), ProtocolError> {
    let mut patience = PATIENCE;
    let mut allowance = ALLOWANCE;
    while allowance > 0 {
        let started = Instant::now();
        let Ok(Update::Output(output)) = received.recv_timeout(patience) else {
            break;
        };
        patience = patience.saturating_sub(started.elapsed());
        if let ToHost::Stdout(bytes) | ToHost::Stderr(bytes) = &output {
            allowance = allowance.saturating_sub(bytes.len());
        }
        output.write(channel)?;
    }
    Ok(())
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
