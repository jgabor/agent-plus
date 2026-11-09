//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. The
//! init must call it from its own loop, and not reap while it runs, because
//! `waitpid(-1)` would take the status this module is waiting for. Orphans
//! wait until it returns.

use crate::linux::wait_for_output;
use crate::protocol::{MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::ffi::OsStr;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

const NOT_EXECUTABLE: u8 = 126;
const NOT_FOUND: u8 = 127;
const SIGNALED: u8 = 128;

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;
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
    // Both ends close on exec, so the program can't hold this open.
    let (ended, ended_writer) = io::pipe()?;
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
        forward(stdout, ToHost::Stdout, updates.clone(), ended.try_clone()?);
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, ToHost::Stderr, updates.clone(), ended.try_clone()?);
    }
    drop(ended);
    wait(child, updates, ended_writer);

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

/// Sends what the program left behind, until the forwarding threads have
/// finished or a background job flooding the pipes has had its allowance.
fn drain(received: &Receiver<Update>, channel: &mut impl Write) -> Result<(), ProtocolError> {
    let mut allowance = ALLOWANCE;
    while allowance > 0 {
        let Ok(Update::Output(output)) = received.recv() else {
            break;
        };
        if let ToHost::Stdout(bytes) | ToHost::Stderr(bytes) = &output {
            allowance = allowance.saturating_sub(bytes.len());
        }
        output.write(channel)?;
    }
    Ok(())
}

/// Sends a pipe's output until it closes, or until the program has ended and
/// the pipe is empty. A background job can hold the pipe open, but everything
/// the program wrote is in the pipe by then, however late this thread runs.
fn forward(
    mut pipe: impl Read + AsFd + Send + 'static,
    wrap: fn(Vec<u8>) -> ToHost,
    updates: SyncSender<Update>,
    ended: PipeReader,
) {
    thread::spawn(move || {
        let mut buffer = vec![0; MAX_OUTPUT];
        // This thread is the pipe's only reader, so the read can't block.
        while let Ok(true) = wait_for_output(pipe.as_fd(), ended.as_fd()) {
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

/// Reports the program's end, and closes `ended` to tell the forwarding
/// threads.
fn wait(mut child: Child, updates: SyncSender<Update>, ended: PipeWriter) {
    thread::spawn(move || {
        let status = child.wait();
        drop(ended);
        let _ = updates.send(Update::Ended(status));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::pipe;
    use std::os::fd::BorrowedFd;
    use std::time::Duration;

    /// A forwarding thread that the scheduler keeps off the CPU before every
    /// read, for longer than a drain could reasonably wait.
    struct Starved(PipeReader);

    impl Read for Starved {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_millis(150));
            self.0.read(buffer)
        }
    }

    impl AsFd for Starved {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    #[test]
    fn a_delayed_forwarder_still_delivers_a_finished_programs_output() {
        let (reader, mut writer) = pipe().unwrap();
        // One page fits in any pipe, so this write can't block.
        let written = vec![b'o'; 4096];
        writer.write_all(&written).unwrap();
        drop(writer);

        let (updates, received) = sync_channel(QUEUE);
        let (ended, program_ended) = pipe().unwrap();
        drop(program_ended);
        forward(Starved(reader), ToHost::Stdout, updates, ended);

        let mut channel = Vec::new();
        drain(&received, &mut channel).unwrap();

        let mut delivered = Vec::new();
        let mut frames = channel.as_slice();
        while let Some(report) = ToHost::read(&mut frames).unwrap() {
            let ToHost::Stdout(bytes) = report else {
                panic!("only output was sent");
            };
            delivered.extend(bytes);
        }
        assert_eq!(delivered.len(), written.len());
    }
}
