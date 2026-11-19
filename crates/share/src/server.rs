//! Answers the guest's FUSE requests for each share, from `<linux/fuse.h>`.
//!
//! Every request comes from the guest, so nothing in it is trusted: a node or
//! a handle it names must be one this server gave it, and a name must be one
//! entry of a directory. Each file is reached from a descriptor the server
//! holds, one name at a time, beneath it and without following a symlink, so
//! nothing a guest sends can reach a host file outside the share's root. A
//! symlink reaches the guest as itself, and the guest resolves it.
//!
//! An `ro` share refuses every write. An `rw` share refuses the writes that
//! SPEC.md's `SHARE` protects, by the place each name has, which [`Place`]
//! works out from the names above it. A copy-on-write share writes none of
//! the host's files, and [`cow`] says where its changes go.
//!
//! File locks pass through: `flock` as `flock` on the guest's open file, and
//! a POSIX lock as an open file description lock on a file the server opens
//! for each lock owner, so the guest's processes don't share one owner on the
//! host. The guest's one vCPU waits for each answer, so a lock never waits:
//! one that would is refused with `EOPNOTSUPP`, as C virtiofsd does.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::rc::Rc;

use crate::landlock::Access;
use crate::sys;

mod cow;

const IN_HEADER: usize = 40;
const OUT_HEADER: usize = 16;
const ROOT: u64 = 1;
/// The most one read, one write, or one directory listing holds.
const MAX_READ: usize = 128 << 10;
const MAX_WRITE: u32 = MAX_READ as u32;
const MAJOR: u32 = 7;
const MINOR: u32 = 31;
const NAME_MAX: usize = 255;

// Opcodes.
const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const GETATTR: u32 = 3;
const SETATTR: u32 = 4;
const READLINK: u32 = 5;
const SYMLINK: u32 = 6;
const MKNOD: u32 = 8;
const MKDIR: u32 = 9;
const UNLINK: u32 = 10;
const RMDIR: u32 = 11;
const RENAME: u32 = 12;
const LINK: u32 = 13;
const OPEN: u32 = 14;
const READ: u32 = 15;
const WRITE: u32 = 16;
const STATFS: u32 = 17;
const RELEASE: u32 = 18;
const FSYNC: u32 = 20;
const FLUSH: u32 = 25;
const INIT: u32 = 26;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;
const FSYNCDIR: u32 = 30;
const GETLK: u32 = 31;
const SETLK: u32 = 32;
const SETLKW: u32 = 33;
const CREATE: u32 = 35;
const INTERRUPT: u32 = 36;
const DESTROY: u32 = 38;
const BATCH_FORGET: u32 = 42;
const FALLOCATE: u32 = 43;
const RENAME2: u32 = 45;
/// Every request that would change a file or a directory.
const WRITES: [u32; 17] = [
    SETATTR, SYMLINK, MKNOD, MKDIR, UNLINK, RMDIR, RENAME, LINK, WRITE, 21, // SETXATTR
    24, // REMOVEXATTR
    CREATE, FALLOCATE, RENAME2, 47, // COPY_FILE_RANGE
    51, // TMPFILE
    48, // SETUPMAPPING
];

/// `FUSE_AUTO_INVAL_DATA`: the guest drops a file's cached pages when its
/// time changes, so a file changed on the host reads as it is now.
const AUTO_INVAL_DATA: u32 = 1 << 12;
/// The guest passes its POSIX locks and its `flock`s here.
const POSIX_LOCKS: u32 = 1 << 1;
const FLOCK_LOCKS: u32 = 1 << 10;
/// A lock request that is a `flock`.
const LK_FLOCK: u32 = 1;

// `SETATTR`'s fields, from `struct fuse_setattr_in`.
const FATTR_MODE: u32 = 1 << 0;
const FATTR_UID: u32 = 1 << 1;
const FATTR_GID: u32 = 1 << 2;
const FATTR_SIZE: u32 = 1 << 3;
const FATTR_ATIME: u32 = 1 << 4;
const FATTR_MTIME: u32 = 1 << 5;
const FATTR_FH: u32 = 1 << 6;
const FATTR_ATIME_NOW: u32 = 1 << 7;
const FATTR_MTIME_NOW: u32 = 1 << 8;

/// The entries of a git directory that git reads to run hooks and
/// configured commands, folded.
const GIT_FILES: [&str; 5] = ["config", "config.worktree", "commondir", "gitdir", "hooks"];
const AGENTFILES: [&str; 2] = ["agentfile", "agentfile.lock"];
const DOT_GIT: &str = ".git";

/// What a share allows. A copy-on-write share keeps the agent's changes
/// in a directory of its own, `P`.
#[derive(Debug)]
pub enum Mode<P = OwnedFd> {
    ReadOnly,
    ReadWrite,
    CopyOnWrite(P),
}

/// A share's root, as the server takes it: a directory, and what the share
/// allows. A file share shows one `file` in the directory, by its name, and
/// nothing else of it.
#[derive(Debug)]
pub struct Root<F = OwnedFd> {
    pub directory: F,
    pub file: Option<CString>,
    pub mode: Mode<F>,
}

impl<F> Root<F> {
    /// The share of the directory `directory`.
    pub fn new(directory: F, mode: Mode<F>) -> Self {
        Self {
            directory,
            file: None,
            mode,
        }
    }

    /// The share of the file `name` in `directory`.
    pub fn file(directory: F, name: CString, mode: Mode<F>) -> Self {
        Self {
            directory,
            file: Some(name),
            mode,
        }
    }
}

impl<F: AsFd> Root<F> {
    /// The same root, borrowing its descriptors.
    pub fn as_fd(&self) -> Root<BorrowedFd<'_>> {
        Root {
            directory: self.directory.as_fd(),
            file: self.file.clone(),
            mode: match &self.mode {
                Mode::ReadOnly => Mode::ReadOnly,
                Mode::ReadWrite => Mode::ReadWrite,
                Mode::CopyOnWrite(pending) => Mode::CopyOnWrite(pending.as_fd()),
            },
        }
    }
}

