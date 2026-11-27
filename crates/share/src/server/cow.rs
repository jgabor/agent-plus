//! A copy-on-write share, which never writes the host's files.
//!
//! The guest sees your file wherever the agent wrote none, read live, and
//! the agent's own copy everywhere else. Its copies live in the share's
//! pending directory, which holds two things:
//!
//! - `tree`, the regular files and directories the agent changed or made,
//!   each at its path, readable only by you. A symlink or a special file the
//!   agent made has an empty regular file there, so nothing in it is a
//!   symlink or a device.
//! - `records`, a line for each change, in the Agentfile's line syntax, read
//!   in order. Each line but `BASE` sets what its path is now, and `DELETE`
//!   removes a path with everything beneath it:
//!
//!   ```text
//!   BASE "src/main.rs" 1234 210 1794567729.123456789 1794567729.123456789
//!   FILE "src/main.rs" 644
//!   DIR "src/new" 755
//!   DIR "build" 755 opaque
//!   SYMLINK "latest" "build/out"
//!   FIFO "pipe" 644
//!   CHAR "null" 666 1 3
//!   DELETE "notes.txt"
//!   ```
//!
//!   `BASE` comes once for each path, before the agent first changes it,
//!   and holds what your file was then: its inode, size, modification time,
//!   and change time, or `none`. A change on your side since then moves the
//!   change time, which no tool sets. An `opaque` directory hides what you
//!   have at its path. A path is quoted, with `\"`, `\\`, and `\xNN` for a
//!   control character or a byte that isn't UTF-8.
//!
//! When the share's root holds a `.git` directory, the pending directory
//! also holds `git`, a copy of everything in it but `objects`, made when the
//! run starts, and `head`, the commit `HEAD` named then, in hex, or `none`.
//! The guest sees that copy in place of your `.git`, but for `objects`,
//! which it reads live. The copy holds each entry as itself, with your
//! mode, and each directory and regular file with your times.
//! The agent's changes there are recorded as anywhere else.
//!
//! The server sets no mode and no time itself once it is confined, so no
//! code of its own can change your files' modes or times. A mode is a
//! record, and a copy's times are set by the times process, which reaches
//! the tree alone.
//!
//! A share of kept state is one of these with no root, so it shows only
//! what the guest wrote, and its directory is its pending directory. It
//! writes no `BASE`, and its next server reads its records back, so the
//! next run sees the same files. A last line with no newline is one a
//! server stopped writing, so it goes, and so does whatever the tree holds
//! with no record. It takes what overlayfs needs of its
//! upper layer, which a share of your files refuses: a chown to the owner a
//! file has, `RENAME_WHITEOUT`, which leaves a whiteout, a `CHAR` numbered
//! 0 0, `RENAME_EXCHANGE`, and extended attributes, each one a record:
//!
//! ```text
//! XATTR "upper/build" "trusted.overlay.opaque" "y"
//! XATTR "upper/build" "trusted.overlay.opaque" none
//! ```
//!
//! The first sets the attribute, and the second removes it. A `DELETE`
//! removes the attributes of everything it removes.
//!
//! A guest's node is a path. Each request resolves it again, one layer at a
//! time, beneath the root or the tree, following no symlink. A directory
//! that has your files beneath it can't be renamed, as in overlayfs, so a
//! tool copies it, and a hard link isn't made, so a tool copies the file.

use std::collections::BTreeSet;

use super::*;
use crate::records::{Words, mode, number, quote, valid_path};
use crate::times::Times;

const TREE: &CStr = c"tree";
const RECORDS: &CStr = c"records";
/// The copy of your `.git`, and the commit its `HEAD` named.
const GIT: &CStr = c"git";
const HEAD: &CStr = c"head";
/// What a file and a directory in the pending directory allow.
const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;
const PERMISSIONS: u32 = 0o7777;
/// The longest name and value of an extended attribute, from
/// `<linux/limits.h>`.
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 1 << 16;
/// The longest list of a file's attributes' names, each ended with a NUL.
const XATTR_LIST_MAX: usize = 1 << 16;
/// `setxattr`'s flags: the attribute must be new, or must be there.
const XATTR_CREATE: u32 = 1;
const XATTR_REPLACE: u32 = 2;
/// `FUSE_GETATTR_FH`: the guest asks about the file a handle has open.
const GETATTR_FH: u32 = 1;
const PATH_FLAGS: i32 = libc::O_PATH | libc::O_NOFOLLOW;
const READ_FLAGS: i32 = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
/// How a file that holds POSIX locks on the agent's copy opens.
const LOCK_FLAGS: i32 = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// A path's extended attributes, by name.
type Xattrs = BTreeMap<Vec<u8>, Vec<u8>>;

/// The whiteout that overlayfs makes, a character device numbered 0, 0.
const WHITEOUT: Entry = Entry::Special {
    mode: libc::S_IFCHR,
    rdev: 0,
};

/// What the agent made of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    File {
        mode: u32,
    },
    Dir {
        mode: u32,
        opaque: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// A FIFO, a socket, or a device: its mode with its kind, and the
    /// device, as the guest encodes it.
    Special {
        mode: u32,
        rdev: u32,
    },
    Deleted,
}

impl Entry {
    /// The mode the guest sees, with its kind.
    fn mode(&self) -> u32 {
        match self {
            Entry::File { mode } => libc::S_IFREG | mode,
            Entry::Dir { mode, .. } => libc::S_IFDIR | mode,
            Entry::Symlink { .. } => libc::S_IFLNK | 0o777,
            Entry::Special { mode, .. } => *mode,
            Entry::Deleted => 0,
        }
    }
}

/// A path as the guest sees it now: the agent's entry, its copy in the
/// tree, and your file, each opened with `O_PATH` where it shows.
struct View {
    entry: Option<Entry>,
    upper: Option<OwnedFd>,
    lower: Option<OwnedFd>,
}

impl View {
    /// The attributes the guest sees, before the inode is the node's.
    fn stat(&self) -> Result<libc::stat, i32> {
        let source = match (&self.entry, &self.lower, &self.upper) {
            (None, Some(lower), _) => lower,
            (_, _, Some(upper)) => upper,
            _ => return Err(libc::ENOENT),
        };
        let mut stat = sys::fstat(source.as_fd()).map_err(errno)?;
        if let Some(entry) = &self.entry {
            stat.st_mode = entry.mode();
            match entry {
                Entry::Symlink { target } => {
                    stat.st_size = target.len() as i64;
                    stat.st_blocks = 0;
                }
                Entry::Special { rdev, .. } => {
                    stat.st_rdev = u64::from(*rdev);
                    stat.st_size = 0;
                }
                _ => {}
            }
        }
        Ok(stat)
    }

    fn is_dir(&self) -> Result<bool, i32> {
        Ok(is(self.stat()?.st_mode, libc::S_IFDIR))
    }
}

/// A node the guest holds: its path, or none once it is gone.
struct Node {
    path: Option<Vec<u8>>,
    lookups: u64,
}

/// One entry of a directory listing.
struct Dirent {
    ino: u64,
    kind: u32,
    name: Vec<u8>,
}

enum Handle {
    /// An open file: the agent's copy, or with `lower`, yours, which is
    /// open for reading alone. `mode` is what the guest saw when it opened
    /// it.
    File {
        fd: OwnedFd,
        node: u64,
        mode: u32,
        lower: bool,
        /// Your file, which this handle had open before the agent's copy
        /// was made, kept so a `flock` on it holds.
        kept: Option<OwnedFd>,
        /// The `flock` this handle holds, `LOCK_SH`, `LOCK_EX`, or none, so
        /// it can be taken again on the agent's copy.
        flock: Option<i32>,
    },
    /// An open directory, listed when it opened.
    Dir { entries: Vec<Dirent> },
}

pub(super) struct Cow {
    /// Your files, or none in a share of kept state.
    root: Option<OwnedFd>,
    owner: Owner,
    pending: OwnedFd,
    /// The copy of your `.git`, if the root held one when the run started.
    git: Option<OwnedFd>,
    tree: OwnedFd,
    /// The process that sets the times of the copies in `tree`.
    times: Times,
    records: OwnedFd,
    /// How long `records` is, so a record that fails half-written goes.
    recorded: std::cell::Cell<u64>,
    /// What the agent made of each path it changed.
    entries: BTreeMap<Vec<u8>, Entry>,
    /// The extended attributes of each path that has any, by name, in a
    /// share of kept state.
    xattrs: BTreeMap<Vec<u8>, Xattrs>,
    /// The paths with a `BASE` record.
    based: BTreeSet<Vec<u8>>,
    nodes: BTreeMap<u64, Node>,
    /// The node of each path the guest holds.
    paths: BTreeMap<Vec<u8>, u64>,
    handles: BTreeMap<u64, Handle>,
    /// Each guest lock owner's POSIX locks, by node and owner.
    locks: BTreeMap<(u64, u64), Locks>,
    /// Lock files on your file, for nodes that have the agent's copy now,
    /// kept so their locks hold until the guest lets them go.
    held: Vec<((u64, u64), OwnedFd)>,
    next_node: u64,
    next_handle: u64,
}

/// The files a copy opened on the agent's copy, with their locks taken,
/// for each lock owner, by node and owner, and each reader, by handle, that
/// it moves there.
#[derive(Default)]
struct Moves {
    locks: Vec<((u64, u64), OwnedFd)>,
    readers: Vec<(u64, OwnedFd)>,
}

/// One guest lock owner's POSIX locks on one file.
struct Locks {
    /// The file that holds them.
    fd: OwnedFd,
    /// Each range the owner holds, by its first byte, its last, and its
    /// type, so the locks can be taken again on the agent's copy.
    ranges: Vec<(i64, i64, i16)>,
}

impl Locks {
    fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            ranges: Vec::new(),
        }
    }

    /// Records `lock`, which the owner has just taken or let go of, as the
    /// kernel applies it: it replaces what the owner held in its range, and
    /// joins a range of the same type that it touches.
    fn record(&mut self, lock: &libc::flock) {
        let (mut start, mut end) = match lock.l_len {
            0 => (lock.l_start, i64::MAX),
            len => (lock.l_start, lock.l_start + len - 1),
        };
        let mut ranges = Vec::new();
        for &(first, last, kind) in &self.ranges {
            if last < start || first > end {
                ranges.push((first, last, kind));
                continue;
            }
            if first < start {
                ranges.push((first, start - 1, kind));
            }
            if last > end {
                ranges.push((end + 1, last, kind));
            }
        }
        if i32::from(lock.l_type) != libc::F_UNLCK {
            ranges.retain(|&(first, last, kind)| {
                let joins = kind == lock.l_type
                    && (last.checked_add(1) == Some(start) || end.checked_add(1) == Some(first));
                if joins {
                    start = start.min(first);
                    end = end.max(last);
                }
                !joins
            });
            ranges.push((start, end, lock.l_type));
        }
        self.ranges = ranges;
    }

    /// Takes the owner's locks again on `fd`.
    fn take(&self, fd: BorrowedFd) -> Result<(), i32> {
        for &(first, last, kind) in &self.ranges {
            // SAFETY: `struct flock` is plain data, valid when zeroed.
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = kind;
            lock.l_whence = libc::SEEK_SET as i16;
            lock.l_start = first;
            lock.l_len = match last {
                i64::MAX => 0,
                last => last - first + 1,
            };
            sys::ofd_lock(fd, libc::F_OFD_SETLK, &mut lock).map_err(errno)?;
        }
        Ok(())
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    match dir.is_empty() {
        true => name.to_vec(),
        false => [dir, b"/", name].concat(),
    }
}

