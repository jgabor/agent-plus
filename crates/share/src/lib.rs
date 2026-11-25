//! The share server: a process of its own, which reaches the host's files
//! for the guest. A run's `rw` shares have one, and its other shares have
//! another.
//!
//! The process that runs the guest can't open a host file, so it relays the
//! guest's FUSE requests here, over a socket made before either process is
//! confined. This process inherits each share's root, opened by the process
//! that starts it, then confines itself: it reaches files only beneath
//! those roots, follows no symlink, can't start a process, refuses every
//! write to an `ro` share, and in an `rw` share refuses the writes
//! [`server`] protects against. It writes a copy-on-write share's changes
//! beneath that share's pending directory, and reads the share's root
//! alone. Landlock doesn't cover a file's mode or times. A server with an
//! `rw` share changes those, so it holds no other share. A server with none
//! can't change them, and a process of its own, in [`times`], sets the
//! times of a copy-on-write share's copies. A server of `rw` shares runs in
//! a user namespace of its own, in [`namespace`], so it can change the mode
//! of a file it can't read. The frames on the socket are the ones
//! [`vmm::fs`] describes. A server with no `rw` share can hold one more
//! file until it exits, such as a run's locked record, so the run stays
//! active until it has answered its last request. Its filter refuses to
//! lock, unlock, or close that file.

mod landlock;
mod namespace;
mod server;
mod sys;
mod times;

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub use namespace::spawn as spawn_read_write;
pub use server::{Mode, Root, Server};
#[doc(hidden)]
pub use server::{when_a_copy_moves, when_git_is_listed};

/// The `argv[0]` that `ap` starts the share server with.
pub const PROGRAM: &str = "ap-share-server";
const READ_ONLY: &str = "ro";
const READ_WRITE: &str = "rw";
const COPY_ON_WRITE: &str = "cow";
/// Before a share's mode, names the one file a file share shows.
const FILE: &str = "file";
/// Before the shares, names a descriptor the server holds until it exits.
const KEEP: &str = "keep";
const STDIN: i32 = 0;
const STDERR: i32 = 2;

/// A share server that has started, and the VMM's end of its socket.
pub struct Started {
    pub process: Child,
    pub socket: OwnedFd,
}

/// The share servers for a run's shares, once they have started.
pub struct Servers {
    /// Each server: one for the `rw` shares, then one for the others, if
    /// there are any of each.
    pub started: Vec<Started>,
    /// For each share, in share order, the index in `started` of the server
    /// that serves it. A server numbers its shares in the same order.
    pub serving: Vec<usize>,
}

/// Starts `program`, a build of `ap`, as the share servers for `roots`, in
/// share order. Each root's directory is one the caller opened, and so is
/// a copy-on-write share's pending directory, which must be empty. A server
/// serves what the caller checked, and never opens a path itself. Each one
/// starts without the environment variables named in `hidden`. The server
/// of shares that aren't `rw`, the only one that keeps pending changes,
/// starts with a copy of `kept`, if there is one, which it holds until it
/// exits. A server of `rw` shares changes modes and times, so it holds
/// nothing but its shares.
pub fn start(
    program: &Path,
    roots: &[Root<BorrowedFd>],
    hidden: &[String],
    kept: Option<BorrowedFd>,
) -> io::Result<Servers> {
    let mut started = Vec::new();
    let mut serving = vec![0; roots.len()];
    for read_write in [true, false] {
        let mut served = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            if matches!(root.mode, Mode::ReadWrite) == read_write {
                serving[index] = started.len();
                served.push(root);
            }
        }
        if !served.is_empty() {
            let kept = kept.filter(|_| !read_write);
            started.push(start_one(program, &served, hidden, kept, read_write)?);
        }
    }
    Ok(Servers { started, serving })
}