/// Where a file sits, for what an `rw` share protects. It follows from the
/// place of the directory above, and the file's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Plain,
    /// A git directory. A `nested` one is beneath `modules` or `worktrees`,
    /// where git keeps other git directories, possibly under names with
    /// several parts, so every directory beneath it counts as one too.
    Git {
        nested: bool,
    },
    /// `modules` or `worktrees` in a git directory.
    Holder,
    /// Read-only, with everything beneath it.
    Fixed,
}

impl Place {
    /// The place of `name` in a directory at this place.
    fn of(self, name: &[u8]) -> Place {
        let folded = fold(name);
        match (self, folded.as_deref()) {
            (Place::Fixed, _) => Place::Fixed,
            (_, Some(name)) if AGENTFILES.contains(&name) => Place::Fixed,
            (_, Some(DOT_GIT)) => Place::Git { nested: false },
            (Place::Git { .. }, Some(name)) if GIT_FILES.contains(&name) => Place::Fixed,
            (Place::Git { .. }, Some("modules" | "worktrees")) => Place::Holder,
            (Place::Holder | Place::Git { nested: true }, _) => Place::Git { nested: true },
            _ => Place::Plain,
        }
    }

    /// The place of the file `name` is, with the file's own mode: a `.git`
    /// that isn't a directory can't be changed.
    fn of_file(self, name: &[u8], mode: u32) -> Place {
        let dot_git = fold(name).as_deref() == Some(DOT_GIT);
        match self.of(name) {
            Place::Git { .. } if dot_git && !is(mode, libc::S_IFDIR) => Place::Fixed,
            place => place,
        }
    }

    /// Whether the entry `name` in a directory at this place can't be made,
    /// removed, or renamed.
    fn fixes(self, name: &[u8]) -> bool {
        self.of(name) == Place::Fixed || fold(name).as_deref() == Some(DOT_GIT)
    }

    /// Whether a file of `mode` may land at `name` in a directory at this
    /// place. A directory or a symlink lands only where git looks for no
    /// git directory, so nothing the agent wrote becomes one, or leads to
    /// one.
    fn takes(self, name: &[u8], mode: u32) -> bool {
        let leads = is(mode, libc::S_IFDIR) || is(mode, libc::S_IFLNK);
        !leads || self.of(name) == Place::Plain
    }
}

/// A name as a case-insensitive filesystem compares it, as far as the names
/// protected here go: ASCII letters in lowercase, the non-ASCII letters that
/// fold to them, and without the code points that HFS+ ignores, as git
/// knows them. `None` for a name that isn't UTF-8, which is none of them.
fn fold(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    let mut folded = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\u{200c}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{206a}'..='\u{206f}'
            | '\u{feff}' => {}
            // The Kelvin sign, and the long s.
            '\u{212a}' => folded.push('k'),
            '\u{17f}' => folded.push('s'),
            // Ligatures that fold to more than one letter.
            '\u{fb00}' => folded.push_str("ff"),
            '\u{fb01}' => folded.push_str("fi"),
            '\u{fb02}' => folded.push_str("fl"),
            '\u{fb03}' => folded.push_str("ffi"),
            '\u{fb04}' => folded.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => folded.push_str("st"),
            c => folded.push(c.to_ascii_lowercase()),
        }
    }
    Some(folded)
}

/// A host file the guest has looked up.
struct Node {
    /// The file itself, opened with `O_PATH`, so it reads nothing.
    fd: Rc<OwnedFd>,
    /// The directory it was found in, and its name there, which opening it
    /// for reading starts from.
    parent: Rc<OwnedFd>,
    name: CString,
    mode: u32,
    device: u64,
    inode: u64,
    lookups: u64,
    place: Place,
}

/// A file or a directory the guest has open.
struct Handle {
    fd: OwnedFd,
    node: u64,
}

/// One share: its root, and what the guest holds of it.
struct Share {
    root: Rc<OwnedFd>,
    owner: Owner,
    read_only: bool,
    /// For a file share, the file's name in the root, and the file, opened
    /// with `O_PATH`. The share shows that file alone, and no other the
    /// name comes to lead to.
    only: Option<(CString, OwnedFd)>,
    nodes: BTreeMap<u64, Node>,
    /// The node each host file has, by device and inode.
    known: BTreeMap<(u64, u64), u64>,
    handles: BTreeMap<u64, Handle>,
    /// The file that holds each guest lock owner's POSIX locks, by node and
    /// owner.
    locks: BTreeMap<(u64, u64), OwnedFd>,
    next_node: u64,
    next_handle: u64,
}

/// Answers every share's requests.
pub struct Server {
    shares: Vec<Served>,
}

/// A share, as the server serves it.
enum Served {
    /// The host's own files, read-only or read-write.
    Direct(Share),
    CopyOnWrite(cow::Cow),
}

/// The request's fields that every answer depends on.
struct Request<'a> {
    opcode: u32,
    unique: u64,
    node: u64,
    body: &'a [u8],
}

type Answer = Result<Vec<u8>, i32>;

