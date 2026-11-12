//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. While
//! a program runs, it reaps every child of the process, so orphans don't stay
//! zombies. A process runs one `serve` at a time, and nothing else in it may
//! wait for a child meanwhile.
//!
//! A [`Start`] runs the harness as the user `agent`, whom [`add_account`]
//! adds to the guest's root first, with the environment and directory the
//! host gives it. Its prompt is its last argument: the host's, or else the
//! one the agent was built with.
//!
//! The init doesn't change user itself. std spawns a program that changes
//! user through a Unix socket, and the guest kernel has none. So the init
//! starts its own program as [`AS_AGENT`], which becomes `agent` in
//! [`exec_as_agent`] and then execs the harness.

use crate::linux::{bytes_waiting, program_has_ended, reap_until};
use crate::protocol::{MAX_OUTPUT, ProtocolError, Start, ToGuest, ToHost};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, chown};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

const NOT_EXECUTABLE: u8 = 126;
const NOT_FOUND: u8 = 127;
const SIGNALED: u8 = 128;

/// The agent's home directory.
pub const HOME: &str = "/home/agent";
/// The agent's user and group id. They are fixed, so no layer chooses them.
const ID: u32 = 1000;
const PASSWD: &str = "agent:x:1000:1000:agent:/home/agent:/bin/sh";
const GROUP: &str = "agent:x:1000:";
/// Where the build puts the agent's prompt.
const PROMPT: &str = "/etc/ap/prompt";
/// The name the init's own program runs under to start the harness.
pub const AS_AGENT: &str = "ap-as-agent";
/// The init's own program, which exec can reach after the root moves.
const SELF: &str = "/proc/self/exe";

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;

/// Serves requests until the host hangs up.
///
/// A refused frame ends the service. The caller powers off, and a program
/// still running goes with the machine.
pub fn serve(channel: &mut (impl Read + Write)) -> Result<(), ProtocolError> {
    while let Some(request) = ToGuest::read(channel)? {
        match request {
            ToGuest::Run { argv } => {
                let (program, args) = argv.split_first().ok_or(ProtocolError::Malformed)?;
                let mut command = Command::new(OsStr::from_bytes(program));
                command.args(args.iter().map(|arg| OsStr::from_bytes(arg)));
                run(command, channel, |error| {
                    ToHost::Exited(status_of_failure(error))
                })?;
            }
            ToGuest::Start(start) => match harness(&start) {
                Ok(command) => run(command, channel, |error| {
                    ToHost::Failed(format!("can't start the harness: {error}").into_bytes())
                })?,
                Err(reason) => ToHost::Failed(reason.into_bytes()).write(channel)?,
            },
        }
    }
    Ok(())
}

/// The harness's command, as `agent`, or why it can't start.
fn harness(start: &Start) -> Result<Command, String> {
    add_account(Path::new("/")).map_err(|error| format!("can't add the user agent: {error}"))?;
    let directory = OsStr::from_bytes(&start.directory);
    if !fs::metadata(directory).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "the working directory {} isn't a directory",
            directory.to_string_lossy()
        ));
    }
    let prompt = match &start.prompt {
        Some(prompt) => prompt.clone(),
        None => match fs::read(PROMPT) {
            Ok(prompt) => prompt,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("can't read the prompt: {error}")),
        },
    };
    if prompt.is_empty() {
        return Err("the agent has no prompt, and interactive runs are not built yet".to_owned());
    }
    let program = OsStr::from_bytes(start.argv.first().ok_or("the harness has no program")?);
    match fs::metadata(program) {
        Ok(metadata) if metadata.is_file() && metadata.mode() & 0o111 != 0 => {}
        _ => {
            return Err(format!(
                "the harness {} isn't an executable file",
                program.to_string_lossy()
            ));
        }
    }
    let mut command = Command::new(SELF);
    command
        .arg0(AS_AGENT)
        .args(start.argv.iter().map(|arg| OsStr::from_bytes(arg)))
        .arg(OsStr::from_bytes(&prompt))
        .env_clear()
        .envs(
            (start.env.iter())
                .map(|(key, value)| (OsStr::from_bytes(key), OsStr::from_bytes(value))),
        )
        .current_dir(directory);
    Ok(command)
}

