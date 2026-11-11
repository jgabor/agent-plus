//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. While
//! a program runs, it reaps every child of the process, so orphans don't stay
//! zombies. A process runs one `serve` at a time, and nothing else in it may
//! wait for a child meanwhile.

use crate::linux::{bytes_waiting, program_has_ended, reap_until};
use crate::protocol::{MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::ffi::OsStr;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

const NOT_EXECUTABLE: u8 = 126;
const NOT_FOUND: u8 = 127;
const SIGNALED: u8 = 128;

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;

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
    wait(child.id(), updates, ended_writer);

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

/// Sends what the program left behind, until the forwarding threads finish.
fn drain(received: &Receiver<Update>, channel: &mut impl Write) -> Result<(), ProtocolError> {
    while let Ok(Update::Output(output)) = received.recv() {
        output.write(channel)?;
    }
    Ok(())
}

/// Sends a pipe's output until it closes, or until the program has ended and
/// what it left has been sent. A background job can hold the pipe open or keep
/// writing to it, so only what is queued when the end is seen gets sent. That
/// includes everything the program wrote, however late this thread runs.
///
/// This thread is the pipe's only reader, so none of its reads can block.
fn forward(
    mut pipe: impl Read + AsFd + Send + 'static,
    wrap: fn(Vec<u8>) -> ToHost,
    updates: SyncSender<Update>,
    ended: PipeReader,
) {
    thread::spawn(move || {
        let mut buffer = vec![0; MAX_OUTPUT];
        loop {
            match program_has_ended(pipe.as_fd(), ended.as_fd()) {
                Ok(false) if send_some(&mut pipe, &mut buffer, wrap, &updates).is_some() => {}
                Ok(true) => break,
                _ => return,
            }
        }
        let Ok(mut left) = bytes_waiting(pipe.as_fd()) else {
            return;
        };
        while left > 0 {
            let wanted = left.min(MAX_OUTPUT);
            match send_some(&mut pipe, &mut buffer[..wanted], wrap, &updates) {
                Some(count) => left -= count,
                None => return,
            }
        }
    });
}

/// Reads once and sends what arrived. Returns how much that was, or nothing
/// if the pipe closed or nobody is listening any more.
fn send_some(
    pipe: &mut impl Read,
    buffer: &mut [u8],
    wrap: fn(Vec<u8>) -> ToHost,
    updates: &SyncSender<Update>,
) -> Option<usize> {
    loop {
        match pipe.read(buffer) {
            Ok(0) => return None,
            Ok(count) => {
                let output = wrap(buffer[..count].to_vec());
                return updates.send(Update::Output(output)).ok().map(|()| count);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Reports the program's end, and closes `ended` to tell the forwarding
/// threads.
fn wait(pid: u32, updates: SyncSender<Update>, ended: PipeWriter) {
    thread::spawn(move || {
        let status = reap_until(pid);
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
    use std::ffi::c_int;
    use std::io::pipe;
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::time::Duration;

    const F_SETPIPE_SZ: c_int = 1031;

    unsafe extern "C" {
        fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    }

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

        assert_eq!(delivered(&received), written.len());
    }

    /// Splicing lets a pipe hold far more than its nominal size, so a program
    /// can exit with a megabyte still queued. A background job may hold the
    /// pipe open as well, and neither may cost the program its output.
    #[test]
    fn everything_queued_when_the_program_ends_is_delivered() {
        const QUEUED: usize = 400 * 1024;
        let (reader, mut writer) = pipe().unwrap();
        // SAFETY: this call takes no pointers, and the descriptor is open.
        let size = unsafe { fcntl(writer.as_raw_fd(), F_SETPIPE_SZ, 2 * QUEUED as c_int) };
        assert!(size >= QUEUED as c_int, "the pipe could not be enlarged");
        writer.write_all(&vec![b'o'; QUEUED]).unwrap();

        let (updates, received) = sync_channel(QUEUE);
        let (ended, program_ended) = pipe().unwrap();
        drop(program_ended);
        forward(reader, ToHost::Stdout, updates, ended);

        assert_eq!(delivered(&received), QUEUED);
        drop(writer);
    }

    fn delivered(received: &Receiver<Update>) -> usize {
        let mut channel = Vec::new();
        drain(received, &mut channel).unwrap();
        let mut total = 0;
        let mut frames = channel.as_slice();
        while let Some(report) = ToHost::read(&mut frames).unwrap() {
            let ToHost::Stdout(bytes) = report else {
                panic!("only output was sent");
            };
            total += bytes.len();
        }
        total
    }
}