fn errno(error: io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The agent's uid and gid in the guest.
const AGENT: u32 = 1000;
/// `nobody`, the owner the guest sees for every other uid and gid.
const NOBODY: u32 = 65534;

/// Who owns a file, as the guest sees it. The guest checks each access
/// against the owner the server reports, so the server's own user and
/// group appear as the agent's. Every other owner appears as `nobody`, as
/// in a user namespace, so none of them passes for the agent.
#[derive(Clone, Copy)]
struct Owner {
    uid: u32,
    gid: u32,
}

impl Owner {
    fn of_server() -> Self {
        // SAFETY: both calls take nothing, and can't fail.
        unsafe {
            Self {
                uid: libc::geteuid(),
                gid: libc::getegid(),
            }
        }
    }

    fn guest(self, stat: &libc::stat) -> (u32, u32) {
        let one = |host: u32, mine: u32| if host == mine { AGENT } else { NOBODY };
        (one(stat.st_uid, self.uid), one(stat.st_gid, self.gid))
    }
}

/// `struct fuse_attr`.
fn attr(stat: &libc::stat, owner: Owner) -> Vec<u8> {
    let mut out = Vec::with_capacity(88);
    out.extend(stat.st_ino.to_le_bytes());
    out.extend((stat.st_size as u64).to_le_bytes());
    out.extend((stat.st_blocks as u64).to_le_bytes());
    out.extend((stat.st_atime as u64).to_le_bytes());
    out.extend((stat.st_mtime as u64).to_le_bytes());
    out.extend((stat.st_ctime as u64).to_le_bytes());
    out.extend((stat.st_atime_nsec as u32).to_le_bytes());
    out.extend((stat.st_mtime_nsec as u32).to_le_bytes());
    out.extend((stat.st_ctime_nsec as u32).to_le_bytes());
    out.extend(stat.st_mode.to_le_bytes());
    out.extend((stat.st_nlink as u32).to_le_bytes());
    let (uid, gid) = owner.guest(stat);
    out.extend(uid.to_le_bytes());
    out.extend(gid.to_le_bytes());
    out.extend((stat.st_rdev as u32).to_le_bytes());
    out.extend((stat.st_blksize as u32).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out
}

/// `struct fuse_entry_out` or `struct fuse_attr_out`, with no time to
/// cache, so the guest asks again each time and sees the host's changes.
fn entry(node: u64, stat: &libc::stat, owner: Owner) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend(node.to_le_bytes());
    out.extend([0; 32]);
    out.extend(attr(stat, owner));
    out
}

fn attr_out(stat: &libc::stat, owner: Owner) -> Vec<u8> {
    let mut out = vec![0; 16];
    out.extend(attr(stat, owner));
    out
}

fn open_out(handle: u64) -> Vec<u8> {
    let mut out = handle.to_le_bytes().to_vec();
    out.extend([0; 8]);
    out
}

fn is(mode: u32, kind: u32) -> bool {
    mode & libc::S_IFMT == kind
}

/// A name the guest looks up, which must name one entry of a directory.
fn entry_name(body: &[u8]) -> Result<CString, i32> {
    let name = CStr::from_bytes_until_nul(body).map_err(|_| libc::EINVAL)?;
    let bytes = name.to_bytes();
    if bytes.is_empty() || bytes.len() > NAME_MAX || bytes.contains(&b'/') {
        return Err(libc::EINVAL);
    }
    if bytes == b"." || bytes == b".." {
        return Err(libc::ENOENT);
    }
    Ok(name.to_owned())
}

/// Two names, one after the other, as a rename sends them.
fn two_names(body: &[u8]) -> Result<(CString, CString), i32> {
    let first = entry_name(body)?;
    let rest = body.get(first.as_bytes().len() + 1..).ok_or(libc::EINVAL)?;
    Ok((first, entry_name(rest)?))
}

/// The flags of a guest's open that reach the host. The rest can't apply to
/// a file that exists, or are the server's to choose.
fn open_flags(flags: i32) -> i32 {
    flags & (libc::O_ACCMODE | libc::O_APPEND | libc::O_TRUNC) | libc::O_NOFOLLOW | libc::O_NONBLOCK
}

fn writes(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY || flags & (libc::O_TRUNC | libc::O_APPEND) != 0
}

/// Whether a file can't change in place because it has another name, which
/// may be a protected one the guest hasn't looked up.
fn aliased(stat: &libc::stat) -> bool {
    !is(stat.st_mode, libc::S_IFDIR) && stat.st_nlink > 1
}

/// A time from `SETATTR`, or the one that leaves it as it is.
fn time(valid: u32, set: u32, now: u32, seconds: u64, nanoseconds: u32) -> libc::timespec {
    let nanoseconds = if valid & now != 0 {
        libc::UTIME_NOW
    } else if valid & set != 0 {
        i64::from(nanoseconds)
    } else {
        libc::UTIME_OMIT
    };
    libc::timespec {
        tv_sec: seconds as i64,
        tv_nsec: nanoseconds,
    }
}

/// The lock a `struct fuse_lk_in` asks about, from offset 16: its range,
/// where an end of `OFFSET_MAX` is the end of the file, and its kind.
fn lock_of(body: &[u8]) -> Result<libc::flock, i32> {
    let start = u64_at(body, 16).ok_or(libc::EINVAL)?;
    let end = u64_at(body, 24).ok_or(libc::EINVAL)?;
    let kind = u32_at(body, 32).ok_or(libc::EINVAL)?;
    let start = i64::try_from(start).map_err(|_| libc::EINVAL)?;
    let len = match i64::try_from(end) {
        Ok(i64::MAX) | Err(_) => 0,
        Ok(end) if end < start => return Err(libc::EINVAL),
        Ok(end) => end - start + 1,
    };
    let kind = match kind {
        0 => libc::F_RDLCK,
        1 => libc::F_WRLCK,
        2 => libc::F_UNLCK,
        _ => return Err(libc::EINVAL),
    };
    // SAFETY: `struct flock` is plain data, valid when zeroed.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as i16;
    lock.l_whence = libc::SEEK_SET as i16;
    lock.l_start = start;
    lock.l_len = len;
    Ok(lock)
}

/// The error for a lock that another holds. A guest that would wait gets
/// `EOPNOTSUPP`, because the server answers one request at a time.
fn busy(error: io::Error, wait: bool) -> i32 {
    match errno(error) {
        libc::EAGAIN | libc::EACCES if wait => libc::EOPNOTSUPP,
        libc::EAGAIN | libc::EACCES => libc::EAGAIN,
        error => error,
    }
}

impl Share {
    fn new(
        root: OwnedFd,
        file: Option<CString>,
        read_only: bool,
        place: Place,
        owner: Owner,
    ) -> io::Result<Self> {
        let stat = sys::fstat(root.as_fd())?;
        if !is(stat.st_mode, libc::S_IFDIR) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let only = match file {
            None => None,
            Some(name) => {
                let flags = libc::O_PATH | libc::O_NOFOLLOW;
                let fd = sys::open_beneath(root.as_fd(), &name, flags)?;
                let mode = sys::fstat(fd.as_fd())?.st_mode;
                if is(mode, libc::S_IFDIR) || is(mode, libc::S_IFLNK) {
                    return Err(io::Error::other("a file share's file isn't a file"));
                }
                Some((name, fd))
            }
        };
        let root = Rc::new(root);
        let node = Node {
            fd: Rc::clone(&root),
            parent: Rc::clone(&root),
            name: c".".to_owned(),
            mode: stat.st_mode,
            device: stat.st_dev,
            inode: stat.st_ino,
            lookups: 1,
            place,
        };
        Ok(Self {
            root,
            owner,
            read_only,
            only,
            nodes: BTreeMap::from([(ROOT, node)]),
            known: BTreeMap::from([((stat.st_dev, stat.st_ino), ROOT)]),
            handles: BTreeMap::new(),
            locks: BTreeMap::new(),
            next_node: ROOT + 1,
            next_handle: 1,
        })
    }

    fn node(&self, node: u64) -> Result<&Node, i32> {
        self.nodes.get(&node).ok_or(libc::ESTALE)
    }

    /// A directory node's descriptor and place.
    fn directory(&self, node: u64) -> Result<(Rc<OwnedFd>, Place), i32> {
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFDIR) {
            return Err(libc::ENOTDIR);
        }
        Ok((Rc::clone(&found.fd), found.place))
    }

    fn handle(&self, body: &[u8]) -> Result<&Handle, i32> {
        let handle = u64_at(body, 0).ok_or(libc::EINVAL)?;
        self.handles.get(&handle).ok_or(libc::EBADF)
    }

    fn add_handle(&mut self, fd: OwnedFd, node: u64) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(handle, Handle { fd, node });
        handle
    }

    /// Opens the file a node is, by the name it was last found by, and
    /// checks that the name still leads to it.
    fn reopen(&self, found: &Node, flags: i32) -> Result<OwnedFd, i32> {
        // Landlock refuses any other file, so this one is told apart first.
        if let Some((_, file)) = &self.only {
            still_the_file(file, &found.parent, &found.name)?;
        }
        let fd = sys::open_beneath(found.parent.as_fd(), &found.name, flags).map_err(errno)?;
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        if (stat.st_dev, stat.st_ino) != (found.device, found.inode) {
            return Err(libc::ESTALE);
        }
        Ok(fd)
    }

    /// Gives the guest a node for `name` in `directory`, and answers with
    /// its entry. `fd` is the file, with `O_PATH`, and `place` is the
    /// directory's. A file the server knows keeps its node, and a fixed one
    /// stays fixed, whichever name it is found by.
    fn remember(
        &mut self,
        directory: Rc<OwnedFd>,
        place: Place,
        name: CString,
        fd: OwnedFd,
    ) -> Answer {
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        let place = place.of_file(name.as_bytes(), stat.st_mode);
        let key = (stat.st_dev, stat.st_ino);
        if let Some(&found) = self.known.get(&key) {
            let node = self.nodes.get_mut(&found).ok_or(libc::EIO)?;
            node.lookups = node.lookups.saturating_add(1);
            if found != ROOT {
                // A file opens from the name it was last found by.
                node.parent = directory;
                node.name = name;
                if node.place != Place::Fixed {
                    node.place = place;
                }
            }
            return Ok(entry(found, &stat, self.owner));
        }
        let id = self.next_node;
        self.next_node += 1;
        self.known.insert(key, id);
        self.nodes.insert(
            id,
            Node {
                fd: Rc::new(fd),
                parent: directory,
                name,
                mode: stat.st_mode,
                device: stat.st_dev,
                inode: stat.st_ino,
                lookups: 1,
                place,
            },
        );
        Ok(entry(id, &stat, self.owner))
    }

    /// Answers with the entry for `name` in the directory `parent`.
    fn lookup(&mut self, parent: u64, body: &[u8]) -> Answer {
        let name = entry_name(body)?;
        if let Some((only, file)) = &self.only
            && parent == ROOT
        {
            if name != *only {
                return Err(libc::ENOENT);
            }
            // The file that was checked is the one the guest gets.
            let fd = still_the_file(file, &self.root, &name)?;
            let (root, place) = (Rc::clone(&self.root), self.node(ROOT)?.place);
            return self.remember(root, place, name, fd);
        }
        let (directory, place) = {
            let found = self.node(parent)?;
            (Rc::clone(&found.fd), found.place)
        };
        self.found(directory, place, name)
    }

    /// Answers with the entry for `name` in `directory`, whose place is
    /// `place`.
    fn found(&mut self, directory: Rc<OwnedFd>, place: Place, name: CString) -> Answer {
        let flags = libc::O_PATH | libc::O_NOFOLLOW;
        let fd = sys::open_beneath(directory.as_fd(), &name, flags).map_err(errno)?;
        self.remember(directory, place, name, fd)
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
            let key = (found.device, found.inode);
            self.nodes.remove(&node);
            self.known.remove(&key);
            self.locks.retain(|(locked, _), _| *locked != node);
        }
    }

    fn open(&mut self, node: u64, body: &[u8]) -> Answer {
        let flags = u32_at(body, 0).ok_or(libc::EINVAL)? as i32;
        if writes(flags) && self.read_only {
            return Err(libc::EROFS);
        }
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFREG) {
            return Err(libc::EINVAL);
        }
        if writes(flags) && found.place == Place::Fixed {
            return Err(libc::EPERM);
        }
        // The name may have moved on to another file since the lookup, so
        // the file is truncated only once it is known to be this one.
        let fd = self.reopen(found, open_flags(flags) & !libc::O_TRUNC)?;
        if writes(flags) && aliased(&sys::fstat(fd.as_fd()).map_err(errno)?) {
            return Err(libc::EPERM);
        }
        if flags & libc::O_TRUNC != 0 {
            sys::ftruncate(fd.as_fd(), 0).map_err(errno)?;
        }
        Ok(open_out(self.add_handle(fd, node)))
    }

    fn opendir(&mut self, node: u64) -> Answer {
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFDIR) {
            return Err(libc::ENOTDIR);
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let fd = sys::open_beneath(found.fd.as_fd(), c".", flags).map_err(errno)?;
        Ok(open_out(self.add_handle(fd, node)))
    }

    fn read(&self, body: &[u8]) -> Answer {
        let fd = self.handle(body)?.fd.as_fd();
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = u32_at(body, 16).ok_or(libc::EINVAL)? as usize;
        let mut data = vec![0; size.min(MAX_READ)];
        let len = sys::pread(fd, &mut data, offset).map_err(errno)?;
        data.truncate(len);
        Ok(data)
    }

    /// Lists a directory as `struct fuse_dirent`s, from the offset the last
    /// listing ended at, as many as fit in the size the guest asks for.
    fn readdir(&self, body: &[u8]) -> Answer {
        let fd = self.handle(body)?.fd.as_fd();
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = (u32_at(body, 16).ok_or(libc::EINVAL)? as usize).min(MAX_READ);
        let mut raw = vec![0; size];
        let len = sys::read_directory(fd, &mut raw, offset).map_err(errno)?;
        let mut out = Vec::new();
        let mut at = 0;
        // `struct linux_dirent64`: inode, offset, record length, type, name.
        while at + 19 < len {
            let record = usize::from(u16::from_le_bytes([raw[at + 16], raw[at + 17]]));
            let Some(name) = (record > 19)
                .then(|| raw.get(at + 19..at + record))
                .flatten()
                .and_then(|bytes| CStr::from_bytes_until_nul(bytes).ok())
            else {
                break;
            };
            let name = name.to_bytes();
            let dirent = (24 + name.len()).next_multiple_of(8);
            if out.len() + dirent > size {
                break;
            }
            out.extend(&raw[at..at + 16]);
            out.extend((name.len() as u32).to_le_bytes());
            out.extend(u32::from(raw[at + 18]).to_le_bytes());
            out.extend(name);
            out.resize(out.len().next_multiple_of(8), 0);
            at += record;
        }
        Ok(out)
    }

    fn readlink(&self, node: u64) -> Answer {
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFLNK) {
            return Err(libc::EINVAL);
        }
        sys::readlink(found.fd.as_fd()).map_err(errno)
    }

    fn statfs(&self) -> Answer {
        statfs(self.root.as_fd())
    }

    /// `struct fuse_create_in`, then the name. Answers with the entry and
    /// the open file.
    fn create(&mut self, parent: u64, body: &[u8]) -> Answer {
        let flags = u32_at(body, 0).ok_or(libc::EINVAL)? as i32;
        let mode = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let umask = u32_at(body, 8).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(16..).ok_or(libc::EINVAL)?)?;
        let (directory, place) = self.directory(parent)?;
        if place.fixes(name.as_bytes()) {
            return Err(libc::EPERM);
        }
        // A file that was there is truncated only once it is known not to be
        // a fixed one.
        let truncate = flags & libc::O_TRUNC != 0;
        let flags = open_flags(flags) & !libc::O_TRUNC | flags & libc::O_EXCL;
        let fd = sys::create_beneath(directory.as_fd(), &name, flags, mode & !umask & 0o7777)
            .map_err(errno)?;
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        // A file that was there may be a fixed one, by another name.
        let fixed = self
            .known
            .get(&(stat.st_dev, stat.st_ino))
            .and_then(|node| self.nodes.get(node))
            .is_some_and(|node| node.place == Place::Fixed);
        if fixed || aliased(&stat) {
            return Err(libc::EPERM);
        }
        let mut out = self.found(directory, place, name)?;
        let node = u64_at(&out, 0).ok_or(libc::EIO)?;
        if self.node(node)?.inode != stat.st_ino {
            return Err(libc::ESTALE);
        }
        if truncate {
            sys::ftruncate(fd.as_fd(), 0).map_err(errno)?;
        }
        out.extend(open_out(self.add_handle(fd, node)));
        Ok(out)
    }

    /// Refuses a change through an open file that is fixed, or has gained
    /// another name since it was opened.
    fn changes(&self, handle: &Handle) -> Result<(), i32> {
        let fixed = self.node(handle.node)?.place == Place::Fixed;
        if fixed || aliased(&sys::fstat(handle.fd.as_fd()).map_err(errno)?) {
            return Err(libc::EPERM);
        }
        Ok(())
    }

    /// `struct fuse_write_in`, then the data.
    fn write(&self, body: &[u8]) -> Answer {
        let handle = self.handle(body)?;
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let size = u32_at(body, 16).ok_or(libc::EINVAL)? as usize;
        let data = body.get(40..40 + size).ok_or(libc::EINVAL)?;
        self.changes(handle)?;
        let len = sys::pwrite(handle.fd.as_fd(), data, offset).map_err(errno)?;
        let mut out = (len as u32).to_le_bytes().to_vec();
        out.extend([0; 4]);
        Ok(out)
    }

    /// `struct fuse_setattr_in`. Owners don't change, because the agent's
    /// aren't the host's.
    fn setattr(&self, node: u64, body: &[u8]) -> Answer {
        let field = |at| u64_at(body, at).ok_or(libc::EINVAL);
        let word = |at| u32_at(body, at).ok_or(libc::EINVAL);
        let valid = word(0)?;
        let found = self.node(node)?;
        if found.place == Place::Fixed || valid & (FATTR_UID | FATTR_GID) != 0 {
            return Err(libc::EPERM);
        }
        let own;
        let fd = if valid & FATTR_FH != 0 {
            let handle = self.handle(body.get(8..).ok_or(libc::EINVAL)?)?;
            if handle.node != node {
                return Err(libc::EBADF);
            }
            handle.fd.as_fd()
        } else if is(found.mode, libc::S_IFDIR) {
            let flags = libc::O_RDONLY | libc::O_DIRECTORY;
            own = sys::open_beneath(found.fd.as_fd(), c".", flags).map_err(errno)?;
            own.as_fd()
        } else {
            let access = match valid & FATTR_SIZE {
                0 => libc::O_RDONLY,
                _ => libc::O_WRONLY,
            };
            own = self.reopen(found, access | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
            own.as_fd()
        };
        if aliased(&sys::fstat(fd).map_err(errno)?) {
            return Err(libc::EPERM);
        }
        if valid & FATTR_SIZE != 0 {
            sys::ftruncate(fd, field(16)?).map_err(errno)?;
        }
        if valid & FATTR_MODE != 0 {
            sys::fchmod(fd, word(68)? & 0o7777).map_err(errno)?;
        }
        let times = FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW;
        if valid & times != 0 {
            let access = time(valid, FATTR_ATIME, FATTR_ATIME_NOW, field(32)?, word(56)?);
            let modify = time(valid, FATTR_MTIME, FATTR_MTIME_NOW, field(40)?, word(60)?);
            sys::futimens(fd, &[access, modify]).map_err(errno)?;
        }
        Ok(attr_out(&sys::fstat(fd).map_err(errno)?, self.owner))
    }

    /// `struct fuse_mkdir_in`, then the name.
    fn mkdir(&mut self, parent: u64, body: &[u8]) -> Answer {
        let mode = u32_at(body, 0).ok_or(libc::EINVAL)?;
        let umask = u32_at(body, 4).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(8..).ok_or(libc::EINVAL)?)?;
        let (directory, place) = self.directory(parent)?;
        if place.fixes(name.as_bytes()) {
            return Err(libc::EPERM);
        }
        sys::mkdirat(directory.as_fd(), &name, mode & !umask & 0o7777).map_err(errno)?;
        self.found(directory, place, name)
    }

    /// The name, then the symlink's target, which only the guest resolves.
    fn symlink(&mut self, parent: u64, body: &[u8]) -> Answer {
        let name = entry_name(body)?;
        let rest = body.get(name.as_bytes().len() + 1..).ok_or(libc::EINVAL)?;
        let target = CStr::from_bytes_until_nul(rest).map_err(|_| libc::EINVAL)?;
        if target.is_empty() {
            return Err(libc::ENOENT);
        }
        let (directory, place) = self.directory(parent)?;
        if place.fixes(name.as_bytes()) || !place.takes(name.as_bytes(), libc::S_IFLNK) {
            return Err(libc::EPERM);
        }
        sys::symlinkat(target, directory.as_fd(), &name).map_err(errno)?;
        self.found(directory, place, name)
    }

    /// `struct fuse_link_in`, then the new name, in the directory `parent`.
    fn link(&mut self, parent: u64, body: &[u8]) -> Answer {
        let old = u64_at(body, 0).ok_or(libc::EINVAL)?;
        let name = entry_name(body.get(8..).ok_or(libc::EINVAL)?)?;
        let (directory, place) = self.directory(parent)?;
        let found = self.node(old)?;
        if found.place == Place::Fixed
            || is(found.mode, libc::S_IFDIR)
            || place.fixes(name.as_bytes())
            || !place.takes(name.as_bytes(), found.mode)
        {
            return Err(libc::EPERM);
        }
        let inode = (found.device, found.inode);
        sys::linkat(found.parent.as_fd(), &found.name, directory.as_fd(), &name).map_err(errno)?;
        let flags = libc::O_PATH | libc::O_NOFOLLOW;
        let fd = sys::open_beneath(directory.as_fd(), &name, flags).map_err(errno)?;
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        // The old name may have moved on to another file since its lookup.
        if (stat.st_dev, stat.st_ino) != inode {
            let _ = sys::unlinkat(directory.as_fd(), &name, 0);
            return Err(libc::ESTALE);
        }
        self.remember(directory, place, name, fd)
    }

    /// `UNLINK` or `RMDIR`: the name.
    fn remove(&self, parent: u64, body: &[u8], flags: i32) -> Answer {
        let name = entry_name(body)?;
        let (directory, place) = self.directory(parent)?;
        if place.fixes(name.as_bytes()) {
            return Err(libc::EPERM);
        }
        sys::unlinkat(directory.as_fd(), &name, flags).map_err(errno)?;
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
        if flags & !(libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE) != 0 {
            return Err(libc::EINVAL);
        }
        let (old, new) = two_names(names.ok_or(libc::EINVAL)?)?;
        let (from, from_place) = self.directory(parent)?;
        let (into, into_place) = self.directory(to)?;
        if from_place.fixes(old.as_bytes()) || into_place.fixes(new.as_bytes()) {
            return Err(libc::EPERM);
        }
        let mode = |directory: &OwnedFd, name: &CStr| {
            let flags = libc::O_PATH | libc::O_NOFOLLOW;
            let fd = sys::open_beneath(directory.as_fd(), name, flags).map_err(errno)?;
            sys::fstat(fd.as_fd())
                .map(|stat| stat.st_mode)
                .map_err(errno)
        };
        if !into_place.takes(new.as_bytes(), mode(&from, &old)?) {
            return Err(libc::EPERM);
        }
        let exchange = flags & libc::RENAME_EXCHANGE != 0;
        if exchange && !from_place.takes(old.as_bytes(), mode(&into, &new)?) {
            return Err(libc::EPERM);
        }
        sys::renameat2(from.as_fd(), &old, into.as_fd(), &new, flags).map_err(errno)?;
        // The guest keeps its nodes, so each file it knows takes its new
        // name and place.
        self.moved(&into, into_place, &new);
        if exchange {
            self.moved(&from, from_place, &old);
        }
        Ok(Vec::new())
    }

    /// Gives the node of the file now at `name` in `directory` that name.
    fn moved(&mut self, directory: &Rc<OwnedFd>, place: Place, name: &CStr) {
        let flags = libc::O_PATH | libc::O_NOFOLLOW;
        let Ok(fd) = sys::open_beneath(directory.as_fd(), name, flags) else {
            return;
        };
        let Ok(stat) = sys::fstat(fd.as_fd()) else {
            return;
        };
        let Some(node) = self
            .known
            .get(&(stat.st_dev, stat.st_ino))
            .and_then(|node| self.nodes.get_mut(node))
        else {
            return;
        };
        node.parent = Rc::clone(directory);
        node.name = name.to_owned();
        if node.place != Place::Fixed {
            node.place = place.of_file(name.to_bytes(), stat.st_mode);
        }
    }

    /// `struct fuse_fallocate_in`.
    fn fallocate(&self, body: &[u8]) -> Answer {
        let handle = self.handle(body)?;
        let offset = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let len = u64_at(body, 16).ok_or(libc::EINVAL)?;
        let mode = u32_at(body, 24).ok_or(libc::EINVAL)? as i32;
        self.changes(handle)?;
        sys::fallocate(handle.fd.as_fd(), mode, offset, len).map_err(errno)?;
        Ok(Vec::new())
    }

    /// `struct fuse_fsync_in`. An `ro` share has nothing to write back.
    fn fsync(&self, body: &[u8]) -> Answer {
        if self.read_only {
            return Ok(Vec::new());
        }
        let handle = self.handle(body)?;
        let data_only = u32_at(body, 8).ok_or(libc::EINVAL)? & 1 != 0;
        sys::fsync(handle.fd.as_fd(), data_only).map_err(errno)?;
        Ok(Vec::new())
    }

    /// The file that holds `owner`'s POSIX locks on `node`, opened the first
    /// time the owner locks it. It is open for writing where the share
    /// allows that, so a write lock can be taken.
    fn lock_file(&mut self, node: u64, owner: u64) -> Result<BorrowedFd<'_>, i32> {
        if !self.locks.contains_key(&(node, owner)) {
            let found = self.node(node)?;
            if !is(found.mode, libc::S_IFREG) {
                return Err(libc::EINVAL);
            }
            let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
            let read = || self.reopen(found, libc::O_RDONLY | flags);
            let fd = match !self.read_only && found.place != Place::Fixed {
                true => self
                    .reopen(found, libc::O_RDWR | flags)
                    .or_else(|_| read())?,
                false => read()?,
            };
            self.locks.insert((node, owner), fd);
        }
        self.locks
            .get(&(node, owner))
            .map(AsFd::as_fd)
            .ok_or(libc::EIO)
    }

    /// `struct fuse_lk_in`: the handle, the owner, the lock, and its flags.
    fn setlk(&mut self, body: &[u8], wait: bool) -> Answer {
        let handle = self.handle(body)?;
        let owner = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let flags = u32_at(body, 40).ok_or(libc::EINVAL)?;
        let mut lock = lock_of(body)?;
        if flags & LK_FLOCK != 0 {
            let operation = match i32::from(lock.l_type) {
                libc::F_RDLCK => libc::LOCK_SH,
                libc::F_WRLCK => libc::LOCK_EX,
                _ => libc::LOCK_UN,
            };
            sys::flock(handle.fd.as_fd(), operation | libc::LOCK_NB)
                .map_err(|error| busy(error, wait))?;
            return Ok(Vec::new());
        }
        let node = handle.node;
        let fd = self.lock_file(node, owner)?;
        sys::ofd_lock(fd, libc::F_OFD_SETLK, &mut lock).map_err(|error| busy(error, wait))?;
        Ok(Vec::new())
    }

    /// Answers with `struct fuse_lk_out`: a lock that stands in the way, or
    /// none.
    fn getlk(&mut self, body: &[u8]) -> Answer {
        let node = self.handle(body)?.node;
        let owner = u64_at(body, 8).ok_or(libc::EINVAL)?;
        let mut lock = lock_of(body)?;
        let fd = self.lock_file(node, owner)?;
        sys::ofd_lock(fd, libc::F_OFD_GETLK, &mut lock).map_err(errno)?;
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
        let node = self.handle(body)?.node;
        let owner = u64_at(body, 16).ok_or(libc::EINVAL)?;
        self.locks.remove(&(node, owner));
        Ok(Vec::new())
    }
}

