//! Runs what the host asks for, and streams back what happens.
//!
//! [`serve`] waits for each program before it reads the next request. While
//! a program runs, [`linux::spawn`] reaps every child of the process, so
//! orphans don't stay zombies, and nothing else in it may wait for a child
//! meanwhile.
//!
//! A [`Start`] runs the harness as the user `agent`, whom [`add_account`]
//! adds to the guest's root first, with the environment and directory the
//! host gives it. Its prompt is its last argument: the host's, or else the
//! one the agent was built with.
//!
//! A start with a terminal runs the harness in a new one, which it gets as
//! its controlling terminal, and owns. What the terminal shows goes to the
//! host as standard output. What the host types, and the terminal's new
//! sizes, come in on another thread, which is then the channel's only
//! reader. A harness with pipes has an empty input, and that thread only
//! waits for a [`ToGuest::Terminate`], which either kind takes as a request
//! to send it `SIGTERM`. Once the harness ends, nothing more is served.
//!
//! Once the harness has started, the agent keeps one connection of its own
//! open to the host's [`EXEC_PORT`](crate::protocol::EXEC_PORT), and waits there for an [`Exec`]. When
//! one arrives, it opens the next connection, and runs the command on this
//! one, beside the harness, as `agent` with the harness's environment and
//! directory. Each command has its own connection, so one that doesn't read
//! its input, or whose output isn't read, holds back only itself.
//!
//! The init doesn't change user itself. std spawns a program that changes
//! user through a Unix socket, and the guest kernel has none. So the init
//! starts its own program as [`AS_AGENT`], which becomes `agent` in
//! [`exec_as_agent`] and then execs the harness, or as [`EXEC_AS_AGENT`]
//! for a command beside it.

use crate::linux::{self, bytes_waiting, program_has_ended, readable_within};
use crate::protocol::{Exec, MAX_OUTPUT, ProtocolError, Size, Start, ToGuest, ToHost};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, IsTerminal, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, chown, fchown};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Arc;
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
/// The name the init's own program runs under to start a command beside the
/// harness.
pub const EXEC_AS_AGENT: &str = "ap-exec-as-agent";
/// The init's own program, which exec can reach after the root moves.
const SELF: &str = "/proc/self/exe";

/// Output waiting to be sent. The bound stops a program from filling the
/// guest's memory faster than the host reads.
const QUEUE: usize = 4;
/// How long a terminal must stay quiet, once its program has ended, before
/// what it shows is taken as complete, in milliseconds.
const QUIET: i32 = 50;
/// The most a terminal is read once its program has ended, so a background
/// job that keeps writing can't hold back the status. It counts bytes, not
/// time, so a host that reads slowly loses nothing.
const LAST_OUTPUT: usize = 1 << 20;

/// What is left to read once a program has ended.
#[derive(Clone, Copy)]
enum Left {
    /// A pipe counts what it holds.
    Counted,
    /// A terminal holds more than it counts, so it is read until it goes
    /// quiet.
    UntilQuiet,
}

/// A channel whose reading can move to another thread, which a harness in a
/// terminal needs.
pub trait Split: Read + Write {
    /// A reader of everything the host sends from now on. Nothing else
    /// reads the channel afterwards.
    fn split_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Split for File {
    fn split_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.try_clone()?))
    }
}

/// Opens a new connection to the host's
/// [`EXEC_PORT`](crate::protocol::EXEC_PORT).
pub type Connect = fn() -> io::Result<File>;

