//! Answers the guest's FUSE requests for each share, from `<linux/fuse.h>`.
//!
//! Every request comes from the guest, so nothing in it is trusted: a node or
//! a handle it names must be one this server gave it, and a name must be one
//! entry of a directory. Each file is reached from a descriptor the server
//! holds, one name at a time, beneath it and without following a symlink, so
//! nothing a guest sends can reach a host file outside the share's root. A
//! symlink reaches the guest as itself, and the guest resolves it.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

use crate::sys;

const IN_HEADER: usize = 40;
const OUT_HEADER: usize = 16;
const ROOT: u64 = 1;
/// The most one read or one directory listing answers with.
const MAX_READ: usize = 128 << 10;
/// Requests that carry writes are refused, so the guest needn't send much.
const MAX_WRITE: u32 = 4096;
const MAJOR: u32 = 7;
const MINOR: u32 = 31;
const NAME_MAX: usize = 255;

// Opcodes.
const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const GETATTR: u32 = 3;
const READLINK: u32 = 5;
const OPEN: u32 = 14;
const READ: u32 = 15;
const STATFS: u32 = 17;
const RELEASE: u32 = 18;
const FSYNC: u32 = 20;
const FLUSH: u32 = 25;
const INIT: u32 = 26;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;
const FSYNCDIR: u32 = 30;
const INTERRUPT: u32 = 36;
const DESTROY: u32 = 38;
const BATCH_FORGET: u32 = 42;
/// Every request that would change a file or a directory.
const WRITES: [u32; 17] = [
    4,  // SETATTR
    6,  // SYMLINK
    8,  // MKNOD
    9,  // MKDIR
    10, // UNLINK
    11, // RMDIR
    12, // RENAME
    13, // LINK
    16, // WRITE
    21, // SETXATTR
    24, // REMOVEXATTR
    35, // CREATE
    43, // FALLOCATE
    45, // RENAME2
    47, // COPY_FILE_RANGE
    51, // TMPFILE
    48, // SETUPMAPPING
];

/// `FUSE_AUTO_INVAL_DATA`: the guest drops a file's cached pages when its
/// time changes, so a file changed on the host reads as it is now.
const AUTO_INVAL_DATA: u32 = 1 << 12;

/// What a share allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ReadOnly,
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
}

/// One share: its root, and what the guest holds of it.
struct Share {
    root: Rc<OwnedFd>,
    mode: Mode,
    nodes: BTreeMap<u64, Node>,
    /// The node each host file has, by device and inode.
    known: BTreeMap<(u64, u64), u64>,
    handles: BTreeMap<u64, OwnedFd>,
    next_node: u64,
    next_handle: u64,
}

/// Answers every share's requests.
pub struct Server {
    shares: Vec<Share>,
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

/// `struct fuse_attr`.
fn attr(stat: &libc::stat) -> Vec<u8> {
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
    out.extend(stat.st_uid.to_le_bytes());
    out.extend(stat.st_gid.to_le_bytes());
    out.extend((stat.st_rdev as u32).to_le_bytes());
    out.extend((stat.st_blksize as u32).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out
}

/// `struct fuse_entry_out` or `struct fuse_attr_out`, with no time to
/// cache, so the guest asks again each time and sees the host's changes.
fn entry(node: u64, stat: &libc::stat) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend(node.to_le_bytes());
    out.extend([0; 32]);
    out.extend(attr(stat));
    out
}

fn attr_out(stat: &libc::stat) -> Vec<u8> {
    let mut out = vec![0; 16];
    out.extend(attr(stat));
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

impl Share {
    fn new(root: OwnedFd, mode: Mode) -> io::Result<Self> {
        let stat = sys::fstat(root.as_fd())?;
        if !is(stat.st_mode, libc::S_IFDIR) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let root = Rc::new(root);
        let node = Node {
            fd: Rc::clone(&root),
            parent: Rc::clone(&root),
            name: c".".to_owned(),
            mode: stat.st_mode,
            device: stat.st_dev,
            inode: stat.st_ino,
            lookups: 1,
        };
        Ok(Self {
            root,
            mode,
            nodes: BTreeMap::from([(ROOT, node)]),
            known: BTreeMap::from([((stat.st_dev, stat.st_ino), ROOT)]),
            handles: BTreeMap::new(),
            next_node: ROOT + 1,
            next_handle: 1,
        })
    }

    fn node(&self, node: u64) -> Result<&Node, i32> {
        self.nodes.get(&node).ok_or(libc::ESTALE)
    }

    fn handle(&self, body: &[u8]) -> Result<BorrowedFd<'_>, i32> {
        let handle = u64_at(body, 0).ok_or(libc::EINVAL)?;
        self.handles
            .get(&handle)
            .map(AsFd::as_fd)
            .ok_or(libc::EBADF)
    }

    fn add_handle(&mut self, fd: OwnedFd) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(handle, fd);
        handle
    }