/// `struct fuse_statfs_out`, for the filesystem `fd` is on.
fn statfs(fd: BorrowedFd) -> Answer {
    let stat = sys::fstatfs(fd).map_err(errno)?;
    let mut out = Vec::with_capacity(80);
    for value in [
        stat.f_blocks,
        stat.f_bfree,
        stat.f_bavail,
        stat.f_files,
        stat.f_ffree,
    ] {
        out.extend(value.to_le_bytes());
    }
    out.extend((stat.f_bsize as u32).to_le_bytes());
    out.extend((stat.f_namelen as u32).to_le_bytes());
    out.extend((stat.f_frsize as u32).to_le_bytes());
    out.resize(80, 0);
    Ok(out)
}

/// `struct fuse_init_out`, for the guest's `struct fuse_init_in`.
fn init(body: &[u8]) -> Answer {
    let major = u32_at(body, 0).ok_or(libc::EINVAL)?;
    let minor = u32_at(body, 4).ok_or(libc::EINVAL)?;
    let readahead = u32_at(body, 8).ok_or(libc::EINVAL)?;
    let offered = u32_at(body, 12).ok_or(libc::EINVAL)?;
    if major < MAJOR {
        return Err(libc::EPROTO);
    }
    let mut out = Vec::with_capacity(64);
    out.extend(MAJOR.to_le_bytes());
    if major > MAJOR {
        // The guest asks again with this major.
        out.resize(8, 0);
        return Ok(out);
    }
    out.extend(minor.min(MINOR).to_le_bytes());
    out.extend(readahead.to_le_bytes());
    let taken = AUTO_INVAL_DATA | POSIX_LOCKS | FLOCK_LOCKS;
    out.extend((offered & taken).to_le_bytes());
    // `max_background` and `congestion_threshold`, then `max_write` and
    // `time_gran`.
    out.extend([0; 4]);
    out.extend(MAX_WRITE.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.resize(64, 0);
    Ok(out)
}

/// The place of a root, from the names along its host path, so that a root
/// in a git directory, or one that is protected, keeps its place.
fn root_place(root: &OwnedFd) -> Place {
    let link = format!("/proc/self/fd/{}", root.as_raw_fd());
    let Ok(path) = std::fs::read_link(link) else {
        return Place::Plain;
    };
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .fold(Place::Plain, |place, name| place.of(name.as_bytes()))
}

impl Server {
    /// A server for these roots, each one an open directory, in share order.
    /// Call it before the process is confined, because it reads where each
    /// root is.
    pub fn new(roots: Vec<Root>) -> io::Result<Self> {
        let owner = Owner::of_server();
        let shares = roots
            .into_iter()
            .map(
                |Root {
                     directory,
                     file,
                     mode,
                 }| match mode {
                    Mode::CopyOnWrite(_) if file.is_some() => Err(io::Error::other(
                        "a copy-on-write file share is not built yet",
                    )),
                    Mode::CopyOnWrite(pending) => {
                        cow::Cow::new(directory, pending, owner).map(Served::CopyOnWrite)
                    }
                    mode => {
                        let place = root_place(&directory);
                        let read_only = matches!(mode, Mode::ReadOnly);
                        Share::new(directory, file, read_only, place, owner).map(Served::Direct)
                    }
                },
            )
            .collect::<io::Result<_>>()?;
        Ok(Self { shares })
    }

    /// The directories the server reaches files beneath, and what it does
    /// there, in share order.
    pub(crate) fn access(&self) -> Vec<(BorrowedFd<'_>, Access)> {
        let mut access = Vec::new();
        for share in &self.shares {
            match share {
                Served::Direct(share) => {
                    // A file share reaches its file alone.
                    let reached = match &share.only {
                        Some((_, file)) => file.as_fd(),
                        None => share.root.as_fd(),
                    };
                    match share.read_only {
                        true => access.push((reached, Access::Read)),
                        false => access.push((reached, Access::ReadWrite)),
                    }
                }
                Served::CopyOnWrite(share) => {
                    let (root, pending) = share.directories();
                    access.push((root, Access::Read));
                    access.push((pending, Access::ReadWrite));
                }
            }
        }
        access
    }

    /// The answer to one request for share `share`, or nothing for a request
    /// that gets no answer.
    pub fn answer(&mut self, share: u32, request: &[u8]) -> Vec<u8> {
        let Some(request) = parse(request) else {
            return Vec::new();
        };
        let result = match self.shares.get_mut(share as usize) {
            Some(found) => match match found {
                Served::Direct(found) => dispatch(found, &request),
                Served::CopyOnWrite(found) => cow::dispatch(found, &request),
            } {
                Some(result) => result,
                None => return Vec::new(),
            },
            None => Err(libc::ENODEV),
        };
        let (error, payload) = match result {
            Ok(payload) => (0, payload),
            Err(error) => (-error, Vec::new()),
        };
        let mut out = Vec::with_capacity(OUT_HEADER + payload.len());
        out.extend(((OUT_HEADER + payload.len()) as u32).to_le_bytes());
        out.extend(error.to_le_bytes());
        out.extend(request.unique.to_le_bytes());
        out.extend(payload);
        out
    }
}

/// The request's header, if it holds a whole one and no more than it says.
fn parse(bytes: &[u8]) -> Option<Request<'_>> {
    let len = u32_at(bytes, 0)? as usize;
    if len < IN_HEADER || len > bytes.len() {
        return None;
    }
    Some(Request {
        opcode: u32_at(bytes, 4)?,
        unique: u64_at(bytes, 8)?,
        node: u64_at(bytes, 16)?,
        body: &bytes[IN_HEADER..len],
    })
}