/// Serves requests until the host hangs up, or until a harness has ended. With `connect`, a harness that starts can have
/// commands run beside it.
///
/// A refused frame ends the service. The caller powers off, and a program
/// still running goes with the machine.
pub fn serve(channel: &mut impl Split, connect: Option<Connect>) -> Result<(), ProtocolError> {
    while let Some(request) = ToGuest::read(channel)? {
        match request {
            ToGuest::Run { argv } => {
                let (program, args) = argv.split_first().ok_or(ProtocolError::Malformed)?;
                let mut command = Command::new(OsStr::from_bytes(program));
                command.args(args.iter().map(|arg| OsStr::from_bytes(arg)));
                run(command, channel, FromHost::Nothing, exec_refused)?;
            }
            ToGuest::Start(start) => {
                let command = harness(&start);
                if let (Ok(_), Some(connect)) = (&command, connect) {
                    let context = Context {
                        env: start.env.clone(),
                        directory: start.directory.clone(),
                    };
                    offer_exec(Arc::new(context), connect);
                }
                match (command, start.terminal) {
                    (Ok(command), None) => {
                        let from_host = FromHost::Stop(channel.split_reader()?);
                        return run(command, channel, from_host, harness_refused);
                    }
                    (Ok(command), Some(size)) => {
                        return run_in_terminal(command, size, channel, harness_refused);
                    }
                    (Err(reason), _) => ToHost::Failed(reason.into_bytes()).write(channel)?,
                }
            }
            // A command beside the harness comes on its own connection, and
            // only a program that runs takes the rest.
            ToGuest::Exec(_)
            | ToGuest::Input(_)
            | ToGuest::EndOfInput
            | ToGuest::Resize(_)
            | ToGuest::Terminate => {
                return Err(ProtocolError::Malformed);
            }
        }
    }
    Ok(())
}

/// What a command beside the harness shares with it.
struct Context {
    env: Vec<(Vec<u8>, Vec<u8>)>,
    directory: Vec<u8>,
}

/// Opens a connection to the host, and runs the first command it asks for
/// there, after opening the next connection. A connection that can't open
/// or that ends first takes no command, and offers no more.
fn offer_exec(context: Arc<Context>, connect: Connect) {
    thread::spawn(move || {
        let Ok(mut channel) = connect() else {
            return;
        };
        let Ok(Some(ToGuest::Exec(exec))) = ToGuest::read(&mut channel) else {
            return;
        };
        offer_exec(Arc::clone(&context), connect);
        // A host that hung up has nobody left to tell.
        let _ = run_exec(&exec, &context, &mut channel);
    });
}

/// Runs `exec` as `agent`, with `context`'s environment and directory, and
/// streams back what it does.
fn run_exec(exec: &Exec, context: &Context, channel: &mut File) -> Result<(), ProtocolError> {
    let mut command = Command::new(SELF);
    command
        .arg0(EXEC_AS_AGENT)
        .args(exec.argv.iter().map(|arg| OsStr::from_bytes(arg)))
        .env_clear()
        .envs(
            (context.env.iter())
                .map(|(key, value)| (OsStr::from_bytes(key), OsStr::from_bytes(value))),
        )
        .current_dir(OsStr::from_bytes(&context.directory));
    match exec.terminal {
        None => {
            let input = channel.split_reader()?;
            run(command, channel, FromHost::Input(input), exec_refused)
        }
        Some(size) => run_in_terminal(command, size, channel, exec_refused),
    }
}

fn exec_refused(error: &io::Error) -> ToHost {
    ToHost::Exited(status_of_failure(error))
}

fn harness_refused(error: &io::Error) -> ToHost {
    ToHost::Failed(format!("can't start the harness: {error}").into_bytes())
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
    if prompt.is_empty() && start.terminal.is_none() {
        return Err("a run without a terminal needs a prompt".to_owned());
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
        .args((!prompt.is_empty()).then(|| OsStr::from_bytes(&prompt)))
        .env_clear()
        .envs(
            (start.env.iter())
                .map(|(key, value)| (OsStr::from_bytes(key), OsStr::from_bytes(value))),
        )
        .current_dir(directory);
    Ok(command)
}

/// Becomes `agent`, with no other groups, and execs `argv`. A terminal on
/// standard input becomes the controlling terminal of a new session, and the
/// agent's, as a login's does. Returns only if that fails.
pub fn exec_as_agent(argv: &[OsString]) -> io::Error {
    let Some((program, args)) = argv.split_first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "no program to run");
    };
    let stdin = io::stdin();
    if stdin.is_terminal()
        && let Err(error) =
            linux::take_terminal().and_then(|()| fchown(stdin.as_fd(), Some(ID), Some(ID)))
    {
        return error;
    }
    // As root, std drops every supplementary group before it changes user.
    Command::new(program).args(args).uid(ID).gid(ID).exec()
}

