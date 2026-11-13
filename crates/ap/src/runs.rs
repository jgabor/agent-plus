//! A run's processes, and the socket `ap run` talks to them over.
//!
//! `ap run` records the run in the store, and starts two processes, each in a
//! process group of its own, so that a key the terminal turns into a signal
//! reaches only `ap run`:
//!
//! - The cleanup process removes the run's socket once `ap run` and the run
//!   process have both exited. Each holds the write end of the pipe it reads,
//!   so it reads the end of the pipe only then. It starts before the socket
//!   exists, so no way out of `ap run` leaves the socket behind.
//! - The run process holds the VM, and is confined as `ap boot` is. Its
//!   parent's death kills it, so the VM stops when `ap run` exits, for any
//!   reason. It locks the run's record for as long as it lives, which makes
//!   the run active. Once confined it can only write, so it can't remove its
//!   socket itself.
//!
//! Between the two, `ap run` binds the run's socket, `<name>.sock`, in the
//! runtime directory, and keeps it until it exits. It talks to the run
//! process over a socket pair of its own.
//!
//! `ap run` sends the layers and how to start the harness. For a harness in
//! a terminal, it then sends what you type and the terminal's new sizes. The
//! run process sends the harness's output, unfiltered, then how the run
//! ended. Each message is a tag byte, a body length as a little-endian
//! `u32`, and the body.
//!
//! Each `ap exec` connects to the run's socket, sends the command, then its
//! input, and reads the command's output and status, as `ap run` reads the
//! harness's. The run process is confined, so it can't take a connection of
//! its own: [`Execs`] in `ap run` takes each one, and passes its messages on
//! to the run process with the id it gave it. Each command has a window
//! both ways, so one whose `ap exec` doesn't keep up holds back only itself.

use std::ffi::{OsStr, OsString};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use guest::protocol::{Exec, MAX_OUTPUT, Size, Start, ToGuest, ToHost};

use crate::boot::{self, EXEC_WINDOW, Error, Input, Layer, Report};

/// The `argv[0]` that starts `ap` as a run process.
pub const RUN_PROCESS: &str = "ap-run-process";
/// The `argv[0]` that starts `ap` as a run's cleanup process.
pub const CLEANUP: &str = "ap-run-cleanup";

/// How long a run may last. A headless run ends with its harness, so this
/// only stops one that never does.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// The largest body either side reads.
const MAX_BODY: usize = 1 << 20;

const LAYER: u8 = 1;
const START: u8 = 2;
const STDOUT: u8 = 3;
const STDERR: u8 = 4;
const EXITED: u8 = 5;
const FAILED: u8 = 6;
const INPUT: u8 = 7;
const RESIZE: u8 = 8;
/// An `ap exec`'s command.
const EXEC: u8 = 9;
const END_OF_INPUT: u8 = 10;
/// A message for or from the `ap exec` whose id leads the body.
const FOR_EXEC: u8 = 11;
/// `ap exec` has written one more message of its command's output.
const TAKEN: u8 = 12;
/// `ap exec` is gone.
const HUNG_UP: u8 = 13;
/// The guest has taken one more message for the command.
const TOOK: u8 = 14;

/// The status `ap exec` exits with when `ap` fails.
const FAILED_STATUS: u8 = 125;

/// The directory that holds the sockets of active runs: `ap` in
/// `XDG_RUNTIME_DIR`, or, if that isn't an absolute path, `runtime` in the
/// store.
pub fn runtime_dir(xdg_runtime_dir: Option<&OsStr>, store_root: &Path) -> PathBuf {
    match xdg_runtime_dir.map(Path::new) {
        Some(dir) if dir.is_absolute() => dir.join("ap"),
        _ => store_root.join("runtime"),
    }
}

/// The name of the socket of the run with this name.
pub fn socket_name(run: &str) -> String {
    format!("{run}.sock")
}

/// A path to `name` in the open directory `dir` that is short whatever the
/// directory's own path is, because a socket's path must fit in 108 bytes.
pub fn short_path(dir: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{name}", dir.as_raw_fd()))
}

/// One of the layers `ap run` sends: its digest, and the instruction that
/// made it, which names it in an error.
pub struct LayerName {
    pub digest: String,
    pub instruction: String,
}

/// How a run ended, as the run process reports it.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// The harness exited with this status.
    Exited(u8),
    /// `ap` failed, and exits with this status.
    Failed(u8, String),
}