/// Starts one share server for `roots`, which are all `rw` shares, as
/// `read_write` says, or none of them.
fn start_one(
    program: &Path,
    roots: &[&Root<BorrowedFd>],
    hidden: &[String],
    kept: Option<BorrowedFd>,
    read_write: bool,
) -> io::Result<Started> {
    let (mine, theirs) = UnixStream::pair()?;
    // Each root reaches the server as a descriptor it inherits, numbered in
    // its arguments. The copies are made just for it, and closed once it
    // has started.
    let mut inherited = Vec::new();
    if let Some(kept) = kept {
        inherited.push(inheritable(kept)?);
    }
    for root in roots {
        inherited.push(inheritable(root.directory)?);
        if let Mode::CopyOnWrite(pending) = root.mode {
            inherited.push(inheritable(pending)?);
        }
    }
    let mut numbers = inherited.iter().map(|fd| fd.as_raw_fd().to_string());
    let mut command = Command::new(program);
    for name in hidden {
        command.env_remove(name);
    }
    command.arg0(PROGRAM);
    if kept.is_some() {
        command.arg(KEEP).arg(numbers.next().unwrap_or_default());
    }
    for root in roots {
        if let Some(name) = &root.file {
            command.arg(FILE).arg(OsStr::from_bytes(name.as_bytes()));
        }
        let mut next = || numbers.next().unwrap_or_default();
        match root.mode {
            Mode::ReadOnly => command.arg(READ_ONLY).arg(next()),
            Mode::ReadWrite => command.arg(READ_WRITE).arg(next()),
            Mode::CopyOnWrite(_) => command.arg(COPY_ON_WRITE).arg(next()).arg(next()),
        };
    }
    command.stdin(OwnedFd::from(theirs)).stdout(Stdio::null());
    let process = match read_write {
        true => namespace::spawn(&mut command)?,
        false => command.spawn()?,
    };
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
/// copy-on-write share the number of its pending directory's after it. A
/// file share has `file` and the file's name before its mode. The shares
/// are all `rw`, or none of them are. Before them, `keep` and the number of
/// another inherited descriptor name one it holds until it exits, so that a
/// lock on that file outlives its last answer. A server of `rw` shares
/// refuses it. It serves requests from its standard input, a socket, until
/// the VMM hangs up, and returns the status to exit with.
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
    // Never closed, so its number is never another file's, which the filter
    // would refuse to lock.
    let (kept, args) = match args {
        [keep, fd, rest @ ..] if keep == KEEP => (Some(open_kept(fd)?), rest),
        _ => (None, args),
    };
    let roots = open_roots(args, kept)?;
    let read_write = |root: &Root| matches!(root.mode, Mode::ReadWrite);
    if roots.iter().any(read_write) && !roots.iter().all(read_write) {
        return Err(io::Error::other(
            "an `rw` share needs a share server of its own",
        ));
    }
    if roots.iter().all(read_write) {
        if kept.is_some() {
            return Err(io::Error::other(
                "a server of `rw` shares keeps nothing but its shares",
            ));
        }
        namespace::lower()?;
    }
    let mut server = Server::new(roots)?;
    raise_descriptor_limit();
    let access = server.access();
    landlock::restrict(&access)?;
    // SAFETY: standard input is the socket `start` passed, and nothing else
    // in this process owns it.
    let mut socket = File::from(unsafe { OwnedFd::from_raw_fd(STDIN) });
    let times = server.times();
    vmm::confine::confine_share_server(socket.as_raw_fd(), server.changes(), &times, kept)?;
    serve(&mut socket, &mut server)
}

/// The descriptor that `keep` names, which must be open and past standard
/// error.
fn open_kept(arg: &OsString) -> io::Result<RawFd> {
    let fd = (arg.to_str())
        .and_then(|fd| fd.parse::<i32>().ok())
        .filter(|fd| *fd > STDERR)
        .ok_or_else(|| io::Error::other("`keep` takes a descriptor"))?;
    // SAFETY: this call takes and returns plain integers.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn open_roots(args: &[OsString], kept: Option<RawFd>) -> io::Result<Vec<Root>> {
    let usage = || {
        io::Error::other(
            "usage: ap-share-server [keep descriptor] ([file name] (ro | rw) descriptor | cow descriptor descriptor)...",
        )
    };
    let mut taken: Vec<i32> = kept.into_iter().collect();
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
    while let Some(mut mode) = args.next() {
        let mut file = None;
        if mode == FILE {
            let name = args.next().ok_or_else(usage)?;
            file = Some(CString::new(name.as_bytes()).map_err(|_| usage())?);
            mode = args.next().ok_or_else(usage)?;
        }
        let directory = take(args.next())?;
        let mode = match mode.to_str() {
            Some(READ_ONLY) => Mode::ReadOnly,
            Some(READ_WRITE) => Mode::ReadWrite,
            Some(COPY_ON_WRITE) => Mode::CopyOnWrite(take(args.next())?),
            _ => return Err(usage()),
        };
        roots.push(Root {
            directory,
            file,
            mode,
        });
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