/// `name` in `directory`, opened with `O_PATH`, or `ESTALE` unless it is
/// still a file share's `file`, which a file the host put in its place
/// isn't.
fn still_the_file(file: &OwnedFd, directory: &OwnedFd, name: &CStr) -> Result<OwnedFd, i32> {
    let flags = libc::O_PATH | libc::O_NOFOLLOW;
    let found = sys::open_beneath(directory.as_fd(), name, flags).map_err(errno)?;
    let identity = |fd: BorrowedFd| {
        sys::fstat(fd)
            .map(|stat| (stat.st_dev, stat.st_ino))
            .map_err(errno)
    };
    match identity(found.as_fd())? == identity(file.as_fd())? {
        true => Ok(found),
        false => Err(libc::ESTALE),
    }
}

/// Whether a file share answers `request`. Its root is the directory that
/// holds the file, so the guest may look the file up in it and nothing
/// more, and no entry in it can be made, removed, or renamed.
fn file_share_allows(request: &Request) -> bool {
    let names = [
        SYMLINK, MKNOD, MKDIR, UNLINK, RMDIR, RENAME, RENAME2, LINK, CREATE,
    ];
    let at_root = [
        INIT,
        LOOKUP,
        GETATTR,
        STATFS,
        DESTROY,
        FORGET,
        BATCH_FORGET,
        INTERRUPT,
    ];
    !names.contains(&request.opcode) && (request.node != ROOT || at_root.contains(&request.opcode))
}