/// What `ap run` reads from the run process, and `ap exec` from `ap run`.
#[derive(Debug, PartialEq, Eq)]
pub enum FromRun {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    End(End),
    /// A message for the `ap exec` with this id: its tag and its body.
    ForExec(u32, u8, Vec<u8>),
}

/// Sends the run process what to run.
pub fn send_request(stream: &mut impl Write, layers: &[LayerName], start: Start) -> io::Result<()> {
    let mut message = Vec::new();
    for layer in layers {
        let body = format!("{} {}", layer.digest, layer.instruction);
        push_frame(&mut message, LAYER, body.as_bytes());
    }
    let mut start_frame = Vec::new();
    ToGuest::Start(start)
        .write(&mut start_frame)
        .map_err(|error| io::Error::other(format!("can't send the run: {error:?}")))?;
    push_frame(&mut message, START, &start_frame);
    stream.write_all(&message)
}

/// Sends the run process what you typed, for the harness's terminal.
pub fn send_input(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let mut message = Vec::new();
    push_frame(&mut message, INPUT, bytes);
    stream.write_all(&message)
}

/// Sends the run process the new size of your terminal.
pub fn send_resize(stream: &mut impl Write, size: Size) -> io::Result<()> {
    let mut body = size.rows.to_le_bytes().to_vec();
    body.extend_from_slice(&size.columns.to_le_bytes());
    let mut message = Vec::new();
    push_frame(&mut message, RESIZE, &body);
    stream.write_all(&message)
}

/// Sends `ap run` the command `ap exec` runs. What you type, and your
/// terminal's sizes, follow as [`send_input`] and [`send_resize`] send them
/// to the run process.
pub fn send_exec(stream: &mut impl Write, exec: Exec) -> io::Result<()> {
    let mut body = Vec::new();
    ToGuest::Exec(exec)
        .write(&mut body)
        .map_err(|error| io::Error::other(format!("can't send the command: {error:?}")))?;
    let mut message = Vec::new();
    push_frame(&mut message, EXEC, &body);
    stream.write_all(&message)
}

/// Sends `ap run` the end of the command's input.
pub fn send_end_of_input(stream: &mut impl Write) -> io::Result<()> {
    let mut message = Vec::new();
    push_frame(&mut message, END_OF_INPUT, &[]);
    stream.write_all(&message)
}

/// What `ap exec` sends `ap run`.
#[derive(Debug, PartialEq, Eq)]
pub enum FromExec {
    Exec(Exec),
    Input(Vec<u8>),
    EndOfInput,
    Resize(Size),
}

/// Reads what `ap exec` sends next. The end of the stream is `None`.
pub fn read_from_exec(stream: &mut impl Read) -> io::Result<Option<FromExec>> {
    let Some((tag, body)) = read_frame(stream)? else {
        return Ok(None);
    };
    Ok(Some(match (tag, body.as_slice()) {
        (EXEC, _) => match ToGuest::read(&mut body.as_slice()) {
            Ok(Some(ToGuest::Exec(exec))) => FromExec::Exec(exec),
            _ => return Err(bad_message()),
        },
        // What doesn't fit in one message to the guest is refused here.
        (INPUT, _) if body.len() <= MAX_OUTPUT => FromExec::Input(body),
        (END_OF_INPUT, []) => FromExec::EndOfInput,
        (RESIZE, [a, b, c, d]) => FromExec::Resize(Size {
            rows: u16::from_le_bytes([*a, *b]),
            columns: u16::from_le_bytes([*c, *d]),
        }),
        _ => return Err(bad_message()),
    }))
}

/// Sends `ap exec` what its command did.
pub fn send_to_exec(stream: &mut impl Write, message: &FromRun) -> io::Result<()> {
    let mut frame = Vec::new();
    match message {
        FromRun::Stdout(bytes) => push_frame(&mut frame, STDOUT, bytes),
        FromRun::Stderr(bytes) => push_frame(&mut frame, STDERR, bytes),
        FromRun::End(End::Exited(status)) => push_frame(&mut frame, EXITED, &[*status]),
        FromRun::End(End::Failed(status, reason)) => {
            push_frame(&mut frame, FAILED, &[&[*status], reason.as_bytes()].concat());
        }
        FromRun::ForExec(..) => return Err(bad_message()),
    }
    stream.write_all(&frame)
}

