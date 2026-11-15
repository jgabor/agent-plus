//! The share server: a process of its own for each run with a share, which
//! reaches the host's files for the guest.
//!
//! The process that runs the guest can't open a host file, so it relays the
//! guest's FUSE requests here, over a socket made before either process is
//! confined. This process opens each share's root once, then confines
//! itself: it reaches files only beneath those roots, follows no symlink,
//! can't start a process, refuses every write to an `ro` share, and in an
//! `rw` share refuses the writes [`server`] protects against. It writes a
//! copy-on-write share's changes beneath that share's pending directory,
//! and reads the share's root alone. The frames on the socket are the ones
//! [`vmm::fs`] describes.

mod landlock;
mod server;
mod sys;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub use server::{Mode, Server};

/// The `argv[0]` that `ap` starts the share server with.
pub const PROGRAM: &str = "ap-share-server";
const READ_ONLY: &str = "ro";
const READ_WRITE: &str = "rw";
const COPY_ON_WRITE: &str = "cow";
const STDIN: i32 = 0;

/// A share server that has started, and the VMM's end of its socket.
pub struct Started {
    pub process: Child,
    pub socket: OwnedFd,
}

/// Starts `program`, a build of `ap`, as the share server for `roots`, in
/// share order. Each root must be a directory, and so must a copy-on-write
/// share's pending directory, which must be empty.
pub fn start(program: &Path, roots: &[(PathBuf, Mode<PathBuf>)]) -> io::Result<Started> {
    let (mine, theirs) = UnixStream::pair()?;
    let mut command = Command::new(program);
    command.arg0(PROGRAM);
    for (root, mode) in roots {
        match mode {
            Mode::ReadOnly => command.arg(READ_ONLY).arg(root),
            Mode::ReadWrite => command.arg(READ_WRITE).arg(root),
            Mode::CopyOnWrite(pending) => command.arg(COPY_ON_WRITE).arg(root).arg(pending),
        };
    }
    let process = command
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()?;
    Ok(Started {
        process,
        socket: OwnedFd::from(mine),
    })
}

/// The share server's process: `args` are a mode and a root for each
/// share, and a copy-on-write share's pending directory after its root. It serves requests from its standard input, a socket, until the
/// VMM hangs up, and returns the status to exit with.
pub fn main(args: &[OsString]) -> u8 {
    match run(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ap: the share server stopped: {error}");
            1
        }
    }
}

fn run(args: &[OsString]) -> io::Result<()> {
    let mut server = Server::new(open_roots(args)?)?;
    raise_descriptor_limit();
    let access = server.access();
    landlock::restrict(&access)?;
    // SAFETY: standard input is the socket `start` passed, and nothing else
    // in this process owns it.
    let mut socket = File::from(unsafe { OwnedFd::from_raw_fd(STDIN) });
    let writes = access
        .iter()
        .any(|(_, access)| matches!(access, landlock::Access::ReadWrite));
    vmm::confine::confine_share_server(socket.as_raw_fd(), writes)?;
    serve(&mut socket, &mut server)
}

fn open_roots(args: &[OsString]) -> io::Result<Vec<(OwnedFd, Mode)>> {
    let usage =
        || io::Error::other("usage: ap-share-server ((ro | rw) root | cow root pending)...");
    let open = |path: &OsString| {
        File::open(path).map(OwnedFd::from).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("can't open {}: {error}", Path::new(path).display()),
            )
        })
    };
    let mut roots = Vec::new();
    let mut args = args.iter();
    while let Some(mode) = args.next() {
        let root = open(args.next().ok_or_else(usage)?)?;
        let mode = match mode.to_str() {
            Some(READ_ONLY) => Mode::ReadOnly,
            Some(READ_WRITE) => Mode::ReadWrite,
            Some(COPY_ON_WRITE) => Mode::CopyOnWrite(open(args.next().ok_or_else(usage)?)?),
            _ => return Err(usage()),
        };
        roots.push((root, mode));
    }
    if roots.is_empty() {
        return Err(usage());
    }
    Ok(roots)
}

/// Each file the guest looks up holds a descriptor, so the server may hold
/// as many as the host lets it.
fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both calls read or write only `limit`.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur = limit.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

/// Answers each frame on `socket` until it closes.
pub fn serve(socket: &mut (impl Read + Write), server: &mut Server) -> io::Result<()> {
    loop {
        let mut header = [0; 8];
        match socket.read_exact(&mut header) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            result => result?,
        }
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let share = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if len > vmm::fs::MAX_FRAME {
            return Err(io::Error::other("a request is too long"));
        }
        let mut request = vec![0; len];
        socket.read_exact(&mut request)?;
        let answer = server.answer(share, &request);
        let mut frame = (answer.len() as u32).to_le_bytes().to_vec();
        frame.extend(answer);
        socket.write_all(&frame)?;
    }
}