/// A path to open from a descriptor: `.` for the root.
fn c(path: &[u8]) -> CString {
    match path.is_empty() {
        true => c".".to_owned(),
        // Names come from `entry_name`, which holds no NUL.
        false => CString::new(path).unwrap_or_default(),
    }
}

/// The directory a path is in, and its name there.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&[], path),
    }
}

/// Each directory above a path, from the root down.
fn parents(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    let ends = path.iter().enumerate().filter(|(_, byte)| **byte == b'/');
    let ends = ends.map(|(at, _)| at);
    (!path.is_empty())
        .then_some(0)
        .into_iter()
        .chain(ends)
        .map(|end| &path[..end])
}

/// Whether `path` is beneath `dir`.
fn beneath(path: &[u8], dir: &[u8]) -> bool {
    match dir.is_empty() {
        true => !path.is_empty(),
        false => path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/',
    }
}

/// A device as the guest's kernel encodes it, from the host's `dev_t`.
fn encode(rdev: u64) -> u32 {
    device(libc::major(rdev), libc::minor(rdev))
}

/// A device as the guest's kernel encodes it, from its major and minor.
fn device(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// The record that sets the extended attribute `name` of `path` to
/// `value`, or removes it.
fn xattr_line(path: &[u8], name: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let mut out = b"XATTR".to_vec();
    quote(&mut out, path);
    quote(&mut out, name);
    match value {
        Some(value) => quote(&mut out, value),
        None => out.extend(b" none"),
    }
    out
}

/// The name of an extended attribute, as the guest sends it.
fn xattr_name(body: &[u8]) -> Result<&[u8], i32> {
    let name = CStr::from_bytes_until_nul(body).map_err(|_| libc::EINVAL)?;
    match name.to_bytes() {
        [] => Err(libc::EINVAL),
        name if name.len() > XATTR_NAME_MAX => Err(libc::ERANGE),
        name => Ok(name),
    }
}

/// What a guest that asks for `size` bytes gets of `value`: its length
/// alone when it asks for none, as `struct fuse_getxattr_out`.
fn sized(value: Vec<u8>, size: u32) -> Answer {
    match size {
        0 => Ok([(value.len() as u32).to_le_bytes(), [0; 4]].concat()),
        size if value.len() > size as usize => Err(libc::ERANGE),
        _ => Ok(value),
    }
}

/// What a share of kept state holds when its server starts, read from its
/// records, and how long they are.
#[derive(Default)]
struct Kept {
    entries: BTreeMap<Vec<u8>, Entry>,
    xattrs: BTreeMap<Vec<u8>, Xattrs>,
    len: u64,
}

impl Kept {
    /// Reads `records` in order. A last line with no newline is one that a
    /// server stopped writing, so it goes.
    fn read(records: BorrowedFd) -> io::Result<Self> {
        let mut text = Vec::new();
        let mut chunk = vec![0; MAX_READ];
        loop {
            match sys::pread(records, &mut chunk, text.len() as u64)? {
                0 => break,
                len => text.extend_from_slice(&chunk[..len]),
            }
        }
        let len = text
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        if len < text.len() {
            sys::ftruncate(records, len as u64)?;
        }
        let mut kept = Kept {
            len: len as u64,
            ..Kept::default()
        };
        for line in text[..len].split_inclusive(|&byte| byte == b'\n') {
            let line = &line[..line.len() - 1];
            kept.read_line(line).ok_or_else(|| {
                let line = String::from_utf8_lossy(line);
                io::Error::new(io::ErrorKind::InvalidData, format!("bad record {line:?}"))
            })?;
        }
        Ok(kept)
    }

    fn read_line(&mut self, line: &[u8]) -> Option<()> {
        let mut words = Words::new(line);
        let word = words.bare()?;
        let path = words.quoted()?;
        // The share's root is `""`, which the guest can change but not
        // remove or replace.
        let root = path.is_empty() && matches!(word, b"DIR" | b"XATTR");
        if !root && !valid_path(&path) {
            return None;
        }
        let entry = match word {
            b"FILE" => Entry::File {
                mode: mode(words.bare()?)?,
            },
            b"DIR" => Entry::Dir {
                mode: mode(words.bare()?)?,
                opaque: false,
            },
            b"SYMLINK" => Entry::Symlink {
                target: words.quoted()?,
            },
            b"FIFO" | b"SOCKET" | b"CHAR" | b"BLOCK" => {
                let (kind, numbered) = match word {
                    b"FIFO" => (libc::S_IFIFO, false),
                    b"SOCKET" => (libc::S_IFSOCK, false),
                    b"CHAR" => (libc::S_IFCHR, true),
                    _ => (libc::S_IFBLK, true),
                };
                let mode = kind | mode(words.bare()?)?;
                let mut rdev = 0;
                if numbered {
                    let major = u32::try_from(number(words.bare()?)?).ok()?;
                    let minor = u32::try_from(number(words.bare()?)?).ok()?;
                    rdev = device(major, minor);
                }
                Entry::Special { mode, rdev }
            }
            b"DELETE" => {
                self.entries.retain(|other, _| !beneath(other, &path));
                self.xattrs
                    .retain(|other, _| *other != path && !beneath(other, &path));
                Entry::Deleted
            }
            b"XATTR" => {
                let name = words.quoted()?;
                let value = match words.bare() {
                    Some(b"none") => None,
                    Some(_) => return None,
                    None => Some(words.quoted()?),
                };
                words.end()?;
                let held = self.xattrs.entry(path.clone()).or_default();
                match value {
                    Some(value) => {
                        held.insert(name, value);
                    }
                    None => {
                        held.remove(&name);
                    }
                }
                if held.is_empty() {
                    self.xattrs.remove(&path);
                }
                return Some(());
            }
            _ => return None,
        };
        words.end()?;
        self.entries.insert(path, entry);
        Some(())
    }
}

/// The record that sets `path` to `entry`.
fn line(path: &[u8], entry: &Entry) -> Vec<u8> {
    let word = match entry {
        Entry::File { .. } => "FILE",
        Entry::Dir { .. } => "DIR",
        Entry::Symlink { .. } => "SYMLINK",
        Entry::Special { mode, .. } => match mode & libc::S_IFMT {
            libc::S_IFIFO => "FIFO",
            libc::S_IFSOCK => "SOCKET",
            libc::S_IFBLK => "BLOCK",
            _ => "CHAR",
        },
        Entry::Deleted => "DELETE",
    };
    let mut out = word.as_bytes().to_vec();
    quote(&mut out, path);
    match entry {
        Entry::File { mode } => out.extend(format!(" {mode:o}").bytes()),
        Entry::Dir { mode, opaque } => {
            out.extend(format!(" {mode:o}").bytes());
            if *opaque {
                out.extend(b" opaque");
            }
        }
        Entry::Symlink { target } => quote(&mut out, target),
        Entry::Special { mode, rdev } => {
            out.extend(format!(" {:o}", mode & PERMISSIONS).bytes());
            if is(*mode, libc::S_IFCHR) || is(*mode, libc::S_IFBLK) {
                let major = (rdev >> 8) & 0xfff;
                let minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00);
                out.extend(format!(" {major} {minor}").bytes());
            }
        }
        Entry::Deleted => {}
    }
    out
}

/// The `BASE` record of `path`, from your file's state, if you have one.
fn base_line(path: &[u8], stat: Option<&libc::stat>) -> Vec<u8> {
    let mut out = b"BASE".to_vec();
    quote(&mut out, path);
    match stat {
        None => out.extend(b" none"),
        Some(stat) => out.extend(
            format!(
                " {} {} {}.{:09} {}.{:09}",
                stat.st_ino,
                stat.st_size,
                stat.st_mtime,
                stat.st_mtime_nsec,
                stat.st_ctime,
                stat.st_ctime_nsec
            )
            .bytes(),
        ),
    }
    out
}

/// The entries of the directory `fd` is, but `.` and `..`: each name, its
/// kind as `getdents64` gives it, and its inode.
fn names(fd: BorrowedFd) -> Result<Vec<(Vec<u8>, u8, u64)>, i32> {
    let dir = sys::open_beneath(fd, c".", libc::O_RDONLY | libc::O_DIRECTORY).map_err(errno)?;
    let mut names = Vec::new();
    let mut raw = vec![0; MAX_READ];
    let mut offset = 0;
    loop {
        let len = sys::read_directory(dir.as_fd(), &mut raw, offset).map_err(errno)?;
        if len == 0 {
            return Ok(names);
        }
        let mut at = 0;
        // `struct linux_dirent64`: inode, offset, record length, type, name.
        while at + 19 < len {
            let record = usize::from(u16::from_le_bytes([raw[at + 16], raw[at + 17]]));
            let name = raw
                .get(at + 19..at + record)
                .and_then(|bytes| CStr::from_bytes_until_nul(bytes).ok())
                .ok_or(libc::EIO)?;
            let ino = u64_at(&raw, at).ok_or(libc::EIO)?;
            offset = u64_at(&raw, at + 8).ok_or(libc::EIO)?;
            if !matches!(name.to_bytes(), b"." | b"..") {
                names.push((name.to_bytes().to_vec(), raw[at + 18], ino));
            }
            at += record.max(1);
        }
    }
}

/// The access and modification times that `stat` describes, as
/// `futimens` takes them.
fn times_of(stat: &libc::stat) -> [libc::timespec; 2] {
    [
        libc::timespec {
            tv_sec: stat.st_atime,
            tv_nsec: stat.st_atime_nsec,
        },
        libc::timespec {
            tv_sec: stat.st_mtime,
            tv_nsec: stat.st_mtime_nsec,
        },
    ]
}

/// Copies what `from` holds to `to`, each an open regular file.
fn copy_contents(from: BorrowedFd, to: BorrowedFd) -> io::Result<()> {
    let mut buffer = vec![0; MAX_READ];
    let mut offset = 0;
    loop {
        let len = sys::pread(from, &mut buffer, offset)?;
        if len == 0 {
            return Ok(());
        }
        let mut done = 0;
        while done < len {
            done += sys::pwrite(to, &buffer[done..len], offset + done as u64)?;
        }
        offset += len as u64;
    }
}

/// What a test runs before a copy opens a file it moves, which can fail it.
type Moving = Box<dyn FnMut() -> Result<(), i32>>;