/// Adds the user `agent` to the root at `root`, unless it has one, and gives
/// it its home directory and the directories directly in it, such as the
/// harness configuration directories its adapter makes, so the harness can
/// keep its state there. Files don't change owner, because changing a file in
/// a layer copies it to the writable layer.
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
    chown(&home, Some(ID), Some(ID))?;
    for entry in fs::read_dir(&home)? {
        let entry = entry?;
        // `file_type` doesn't follow a symlink, which would hand its target
        // to the agent.
        if entry.file_type()?.is_dir() {
            chown(entry.path(), Some(ID), Some(ID))?;
        }
    }
    Ok(())
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

/// What the host sends while a program with pipes runs.
enum FromHost {
    /// Nothing, and the program's input is empty.
    Nothing,
    /// The program's input, until the host ends it.
    Input(Box<dyn Read + Send>),
    /// Only a request to stop, and the program's input is empty.
    Stop(Box<dyn Read + Send>),
}

/// Runs `command`, and streams back its output and status. Its input is
/// what the host sends `from_host`. If it can't be spawned, `refused` says
/// what to report instead.
fn run(
    mut command: Command,
    channel: &mut impl Write,
    from_host: FromHost,
    refused: fn(&io::Error) -> ToHost,
) -> Result<(), ProtocolError> {
    // Both ends close on exec, so the program can't hold this open.
    let (ended, ended_writer) = io::pipe()?;
    command
        .stdin(match from_host {
            FromHost::Input(_) => Stdio::piped(),
            _ => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, status) = match linux::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(error) => return refused(&error).write(channel),
    };
    match from_host {
        FromHost::Nothing => {}
        FromHost::Input(input) => feed(input, child.stdin.take(), child.id()),
        FromHost::Stop(input) => feed(input, None, child.id()),
    }

    let (updates, received) = sync_channel(QUEUE);
    if let Some(stdout) = child.stdout.take() {
        let ended = ended.try_clone()?;
        forward(
            stdout,
            ToHost::Stdout,
            Left::Counted,
            updates.clone(),
            ended,
        );
    }
    if let Some(stderr) = child.stderr.take() {
        let ended = ended.try_clone()?;
        forward(
            stderr,
            ToHost::Stderr,
            Left::Counted,
            updates.clone(),
            ended,
        );
    }
    drop(ended);
    wait(status, updates, ended_writer);
    report(&received, channel)
}

/// Passes what the host sends to the program's input, if it has one, and
/// sends the program `pid` `SIGTERM` when the host asks, until the host
/// hangs up. Once the program stops reading, the rest of its input is
/// dropped, so the host is never held back.
fn feed(mut from_host: Box<dyn Read + Send>, mut stdin: Option<ChildStdin>, pid: u32) {
    thread::spawn(move || {
        while let Ok(Some(message)) = ToGuest::read(&mut from_host) {
            match message {
                ToGuest::Input(bytes) => {
                    if let Some(open) = &mut stdin
                        && open.write_all(&bytes).is_err()
                    {
                        stdin = None;
                    }
                }
                ToGuest::EndOfInput => stdin = None,
                ToGuest::Terminate => {
                    // A program that has ended has nothing to stop.
                    let _ = linux::terminate(pid);
                }
                _ => return,
            }
        }
    });
}