/// Reads what the run process sends next. The end of the stream before the
/// run ended is `None`.
pub fn read_from_run(stream: &mut impl Read) -> io::Result<Option<FromRun>> {
    let Some((tag, body)) = read_frame(stream)? else {
        return Ok(None);
    };
    Ok(Some(match (tag, body.as_slice()) {
        (STDOUT, _) => FromRun::Stdout(body),
        (STDERR, _) => FromRun::Stderr(body),
        (EXITED, [status]) => FromRun::End(End::Exited(*status)),
        (FAILED, [status, message @ ..]) => FromRun::End(End::Failed(
            *status,
            String::from_utf8_lossy(message).into_owned(),
        )),
        (FOR_EXEC, [a, b, c, d, tag, rest @ ..]) => {
            FromRun::ForExec(u32::from_le_bytes([*a, *b, *c, *d]), *tag, rest.to_vec())
        }
        _ => return Err(bad_message()),
    }))
}

/// A frame for or from the `ap exec` with this id.
fn push_exec_frame(message: &mut Vec<u8>, id: u32, tag: u8, body: &[u8]) {
    push_frame(message, FOR_EXEC, &[&id.to_le_bytes()[..], &[tag], body].concat());
}

/// The `ap exec`s of a run, which `ap run` takes from the run's socket and
/// relays to the run process.
pub struct Execs {
    /// The stream to the run process, which `ap run` writes to as well.
    to_run: Arc<Mutex<UnixStream>>,
    clients: Mutex<BTreeMap<u32, Client>>,
    next: AtomicU32,
}

/// One `ap exec`.
struct Client {
    /// Output for the thread that writes to `ap exec`.
    output: mpsc::Sender<FromRun>,
    window: Arc<Window>,
}

/// How many of an `ap exec`'s messages the guest hasn't taken.
#[derive(Default)]
struct Window {
    unsent: Mutex<(usize, bool)>,
    room: Condvar,
}

impl Window {
    fn lock(&self) -> MutexGuard<'_, (usize, bool)> {
        self.unsent.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for room for one more message. False once the command ended.
    fn take(&self) -> bool {
        let mut unsent = self.lock();
        while unsent.0 >= EXEC_WINDOW && !unsent.1 {
            unsent = self.room.wait(unsent).unwrap_or_else(PoisonError::into_inner);
        }
        unsent.0 += 1;
        !unsent.1
    }

    fn give(&self) {
        let mut unsent = self.lock();
        unsent.0 = unsent.0.saturating_sub(1);
        self.room.notify_all();
    }

    fn close(&self) {
        self.lock().1 = true;
        self.room.notify_all();
    }
}

