//! The share server: a process of its own for each run with a share, which
//! reaches the host's files for the guest.
//!
//! The process that runs the guest can't open a host file, so it relays the
//! guest's FUSE requests here, over a socket made before either process is
//! confined. This process inherits each share's root, opened by the process
//! that starts it, then confines itself: it reaches files only beneath those roots, follows no symlink,
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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub use server::{Mode, Server};

/// The `argv[0]` that `ap` starts the share server with.
pub const PROGRAM: &str = "ap-share-server";
const READ_ONLY: &str = "ro";
const READ_WRITE: &str = "rw";
const COPY_ON_WRITE: &str = "cow";
const STDIN: i32 = 0;
const STDERR: i32 = 2;

/// A share server that has started, and the VMM's end of its socket.
pub struct Started {
    pub process: Child,
    pub socket: OwnedFd,
}

/// Starts `program`, a build of `ap`, as the share server for `roots`, in
/// share order. Each root is a directory that the caller opened, and so is
/// a copy-on-write share's pending directory, which must be empty. The
/// server serves what the caller checked, and never opens a path itself.
pub fn start(program: &Path, roots: &[(BorrowedFd, Mode<BorrowedFd>)]) -> io::Result<Started> {
    let (mine, theirs) = UnixStream::pair()?;
    // Each root reaches the server as a descriptor it inherits, numbered in
    // its arguments. The copies are made just for it, and closed once it
    // has started.
    let mut inherited = Vec::new();
    for (root, mode) in roots {
        inherited.push(inheritable(*root)?);
        if let Mode::CopyOnWrite(pending) = mode {
            inherited.push(inheritable(*pending)?);
        }
    }
    let mut numbers = inherited.iter().map(|fd| fd.as_raw_fd().to_string());
    let mut command = Command::new(program);
    command.arg0(PROGRAM);
    for (_, mode) in roots {
        let mut next = || numbers.next().unwrap_or_default();
        match mode {
            Mode::ReadOnly => command.arg(READ_ONLY).arg(next()),
            Mode::ReadWrite => command.arg(READ_WRITE).arg(next()),
            Mode::CopyOnWrite(_) => command.arg(COPY_ON_WRITE).arg(next()).arg(next()),
        };
    }
    let process = command
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()?;
    drop(inherited);
    Ok(Started {
        process,
        socket: OwnedFd::from(mine),
    })
}

/// A copy of `fd` that a program this process starts inherits, numbered
/// past standard error. Make it just before the program starts, and drop
/// it just after, so no other program inherits it.
pub fn inheritable(fd: BorrowedFd) -> io::Result<OwnedFd> {
    // SAFETY: `F_DUPFD` takes an open descriptor and returns a new one.
    let copy = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD, STDERR + 1) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is new, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// The share server's process: `args` are a mode and the number of an
/// inherited descriptor of a root directory for each share, and for a
/// copy-on-write share the number of its pending directory's after it. It serves requests from its standard input, a socket, until the
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
    let usage = || {
        io::Error::other(
            "usage: ap-share-server ((ro | rw) descriptor | cow descriptor descriptor)...",
        )
    };
    let mut taken: Vec<i32> = Vec::new();
    let mut take = |arg: Option<&OsString>| -> io::Result<OwnedFd> {
        let fd = (arg.and_then(|arg| arg.to_str()))
            .and_then(|fd| fd.parse::<i32>().ok())
            .filter(|fd| *fd > STDERR)
            .ok_or_else(usage)?;
        // A descriptor named twice would be owned twice.
        if taken.contains(&fd) {
            return Err(usage());
        }
        // SAFETY: this call takes and returns plain integers.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        taken.push(fd);
        // SAFETY: the descriptor is open, `start` passed it for this
        // process alone, and nothing else here owns it.
        let directory = unsafe { OwnedFd::from_raw_fd(fd) };
        let stat = sys::fstat(directory.as_fd())?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::other(format!(
                "descriptor {fd} isn't a directory"
            )));
        }
        Ok(directory)
    };
    let mut roots = Vec::new();
    let mut args = args.iter();
    while let Some(mode) = args.next() {
        let root = take(args.next())?;
        let mode = match mode.to_str() {
            Some(READ_ONLY) => Mode::ReadOnly,
            Some(READ_WRITE) => Mode::ReadWrite,
            Some(COPY_ON_WRITE) => Mode::CopyOnWrite(take(args.next())?),
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