    fn lookup(&mut self, parent: u64, body: &[u8]) -> Answer {
        let name = entry_name(body)?;
        let directory = Rc::clone(&self.node(parent)?.fd);
        let flags = libc::O_PATH | libc::O_NOFOLLOW;
        let fd = sys::open_beneath(directory.as_fd(), &name, flags).map_err(errno)?;
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        let key = (stat.st_dev, stat.st_ino);
        if let Some(&found) = self.known.get(&key) {
            let node = self.nodes.get_mut(&found).ok_or(libc::EIO)?;
            node.lookups = node.lookups.saturating_add(1);
            if found != ROOT {
                // A file opens from the name it was last found by.
                node.parent = directory;
                node.name = name;
            }
            return Ok(entry(found, &stat));
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
            },
        );
        Ok(entry(id, &stat))
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
        }
    }

    fn open(&mut self, node: u64, body: &[u8]) -> Answer {
        let flags = u32_at(body, 0).ok_or(libc::EINVAL)? as i32;
        let writes = flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_TRUNC | libc::O_APPEND) != 0;
        if writes && self.mode == Mode::ReadOnly {
            return Err(libc::EROFS);
        }
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFREG) {
            return Err(libc::EINVAL);
        }
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let fd = sys::open_beneath(found.parent.as_fd(), &found.name, flags).map_err(errno)?;
        // The name may have moved on to another file since the lookup.
        let stat = sys::fstat(fd.as_fd()).map_err(errno)?;
        if (stat.st_dev, stat.st_ino) != (found.device, found.inode) {
            return Err(libc::ESTALE);
        }
        Ok(open_out(self.add_handle(fd)))
    }

    fn opendir(&mut self, node: u64) -> Answer {
        let found = self.node(node)?;
        if !is(found.mode, libc::S_IFDIR) {
            return Err(libc::ENOTDIR);
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let fd = sys::open_beneath(found.fd.as_fd(), c".", flags).map_err(errno)?;
        Ok(open_out(self.add_handle(fd)))
    }

    fn read(&self, body: &[u8]) -> Answer {
        let fd = self.handle(body)?;
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
        let fd = self.handle(body)?;
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
        let stat = sys::fstatfs(self.root.as_fd()).map_err(errno)?;
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
    out.extend((offered & AUTO_INVAL_DATA).to_le_bytes());
    // `max_background` and `congestion_threshold`, then `max_write` and
    // `time_gran`.
    out.extend([0; 4]);
    out.extend(MAX_WRITE.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.resize(64, 0);
    Ok(out)
}

impl Server {
    /// A server for these roots, each one an open directory, in share order.
    pub fn new(roots: Vec<(OwnedFd, Mode)>) -> io::Result<Self> {
        let shares = roots
            .into_iter()
            .map(|(root, mode)| Share::new(root, mode))
            .collect::<io::Result<_>>()?;
        Ok(Self { shares })
    }

    /// The answer to one request for share `share`, or nothing for a request
    /// that gets no answer.
    pub fn answer(&mut self, share: u32, request: &[u8]) -> Vec<u8> {
        let Some(request) = parse(request) else {
            return Vec::new();
        };
        let result = match self.shares.get_mut(share as usize) {
            Some(found) => match dispatch(found, &request) {
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

/// The answer to a request, or `None` for one that gets no answer.
fn dispatch(share: &mut Share, request: &Request) -> Option<Answer> {
    let Request { node, body, .. } = *request;
    let mode = share.mode;
    Some(match request.opcode {
        INIT => init(body),
        LOOKUP => share.lookup(node, body),
        GETATTR => share
            .node(node)
            .and_then(|found| sys::fstat(found.fd.as_fd()).map_err(errno))
            .map(|stat| attr_out(&stat)),
        READLINK => share.readlink(node),
        OPEN => share.open(node, body),
        READ => share.read(body),
        RELEASE | RELEASEDIR => {
            if let Some(handle) = u64_at(body, 0) {
                share.handles.remove(&handle);
            }
            Ok(Vec::new())
        }
        OPENDIR => share.opendir(node),
        READDIR => share.readdir(body),
        STATFS => share.statfs(),
        FLUSH | FSYNC | FSYNCDIR | DESTROY => Ok(Vec::new()),
        FORGET => {
            share.forget(node, u64_at(body, 0).unwrap_or(0));
            return None;
        }
        BATCH_FORGET => {
            let count = u32_at(body, 0).unwrap_or(0) as usize;
            for one in body.get(8..).unwrap_or(&[]).chunks_exact(16).take(count) {
                let (Some(node), Some(lookups)) = (u64_at(one, 0), u64_at(one, 8)) else {
                    break;
                };
                share.forget(node, lookups);
            }
            return None;
        }
        INTERRUPT => return None,
        opcode if WRITES.contains(&opcode) && mode == Mode::ReadOnly => Err(libc::EROFS),
        _ => Err(libc::ENOSYS),
    })
}
