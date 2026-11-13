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
//! runtime directory, and connects to it, so the first connection the run
//! process accepts is always its own.
//!
//! `ap run` sends the layers and how to start the harness. For a harness in
//! a terminal, it then sends what you type and the terminal's new sizes. The
//! run process sends the harness's output, unfiltered, then how the run
//! ended. Each message is a tag byte, a body length as a little-endian
//! `u32`, and the body.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use guest::protocol::{Size, Start, ToGuest};

use crate::boot::{self, Error, Input, Layer, Report};

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

/// What `ap run` reads from the run process.
#[derive(Debug, PartialEq, Eq)]
pub enum FromRun {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    End(End),
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
        _ => return Err(bad_message()),
    }))
}

/// The run process: `ap` started as [`RUN_PROCESS`] by the `ap run` whose
/// process id is the first argument, for the run named by the second, with
/// the run's listening socket as its standard input. It returns the status
/// to exit with, which nobody reads, because it reports how the run ended
/// over the socket.
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
    // SAFETY: `ap run` passes the listening socket as standard input, and
    // nothing else in this process uses it.
    let listener = UnixListener::from(unsafe { OwnedFd::from_raw_fd(0) });
    let Ok((stream, _)) = listener.accept() else {
        return 125;
    };
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
    // The listener and the lock last until here, so the socket accepts
    // connections for as long as the run is active.
    drop(listener);
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

    let input = match start.terminal {
        // Confined, this process can `read` but not `recv`, so it reads
        // `ap run` as a file too.
        Some(_) => {
            let source = stream
                .try_clone()
                .map(|clone| File::from(OwnedFd::from(clone)))
                .map_err(lost)?;
            let input = Arc::new(Input::new(source.as_raw_fd()));
            pass_input(source, Arc::clone(&input));
            Some(input)
        }
        None => None,
    };
    let mut output = output
        .try_clone()
        .map_err(|error| Error::Failed(format!("can't send the run's output: {error}")))?;
    boot::boot_agent(TIMEOUT, layers, start, input.as_deref(), move |report| {
        let mut message = Vec::new();
        match report {
            Report::Ready(_) => return,
            Report::Stdout(bytes) => push_frame(&mut message, STDOUT, bytes),
            Report::Stderr(bytes) => push_frame(&mut message, STDERR, bytes),
        }
        // If `ap run` is gone, so is this process, any moment now.
        let _ = output.write_all(&message);
    })
}

/// Passes what `ap run` sends from `source` on to the guest, until it sends
/// something else or is gone. `ap run` exits when the run ends, so this
/// thread ends with this process.
fn pass_input(mut source: File, input: Arc<Input>) {
    thread::spawn(move || {
        while let Ok(Some((tag, body))) = read_frame(&mut source) {
            let message = match (tag, body.as_slice()) {
                (INPUT, _) => ToGuest::Input(body),
                (RESIZE, [a, b, c, d]) => ToGuest::Resize(Size {
                    rows: u16::from_le_bytes([*a, *b]),
                    columns: u16::from_le_bytes([*c, *d]),
                }),
                _ => return,
            };
            if input.send(&message).is_err() {
                return;
            }
        }
    });
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