thread_local! {
    /// What runs each time a capture on this thread has listed `.git`.
    static LISTED: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// What runs on this thread before a copy opens the agent's copy for
    /// a reader or a lock owner it moves there.
    static MOVING: std::cell::RefCell<Option<Moving>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `hook` each time a capture of `.git` on this thread has listed it,
/// until it is set again. A test changes `.git` with it.
#[doc(hidden)]
pub fn when_git_is_listed(hook: Option<Box<dyn FnMut()>>) {
    LISTED.with(|listed| *listed.borrow_mut() = hook);
}

/// Runs `hook` before each file a copy on this thread opens on the agent's
/// copy, for a reader or a lock owner it moves there, until it is set
/// again. An error from `hook` fails that open. A test fails a move with it.
#[doc(hidden)]
pub fn when_a_copy_moves(hook: Option<Moving>) {
    MOVING.with(|moving| *moving.borrow_mut() = hook);
}

/// How many times a capture copies a `.git` that changes while it copies.
const CAPTURES: usize = 5;

/// What a capture saw of each entry beneath a git directory, but for what
/// `objects` holds, by path. Git can't change an entry without changing
/// what [`seen`] gives for it, or which entries there are.
type Seen = BTreeMap<Vec<u8>, (u32, u64, i64, [i64; 4])>;

/// The mode and inode of an entry, and if it isn't a directory, its size
/// and modification and change times. A directory's times move as git
/// changes its entries, which are seen on their own.
fn seen(stat: &libc::stat) -> (u32, u64, i64, [i64; 4]) {
    match is(stat.st_mode, libc::S_IFDIR) {
        true => (stat.st_mode, stat.st_ino, 0, [0; 4]),
        false => (
            stat.st_mode,
            stat.st_ino,
            stat.st_size,
            [
                stat.st_mtime,
                stat.st_mtime_nsec,
                stat.st_ctime,
                stat.st_ctime_nsec,
            ],
        ),
    }
}

/// Copies the `.git` directory at `root`, if it has one, to `git` in
/// `pending`, but for what is in its `objects`, and writes `head`. Returns
/// the copy. Git can change `.git` while it is copied, as when it packs
/// refs, so the copy is checked against what `.git` holds afterwards, and
/// made again if they differ.
fn capture(root: BorrowedFd, pending: BorrowedFd) -> io::Result<Option<OwnedFd>> {
    let git = match sys::open_beneath(root, c".git", PATH_FLAGS) {
        Ok(git) => git,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(error) => return Err(error),
    };
    let stat = sys::fstat(git.as_fd())?;
    // A `.git` file, as in a linked worktree, or a symlink, stays live.
    if !is(stat.st_mode, libc::S_IFDIR) {
        return Ok(None);
    }
    for _ in 0..CAPTURES {
        sys::mkdirat(pending, GIT, DIR_MODE)?;
        let copy = sys::open_beneath(pending, GIT, libc::O_PATH | libc::O_DIRECTORY)?;
        let mut linked = false;
        let mut copied = Seen::new();
        copy_tree(git.as_fd(), copy.as_fd(), b"", &mut linked, &mut copied)?;
        let mut now = Seen::new();
        walk(git.as_fd(), b"", &mut now)?;
        if copied == now {
            finish(copy.as_fd(), &stat)?;
            write_head(pending, copy.as_fd(), linked)?;
            return Ok(Some(copy));
        }
        remove_tree(pending, GIT)?;
    }
    Err(io::Error::other(
        "git kept changing `.git` while it was captured",
    ))
}

/// Writes `head` in `pending`, for the copy of `.git`.
fn write_head(pending: BorrowedFd, copy: BorrowedFd, linked: bool) -> io::Result<()> {
    // `HEAD` and the refs are read from the copy, so they are what the
    // guest sees. A ref that is a symlink is read as a symbolic ref, but a
    // symlink at `packed-refs`, or on the way to a ref, would be followed,
    // and a special file there could block the read, so then no ref is
    // read. A `HEAD` that names no commit git could read
    // counts as none, so a branch import starts from the agent's history
    // alone.
    let path = format!("/proc/self/fd/{}", copy.as_raw_fd());
    let head = match linked {
        true => Ok(None),
        false => git::Repository::at(path).resolve("HEAD"),
    };
    let head = match head {
        Ok(Some(id)) => format!("{id}\n"),
        Ok(None) | Err(git::Error::Corrupt(_)) => "none\n".to_owned(),
        Err(git::Error::Io(error)) => return Err(error),
    };
    let flags = libc::O_WRONLY | libc::O_EXCL;
    let file = sys::create_beneath(pending, HEAD, flags, FILE_MODE)?;
    if sys::pwrite(file.as_fd(), head.as_bytes(), 0)? != head.len() {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(())
}

/// Records what each entry beneath the directory `from`, at `path` in a
/// git directory, is now, but for what `objects` holds.
fn walk(from: BorrowedFd, path: &[u8], out: &mut Seen) -> io::Result<()> {
    for (name, _, _) in names(from).map_err(io::Error::from_raw_os_error)? {
        let inner = join(path, &name);
        let entry = match sys::open_beneath(from, &c(&name), PATH_FLAGS) {
            Ok(entry) => entry,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(error),
        };
        let stat = sys::fstat(entry.as_fd())?;
        if is(stat.st_mode, libc::S_IFDIR) && inner != b"objects" {
            walk(entry.as_fd(), &inner, out)?;
        }
        out.insert(inner, seen(&stat));
    }
    Ok(())
}

/// Removes the directory `name` in `directory`, which a capture made, and
/// everything in it.
fn remove_tree(directory: BorrowedFd, name: &CStr) -> io::Result<()> {
    // An empty one goes as it is, as `objects` does, whose mode may not let
    // it be read.
    match sys::unlinkat(directory, name, libc::AT_REMOVEDIR) {
        Err(error) if error.raw_os_error() == Some(libc::ENOTEMPTY) => {}
        removed => return removed,
    }
    let made = sys::open_beneath(directory, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    // The copy can have your mode, which may not let it be changed.
    sys::fchmod(made.as_fd(), DIR_MODE)?;
    for (entry, _, _) in names(made.as_fd()).map_err(io::Error::from_raw_os_error)? {
        let entry = c(&entry);
        match sys::unlinkat(made.as_fd(), &entry, 0) {
            Err(error) if error.raw_os_error() == Some(libc::EISDIR) => {
                remove_tree(made.as_fd(), &entry)?;
            }
            removed => removed?,
        }
    }
    sys::unlinkat(directory, name, libc::AT_REMOVEDIR)
}

/// Removes what the tree of a share of kept state holds beneath `path` with
/// no record: what a server that stopped between changing the tree and
/// recording it left there.
fn prune(tree: BorrowedFd, path: &[u8], entries: &BTreeMap<Vec<u8>, Entry>) -> io::Result<()> {
    let dir = sys::open_beneath(tree, &c(path), libc::O_PATH | libc::O_DIRECTORY)?;
    for (name, kind, _) in names(dir.as_fd()).map_err(io::Error::from_raw_os_error)? {
        let at = join(path, &name);
        match entries.get(&at) {
            Some(Entry::Dir { .. }) if kind == libc::DT_DIR => prune(tree, &at, entries)?,
            Some(Entry::Deleted) | None => match sys::unlinkat(dir.as_fd(), &c(&name), 0) {
                Err(error) if error.raw_os_error() == Some(libc::EISDIR) => {
                    remove_tree(dir.as_fd(), &c(&name))?;
                }
                removed => removed?,
            },
            Some(_) => {}
        }
    }
    Ok(())
}

/// Copies the directory `from`, at `path` in a git directory, into the
/// directory `to`, but for what `objects` holds. `linked` is set if
/// `packed-refs`, `refs`, or anything beneath it is a symlink, or if one of
/// them or `HEAD` is a special file. What it copied goes in `copied`.
fn copy_tree(
    from: BorrowedFd,
    to: BorrowedFd,
    path: &[u8],
    linked: &mut bool,
    copied: &mut Seen,
) -> io::Result<()> {
    let listed = names(from).map_err(io::Error::from_raw_os_error)?;
    if path.is_empty() {
        LISTED.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook()));
    }
    for (name, _, _) in listed {
        let inner = join(path, &name);
        let name = c(&name);
        let entry = match sys::open_beneath(from, &name, PATH_FLAGS) {
            Ok(entry) => entry,
            // Git removed it since, such as a lock file.
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(error),
        };
        let stat = sys::fstat(entry.as_fd())?;
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                sys::mkdirat(to, &name, DIR_MODE)?;
                let made = sys::open_beneath(to, &name, libc::O_PATH | libc::O_DIRECTORY)?;
                // The guest reads `objects` live, so it is left empty here.
                if inner != b"objects" {
                    copy_tree(entry.as_fd(), made.as_fd(), &inner, linked, copied)?;
                }
                finish(made.as_fd(), &stat)?;
                copied.insert(inner, seen(&stat));
            }
            libc::S_IFREG => {
                // Git can replace the file since, even with a symlink or a
                // directory. It is left out, so the check sees the change.
                let source = match sys::open_beneath(from, &name, READ_FLAGS) {
                    Ok(source) => source,
                    Err(error)
                        if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ELOOP)) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let stat = sys::fstat(source.as_fd())?;
                if !is(stat.st_mode, libc::S_IFREG) {
                    continue;
                }
                let flags = libc::O_WRONLY | libc::O_EXCL;
                let made = sys::create_beneath(to, &name, flags, FILE_MODE)?;
                copy_contents(source.as_fd(), made.as_fd())?;
                finish(made.as_fd(), &stat)?;
                copied.insert(inner, seen(&stat));
            }
            libc::S_IFLNK => {
                let target = sys::readlink(entry.as_fd(), c"")?;
                let target =
                    CString::new(target).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                sys::symlinkat(&target, to, &name)?;
                *linked |=
                    matches!(&inner[..], b"packed-refs" | b"refs") || inner.starts_with(b"refs/");
                copied.insert(inner, seen(&stat));
            }
            _ => {
                *linked |= matches!(&inner[..], b"HEAD" | b"packed-refs" | b"refs")
                    || inner.starts_with(b"refs/");
                sys::mknodat(
                    to,
                    &name,
                    stat.st_mode & libc::S_IFMT | FILE_MODE,
                    stat.st_rdev,
                )?;
                // A special file has no contents, and nothing opens it to
                // set its mode and times.
                let made = sys::open_beneath(to, &name, PATH_FLAGS)?;
                let path = CString::new(format!("/proc/self/fd/{}", made.as_raw_fd()))
                    .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                // SAFETY: `path` is NUL-terminated and outlives the call.
                if unsafe { libc::chmod(path.as_ptr(), stat.st_mode & PERMISSIONS) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                copied.insert(inner, seen(&stat));
            }
        }
    }
    Ok(())
}