/// The answer to a request, or `None` for one that gets no answer.
fn dispatch(share: &mut Share, request: &Request) -> Option<Answer> {
    let Request { node, body, .. } = *request;
    if WRITES.contains(&request.opcode) && share.read_only {
        return Some(Err(libc::EROFS));
    }
    if share.only.is_some() && !file_share_allows(request) {
        return Some(Err(libc::EPERM));
    }
    Some(match request.opcode {
        INIT => init(body),
        LOOKUP => share.lookup(node, body),
        GETATTR => share
            .node(node)
            .and_then(|found| sys::fstat(found.fd.as_fd()).map_err(errno))
            .map(|stat| attr_out(&stat, share.owner)),
        SETATTR => share.setattr(node, body),
        READLINK => share.readlink(node),
        SYMLINK => share.symlink(node, body),
        // A device, a FIFO, or a socket is never made on the host. The
        // guest's kernel makes a regular file with `CREATE`.
        MKNOD => Err(libc::EPERM),
        MKDIR => share.mkdir(node, body),
        UNLINK => share.remove(node, body, 0),
        RMDIR => share.remove(node, body, libc::AT_REMOVEDIR),
        RENAME => share.rename(node, body, false),
        RENAME2 => share.rename(node, body, true),
        LINK => share.link(node, body),
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