/// Runs `command` in a new terminal of this size, and streams back what the
/// terminal shows and the status. What the host sends meanwhile reaches the
/// terminal.
fn run_in_terminal(
    mut command: Command,
    size: Size,
    channel: &mut impl Split,
    refused: fn(&io::Error) -> ToHost,
) -> Result<(), ProtocolError> {
    let (controller, terminal) = match linux::open_terminal(size) {
        Ok(pair) => pair,
        Err(error) => {
            let reason = format!("can't open a terminal: {error}");
            return ToHost::Failed(reason.into_bytes()).write(channel);
        }
    };
    let (ended, ended_writer) = io::pipe()?;
    // The harness takes the terminal as its controlling one itself, in
    // `exec_as_agent`. Doing it here, between fork and exec, would make std
    // fork through a Unix socket, which the guest kernel doesn't have.
    command
        .stdin(terminal.try_clone()?)
        .stdout(terminal.try_clone()?)
        .stderr(terminal);
    let spawned = linux::spawn(&mut command);
    // The terminal must close here, so that it ends with the program's last
    // use of it.
    drop(command);
    let (child, status) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => return refused(&error).write(channel),
    };

    let (updates, received) = sync_channel(QUEUE);
    forward(
        controller.try_clone()?,
        ToHost::Stdout,
        Left::UntilQuiet,
        updates.clone(),
        ended,
    );
    type_into(channel.split_reader()?, controller, child.id());
    wait(status, updates, ended_writer);
    report(&received, channel)
}

/// Passes what the host types to the terminal, resizes it, and sends the
/// program `pid` `SIGTERM` when the host asks, until the host sends
/// something else or hangs up. The end of input changes nothing, as a
/// terminal's own input has no end. A write blocks while the program
/// doesn't read, which holds back the host in turn.
fn type_into(mut from_host: impl Read + Send + 'static, mut terminal: File, pid: u32) {
    thread::spawn(move || {
        while let Ok(Some(message)) = ToGuest::read(&mut from_host) {
            let typed = match message {
                ToGuest::Input(bytes) => terminal.write_all(&bytes),
                ToGuest::Resize(size) => linux::resize(terminal.as_fd(), size),
                ToGuest::EndOfInput => Ok(()),
                ToGuest::Terminate => {
                    // A program that has ended has nothing to stop.
                    let _ = linux::terminate(pid);
                    Ok(())
                }
                _ => return,
            };
            if typed.is_err() {
                return;
            }
        }
    });
}

/// Sends the program's output as it arrives, then its status.
fn report(received: &Receiver<Update>, channel: &mut impl Write) -> Result<(), ProtocolError> {
    let status = loop {
        match received.recv() {
            Ok(Update::Output(output)) => output.write(channel)?,
            Ok(Update::Ended(status)) => break status?,
            Err(_) => return Err(io::Error::other("the program's waiter stopped").into()),
        }
    };
    drain(received, channel)?;
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
    left: Left,
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
        if let Left::UntilQuiet = left {
            let mut allowed = LAST_OUTPUT;
            while allowed > 0 && readable_within(pipe.as_fd(), QUIET).unwrap_or(false) {
                match send_some(&mut pipe, &mut buffer, wrap, &updates) {
                    Some(count) => allowed = allowed.saturating_sub(count),
                    None => return,
                }
            }
            return;
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
fn wait(status: Receiver<io::Result<ExitStatus>>, updates: SyncSender<Update>, ended: PipeWriter) {
    thread::spawn(move || {
        let status = status
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("the reaper stopped")));
        drop(ended);
        let _ = updates.send(Update::Ended(status));
    });
}