/// Gives a copy the mode and times of your file, which `stat` describes.
fn finish(copy: BorrowedFd, stat: &libc::stat) -> io::Result<()> {
    let opened;
    let fd = match is(stat.st_mode, libc::S_IFDIR) {
        // `fchmod` and `futimens` need a descriptor that isn't `O_PATH`.
        true => {
            opened = sys::open_beneath(copy, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
            opened.as_fd()
        }
        false => copy,
    };
    sys::fchmod(fd, stat.st_mode & PERMISSIONS)?;
    sys::futimens(fd, &times_of(stat))
}

impl Cow {
    /// A share of `root` that keeps its changes in `pending`, which must be
    /// empty, so a run never takes over another's changes.
    pub(super) fn new(root: OwnedFd, pending: OwnedFd, owner: Owner) -> io::Result<Self> {
        for fd in [&root, &pending] {
            if !is(sys::fstat(fd.as_fd())?.st_mode, libc::S_IFDIR) {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
        }
        sys::mkdirat(pending.as_fd(), TREE, DIR_MODE)?;
        let tree = sys::open_beneath(pending.as_fd(), TREE, libc::O_PATH | libc::O_DIRECTORY)?;
        let flags = libc::O_WRONLY | libc::O_APPEND | libc::O_EXCL;
        let records = sys::create_beneath(pending.as_fd(), RECORDS, flags, FILE_MODE)?;
        let git = capture(root.as_fd(), pending.as_fd())?;
        let made = Kept::default();
        Self::serving(Some(root), pending, owner, git, tree, records, made)
    }

    /// A share of kept state in `directory`, which holds what an earlier
    /// run's guest wrote, or nothing.
    pub(super) fn state(directory: OwnedFd, owner: Owner) -> io::Result<Self> {
        if !is(sys::fstat(directory.as_fd())?.st_mode, libc::S_IFDIR) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        match sys::mkdirat(directory.as_fd(), TREE, DIR_MODE) {
            Err(error) if error.raw_os_error() != Some(libc::EEXIST) => return Err(error),
            _ => {}
        }
        let tree = sys::open_beneath(directory.as_fd(), TREE, libc::O_PATH | libc::O_DIRECTORY)?;
        let flags = libc::O_RDWR | libc::O_APPEND;
        let records = sys::create_beneath(directory.as_fd(), RECORDS, flags, FILE_MODE)?;
        let kept = Kept::read(records.as_fd())?;
        prune(tree.as_fd(), b"", &kept.entries)?;
        Self::serving(None, directory, owner, None, tree, records, kept)
    }

    fn serving(
        root: Option<OwnedFd>,
        pending: OwnedFd,
        owner: Owner,
        git: Option<OwnedFd>,
        tree: OwnedFd,
        records: OwnedFd,
        kept: Kept,
    ) -> io::Result<Self> {
        let times = Times::start(tree.as_fd())?;
        Ok(Self {
            root,
            owner,
            pending,
            git,
            tree,
            times,
            records,
            recorded: std::cell::Cell::new(kept.len),
            entries: kept.entries,
            xattrs: kept.xattrs,
            based: BTreeSet::new(),
            nodes: BTreeMap::from([(
                ROOT,
                Node {
                    path: Some(Vec::new()),
                    lookups: 1,
                },
            )]),
            paths: BTreeMap::from([(Vec::new(), ROOT)]),
            handles: BTreeMap::new(),
            locks: BTreeMap::new(),
            held: Vec::new(),
            next_node: ROOT + 1,
            next_handle: 1,
        })
    }

    /// The share's root, if it has one, and its pending directory.
    pub(super) fn directories(&self) -> (Option<BorrowedFd<'_>>, BorrowedFd<'_>) {
        (self.root.as_ref().map(AsFd::as_fd), self.pending.as_fd())
    }

    /// Whether this is a share of kept state, which shows none of your
    /// files.
    fn kept(&self) -> bool {
        self.root.is_none()
    }

    /// The socket of the process that sets the times of the copies.
    pub(super) fn times(&self) -> RawFd {
        self.times.socket()
    }

    fn open_lower(&self, path: &[u8], flags: i32) -> Result<OwnedFd, i32> {
        let (dir, path) = self.lower(path).ok_or(libc::ENOENT)?;
        sys::open_beneath(dir, &c(path), flags).map_err(errno)
    }

    /// Where your file at `path` is, as the guest sees it: the copy of your
    /// `.git` for what it holds, and the root for everything else.
    fn lower<'a>(&self, path: &'a [u8]) -> Option<(BorrowedFd<'_>, &'a [u8])> {
        let root = self.root.as_ref()?;
        if let Some(git) = &self.git {
            let rest = match path {
                b".git" => Some(&b""[..]),
                _ => path.strip_prefix(b".git/"),
            };
            if let Some(rest) = rest
                && rest != b"objects"
                && !rest.starts_with(b"objects/")
            {
                return Some((git.as_fd(), rest));
            }
        }
        Some((root.as_fd(), path))
    }

    fn open_upper(&self, path: &[u8], flags: i32) -> Result<OwnedFd, i32> {
        sys::open_beneath(self.tree.as_fd(), &c(path), flags).map_err(errno)
    }

    /// What the guest sees at `path`.
    fn view(&self, path: &[u8]) -> Result<View, i32> {
        let mut hidden = false;
        for parent in parents(path) {
            match self.entries.get(parent) {
                None => {}
                Some(Entry::Dir { opaque, .. }) => hidden |= opaque,
                Some(Entry::Deleted) => return Err(libc::ENOENT),
                Some(_) => return Err(libc::ENOTDIR),
            }
        }
        match self.entries.get(path) {
            Some(Entry::Deleted) => Err(libc::ENOENT),
            Some(entry) => {
                let upper = self.open_upper(path, PATH_FLAGS)?;
                let lower = match entry {
                    Entry::Dir { opaque: false, .. } if !hidden => self.lower_dir(path),
                    _ => None,
                };
                Ok(View {
                    entry: Some(entry.clone()),
                    upper: Some(upper),
                    lower,
                })
            }
            None => {
                // A directory is in the tree with no entry only to hold what
                // the agent changed beneath it.
                let upper = self.open_upper(path, PATH_FLAGS).ok().filter(|fd| {
                    sys::fstat(fd.as_fd()).is_ok_and(|stat| is(stat.st_mode, libc::S_IFDIR))
                });
                let lower = match (hidden, &upper) {
                    (true, _) => None,
                    (false, None) => Some(self.open_lower(path, PATH_FLAGS)?),
                    (false, Some(_)) => self.lower_dir(path),
                };
                if upper.is_none() && lower.is_none() {
                    return Err(libc::ENOENT);
                }
                Ok(View {
                    entry: None,
                    upper,
                    lower,
                })
            }
        }
    }

    /// Your directory at `path`, if you have one.
    fn lower_dir(&self, path: &[u8]) -> Option<OwnedFd> {
        self.open_lower(path, PATH_FLAGS)
            .ok()
            .filter(|fd| sys::fstat(fd.as_fd()).is_ok_and(|stat| is(stat.st_mode, libc::S_IFDIR)))
    }

    fn path(&self, node: u64) -> Result<Vec<u8>, i32> {
        self.nodes
            .get(&node)
            .ok_or(libc::ESTALE)?
            .path
            .clone()
            .ok_or(libc::ENOENT)
    }

    /// The path of a directory node.
    fn directory(&self, node: u64) -> Result<Vec<u8>, i32> {
        let path = self.path(node)?;
        match self.view(&path)?.is_dir()? {
            true => Ok(path),
            false => Err(libc::ENOTDIR),
        }
    }

    fn record(&self, mut line: Vec<u8>) -> Result<(), i32> {
        line.push(b'\n');
        // The file is open to append, so this lands at its end.
        let written = sys::pwrite(self.records.as_fd(), &line, 0);
        if let Ok(len) = written
            && len == line.len()
        {
            self.recorded.set(self.recorded.get() + len as u64);
            return Ok(());
        }
        // A record is a whole line or none, so the next one starts on a line
        // of its own.
        let _ = sys::ftruncate(self.records.as_fd(), self.recorded.get());
        Err(written.map_or_else(errno, |_| libc::EIO))
    }

    /// Records that `path` is now `entry`. A deleted path's extended
    /// attributes go with it, and those of everything beneath it.
    fn set(&mut self, path: &[u8], entry: Entry) -> Result<(), i32> {
        self.record(line(path, &entry))?;
        if entry == Entry::Deleted {
            self.take_xattrs(path);
        }
        self.entries.insert(path.to_vec(), entry);
        Ok(())
    }

    /// Takes the extended attributes of `path` and of everything beneath
    /// it away, and returns them by what follows `path` in each path.
    fn take_xattrs(&mut self, path: &[u8]) -> Vec<(Vec<u8>, Xattrs)> {
        let found: Vec<_> = (self.xattrs.keys())
            .filter(|key| *key == path || beneath(key, path))
            .cloned()
            .collect();
        let taken = found.into_iter().filter_map(|key| {
            let held = self.xattrs.remove(&key)?;
            Some((key[path.len()..].to_vec(), held))
        });
        taken.collect()
    }

    /// Records each of `held`, the extended attributes of `path`.
    fn set_xattrs(&mut self, path: &[u8], held: Xattrs) -> Result<(), i32> {
        for (name, value) in &held {
            self.record(xattr_line(path, name, Some(value)))?;
        }
        self.xattrs.insert(path.to_vec(), held);
        Ok(())
    }

    /// Records that `path`, just made in the tree, is `entry`, or removes it
    /// from the tree again, so a path is in the tree only with its record.
    fn keep(&mut self, path: &[u8], entry: Entry) -> Result<(), i32> {
        let flags = match entry {
            Entry::Dir { .. } => libc::AT_REMOVEDIR,
            _ => 0,
        };
        self.set(path, entry).inspect_err(|_| {
            let _ = self.unlink_upper(path, flags);
        })
    }

    /// Records your file at `path` as it is before the agent's first change
    /// there. `stat` is your file's, if the caller has it.
    fn base(&mut self, path: &[u8], stat: Option<&libc::stat>) -> Result<(), i32> {
        if self.kept() || self.based.contains(path) {
            return Ok(());
        }
        let found;
        let stat = match stat {
            Some(stat) => Some(stat),
            None => {
                found = self
                    .open_lower(path, PATH_FLAGS)
                    .ok()
                    .and_then(|fd| sys::fstat(fd.as_fd()).ok());
                found.as_ref()
            }
        };
        self.record(base_line(path, stat))?;
        self.based.insert(path.to_vec());
        Ok(())
    }

    /// Makes `path`'s directory in the tree.
    fn mkdir_upper(&self, path: &[u8]) -> Result<(), i32> {
        let (dir, name) = split(path);
        let dir = self.open_upper(dir, libc::O_PATH | libc::O_DIRECTORY)?;
        sys::mkdirat(dir.as_fd(), &c(name), DIR_MODE).map_err(errno)
    }

    /// Makes `path` a regular file in the tree, open with `flags`.
    fn create_upper(&self, path: &[u8], flags: i32) -> Result<OwnedFd, i32> {
        let flags = flags | libc::O_EXCL | libc::O_NOFOLLOW;
        sys::create_beneath(self.tree.as_fd(), &c(path), flags, FILE_MODE).map_err(errno)
    }

    /// Makes each directory above `path` in the tree.
    fn make_parents(&self, path: &[u8]) -> Result<(), i32> {
        for parent in parents(path).skip(1) {
            match self.open_upper(parent, PATH_FLAGS) {
                Ok(_) => {}
                Err(libc::ENOENT) => self.mkdir_upper(parent)?,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Gives `path` the agent's own copy of your file, if it has none yet.
    fn copy_up(&mut self, path: &[u8]) -> Result<(), i32> {
        let view = self.view(path)?;
        if view.entry.is_some() {
            return Ok(());
        }
        self.make_parents(path)?;
        let Some(lower) = &view.lower else {
            // A directory in the tree that you have since removed.
            self.base(path, None)?;
            let entry = Entry::Dir {
                mode: 0o755,
                opaque: false,
            };
            return self.set(path, entry);
        };
        let stat = sys::fstat(lower.as_fd()).map_err(errno)?;
        self.base(path, Some(&stat))?;
        let mode = stat.st_mode & PERMISSIONS;
        let (entry, moves) = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                if view.upper.is_none() {
                    self.mkdir_upper(path)?;
                }
                self.times.set(path, &times_of(&stat))?;
                let entry = Entry::Dir {
                    mode,
                    opaque: false,
                };
                return self.set(path, entry);
            }
            kind => {
                let copy = self.create_upper(path, libc::O_WRONLY)?;
                let made = (|| {
                    let entry = match kind {
                        libc::S_IFREG => {
                            self.copy_file(path, &stat, copy.as_fd())?;
                            Entry::File { mode }
                        }
                        libc::S_IFLNK => Entry::Symlink {
                            target: sys::readlink(lower.as_fd(), c"").map_err(errno)?,
                        },
                        _ => Entry::Special {
                            mode: stat.st_mode,
                            rdev: encode(stat.st_rdev),
                        },
                    };
                    self.times.set(path, &times_of(&stat))?;
                    Ok((entry, self.ready_moves(path)?))
                })();
                // Removing the copy may need the descriptor it holds, as
                // when the move failed with `EMFILE`.
                drop(copy);
                match made {
                    Ok(made) => made,
                    Err(error) => {
                        // A copy with no entry would read as a directory.
                        let _ = self.unlink_upper(path, 0);
                        return Err(error);
                    }
                }
            }
        };
        self.keep(path, entry)?;
        // What the guest has open of your file reads the copy from now on,
        // and its locks are taken there, as well as the ones it holds. The
        // locks it holds on your file hold until it lets them go.
        for (key, fd) in moves.locks {
            if let Some(locks) = self.locks.get_mut(&key) {
                self.held.push((key, std::mem::replace(&mut locks.fd, fd)));
            }
        }
        for (id, fd) in moves.readers {
            if let Some(Handle::File {
                fd: open,
                lower,
                kept,
                ..
            }) = self.handles.get_mut(&id)
            {
                *kept = Some(std::mem::replace(open, fd));
                *lower = false;
            }
        }
        Ok(())
    }

    /// Opens the agent's copy at `path` for each lock owner and each reader
    /// the guest has of your file there, and takes their locks on it, so
    /// that the copy's record is kept only once nothing can fail its move.
    fn ready_moves(&self, path: &[u8]) -> Result<Moves, i32> {
        let mut moves = Moves::default();
        let Some(node) = self.paths.get(path).copied() else {
            return Ok(moves);
        };
        for (&key, locks) in self.locks.range((node, 0)..=(node, u64::MAX)) {
            let fd = self.open_moved(path, LOCK_FLAGS)?;
            locks.take(fd.as_fd())?;
            moves.locks.push((key, fd));
        }
        for (&id, handle) in &self.handles {
            if let Handle::File {
                node: open,
                lower: true,
                flock,
                ..
            } = handle
                && *open == node
            {
                let fd = self.open_moved(path, READ_FLAGS)?;
                if let Some(operation) = *flock {
                    sys::flock(fd.as_fd(), operation | libc::LOCK_NB).map_err(errno)?;
                }
                moves.readers.push((id, fd));
            }
        }
        Ok(moves)
    }

    /// Opens the agent's copy at `path` for what the guest moves there.
    fn open_moved(&self, path: &[u8], flags: i32) -> Result<OwnedFd, i32> {
        MOVING.with(|moving| moving.borrow_mut().as_mut().map_or(Ok(()), |hook| hook()))?;
        self.open_upper(path, flags)
    }

    /// Removes `path` from the tree.
    fn unlink_upper(&self, path: &[u8], flags: i32) -> Result<(), i32> {
        let (dir, name) = split(path);
        let dir = self.open_upper(dir, libc::O_PATH | libc::O_DIRECTORY)?;
        sys::unlinkat(dir.as_fd(), &c(name), flags).map_err(errno)
    }

    /// Copies your file at `path`, which `stat` describes, to `copy`.
    fn copy_file(&self, path: &[u8], stat: &libc::stat, copy: BorrowedFd) -> Result<(), i32> {
        let from = self.open_lower(path, READ_FLAGS)?;
        let found = sys::fstat(from.as_fd()).map_err(errno)?;
        if (found.st_dev, found.st_ino) != (stat.st_dev, stat.st_ino) {
            return Err(libc::ESTALE);
        }
        copy_contents(from.as_fd(), copy).map_err(errno)
    }

    /// The attributes of `node`, which `view` shows.
    fn attributes(&self, node: u64, view: &View) -> Result<libc::stat, i32> {
        let mut stat = view.stat()?;
        stat.st_ino = node;
        Ok(stat)
    }

    /// Gives the guest a node for `path`, and answers with its entry.
    fn remember(&mut self, path: Vec<u8>) -> Answer {
        let view = self.view(&path)?;
        let node = match self.paths.get(&path) {
            Some(&node) => {
                let found = self.nodes.get_mut(&node).ok_or(libc::EIO)?;
                found.lookups = found.lookups.saturating_add(1);
                node
            }
            None => {
                let node = self.next_node;
                self.next_node += 1;
                self.paths.insert(path.clone(), node);
                self.nodes.insert(
                    node,
                    Node {
                        path: Some(path),
                        lookups: 1,
                    },
                );
                node
            }
        };
        Ok(entry(node, &self.attributes(node, &view)?, self.owner))
    }

    fn lookup(&mut self, parent: u64, body: &[u8]) -> Answer {
        let name = entry_name(body)?;
        let dir = self.directory(parent)?;
        self.remember(join(&dir, name.as_bytes()))
    }

    fn forget(&mut self, node: u64, count: u64) {
        if node == ROOT {
            return;
        }
        let Some(found) = self.nodes.get_mut(&node) else {
            return;
        };
        found.lookups = found.lookups.saturating_sub(count);
        if found.lookups == 0 {
            if let Some(path) = &found.path {
                self.paths.remove(path);
            }
            self.nodes.remove(&node);
            self.locks.retain(|(locked, _), _| *locked != node);
            self.held.retain(|((locked, _), _)| *locked != node);
        }
    }

    /// `struct fuse_getattr_in`. A node that is gone still has the file a
    /// handle holds open.
    fn getattr(&self, node: u64, body: &[u8]) -> Answer {
        match self.path(node) {
            Ok(path) => Ok(attr_out(
                &self.attributes(node, &self.view(&path)?)?,
                self.owner,
            )),
            Err(libc::ENOENT) if u32_at(body, 0).ok_or(libc::EINVAL)? & GETATTR_FH != 0 => {
                let Some(Handle::File { fd, mode, .. }) =
                    self.handles.get(&u64_at(body, 8).ok_or(libc::EINVAL)?)
                else {
                    return Err(libc::ENOENT);
                };
                let mut stat = sys::fstat(fd.as_fd()).map_err(errno)?;
                stat.st_mode = *mode;
                stat.st_ino = node;
                Ok(attr_out(&stat, self.owner))
            }
            Err(error) => Err(error),
        }
    }

    fn add_handle(&mut self, handle: Handle) -> u64 {
        let id = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(id, handle);
        id
    }

    fn file(&self, body: &[u8]) -> Result<(&OwnedFd, u64), i32> {
        let handle = u64_at(body, 0).ok_or(libc::EINVAL)?;
        match self.handles.get(&handle) {
            Some(Handle::File { fd, node, .. }) => Ok((fd, *node)),
            Some(Handle::Dir { .. }) => Err(libc::EISDIR),
            None => Err(libc::EBADF),
        }
    }

    /// Opens the file at `path` as `node`, with the guest's `flags`. A write
    /// opens the agent's copy, which it makes first.
    fn open_path(&mut self, node: u64, path: &[u8], flags: i32) -> Result<u64, i32> {
        let view = self.view(path)?;
        let mode = view.stat()?.st_mode;
        if !is(mode, libc::S_IFREG) {
            return Err(libc::EINVAL);
        }
        let lower = !writes(flags) && view.entry.is_none();
        let fd = if writes(flags) {
            self.copy_up(path)?;
            let fd = self.open_upper(path, open_flags(flags) & !libc::O_TRUNC)?;
            if flags & libc::O_TRUNC != 0 {
                sys::ftruncate(fd.as_fd(), 0).map_err(errno)?;
            }
            fd
        } else if lower {
            self.open_lower(path, open_flags(flags))?
        } else {
            self.open_upper(path, open_flags(flags))?
        };
        Ok(self.add_handle(Handle::File {
            fd,
            node,
            mode,
            lower,
            kept: None,
            flock: None,
        }))
    }

    fn open(&mut self, node: u64, body: &[u8]) -> Answer {
        let flags = u32_at(body, 0).ok_or(libc::EINVAL)? as i32;
        let path = self.path(node)?;
        Ok(open_out(self.open_path(node, &path, flags)?))
    }

    /// What the guest lists in the directory at `path`, which `view` shows:
    /// the agent's entries, and yours where it has none.
    fn listing(&self, path: &[u8], view: &View) -> Result<Vec<Dirent>, i32> {
        let mut listed = BTreeMap::new();
        let ino = |name: &[u8], host: u64| {
            self.paths
                .get(&join(path, name))
                .copied()
                .unwrap_or(1 << 63 | host)
        };
        if let Some(upper) = &view.upper {
            for (name, _, host) in names(upper.as_fd())? {
                let kind = match self.entries.get(&join(path, &name)) {
                    Some(Entry::Deleted) => continue,
                    Some(entry) => entry.mode() >> 12,
                    None => libc::S_IFDIR >> 12,
                };
                let ino = ino(&name, host);
                listed.insert(name.clone(), Dirent { ino, kind, name });
            }
        }
        if let Some(lower) = &view.lower {
            for (name, kind, host) in names(lower.as_fd())? {
                if listed.contains_key(&name) || self.entries.contains_key(&join(path, &name)) {
                    continue;
                }
                let ino = ino(&name, host);
                let kind = u32::from(kind);
                listed.insert(name.clone(), Dirent { ino, kind, name });
            }
        }
        Ok(listed.into_values().collect())
    }

    fn opendir(&mut self, node: u64) -> Answer {
        let path = self.path(node)?;
        let view = self.view(&path)?;
        if !view.is_dir()? {
            return Err(libc::ENOTDIR);
        }
        let dot = |name: &[u8]| Dirent {
            ino: node,
            kind: libc::S_IFDIR >> 12,
            name: name.to_vec(),
        };
        let mut entries = vec![dot(b"."), dot(b"..")];
        entries.extend(self.listing(&path, &view)?);
        Ok(open_out(self.add_handle(Handle::Dir { entries })))
    }

    /// Lists a directory as `struct fuse_dirent`s, from the offset the last
    /// listing ended at, as many as fit in the size the guest asks for.
    fn readdir(&self, body: &[u8]) -> Answer {
        let handle = u64_at(body, 0).ok_or(libc::EINVAL)?;
        let Some(Handle::Dir { entries }) = self.handles.get(&handle) else {
            return Err(libc::EBADF);
        };
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = (u32_at(body, 16).ok_or(libc::EINVAL)? as usize).min(MAX_READ);
        let mut out = Vec::new();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (at, dirent) in entries.iter().enumerate().skip(start) {
            if out.len() + (24 + dirent.name.len()).next_multiple_of(8) > size {
                break;
            }
            out.extend(dirent.ino.to_le_bytes());
            out.extend((at as u64 + 1).to_le_bytes());
            out.extend((dirent.name.len() as u32).to_le_bytes());
            out.extend(dirent.kind.to_le_bytes());
            out.extend(&dirent.name);
            out.resize(out.len().next_multiple_of(8), 0);
        }
        Ok(out)
    }

    fn readlink(&self, node: u64) -> Answer {
        let view = self.view(&self.path(node)?)?;
        match (&view.entry, &view.lower) {
            (Some(Entry::Symlink { target }), _) => Ok(target.clone()),
            (None, Some(lower)) if is(view.stat()?.st_mode, libc::S_IFLNK) => {
                sys::readlink(lower.as_fd(), c"").map_err(errno)
            }
            _ => Err(libc::EINVAL),
        }
    }

    /// The path for a new entry `name` in the directory `parent`, which the
    /// guest has nothing at, with the tree's directories above it made, and
    /// your file there recorded.
    fn fresh(&mut self, parent: u64, name: &CStr) -> Result<Vec<u8>, i32> {
        let path = join(&self.directory(parent)?, name.to_bytes());
        match self.view(&path) {
            Ok(_) => return Err(libc::EEXIST),
            Err(libc::ENOENT) => {}
            Err(error) => return Err(error),
        }
        self.make_parents(&path)?;
        self.base(&path, None)?;
        Ok(path)
    }

    /// `struct fuse_create_in`, then the name. Answers with the entry and
    /// the open file.
    fn create(&mut self, parent: u64, body: &[u8]) -> Answer {
        let flags = u32_at(body, 0).ok_or(libc::EINVAL)? as i32;
        let mode = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let umask = u32_at(body, 8).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(16..).ok_or(libc::EINVAL)?)?;
        let path = join(&self.directory(parent)?, name.as_bytes());
        let exists = match self.view(&path) {
            Ok(_) => true,
            Err(libc::ENOENT) => false,
            Err(error) => return Err(error),
        };
        if exists && flags & libc::O_EXCL != 0 {
            return Err(libc::EEXIST);
        }
        if !exists {
            self.fresh(parent, &name)?;
            let fd = self.create_upper(&path, open_flags(flags) & !libc::O_TRUNC)?;
            let mode = mode & !umask & PERMISSIONS;
            self.keep(&path, Entry::File { mode })?;
            let mut out = self.remember(path)?;
            let node = u64_at(&out, 0).ok_or(libc::EIO)?;
            let mode = libc::S_IFREG | mode;
            let handle = Handle::File {
                fd,
                node,
                mode,
                lower: false,
                kept: None,
                flock: None,
            };
            out.extend(open_out(self.add_handle(handle)));
            return Ok(out);
        }
        let mut out = self.remember(path.clone())?;
        let node = u64_at(&out, 0).ok_or(libc::EIO)?;
        match self.open_path(node, &path, flags) {
            Ok(handle) => {
                out.extend(open_out(handle));
                Ok(out)
            }
            Err(error) => {
                self.forget(node, 1);
                Err(error)
            }
        }
    }

    /// `struct fuse_mkdir_in`, then the name.
    fn mkdir(&mut self, parent: u64, body: &[u8]) -> Answer {
        let mode = u32_at(body, 0).ok_or(libc::EINVAL)?;
        let umask = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(8..).ok_or(libc::EINVAL)?)?;
        let path = self.fresh(parent, &name)?;
        self.mkdir_upper(&path)?;
        // What you have at this path stays out of the agent's directory.
        let opaque = self.open_lower(&path, PATH_FLAGS).is_ok();
        let mode = mode & !umask & PERMISSIONS;
        self.keep(&path, Entry::Dir { mode, opaque })?;
        self.remember(path)
    }

    /// Makes `entry`, a symlink or a special file, at `name` in `parent`,
    /// with an empty file in the tree to hold its attributes.
    fn make(&mut self, parent: u64, name: &CStr, entry: Entry) -> Answer {
        let path = self.fresh(parent, name)?;
        self.create_upper(&path, libc::O_WRONLY)?;
        self.keep(&path, entry)?;
        self.remember(path)
    }

    /// The name, then the symlink's target, which only the guest resolves.
    fn symlink(&mut self, parent: u64, body: &[u8]) -> Answer {
        let name = entry_name(body)?;
        let rest = body.get(name.as_bytes().len() + 1..).ok_or(libc::EINVAL)?;
        let target = CStr::from_bytes_until_nul(rest).map_err(|_| libc::EINVAL)?;
        if target.is_empty() {
            return Err(libc::ENOENT);
        }
        let target = target.to_bytes().to_vec();
        self.make(parent, &name, Entry::Symlink { target })
    }

    /// `struct fuse_mknod_in`, then the name.
    fn mknod(&mut self, parent: u64, body: &[u8]) -> Answer {
        let mode = u32_at(body, 0).ok_or(libc::EINVAL)?;
        let rdev = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let umask = u32_at(body, 8).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(16..).ok_or(libc::EINVAL)?)?;
        let permissions = mode & !umask & PERMISSIONS;
        let entry = match mode & libc::S_IFMT {
            libc::S_IFREG => Entry::File { mode: permissions },
            kind @ (libc::S_IFIFO | libc::S_IFSOCK | libc::S_IFCHR | libc::S_IFBLK) => {
                let rdev = match kind {
                    libc::S_IFCHR | libc::S_IFBLK => rdev,
                    _ => 0,
                };
                Entry::Special {
                    mode: kind | permissions,
                    rdev,
                }
            }
            _ => return Err(libc::EINVAL),
        };
        self.make(parent, &name, entry)
    }

    /// Takes every entry beneath `path` away, and returns each one that
    /// isn't a deletion, by what follows `path` in its own path.
    fn take_beneath(&mut self, path: &[u8]) -> Vec<(Vec<u8>, Entry)> {
        let found: Vec<_> = self
            .entries
            .keys()
            .filter(|key| beneath(key, path))
            .cloned()
            .collect();
        found
            .into_iter()
            .filter_map(|key| {
                let entry = self.entries.remove(&key)?;
                (entry != Entry::Deleted).then(|| (key[path.len()..].to_vec(), entry))
            })
            .collect()
    }

    /// Takes the nodes at `path` and beneath it away from their paths, and
    /// with `to`, gives each the same place beneath `to`.
    fn move_nodes(&mut self, path: &[u8], to: Option<&[u8]>) {
        let found: Vec<_> = self
            .paths
            .keys()
            .filter(|key| *key == path || beneath(key, path))
            .cloned()
            .collect();
        for key in found {
            let Some(node) = self.paths.remove(&key) else {
                continue;
            };
            let moved = to.map(|to| [to, &key[path.len()..]].concat());
            if let Some(moved) = &moved {
                self.paths.insert(moved.clone(), node);
            }
            if let Some(found) = self.nodes.get_mut(&node) {
                found.path = moved;
            }
        }
    }

    /// `UNLINK` or `RMDIR`: the name.
    fn remove(&mut self, parent: u64, body: &[u8], dir: bool) -> Answer {
        let name = entry_name(body)?;
        let path = join(&self.directory(parent)?, name.as_bytes());
        let view = self.view(&path)?;
        match (dir, view.is_dir()?) {
            (true, false) => return Err(libc::ENOTDIR),
            (false, true) => return Err(libc::EISDIR),
            (true, true) if !self.listing(&path, &view)?.is_empty() => {
                return Err(libc::ENOTEMPTY);
            }
            _ => {}
        }
        self.base(&path, None)?;
        if view.upper.is_some() {
            self.unlink_upper(&path, if dir { libc::AT_REMOVEDIR } else { 0 })?;
        }
        self.take_beneath(&path);
        self.set(&path, Entry::Deleted)?;
        self.move_nodes(&path, None);
        Ok(Vec::new())
    }

    /// `struct fuse_rename_in`, or with `with_flags`, `struct
    /// fuse_rename2_in`, then the old name and the new one.
    fn rename(&mut self, parent: u64, body: &[u8], with_flags: bool) -> Answer {
        let to = u64_at(body, 0).ok_or(libc::EINVAL)?;
        let (flags, names) = match with_flags {
            true => (u32_at(body, 8).ok_or(libc::EINVAL)?, body.get(16..)),
            false => (0, body.get(8..)),
        };
        // A share of kept state takes the flags overlayfs needs of its
        // upper layer too.
        let allowed = match self.kept() {
            true => libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT,
            false => libc::RENAME_NOREPLACE,
        };
        if flags & !allowed != 0 {
            return Err(libc::EINVAL);
        }
        let (old, new) = two_names(names.ok_or(libc::EINVAL)?)?;
        let from = join(&self.directory(parent)?, old.as_bytes());
        let into = join(&self.directory(to)?, new.as_bytes());
        if flags & libc::RENAME_EXCHANGE != 0 {
            return match flags {
                libc::RENAME_EXCHANGE => self.exchange(&from, &into),
                _ => Err(libc::EINVAL),
            };
        }
        let whiteout = flags & libc::RENAME_WHITEOUT != 0;
        let source = self.view(&from)?;
        let source_dir = source.is_dir()?;
        let target = match self.view(&into) {
            Ok(view) => Some(view),
            Err(libc::ENOENT) => None,
            Err(error) => return Err(error),
        };
        if from == into {
            return Ok(Vec::new());
        }
        if beneath(&into, &from) {
            return Err(libc::EINVAL);
        }
        if let Some(target) = &target {
            if flags & libc::RENAME_NOREPLACE != 0 {
                return Err(libc::EEXIST);
            }
            match (source_dir, target.is_dir()?) {
                (true, false) => return Err(libc::ENOTDIR),
                (false, true) => return Err(libc::EISDIR),
                (true, true) if !self.listing(&into, target)?.is_empty() => {
                    return Err(libc::ENOTEMPTY);
                }
                _ => {}
            }
        }
        if source_dir && source.lower.is_some() {
            return Err(libc::EXDEV);
        }
        self.copy_up(&from)?;
        self.make_parents(&into)?;
        self.base(&into, None)?;
        let target_upper = target.as_ref().is_some_and(|target| target.upper.is_some());
        let (from_dir, from_name) = split(&from);
        let (into_dir, into_name) = split(&into);
        let flags = libc::O_PATH | libc::O_DIRECTORY;
        let from_dir = self.open_upper(from_dir, flags)?;
        let into_dir = self.open_upper(into_dir, flags)?;
        if target_upper && source_dir {
            // It is empty, as the guest sees it, so it is empty in the tree.
            sys::unlinkat(into_dir.as_fd(), &c(into_name), libc::AT_REMOVEDIR).map_err(errno)?;
        }
        sys::renameat2(
            from_dir.as_fd(),
            &c(from_name),
            into_dir.as_fd(),
            &c(into_name),
            0,
        )
        .map_err(errno)?;
        let moved = self.take_beneath(&from);
        let xattrs = self.take_xattrs(&from);
        let replaced = target.is_some() || self.entries.keys().any(|key| beneath(key, &into));
        self.take_beneath(&into);
        self.take_xattrs(&into);
        if replaced {
            // Read in order, the records drop what was there first.
            self.record(line(&into, &Entry::Deleted))?;
        }
        let mut entry = self.entries.remove(&from).ok_or(libc::EIO)?;
        if let Entry::Dir { opaque, .. } = &mut entry {
            *opaque = self.open_lower(&into, PATH_FLAGS).is_ok();
        }
        self.set(&into, entry)?;
        for (rest, entry) in moved {
            let path = [&into[..], &rest].concat();
            self.base(&path, None)?;
            self.set(&path, entry)?;
        }
        for (rest, held) in xattrs {
            self.set_xattrs(&[&into[..], &rest].concat(), held)?;
        }
        self.set(&from, Entry::Deleted)?;
        if whiteout {
            self.create_upper(&from, libc::O_WRONLY)?;
            self.keep(&from, WHITEOUT)?;
        }
        self.move_nodes(&into, None);
        self.move_nodes(&from, Some(&into));
        Ok(Vec::new())
    }

    /// `RENAME_EXCHANGE`, in a share of kept state, where everything the
    /// guest sees is in the tree, with an entry: `from` and `into` trade
    /// places, with everything beneath them.
    fn exchange(&mut self, from: &[u8], into: &[u8]) -> Answer {
        self.view(from)?;
        self.view(into)?;
        if from == into {
            return Ok(Vec::new());
        }
        if beneath(into, from) || beneath(from, into) {
            return Err(libc::EINVAL);
        }
        if !self.entries.contains_key(from) || !self.entries.contains_key(into) {
            return Err(libc::EIO);
        }
        let (from_dir, from_name) = split(from);
        let (into_dir, into_name) = split(into);
        let flags = libc::O_PATH | libc::O_DIRECTORY;
        sys::renameat2(
            self.open_upper(from_dir, flags)?.as_fd(),
            &c(from_name),
            self.open_upper(into_dir, flags)?.as_fd(),
            &c(into_name),
            libc::RENAME_EXCHANGE,
        )
        .map_err(errno)?;
        let mut taken = Vec::new();
        for path in [from, into] {
            let mut entries = vec![(Vec::new(), self.entries.remove(path).ok_or(libc::EIO)?)];
            entries.extend(self.take_beneath(path));
            taken.push((entries, self.take_xattrs(path)));
        }
        // Read in order, the records drop what was at both places first.
        self.record(line(from, &Entry::Deleted))?;
        self.record(line(into, &Entry::Deleted))?;
        for ((entries, xattrs), to) in taken.into_iter().zip([into, from]) {
            for (rest, entry) in entries {
                self.set(&[to, &rest].concat(), entry)?;
            }
            for (rest, held) in xattrs {
                self.set_xattrs(&[to, &rest].concat(), held)?;
            }
        }
        let paths: Vec<_> = (self.paths.keys())
            .filter(|key| [from, into].iter().any(|at| *key == at || beneath(key, at)))
            .cloned()
            .collect();
        let mut moved = Vec::new();
        for key in paths {
            let Some(node) = self.paths.remove(&key) else {
                continue;
            };
            let (at, to) = match key == from || beneath(&key, from) {
                true => (from, into),
                false => (into, from),
            };
            moved.push(([to, &key[at.len()..]].concat(), node));
        }
        for (path, node) in moved {
            if let Some(found) = self.nodes.get_mut(&node) {
                found.path = Some(path.clone());
            }
            self.paths.insert(path, node);
        }
        Ok(Vec::new())
    }

    /// The node `node`'s path, which must show something, in a share of
    /// kept state, whose extended attributes the guest reaches.
    fn xattr_path(&self, node: u64) -> Result<Vec<u8>, i32> {
        if !self.kept() {
            return Err(libc::ENOSYS);
        }
        let path = self.path(node)?;
        self.view(&path)?;
        Ok(path)
    }

    /// `struct fuse_setxattr_in`, then the name and the value.
    fn setxattr(&mut self, node: u64, body: &[u8]) -> Answer {
        let path = self.xattr_path(node)?;
        let size = u32_at(body, 0).ok_or(libc::EINVAL)? as usize;
        let flags = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let name = xattr_name(body.get(8..).ok_or(libc::EINVAL)?)?.to_vec();
        let value = body.get(8 + name.len() + 1..).unwrap_or_default();
        let value = value.get(..size).ok_or(libc::EINVAL)?;
        if size > XATTR_SIZE_MAX {
            return Err(libc::E2BIG);
        }
        if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
            return Err(libc::EINVAL);
        }
        let names = self.xattrs.get(&path).into_iter().flat_map(BTreeMap::keys);
        let listed: usize = names
            .filter(|held| **held != name)
            .map(|held| held.len() + 1)
            .sum();
        if listed + name.len() + 1 > XATTR_LIST_MAX {
            return Err(libc::ENOSPC);
        }
        let held = self
            .xattrs
            .get(&path)
            .is_some_and(|held| held.contains_key(&name));
        if flags & XATTR_CREATE != 0 && held {
            return Err(libc::EEXIST);
        }
        if flags & XATTR_REPLACE != 0 && !held {
            return Err(libc::ENODATA);
        }
        self.record(xattr_line(&path, &name, Some(value)))?;
        self.xattrs
            .entry(path)
            .or_default()
            .insert(name, value.to_vec());
        Ok(Vec::new())
    }

    /// `struct fuse_getxattr_in`, then the name.
    fn getxattr(&self, node: u64, body: &[u8]) -> Answer {
        let path = self.xattr_path(node)?;
        let size = u32_at(body, 0).ok_or(libc::EINVAL)?;
        let name = xattr_name(body.get(8..).ok_or(libc::EINVAL)?)?;
        let value = self.xattrs.get(&path).and_then(|held| held.get(name));
        sized(value.ok_or(libc::ENODATA)?.clone(), size)
    }

    /// `struct fuse_getxattr_in`: each name, ended with a NUL.
    fn listxattr(&self, node: u64, body: &[u8]) -> Answer {
        let path = self.xattr_path(node)?;
        let size = u32_at(body, 0).ok_or(libc::EINVAL)?;
        let names = self.xattrs.get(&path).into_iter().flat_map(BTreeMap::keys);
        sized(
            names.flat_map(|name| [&name[..], b"\0"].concat()).collect(),
            size,
        )
    }

    /// The name.
    fn removexattr(&mut self, node: u64, body: &[u8]) -> Answer {
        let path = self.xattr_path(node)?;
        let name = xattr_name(body)?;
        let held = self.xattrs.get_mut(&path).ok_or(libc::ENODATA)?;
        if !held.contains_key(name) {
            return Err(libc::ENODATA);
        }
        self.record(xattr_line(&path, name, None))?;
        if let Some(held) = self.xattrs.get_mut(&path) {
            held.remove(name);
            if held.is_empty() {
                self.xattrs.remove(&path);
            }
        }
        Ok(Vec::new())
    }

    /// `struct fuse_write_in`, then the data. Your file is open for reading
    /// alone, so a write to it fails.
    fn write(&self, body: &[u8]) -> Answer {
        let (fd, _) = self.file(body)?;
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = u32_at(body, 16).ok_or(libc::EINVAL)? as usize;
        let data = body.get(40..40 + size).ok_or(libc::EINVAL)?;
        let len = sys::pwrite(fd.as_fd(), data, offset).map_err(errno)?;
        let mut out = (len as u32).to_le_bytes().to_vec();
        out.extend([0; 4]);
        Ok(out)
    }

    fn read(&self, body: &[u8]) -> Answer {
        let (fd, _) = self.file(body)?;
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = u32_at(body, 16).ok_or(libc::EINVAL)? as usize;
        let mut data = vec![0; size.min(MAX_READ)];
        let len = sys::pread(fd.as_fd(), &mut data, offset).map_err(errno)?;
        data.truncate(len);
        Ok(data)
    }

    /// `struct fuse_setattr_in`. Each change is to the agent's copy. Owners
    /// don't change, because the agent's aren't the host's, and a mode is a
    /// record.
    fn setattr(&mut self, node: u64, body: &[u8]) -> Answer {
        let field = |at| u64_at(body, at).ok_or(libc::EINVAL);
        let word = |at| u32_at(body, at).ok_or(libc::EINVAL);
        let valid = word(0)?;
        if valid & (FATTR_UID | FATTR_GID) != 0 && !self.keeps_owner(node, valid, body)? {
            return Err(libc::EPERM);
        }
        let path = match self.path(node) {
            Err(libc::ENOENT) if valid & FATTR_FH != 0 => return self.setattr_open(node, body),
            path => path?,
        };
        self.copy_up(&path)?;
        let entry = self.view(&path)?.entry.ok_or(libc::EIO)?;
        let dir = matches!(entry, Entry::Dir { .. });
        if valid & FATTR_SIZE != 0 {
            if dir {
                return Err(libc::EISDIR);
            }
            if !matches!(entry, Entry::File { .. }) {
                return Err(libc::EINVAL);
            }
            let fd =
                self.open_upper(&path, libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
            sys::ftruncate(fd.as_fd(), field(16)?).map_err(errno)?;
        }
        if valid & FATTR_MODE != 0 {
            let new = word(68)? & PERMISSIONS;
            let changed = match entry {
                Entry::File { .. } => Some(Entry::File { mode: new }),
                Entry::Dir { opaque, .. } => Some(Entry::Dir { mode: new, opaque }),
                Entry::Special { mode, rdev } => Some(Entry::Special {
                    mode: mode & libc::S_IFMT | new,
                    rdev,
                }),
                _ => None,
            };
            if let Some(changed) = changed {
                self.set(&path, changed.clone())?;
                if let Entry::File { mode } = changed {
                    self.set_handle_modes(node, libc::S_IFREG | mode);
                }
            }
        }
        let times = FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW;
        if valid & times != 0 {
            let access = time(valid, FATTR_ATIME, FATTR_ATIME_NOW, field(32)?, word(56)?);
            let modify = time(valid, FATTR_MTIME, FATTR_MTIME_NOW, field(40)?, word(60)?);
            self.times.set(&path, &[access, modify])?;
        }
        Ok(attr_out(
            &self.attributes(node, &self.view(&path)?)?,
            self.owner,
        ))
    }

    /// Whether `SETATTR`'s owner, in `body`, is the one the guest sees
    /// `node` have. A share of kept state lets the guest give a file the
    /// owner it has, which changes nothing, as overlayfs does with each file
    /// it copies up.
    fn keeps_owner(&self, node: u64, valid: u32, body: &[u8]) -> Result<bool, i32> {
        if !self.kept() {
            return Ok(false);
        }
        let (uid, gid) = self.owner.guest(&self.view(&self.path(node)?)?.stat()?);
        let word = |at| u32_at(body, at).ok_or(libc::EINVAL);
        Ok((valid & FATTR_UID == 0 || word(76)? == uid)
            && (valid & FATTR_GID == 0 || word(80)? == gid))
    }

    /// `SETATTR` for a node that is gone, through the agent's copy that a
    /// handle holds open. Its mode has no record left to change, so the
    /// handle keeps it.
    fn setattr_open(&mut self, node: u64, body: &[u8]) -> Answer {
        let field = |at| u64_at(body, at).ok_or(libc::EINVAL);
        let word = |at| u32_at(body, at).ok_or(libc::EINVAL);
        let valid = word(0)?;
        let handle = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let Some(Handle::File {
            fd,
            node: open,
            lower: false,
            ..
        }) = self.handles.get(&handle)
        else {
            return Err(libc::ENOENT);
        };
        if *open != node {
            return Err(libc::EBADF);
        }
        if valid & FATTR_SIZE != 0 {
            sys::ftruncate(fd.as_fd(), field(16)?).map_err(errno)?;
        }
        let times = FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW;
        if valid & times != 0 {
            let access = time(valid, FATTR_ATIME, FATTR_ATIME_NOW, field(32)?, word(56)?);
            let modify = time(valid, FATTR_MTIME, FATTR_MTIME_NOW, field(40)?, word(60)?);
            // Its name is gone, so the times process takes the file itself.
            self.times.set_unnamed(fd.as_fd(), &[access, modify])?;
        }
        let mut stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        stat.st_ino = node;
        if valid & FATTR_MODE != 0 {
            self.set_handle_modes(node, libc::S_IFREG | word(68)? & PERMISSIONS);
        }
        let Some(Handle::File { mode, .. }) = self.handles.get(&handle) else {
            return Err(libc::EIO);
        };
        stat.st_mode = *mode;
        Ok(attr_out(&stat, self.owner))
    }

    /// Gives each handle the guest has open on `node` the mode `mode`, for
    /// when its node is gone.
    fn set_handle_modes(&mut self, node: u64, mode: u32) {
        for handle in self.handles.values_mut() {
            if let Handle::File {
                node: open,
                mode: old,
                ..
            } = handle
                && *open == node
            {
                *old = mode;
            }
        }
    }

    /// `struct fuse_fallocate_in`.
    fn fallocate(&self, body: &[u8]) -> Answer {
        let (fd, _) = self.file(body)?;
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let len = u64_at(body, 16).ok_or(libc::EINVAL)?;
        let mode = u32_at(body, 24).ok_or(libc::EINVAL)? as i32;
        sys::fallocate(fd.as_fd(), mode, offset, len).map_err(errno)?;
        Ok(Vec::new())
    }

    /// `struct fuse_fsync_in`.
    fn fsync(&self, body: &[u8]) -> Answer {
        let data_only = u32_at(body, 8).ok_or(libc::EINVAL)? & 1 != 0;
        match self.file(body) {
            Ok((fd, _)) => sys::fsync(fd.as_fd(), data_only).map_err(errno)?,
            Err(libc::EISDIR) => {}
            Err(error) => return Err(error),
        }
        Ok(Vec::new())
    }

    /// The file that holds `owner`'s POSIX locks on `node`, opened the first
    /// time the owner locks it: the agent's copy, open for writing, or your
    /// file, open for reading.
    fn lock_file(&mut self, node: u64, owner: u64) -> Result<&mut Locks, i32> {
        if !self.locks.contains_key(&(node, owner)) {
            let path = self.path(node)?;
            let view = self.view(&path)?;
            if !is(view.stat()?.st_mode, libc::S_IFREG) {
                return Err(libc::EINVAL);
            }
            let fd = match view.entry {
                Some(_) => self.open_upper(&path, LOCK_FLAGS)?,
                None => self.open_lower(&path, READ_FLAGS)?,
            };
            self.locks.insert((node, owner), Locks::new(fd));
        }
        self.locks.get_mut(&(node, owner)).ok_or(libc::EIO)
    }

    /// `struct fuse_lk_in`: the handle, the owner, the lock, and its flags.
    fn setlk(&mut self, body: &[u8], wait: bool) -> Answer {
        let (fd, node) = self.file(body)?;
        let owner = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let flags = u32_at(body, 40).ok_or(libc::EINVAL)?;
        let mut lock = lock_of(body)?;
        if flags & LK_FLOCK != 0 {
            let operation = match i32::from(lock.l_type) {
                libc::F_RDLCK => libc::LOCK_SH,
                libc::F_WRLCK => libc::LOCK_EX,
                _ => libc::LOCK_UN,
            };
            let taken = sys::flock(fd.as_fd(), operation | libc::LOCK_NB);
            // A change that fails has let go of what the handle held.
            let handle = u64_at(body, 0).ok_or(libc::EINVAL)?;
            if let Some(Handle::File { flock, .. }) = self.handles.get_mut(&handle) {
                *flock = (taken.is_ok() && operation != libc::LOCK_UN).then_some(operation);
            }
            taken.map_err(|error| busy(error, wait))?;
            return Ok(Vec::new());
        }
        let locks = self.lock_file(node, owner)?;
        sys::ofd_lock(locks.fd.as_fd(), libc::F_OFD_SETLK, &mut lock)
            .map_err(|error| busy(error, wait))?;
        locks.record(&lock);
        Ok(Vec::new())
    }

    /// Answers with `struct fuse_lk_out`: a lock that stands in the way, or
    /// none.
    fn getlk(&mut self, body: &[u8]) -> Answer {
        let (_, node) = self.file(body)?;
        let owner = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let mut lock = lock_of(body)?;
        let locks = self.lock_file(node, owner)?;
        sys::ofd_lock(locks.fd.as_fd(), libc::F_OFD_GETLK, &mut lock).map_err(errno)?;
        let end = match lock.l_len {
            0 => i64::MAX,
            len => lock.l_start + len - 1,
        };
        let mut out = (lock.l_start as u64).to_le_bytes().to_vec();
        out.extend((end as u64).to_le_bytes());
        out.extend(u32::from(lock.l_type as u16).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        Ok(out)
    }

    /// `struct fuse_flush_in`: the guest closed a file, so its owner's POSIX
    /// locks on it go, as POSIX says.
    fn flush(&mut self, body: &[u8]) -> Answer {
        let (_, node) = self.file(body)?;
        let owner = u64_at(body, 16).ok_or(libc::EINVAL)?;
        self.locks.remove(&(node, owner));
        self.held.retain(|(key, _)| *key != (node, owner));
        Ok(Vec::new())
    }

    fn statfs(&self) -> Answer {
        statfs(self.root.as_ref().unwrap_or(&self.pending).as_fd())
    }
}

/// The answer to a request, or `None` for one that gets no answer.
pub(super) fn dispatch(share: &mut Cow, request: &Request) -> Option<Answer> {
    let Request { node, body, .. } = *request;
    Some(match request.opcode {
        INIT => init(body),
        LOOKUP => share.lookup(node, body),
        GETATTR => share.getattr(node, body),
        SETATTR => share.setattr(node, body),
        READLINK => share.readlink(node),
        SYMLINK => share.symlink(node, body),
        MKNOD => share.mknod(node, body),
        MKDIR => share.mkdir(node, body),
        UNLINK => share.remove(node, body, false),
        RMDIR => share.remove(node, body, true),
        RENAME => share.rename(node, body, false),
        RENAME2 => share.rename(node, body, true),
        LINK => Err(libc::EXDEV),
        OPEN => share.open(node, body),
        READ => share.read(body),
        WRITE => share.write(body),
        CREATE => share.create(node, body),
        FALLOCATE => share.fallocate(body),
        RELEASE | RELEASEDIR => {
            if let Some(handle) = u64_at(body, 0) {
                share.handles.remove(&handle);
            }
            Ok(Vec::new())
        }
        OPENDIR => share.opendir(node),
        READDIR => share.readdir(body),
        STATFS => share.statfs(),
        FSYNC | FSYNCDIR => share.fsync(body),
        FLUSH => share.flush(body),
        GETLK => share.getlk(body),
        SETLK => share.setlk(body, false),
        SETLKW => share.setlk(body, true),
        SETXATTR => share.setxattr(node, body),
        GETXATTR => share.getxattr(node, body),
        LISTXATTR => share.listxattr(node, body),
        REMOVEXATTR => share.removexattr(node, body),
        DESTROY => Ok(Vec::new()),
        FORGET => {
            share.forget(node, u64_at(body, 0).unwrap_or(0));
            return None;
        }
        BATCH_FORGET => {
            let count = u32_at(body, 0).unwrap_or(0) as usize;
            let forgets = body.get(8..).unwrap_or(&[]).as_chunks::<16>().0;
            for one in forgets.iter().take(count) {
                let (Some(node), Some(lookups)) = (u64_at(one, 0), u64_at(one, 8)) else {
                    break;
                };
                share.forget(node, lookups);
            }
            return None;
        }
        INTERRUPT => return None,
        _ => Err(libc::ENOSYS),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd};

    #[test]
    fn a_path_s_parents_run_from_the_root_down() {
        let all: Vec<_> = parents(b"a/b/c").collect();
        assert_eq!(all, [&b""[..], b"a", b"a/b"]);
        assert_eq!(parents(b"a").collect::<Vec<_>>(), [&b""[..]]);
        assert_eq!(parents(b"").count(), 0);
        assert_eq!(split(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split(b"c"), (&b""[..], &b"c"[..]));
        assert!(beneath(b"a/b", b"a"));
        assert!(beneath(b"a", b""));
        assert!(!beneath(b"ab", b"a"));
        assert!(!beneath(b"a", b"a"));
        assert!(!beneath(b"", b""));
    }

    #[test]
    fn a_device_reaches_a_record_as_its_major_and_minor() {
        let null = libc::makedev(1, 3);
        let big = libc::makedev(300, 70_000);
        let entry = |rdev| Entry::Special {
            mode: libc::S_IFCHR | 0o600,
            rdev: encode(rdev),
        };
        assert_eq!(line(b"null", &entry(null)), b"CHAR \"null\" 600 1 3");
        assert_eq!(line(b"big", &entry(big)), b"CHAR \"big\" 600 300 70000");
    }

    fn memfd() -> OwnedFd {
        // SAFETY: the name is a C string, and the call returns a new fd.
        let fd = unsafe { libc::memfd_create(c"locks".as_ptr(), 0) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a new fd that nothing else owns.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    /// Opens the file behind `fd` again, as a new open file description.
    fn again(fd: &OwnedFd) -> OwnedFd {
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
            .into()
    }

    /// The type of the lock that another owner sees on `at`, and on until
    /// the end of the file.
    fn seen(probe: &OwnedFd, at: i64) -> (i16, i16) {
        let test = |len| {
            // SAFETY: `struct flock` is plain data, valid when zeroed.
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as i16;
            lock.l_start = at;
            lock.l_len = len;
            sys::ofd_lock(probe.as_fd(), libc::F_OFD_GETLK, &mut lock).unwrap();
            lock.l_type
        };
        (test(1), test(0))
    }

    #[test]
    fn locks_taken_again_match_what_the_kernel_holds() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below) as i64
        };
        for _ in 0..200 {
            let kernel = memfd();
            let mut locks = Locks::new(again(&kernel));
            for _ in 0..12 {
                // SAFETY: `struct flock` is plain data, valid when zeroed.
                let mut lock: libc::flock = unsafe { std::mem::zeroed() };
                lock.l_type =
                    [libc::F_RDLCK, libc::F_WRLCK, libc::F_UNLCK][next(3) as usize] as i16;
                // Some ranges end where the largest offset does.
                (lock.l_start, lock.l_len) = match next(4) {
                    0 => (i64::MAX - 3 + next(3), next(2)),
                    _ => (next(16), next(6)),
                };
                sys::ofd_lock(locks.fd.as_fd(), libc::F_OFD_SETLK, &mut lock).unwrap();
                locks.record(&lock);
            }
            let copy = memfd();
            locks.take(copy.as_fd()).unwrap();
            let (held, taken) = (again(&kernel), again(&copy));
            for at in (0..24).chain(i64::MAX - 4..i64::MAX) {
                assert_eq!(
                    seen(&held, at),
                    seen(&taken, at),
                    "at {at}: {:?}",
                    locks.ranges
                );
            }
            let mut sorted = locks.ranges.clone();
            sorted.sort();
            assert!(
                sorted.windows(2).all(|pair| pair[0].1 < pair[1].0
                    && (pair[0].2 != pair[1].2 || pair[0].1 + 1 < pair[1].0)),
                "{sorted:?}"
            );
        }
    }
}
