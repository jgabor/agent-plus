//! A guest's view of a share: FUSE requests as a guest's kernel, or a
//! guest that took its kernel over, would send them, and a tree to share.

#![allow(dead_code)]

use share::{Mode, Server};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vmm::fs::{Relay, Server as _};

pub const CANARY: &[u8] = b"a host file outside the share";
pub const INSIDE: &[u8] = b"inside the share";

pub const LOOKUP: u32 = 1;
pub const FORGET: u32 = 2;
pub const GETATTR: u32 = 3;
pub const READLINK: u32 = 5;
pub const OPEN: u32 = 14;
pub const READ: u32 = 15;
pub const STATFS: u32 = 17;
pub const RELEASE: u32 = 18;
pub const INIT: u32 = 26;
pub const OPENDIR: u32 = 27;
pub const READDIR: u32 = 28;
pub const BATCH_FORGET: u32 = 42;
pub const WRITES: [u32; 17] = [
    4, 6, 8, 9, 10, 11, 12, 13, 16, 21, 24, 35, 43, 45, 47, 48, 51,
];

pub const ENOENT: i32 = 2;
pub const EBADF: i32 = 9;
pub const ENODEV: i32 = 19;
pub const ENOTDIR: i32 = 20;
pub const EINVAL: i32 = 22;
pub const EROFS: i32 = 30;
pub const ENOSYS: i32 = 38;
pub const ELOOP: i32 = 40;
pub const ESTALE: i32 = 116;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;

/// A directory tree with a share's root and a host file beside it.
pub struct Tree {
    pub base: PathBuf,
    pub root: PathBuf,
    pub secret: PathBuf,
}