/// The status a shell gives a program that couldn't run for this reason:
/// 127 if it wasn't found, and 126 otherwise.
pub fn status_of_failure(error: &io::Error) -> u8 {
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

    /// A host that has sent these messages, and records the guest's.
    #[test]
    fn a_program_with_pipes_ends_by_sigterm_when_the_host_asks() {
        let mut sent = Vec::new();
        ToGuest::Terminate.write(&mut sent).unwrap();
        let mut host = Host {
            sent: io::Cursor::new(Vec::new()),
            received: Vec::new(),
        };
        let mut command = Command::new("sleep");
        command.arg("60");
        let stop: Box<dyn Read + Send> = Box::new(io::Cursor::new(sent));
        run(command, &mut host, FromHost::Stop(stop), harness_refused).unwrap();

        let mut wire = host.received.as_slice();
        let mut status = None;
        while let Some(report) = ToHost::read(&mut wire).unwrap() {
            match report {
                ToHost::Exited(code) => status = Some(code),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(status, Some(SIGNALED + 15));
    }

    struct Host {
        sent: io::Cursor<Vec<u8>>,
        received: Vec<u8>,
    }

    impl Read for Host {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.sent.read(buffer)
        }
    }

    impl Write for Host {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.received.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Split for Host {
        fn split_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            let mut rest = Vec::new();
            self.sent.read_to_end(&mut rest)?;
            Ok(Box::new(io::Cursor::new(rest)))
        }
    }

    #[test]
    fn a_program_in_a_terminal_sees_its_size_and_what_the_host_types() {
        let mut sent = Vec::new();
        for message in [
            ToGuest::Resize(Size {
                rows: 31,
                columns: 101,
            }),
            ToGuest::Input(b"hello\n".to_vec()),
        ] {
            message.write(&mut sent).unwrap();
        }
        let mut host = Host {
            sent: io::Cursor::new(sent),
            received: Vec::new(),
        };
        let mut command = Command::new("sh");
        // The resize may land before or after `stty` runs, so `read` waits
        // for the typing, which comes after it.
        command.args([
            "-c",
            "test -t 0 && read line && echo \"got $line\" && stty size",
        ]);
        let size = Size {
            rows: 30,
            columns: 100,
        };
        run_in_terminal(command, size, &mut host, harness_refused).unwrap();

        let mut wire = host.received.as_slice();
        let mut shown = Vec::new();
        let mut status = None;
        while let Some(report) = ToHost::read(&mut wire).unwrap() {
            match report {
                ToHost::Stdout(bytes) => shown.extend(bytes),
                ToHost::Exited(code) => status = Some(code),
                other => panic!("{other:?}"),
            }
        }
        let shown = String::from_utf8_lossy(&shown);
        assert_eq!(status, Some(0), "{shown}");
        assert!(shown.contains("got hello"), "{shown}");
        assert!(shown.contains("31 101"), "{shown}");
    }

    #[test]
    fn a_program_with_input_reads_what_the_host_sends_until_it_ends() {
        let mut sent = Vec::new();
        for message in [
            ToGuest::Input(b"hello, ".to_vec()),
            ToGuest::Input(b"cat".to_vec()),
            ToGuest::EndOfInput,
        ] {
            message.write(&mut sent).unwrap();
        }
        let mut host = Host {
            sent: io::Cursor::new(Vec::new()),
            received: Vec::new(),
        };
        // `cat` exits only once its input ends.
        let input: Box<dyn Read + Send> = Box::new(io::Cursor::new(sent));
        run(
            Command::new("cat"),
            &mut host,
            FromHost::Input(input),
            exec_refused,
        )
        .unwrap();

        let mut wire = host.received.as_slice();
        let mut shown = Vec::new();
        let mut status = None;
        while let Some(report) = ToHost::read(&mut wire).unwrap() {
            match report {
                ToHost::Stdout(bytes) => shown.extend(bytes),
                ToHost::Exited(code) => status = Some(code),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(status, Some(0));
        assert_eq!(shown, b"hello, cat");
    }

    /// A terminal holds what was written in more than one buffer, and
    /// its byte count sees only the first.
    #[test]
    fn everything_a_finished_program_left_in_a_terminal_is_delivered() {
        const WRITTEN: usize = 40_000;
        let size = Size {
            rows: 24,
            columns: 80,
        };
        let (controller, mut terminal) = linux::open_terminal(size).unwrap();
        let writer = thread::spawn(move || terminal.write_all(&[b'x'; WRITTEN]).unwrap());
        thread::sleep(Duration::from_millis(100));

        let (updates, received) = sync_channel(QUEUE);
        let (ended, program_ended) = pipe().unwrap();
        drop(program_ended);
        forward(controller, ToHost::Stdout, Left::UntilQuiet, updates, ended);
        // A host that reads slowly still gets it all.
        thread::sleep(Duration::from_millis(1500));
        assert_eq!(delivered(&received), WRITTEN);
        writer.join().unwrap();
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
        forward(
            Starved(reader),
            ToHost::Stdout,
            Left::Counted,
            updates,
            ended,
        );

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
        forward(reader, ToHost::Stdout, Left::Counted, updates, ended);

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