/// Becomes `agent`, with no other groups, and execs `argv`. Returns only if
/// that fails.
pub fn exec_as_agent(argv: &[OsString]) -> io::Error {
    let Some((program, args)) = argv.split_first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "no program to run");
    };
    // As root, std drops every supplementary group before it changes user.
    Command::new(program).args(args).uid(ID).gid(ID).exec()
}

/// Adds the user `agent` to the root at `root`, unless it has one, and gives
/// it its home directory. Only the directory itself changes owner, because
/// changing a file in a layer copies it to the writable layer.
pub fn add_account(root: &Path) -> io::Result<()> {
    let etc = root.join("etc");
    fs::create_dir_all(&etc)?;
    for (file, line) in [("passwd", PASSWD), ("group", GROUP)] {
        let path = etc.join(file);
        let text = match fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        if let Some(text) = with_agent(&text, line) {
            fs::write(&path, text)?;
        }
    }
    let home = root.join(HOME.trim_start_matches('/'));
    fs::create_dir_all(&home)?;
    // A symlink here would hand its target to the agent.
    if !fs::symlink_metadata(&home)?.is_dir() {
        return Err(io::Error::other(format!("{HOME} isn't a directory")));
    }
    chown(&home, Some(ID), Some(ID))
}

/// `passwd` or `group` with `line` added, or `None` if it already names
/// `agent`.
pub fn with_agent(text: &[u8], line: &str) -> Option<Vec<u8>> {
    if text
        .split(|byte| *byte == b'\n')
        .any(|entry| entry.starts_with(b"agent:"))
    {
        return None;
    }
    let mut text = text.to_vec();
    if !text.is_empty() && !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    text.extend_from_slice(line.as_bytes());
    text.push(b'\n');
    Some(text)
}

enum Update {
    Output(ToHost),
    Ended(io::Result<ExitStatus>),
}

/// Runs `command`, and streams back its output and status. If it can't be
/// spawned, `refused` says what to report instead.
fn run(
    mut command: Command,
    channel: &mut impl Write,
    refused: fn(&io::Error) -> ToHost,
) -> Result<(), ProtocolError> {
    // Both ends close on exec, so the program can't hold this open.
    let (ended, ended_writer) = io::pipe()?;
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return refused(&error).write(channel),
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
        while left != 0 {
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
    fn the_agent_is_added_once_after_every_other_entry() {
        let line = "agent:x:1000:1000:agent:/home/agent:/bin/sh";
        assert_eq!(
            with_agent(b"", line).unwrap(),
            format!("{line}\n").as_bytes()
        );
        assert_eq!(
            with_agent(b"root:x:0:0:root:/root:/bin/sh", line).unwrap(),
            format!("root:x:0:0:root:/root:/bin/sh\n{line}\n").as_bytes()
        );
        assert_eq!(
            with_agent(b"root:x:0:\n", "agent:x:1000:").unwrap(),
            b"root:x:0:\nagent:x:1000:\n"
        );
        assert_eq!(with_agent(b"root:x:0:\nagent:x:7:\n", line), None);
        // Another user whose name starts with `agent` is someone else.
        assert!(with_agent(b"agents:x:7:\n", line).is_some());
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

    /// Fails each read with the next of these errors, then reads one byte.
    struct Failing(Vec<io::ErrorKind>);

    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(kind) = self.0.pop() {
                return Err(kind.into());
            }
            buffer[0] = b'o';
            Ok(1)
        }
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let (updates, received) = sync_channel(QUEUE);
        let mut pipe = Failing(vec![io::ErrorKind::Interrupted]);
        let sent = send_some(&mut pipe, &mut [0; 8], ToHost::Stdout, &updates);
        drop(updates);
        assert_eq!(sent, Some(1));
        assert_eq!(delivered(&received), 1);
    }

    #[test]
    fn a_failed_read_ends_the_output() {
        let (updates, received) = sync_channel(QUEUE);
        let mut pipe = Failing(vec![io::ErrorKind::Other]);
        let sent = send_some(&mut pipe, &mut [0; 8], ToHost::Stdout, &updates);
        drop(updates);
        assert_eq!(sent, None);
        assert_eq!(delivered(&received), 0);
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