impl Tree {
    pub fn new(label: &str) -> Self {
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("share-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let secret = base.join("outside/secret");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        fs::write(&secret, CANARY).unwrap();
        fs::write(root.join("hello.txt"), INSIDE).unwrap();
        fs::write(root.join("dir/nested.txt"), b"nested").unwrap();
        symlink("hello.txt", root.join("link")).unwrap();
        symlink("../outside/secret", root.join("escape")).unwrap();
        symlink(&secret, root.join("absolute")).unwrap();
        symlink("../outside", root.join("dirlink")).unwrap();
        Tree { base, root, secret }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// A guest's view: requests go through the device's relay to a server on a
/// thread of its own, as they go to the share server's process.
pub struct Guest {
    pub relay: Relay,
    pub unique: u64,
    pub server: Option<JoinHandle<()>>,
}

/// What came back: the error, and the payload.
pub type Reply = (i32, Vec<u8>);

impl Guest {
    pub fn new(root: &Path) -> Self {
        Self::with(root, Mode::ReadOnly)
    }

    pub fn with(root: &Path, mode: Mode) -> Self {
        let (mine, theirs) = UnixStream::pair().unwrap();
        let root = OwnedFd::from(File::open(root).unwrap());
        let server = thread::spawn(move || {
            let mut server = Server::new(vec![(root, mode)]).unwrap();
            let mut theirs = theirs;
            share::serve(&mut theirs, &mut server).unwrap();
        });
        let mut guest = Self::over(OwnedFd::from(mine));
        guest.server = Some(server);
        guest
    }

    /// A guest of the server at the other end of `socket`, which runs on
    /// its own.
    pub fn over(socket: OwnedFd) -> Self {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut guest = Guest {
            relay: Relay::new(socket, deadline).unwrap(),
            unique: 0,
            server: None,
        };
        let mut init = Vec::new();
        for value in [7u32, 41, 1 << 17, u32::MAX] {
            init.extend(value.to_le_bytes());
        }
        init.resize(64, 0);
        let (error, payload) = guest.call(INIT, 0, &init);
        assert_eq!(error, 0);
        assert_eq!(payload.len(), 64);
        guest
    }

    pub fn raw(&mut self, share: u32, request: &[u8]) -> Vec<u8> {
        self.relay.call(share, request).unwrap()
    }

    pub fn call_share(&mut self, share: u32, opcode: u32, node: u64, body: &[u8]) -> Reply {
        self.unique += 2;
        let mut request = Vec::new();
        request.extend(((40 + body.len()) as u32).to_le_bytes());
        request.extend(opcode.to_le_bytes());
        request.extend(self.unique.to_le_bytes());
        request.extend(node.to_le_bytes());
        request.extend([0; 16]);
        request.extend(body);
        let answer = self.raw(share, &request);
        if answer.is_empty() {
            return (0, answer);
        }
        assert_eq!(
            u32::from_le_bytes(answer[..4].try_into().unwrap()) as usize,
            answer.len()
        );
        assert_eq!(answer[8..16], self.unique.to_le_bytes());
        let error = -i32::from_le_bytes(answer[4..8].try_into().unwrap());
        (error, answer[16..].to_vec())
    }

    pub fn call(&mut self, opcode: u32, node: u64, body: &[u8]) -> Reply {
        self.call_share(0, opcode, node, body)
    }

    pub fn lookup(&mut self, parent: u64, name: &[u8]) -> Result<(u64, u32), i32> {
        let mut body = name.to_vec();
        body.push(0);
        match self.call(LOOKUP, parent, &body) {
            (0, entry) => {
                let node = u64::from_le_bytes(entry[..8].try_into().unwrap());
                // The mode, in the attributes after the entry's 40 bytes.
                let mode = u32::from_le_bytes(entry[40 + 60..40 + 64].try_into().unwrap());
                Ok((node, mode))
            }
            (error, _) => Err(error),
        }
    }

    pub fn open(&mut self, node: u64, flags: u32) -> Result<u64, i32> {
        let mut body = flags.to_le_bytes().to_vec();
        body.extend([0; 4]);
        match self.call(OPEN, node, &body) {
            (0, out) => Ok(u64::from_le_bytes(out[..8].try_into().unwrap())),
            (error, _) => Err(error),
        }
    }

    pub fn read(&mut self, opcode: u32, handle: u64, offset: u64, size: u32) -> Reply {
        let mut body = handle.to_le_bytes().to_vec();
        body.extend(offset.to_le_bytes());
        body.extend(size.to_le_bytes());
        body.extend([0; 20]);
        self.call(opcode, 0, &body)
    }

    /// Looks up a path one name at a time, opens it, and reads it.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, i32> {
        let mut node = 1;
        for name in path.split('/') {
            node = self.lookup(node, name.as_bytes())?.0;
        }
        let handle = self.open(node, 0)?;
        let (error, data) = self.read(READ, handle, 0, 4096);
        self.read(RELEASE, handle, 0, 0);
        match error {
            0 => Ok(data),
            error => Err(error),
        }
    }

    pub fn list(&mut self, node: u64) -> BTreeSet<String> {
        let (error, out) = self.call(OPENDIR, node, &[0; 8]);
        assert_eq!(error, 0);
        let handle = u64::from_le_bytes(out[..8].try_into().unwrap());
        let mut names = BTreeSet::new();
        let mut offset = 0;
        loop {
            // A small size, so the listing takes several requests.
            let (error, out) = self.read(READDIR, handle, offset, 80);
            assert_eq!(error, 0);
            if out.is_empty() {
                return names;
            }
            let mut at = 0;
            while at < out.len() {
                offset = u64::from_le_bytes(out[at + 8..at + 16].try_into().unwrap());
                let len = u32::from_le_bytes(out[at + 16..at + 20].try_into().unwrap()) as usize;
                names.insert(String::from_utf8(out[at + 24..at + 24 + len].to_vec()).unwrap());
                at += (24 + len).next_multiple_of(8);
            }
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // Closing the relay ends the server's loop.
        let (mine, _) = UnixStream::pair().unwrap();
        self.relay = Relay::new(OwnedFd::from(mine), Instant::now()).unwrap();
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

pub fn kind(mode: u32) -> u32 {
    mode & S_IFMT
}

pub const SETATTR: u32 = 4;
pub const SYMLINK: u32 = 6;
pub const MKNOD: u32 = 8;
pub const MKDIR: u32 = 9;
pub const UNLINK: u32 = 10;
pub const RMDIR: u32 = 11;
pub const RENAME: u32 = 12;
pub const LINK: u32 = 13;
pub const WRITE: u32 = 16;
pub const FLUSH: u32 = 25;
pub const GETLK: u32 = 31;
pub const SETLK: u32 = 32;
pub const SETLKW: u32 = 33;
pub const CREATE: u32 = 35;
pub const RENAME2: u32 = 45;
pub const EPERM: i32 = 1;
pub const EAGAIN: i32 = 11;
pub const EEXIST: i32 = 17;
pub const EOPNOTSUPP: i32 = 95;
pub const O_EXCL: u32 = 0o200;
pub const FATTR_MODE: u32 = 1;
pub const FATTR_UID: u32 = 2;
pub const FATTR_SIZE: u32 = 8;
pub const FATTR_MTIME: u32 = 32;
pub const POSIX_LOCKS: u32 = 1 << 1;
pub const FLOCK_LOCKS: u32 = 1 << 10;
pub const LK_FLOCK: u32 = 1;
pub const F_RDLCK: u32 = 0;
pub const F_WRLCK: u32 = 1;
pub const F_UNLCK: u32 = 2;
pub const RENAME_NOREPLACE: u32 = 1;
pub const RENAME_EXCHANGE: u32 = 2;

pub fn named(name: &[u8]) -> Vec<u8> {
    let mut body = name.to_vec();
    body.push(0);
    body
}

pub fn node_of(entry: &[u8]) -> u64 {
    u64::from_le_bytes(entry[..8].try_into().unwrap())
}

/// The ways an agent changes a file in a share, as the guest's kernel
/// sends them.
impl Guest {
    pub fn walk(&mut self, path: &str) -> Result<u64, i32> {
        let mut node = 1;
        for name in path.split('/').filter(|name| !name.is_empty()) {
            node = self.lookup(node, name.as_bytes())?.0;
        }
        Ok(node)
    }

    /// Creates `name` in `parent`, and returns its node and handle.
    pub fn create(&mut self, parent: u64, name: &[u8], flags: u32) -> Result<(u64, u64), i32> {
        let mut body = Vec::new();
        for value in [flags | O_RDWR, 0o644, 0o022, 0] {
            body.extend(value.to_le_bytes());
        }
        body.extend(named(name));
        match self.call(CREATE, parent, &body) {
            (0, out) => {
                assert_eq!(out.len(), 128 + 16);
                let handle = u64::from_le_bytes(out[128..136].try_into().unwrap());
                Ok((node_of(&out), handle))
            }
            (error, _) => Err(error),
        }
    }

    pub fn write(&mut self, handle: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let mut body = handle.to_le_bytes().to_vec();
        body.extend(offset.to_le_bytes());
        body.extend((data.len() as u32).to_le_bytes());
        body.extend([0; 20]);
        body.extend(data);
        match self.call(WRITE, 0, &body) {
            (0, out) => Ok(u32::from_le_bytes(out[..4].try_into().unwrap())),
            (error, _) => Err(error),
        }
    }

    pub fn release(&mut self, handle: u64) {
        self.read(RELEASE, handle, 0, 0);
    }

    /// Opens a path for writing, writes `data` over it, and closes it.
    pub fn overwrite(&mut self, path: &str, data: &[u8]) -> Result<(), i32> {
        let node = self.walk(path)?;
        let handle = self.open(node, O_WRONLY | O_TRUNC)?;
        let written = self.write(handle, 0, data);
        self.release(handle);
        written.map(|_| ())
    }

    pub fn mkdir(&mut self, parent: u64, name: &[u8]) -> Result<u64, i32> {
        let mut body = 0o755u32.to_le_bytes().to_vec();
        body.extend(0o022u32.to_le_bytes());
        body.extend(named(name));
        match self.call(MKDIR, parent, &body) {
            (0, out) => Ok(node_of(&out)),
            (error, _) => Err(error),
        }
    }

    pub fn symlink(&mut self, parent: u64, name: &[u8], target: &[u8]) -> Result<u64, i32> {
        let mut body = named(name);
        body.extend(named(target));
        match self.call(SYMLINK, parent, &body) {
            (0, out) => Ok(node_of(&out)),
            (error, _) => Err(error),
        }
    }

    pub fn link(&mut self, node: u64, parent: u64, name: &[u8]) -> Result<u64, i32> {
        let mut body = node.to_le_bytes().to_vec();
        body.extend(named(name));
        match self.call(LINK, parent, &body) {
            (0, out) => Ok(node_of(&out)),
            (error, _) => Err(error),
        }
    }

    pub fn remove(&mut self, opcode: u32, parent: u64, name: &[u8]) -> i32 {
        self.call(opcode, parent, &named(name)).0
    }

    pub fn rename(&mut self, from: u64, old: &[u8], to: u64, new: &[u8], flags: u32) -> i32 {
        let mut body = to.to_le_bytes().to_vec();
        let opcode = match flags {
            0 => RENAME,
            _ => {
                body.extend(flags.to_le_bytes());
                body.extend([0; 4]);
                RENAME2
            }
        };
        body.extend(named(old));
        body.extend(named(new));
        self.call(opcode, from, &body).0
    }

    pub fn setattr(&mut self, node: u64, valid: u32, size: u64, mode: u32) -> i32 {
        let mut body = valid.to_le_bytes().to_vec();
        body.extend([0; 4]);
        body.extend(0u64.to_le_bytes());
        body.extend(size.to_le_bytes());
        body.extend([0; 8 * 4 + 4 * 3]);
        body.extend(mode.to_le_bytes());
        body.extend([0; 4 * 4]);
        assert_eq!(body.len(), 88);
        self.call(SETATTR, node, &body).0
    }

    /// A lock request: `fuse_lk_in`, for the whole file.
    pub fn lock(&mut self, opcode: u32, handle: u64, owner: u64, kind: u32, flags: u32) -> Reply {
        let mut body = handle.to_le_bytes().to_vec();
        body.extend(owner.to_le_bytes());
        body.extend(0u64.to_le_bytes());
        body.extend(i64::MAX.to_le_bytes());
        body.extend(kind.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(flags.to_le_bytes());
        body.extend([0; 4]);
        self.call(opcode, 0, &body)
    }
}

/// A tree that holds what SPEC.md protects in an `rw` share.
pub fn protected_tree(label: &str) -> Tree {
    let tree = Tree::new(label);
    let root = &tree.root;
    for dir in [
        ".git/hooks",
        ".git/refs/heads",
        ".git/modules/sub/hooks",
        ".git/modules/libs/deep/hooks",
        ".git/worktrees/wt",
        "nested/.git/hooks",
        "app",
        "worktree",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for file in [
        "Agentfile",
        "Agentfile.lock",
        "app/Agentfile",
        ".git/HEAD",
        ".git/config",
        ".git/config.worktree",
        ".git/hooks/pre-commit",
        ".git/refs/heads/main",
        ".git/modules/sub/config",
        ".git/modules/sub/hooks/post-checkout",
        ".git/modules/libs/deep/config",
        ".git/worktrees/wt/commondir",
        ".git/worktrees/wt/gitdir",
        ".git/worktrees/wt/config.worktree",
        "nested/.git/config",
        "worktree/.git",
    ] {
        fs::write(root.join(file), b"host").unwrap();
    }
    tree
}

/// Whether the host can't take a lock of `kind` on the whole file now.
pub fn host_locked(path: &Path, kind: i16) -> bool {
    let file = File::options().read(true).write(true).open(path).unwrap();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    // SAFETY: `lock` is a `struct flock`, which outlives the call.
    let got = unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::F_OFD_GETLK,
            &mut lock,
        )
    };
    assert_eq!(got, 0);
    lock.l_type != libc::F_UNLCK as i16
}

pub const EXDEV: i32 = 18;
pub const ENOTEMPTY: i32 = 39;
pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFSOCK: u32 = 0o140_000;

impl Guest {
    /// A guest of a copy-on-write share of `root`, which keeps the agent's
    /// changes in `pending`, a new directory.
    pub fn cow(root: &Path, pending: &Path) -> Self {
        fs::create_dir(pending).unwrap();
        let pending = OwnedFd::from(File::open(pending).unwrap());
        Self::with(root, Mode::CopyOnWrite(pending))
    }

    pub fn mknod(&mut self, parent: u64, name: &[u8], mode: u32, rdev: u32) -> Result<u64, i32> {
        let mut body = mode.to_le_bytes().to_vec();
        body.extend(rdev.to_le_bytes());
        body.extend([0; 8]);
        body.extend(named(name));
        match self.call(MKNOD, parent, &body) {
            (0, out) => Ok(node_of(&out)),
            (error, _) => Err(error),
        }
    }

    /// The mode of the file at `path`, as the guest sees it.
    pub fn mode(&mut self, path: &str) -> Result<u32, i32> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = self.walk(parent)?;
        self.lookup(parent, name.as_bytes()).map(|(_, mode)| mode)
    }
}

/// The records a copy-on-write share kept in `pending`, one to a line.
pub fn records(pending: &Path) -> Vec<String> {
    fs::read_to_string(pending.join("records"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}