impl Execs {
    /// Takes each `ap exec` that connects to `listener`, on a thread of its
    /// own, for as long as this process lives.
    pub fn serve(listener: UnixListener, to_run: Arc<Mutex<UnixStream>>) -> Arc<Execs> {
        let execs = Arc::new(Execs {
            to_run,
            clients: Mutex::new(BTreeMap::new()),
            next: AtomicU32::new(1),
        });
        let serving = Arc::clone(&execs);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let execs = Arc::clone(&serving);
                thread::spawn(move || execs.take(stream));
            }
        });
        execs
    }

    /// Hands on what the run process sent for the `ap exec` with this id.
    /// What arrives for one that is gone is dropped.
    pub fn deliver(&self, id: u32, tag: u8, body: Vec<u8>) {
        let mut clients = self.clients();
        let Some(client) = clients.get(&id) else {
            return;
        };
        let message = match (tag, body.as_slice()) {
            (TOOK, []) => return client.window.give(),
            (STDOUT, _) => FromRun::Stdout(body),
            (STDERR, _) => FromRun::Stderr(body),
            (EXITED, [status]) => FromRun::End(End::Exited(*status)),
            (FAILED, [status, reason @ ..]) => FromRun::End(End::Failed(
                *status,
                String::from_utf8_lossy(reason).into_owned(),
            )),
            _ => FromRun::End(End::Failed(
                FAILED_STATUS,
                bad_message().to_string(),
            )),
        };
        let ended = matches!(message, FromRun::End(_));
        let _ = client.output.send(message);
        if ended && let Some(client) = clients.remove(&id) {
            client.window.close();
        }
    }

    fn clients(&self) -> MutexGuard<'_, BTreeMap<u32, Client>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends the run process a message for the command with this id.
    fn send(&self, id: u32, tag: u8, body: &[u8]) -> io::Result<()> {
        let mut message = Vec::new();
        push_exec_frame(&mut message, id, tag, body);
        let mut to_run = self.to_run.lock().unwrap_or_else(PoisonError::into_inner);
        to_run.write_all(&message)
    }

    /// Relays one `ap exec`: its command and input to the run process, and
    /// what the command does back. If the run is gone, `ap exec` reads the
    /// end of the stream.
    fn take(self: Arc<Execs>, stream: UnixStream) {
        let mut reader = &stream;
        let Ok(Some(FromExec::Exec(exec))) = read_from_exec(&mut reader) else {
            return;
        };
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let window = Arc::new(Window::default());
        let (output, written) = mpsc::channel();
        self.clients().insert(
            id,
            Client {
                output,
                window: Arc::clone(&window),
            },
        );
        let mut request = Vec::new();
        let sent = ToGuest::Exec(exec)
            .write(&mut request)
            .map_err(|_| bad_message())
            .and_then(|()| {
                window.take();
                self.send(id, EXEC, &request)
            });
        if sent.is_err() {
            self.clients().remove(&id);
            return;
        }
        let execs = Arc::clone(&self);
        thread::spawn(move || execs.write_to(id, writer, &written));
        loop {
            let (tag, body) = match read_from_exec(&mut reader) {
                Ok(Some(FromExec::Input(bytes))) => (INPUT, bytes),
                Ok(Some(FromExec::EndOfInput)) => (END_OF_INPUT, Vec::new()),
                Ok(Some(FromExec::Resize(size))) => {
                    let mut body = size.rows.to_le_bytes().to_vec();
                    body.extend_from_slice(&size.columns.to_le_bytes());
                    (RESIZE, body)
                }
                _ => break,
            };
            if !window.take() || self.send(id, tag, &body).is_err() {
                return;
            }
        }
        let _ = self.send(id, HUNG_UP, &[]);
    }

    /// Writes what the command does to its `ap exec`, and tells the run
    /// process as each message is written, until the command ends.
    fn write_to(&self, id: u32, mut stream: UnixStream, written: &Receiver<FromRun>) {
        for message in written {
            if send_to_exec(&mut stream, &message).is_err() {
                break;
            }
            match message {
                FromRun::End(_) => break,
                _ => {
                    if self.send(id, TAKEN, &[]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// The run process: `ap` started as [`RUN_PROCESS`] by the `ap run` whose
/// process id is the first argument, for the run named by the second, with
/// its end of a socket pair with `ap run` as its standard input. It returns
/// the status to exit with, which nobody reads, because it reports how the
/// run ended over the socket.
pub fn run_process(args: &[OsString]) -> u8 {
    let parent = args
        .first()
        .and_then(|arg| arg.to_str()?.parse::<libc::pid_t>().ok());
    // SAFETY: both calls take and return plain integers.
    let orphaned = unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0
            || Some(libc::getppid()) != parent
    };
    if args.len() != 2 || orphaned {
        // `ap run` exited before its death could stop this process.
        return 125;
    }
    // SAFETY: `ap run` passes its socket as standard input, and nothing else
    // in this process uses it.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(0) });
    // Confined, this process can `write` but not `send`, which a socket's
    // own writes use, so it writes to `ap run` as to a file.
    let Ok(mut output) = stream
        .try_clone()
        .map(|clone| File::from(OwnedFd::from(clone)))
    else {
        return 125;
    };
    // The lock on the run's record lasts until this process exits.
    let mut record = None;
    let end = match serve(&args[1].to_string_lossy(), &stream, &output, &mut record) {
        Ok(status) => End::Exited(status),
        Err(error) => End::Failed(error.exit_code(), error.to_string()),
    };
    let mut message = Vec::new();
    match &end {
        End::Exited(status) => push_frame(&mut message, EXITED, &[*status]),
        End::Failed(status, reason) => {
            let mut body = vec![*status];
            body.extend_from_slice(reason.as_bytes());
            push_frame(&mut message, FAILED, &body);
        }
    }
    // A write fails only if `ap run` is gone, and then nobody is left to tell.
    let _ = output.write_all(&message);
    drop(record);
    match end {
        End::Exited(status) => status,
        End::Failed(status, _) => status,
    }
}

/// Locks the record of the run named `name` into `record`, reads the
/// request from `stream`, and boots the agent with its output sent to
/// `output`.
fn serve(
    name: &str,
    stream: &UnixStream,
    output: &File,
    record: &mut Option<File>,
) -> Result<u8, Error> {
    let root = crate::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .ok_or_else(|| {
        Error::Failed("HOME isn't an absolute path, so there's no place for the store".to_owned())
    })?;
    *record = Some(
        ::store::Runs::new(&root)
            .hold(name)
            .map_err(|error| Error::Failed(format!("can't lock the run's record: {error}")))?,
    );
    let lost = |error: io::Error| Error::Failed(format!("can't read the run from ap run: {error}"));
    let mut reader = stream;
    let mut names = Vec::new();
    let start = loop {
        match read_frame(&mut reader).map_err(lost)? {
            Some((LAYER, body)) => {
                let body = String::from_utf8(body).map_err(|_| lost(bad_message()))?;
                let (digest, instruction) =
                    body.split_once(' ').ok_or_else(|| lost(bad_message()))?;
                names.push(LayerName {
                    digest: digest.to_owned(),
                    instruction: instruction.to_owned(),
                });
            }
            Some((START, body)) => match ToGuest::read(&mut body.as_slice()) {
                Ok(Some(ToGuest::Start(start))) => break start,
                _ => return Err(lost(bad_message())),
            },
            _ => return Err(lost(bad_message())),
        }
    };

    let store = ::store::Layers::new(&root);
    let layers = names
        .into_iter()
        .map(|name| {
            let disk = store.get(&name.digest).map_err(|error| {
                Error::Failed(format!(
                    "can't read the layer for {}: {error}",
                    name.instruction
                ))
            })?;
            Ok(Layer {
                digest: name.digest,
                disk,
            })
        })
        .collect::<Result<_, Error>>()?;

    // Confined, this process can `read` but not `recv`, so it reads `ap run`
    // as a file too.
    let source = stream
        .try_clone()
        .map(|clone| File::from(OwnedFd::from(clone)))
        .map_err(lost)?;
    let input = Arc::new(Input::new(source.as_raw_fd()));
    pass_input(source, Arc::clone(&input), start.terminal.is_some());
    let mut output = output
        .try_clone()
        .map_err(|error| Error::Failed(format!("can't send the run's output: {error}")))?;
    boot::boot_agent(TIMEOUT, layers, start, Some(&input), move |report| {
        let mut message = Vec::new();
        match report {
            Report::Ready(_) => return,
            Report::Stdout(bytes) => push_frame(&mut message, STDOUT, bytes),
            Report::Stderr(bytes) => push_frame(&mut message, STDERR, bytes),
            Report::Exec(id, ToHost::Stdout(bytes)) => {
                push_exec_frame(&mut message, id, STDOUT, bytes);
            }
            Report::Exec(id, ToHost::Stderr(bytes)) => {
                push_exec_frame(&mut message, id, STDERR, bytes);
            }
            Report::Exec(id, ToHost::Exited(status)) => {
                push_exec_frame(&mut message, id, EXITED, &[*status]);
            }
            Report::Exec(id, ToHost::Failed(reason)) => {
                let body = [&[FAILED_STATUS], &boot::shown(reason).into_bytes()[..]].concat();
                push_exec_frame(&mut message, id, FAILED, &body);
            }
            Report::Exec(_, ToHost::Ready(_)) => return,
            Report::ExecTook(id) => push_exec_frame(&mut message, id, TOOK, &[]),
        }
        // If `ap run` is gone, so is this process, any moment now.
        let _ = output.write_all(&message);
    })
}

/// Passes what `ap run` sends from `source` on to the guest, until it sends
/// something else or is gone: keys and resizes for a harness in a
/// `terminal`, and what each `ap exec` sends. `ap run` exits when the run
/// ends, so this thread ends with this process.
fn pass_input(mut source: File, input: Arc<Input>, terminal: bool) {
    thread::spawn(move || {
        while let Ok(Some((tag, body))) = read_frame(&mut source) {
            let passed = match (tag, body.as_slice()) {
                (INPUT, _) if terminal => input.send(&ToGuest::Input(body)),
                (RESIZE, [a, b, c, d]) if terminal => input.send(&ToGuest::Resize(Size {
                    rows: u16::from_le_bytes([*a, *b]),
                    columns: u16::from_le_bytes([*c, *d]),
                })),
                (FOR_EXEC, [a, b, c, d, tag, rest @ ..]) => {
                    pass_to_exec(&input, u32::from_le_bytes([*a, *b, *c, *d]), *tag, rest)
                }
                _ => return,
            };
            if passed.is_err() {
                return;
            }
        }
    });
}

/// Passes on a message from the `ap exec` with this id.
fn pass_to_exec(input: &Input, id: u32, tag: u8, body: &[u8]) -> Result<(), Error> {
    let message = match (tag, body) {
        (EXEC, _) => match ToGuest::read(&mut &body[..]) {
            Ok(Some(exec @ ToGuest::Exec(_))) => exec,
            _ => return Err(Error::Failed(bad_message().to_string())),
        },
        (INPUT, _) => ToGuest::Input(body.to_vec()),
        (END_OF_INPUT, []) => ToGuest::EndOfInput,
        (RESIZE, [a, b, c, d]) => ToGuest::Resize(Size {
            rows: u16::from_le_bytes([*a, *b]),
            columns: u16::from_le_bytes([*c, *d]),
        }),
        (TAKEN, []) => {
            input.exec_taken(id);
            return Ok(());
        }
        (HUNG_UP, []) => {
            input.exec_hung_up(id);
            return Ok(());
        }
        _ => return Err(Error::Failed(bad_message().to_string())),
    };
    input.send_exec(id, &message)
}

/// The cleanup process: `ap` started as [`CLEANUP`] with the run's socket as
/// its one argument, and a pipe as its standard input. Once nothing can
/// write to the pipe, it removes the socket.
pub fn cleanup(args: &[OsString]) -> u8 {
    let [socket] = args else {
        return 2;
    };
    // What arrives doesn't matter, only that nothing more can.
    let _ = io::copy(&mut io::stdin(), &mut io::sink());
    match std::fs::remove_file(socket) {
        Ok(()) => 0,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(_) => 1,
    }
}

fn push_frame(message: &mut Vec<u8>, tag: u8, body: &[u8]) {
    message.push(tag);
    message.extend_from_slice(&(body.len() as u32).to_le_bytes());
    message.extend_from_slice(body);
}

/// Reads one message. The end of the stream before a message starts is
/// `None`, and anywhere else an error.
fn read_frame(stream: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0];
    if stream.read(&mut tag)? == 0 {
        return Ok(None);
    }
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_BODY {
        return Err(bad_message());
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(Some((tag[0], body)))
}

fn bad_message() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "a message from the run is malformed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_directory_is_xdg_s_or_the_store_s() {
        let store = Path::new("/home/me/.local/share/ap");
        assert_eq!(
            runtime_dir(Some(OsStr::new("/run/user/1000")), store),
            Path::new("/run/user/1000/ap")
        );
        for unset in [None, Some(OsStr::new("")), Some(OsStr::new("run"))] {
            assert_eq!(
                runtime_dir(unset, store),
                Path::new("/home/me/.local/share/ap/runtime")
            );
        }
    }

    #[test]
    fn output_and_the_end_read_back() {
        let mut stream = Vec::new();
        push_frame(&mut stream, STDOUT, b"out");
        push_frame(&mut stream, STDERR, b"");
        push_frame(&mut stream, FAILED, b"\x7cthe guest didn't finish");
        push_frame(&mut stream, EXITED, &[3]);
        let mut reader = stream.as_slice();
        let mut read = || read_from_run(&mut reader).unwrap();
        assert_eq!(read(), Some(FromRun::Stdout(b"out".to_vec())));
        assert_eq!(read(), Some(FromRun::Stderr(Vec::new())));
        assert_eq!(
            read(),
            Some(FromRun::End(End::Failed(
                124,
                "the guest didn't finish".to_owned()
            )))
        );
        assert_eq!(read(), Some(FromRun::End(End::Exited(3))));
        assert_eq!(read(), None);
    }

    #[test]
    fn a_cut_short_or_unknown_or_oversized_message_is_an_error() {
        let mut whole = Vec::new();
        push_frame(&mut whole, STDOUT, b"out");
        let mut unknown = Vec::new();
        push_frame(&mut unknown, 99, b"");
        let mut exited_without_status = Vec::new();
        push_frame(&mut exited_without_status, EXITED, b"");
        let mut oversized = vec![STDOUT];
        oversized.extend_from_slice(&(MAX_BODY as u32 + 1).to_le_bytes());
        for bad in [
            &whole[..whole.len() - 1],
            &whole[..3],
            &unknown,
            &exited_without_status,
            &oversized,
        ] {
            assert!(read_from_run(&mut &bad[..]).is_err(), "{bad:?}");
        }
    }
}
