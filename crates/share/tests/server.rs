//! The share server's rules, against the device's own relay in this
//! process: each request is a FUSE request as a guest's kernel, or a guest
//! that took its kernel over, would send it.

use share::{Mode, Server};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vmm::fs::{Relay, Server as _};

const CANARY: &[u8] = b"a host file outside the share";
const INSIDE: &[u8] = b"inside the share";

const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const GETATTR: u32 = 3;
const READLINK: u32 = 5;
const OPEN: u32 = 14;
const READ: u32 = 15;
const STATFS: u32 = 17;
const RELEASE: u32 = 18;
const INIT: u32 = 26;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const BATCH_FORGET: u32 = 42;
const WRITES: [u32; 17] = [4, 6, 8, 9, 10, 11, 12, 13, 16, 21, 24, 35, 43, 45, 47, 48, 51];

const ENOENT: i32 = 2;
const EBADF: i32 = 9;
const ENODEV: i32 = 19;
const ENOTDIR: i32 = 20;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOSYS: i32 = 38;
const ELOOP: i32 = 40;
const ESTALE: i32 = 116;
const O_WRONLY: u32 = 1;
const O_RDWR: u32 = 2;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;

/// A directory tree with a share's root and a host file beside it.
struct Tree {
    base: PathBuf,
    root: PathBuf,
    secret: PathBuf,
}

impl Tree {
    fn new(label: &str) -> Self {
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
struct Guest {
    relay: Relay,
    unique: u64,
    server: Option<JoinHandle<()>>,
}

/// What came back: the error, and the payload.
type Reply = (i32, Vec<u8>);

impl Guest {
    fn new(root: &Path) -> Self {
        let (mine, theirs) = UnixStream::pair().unwrap();
        let root = OwnedFd::from(File::open(root).unwrap());
        let server = thread::spawn(move || {
            let mut server = Server::new(vec![(root, Mode::ReadOnly)]).unwrap();
            let mut theirs = theirs;
            share::serve(&mut theirs, &mut server).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut guest = Guest {
            relay: Relay::new(OwnedFd::from(mine), deadline).unwrap(),
            unique: 0,
            server: Some(server),
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

    fn raw(&mut self, share: u32, request: &[u8]) -> Vec<u8> {
        self.relay.call(share, request).unwrap()
    }

    fn call_share(&mut self, share: u32, opcode: u32, node: u64, body: &[u8]) -> Reply {
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

    fn call(&mut self, opcode: u32, node: u64, body: &[u8]) -> Reply {
        self.call_share(0, opcode, node, body)
    }

    fn lookup(&mut self, parent: u64, name: &[u8]) -> Result<(u64, u32), i32> {
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

    fn open(&mut self, node: u64, flags: u32) -> Result<u64, i32> {
        let mut body = flags.to_le_bytes().to_vec();
        body.extend([0; 4]);
        match self.call(OPEN, node, &body) {
            (0, out) => Ok(u64::from_le_bytes(out[..8].try_into().unwrap())),
            (error, _) => Err(error),
        }
    }

    fn read(&mut self, opcode: u32, handle: u64, offset: u64, size: u32) -> Reply {
        let mut body = handle.to_le_bytes().to_vec();
        body.extend(offset.to_le_bytes());
        body.extend(size.to_le_bytes());
        body.extend([0; 20]);
        self.call(opcode, 0, &body)
    }

    /// Looks up a path one name at a time, opens it, and reads it.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, i32> {
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

    fn list(&mut self, node: u64) -> BTreeSet<String> {
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

fn kind(mode: u32) -> u32 {
    mode & S_IFMT
}

#[test]
fn files_beneath_the_root_read_as_they_are() {
    let tree = Tree::new("read");
    let mut guest = Guest::new(&tree.root);
    assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
    assert_eq!(guest.read_file("dir/nested.txt").unwrap(), b"nested");
    // A change on the host shows at the next read.
    fs::write(tree.root.join("hello.txt"), b"changed").unwrap();
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"changed");
    let (node, mode) = guest.lookup(1, b"dir").unwrap();
    assert_eq!(kind(mode), S_IFDIR);
    let (error, attr) = guest.call(GETATTR, node, &[0; 16]);
    assert_eq!(error, 0);
    assert_eq!(attr.len(), 104);
    let (error, statfs) = guest.call(STATFS, 1, &[]);
    assert_eq!((error, statfs.len()), (0, 80));
}

#[test]
fn a_directory_lists_its_entries_across_requests() {
    let tree = Tree::new("list");
    let mut guest = Guest::new(&tree.root);
    let names = guest.list(1);
    let expected = [
        ".",
        "..",
        "absolute",
        "dir",
        "dirlink",
        "escape",
        "hello.txt",
        "link",
    ];
    assert_eq!(names, expected.into_iter().map(String::from).collect());
}

#[test]
fn a_name_that_isn_t_one_entry_is_refused() {
    let tree = Tree::new("names");
    let mut guest = Guest::new(&tree.root);
    let secret = tree.secret.to_str().unwrap().as_bytes().to_vec();
    for (name, error) in [
        (&b".."[..], ENOENT),
        (b".", ENOENT),
        (b"", EINVAL),
        (b"dir/nested.txt", EINVAL),
        (b"../outside/secret", EINVAL),
        (&secret[..], EINVAL),
        (b"/etc/passwd", EINVAL),
        (&[b'a'; 256], EINVAL),
    ] {
        assert_eq!(guest.lookup(1, name), Err(error), "{name:?}");
    }
    // A name with no end is refused, rather than read past.
    assert_eq!(guest.call(LOOKUP, 1, b"hello.txt").0, EINVAL);
    let (dir, _) = guest.lookup(1, b"dir").unwrap();
    assert_eq!(guest.lookup(dir, b".."), Err(ENOENT));
}

// spec(SHARE): The agent sees only what is under the share's root.
#[test]
fn a_symlink_reaches_the_guest_as_itself_and_leads_nowhere_on_the_host() {
    let tree = Tree::new("symlinks");
    let mut guest = Guest::new(&tree.root);
    let secret = tree.secret.to_str().unwrap().as_bytes().to_vec();
    for (name, target) in [
        (&b"link"[..], &b"hello.txt"[..]),
        (b"escape", b"../outside/secret"),
        (b"absolute", &secret),
        (b"dirlink", b"../outside"),
    ] {
        let (node, mode) = guest.lookup(1, name).unwrap();
        assert_eq!(kind(mode), S_IFLNK, "{name:?}");
        assert_eq!(guest.call(READLINK, node, &[]), (0, target.to_vec()));
        assert_eq!(guest.open(node, 0), Err(EINVAL));
        assert_eq!(guest.call(OPENDIR, node, &[0; 8]).0, ENOTDIR);
        assert_eq!(guest.lookup(node, b"secret"), Err(ENOTDIR));
    }
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(guest.call(READLINK, file, &[]).0, EINVAL);
    assert_eq!(guest.lookup(file, b"x"), Err(ENOTDIR));
}

#[test]
fn a_symlink_swapped_in_after_a_lookup_is_never_followed() {
    let tree = Tree::new("swap");
    let mut guest = Guest::new(&tree.root);
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    let (dir, _) = guest.lookup(1, b"dir").unwrap();
    let swapped = tree.root.join("swapped");
    symlink(&tree.secret, &swapped).unwrap();
    fs::rename(&swapped, tree.root.join("hello.txt")).unwrap();
    fs::rename(tree.root.join("dir"), tree.root.join("dir-old")).unwrap();
    symlink("../outside", tree.root.join("dir")).unwrap();
    // The file's name is a symlink now, which opening refuses to follow.
    assert_eq!(guest.open(file, 0), Err(ELOOP));
    // The directory is still the one looked up, wherever it moved.
    let (nested, _) = guest.lookup(dir, b"nested.txt").unwrap();
    assert!(guest.open(nested, 0).is_ok());
    assert_eq!(guest.lookup(dir, b"secret"), Err(ENOENT));
    // A file put in the old one's place is a different file.
    fs::remove_file(tree.root.join("hello.txt")).unwrap();
    fs::write(tree.root.join("hello.txt"), b"another").unwrap();
    assert_eq!(guest.open(file, 0), Err(ESTALE));
}

#[test]
fn a_symlink_swapped_in_during_requests_never_reaches_outside() {
    let tree = Tree::new("race");
    let mut guest = Guest::new(&tree.root);
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (root, secret, stop) = (tree.root.clone(), tree.secret.clone(), Arc::clone(&stop));
        thread::spawn(move || {
            let (file, link) = (root.join("file.new"), root.join("link.new"));
            let mut swaps = 0;
            while !stop.load(Ordering::Relaxed) {
                fs::write(&file, INSIDE).unwrap();
                fs::rename(&file, root.join("target")).unwrap();
                let _ = fs::remove_file(&link);
                symlink(&secret, &link).unwrap();
                fs::rename(&link, root.join("target")).unwrap();
                swaps += 1;
            }
            swaps
        })
    };
    let mut read = 0;
    for _ in 0..2000 {
        if let Ok(data) = guest.read_file("target") {
            assert_eq!(data, INSIDE);
            read += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let swaps = swapper.join().unwrap();
    println!("{read} reads went through, across {swaps} swaps");
    assert!(swaps > 0);
}

#[test]
fn every_write_to_a_read_only_share_is_refused() {
    let tree = Tree::new("writes");
    let mut guest = Guest::new(&tree.root);
    let (file, mode) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(kind(mode), S_IFREG);
    for opcode in WRITES {
        for node in [1, file] {
            assert_eq!(guest.call(opcode, node, &[0; 64]).0, EROFS, "{opcode}");
        }
    }
    for flags in [O_WRONLY, O_RDWR, O_TRUNC, O_APPEND, O_WRONLY | O_TRUNC] {
        assert_eq!(guest.open(file, flags), Err(EROFS), "{flags:#o}");
    }
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
}

#[test]
fn a_node_or_a_handle_the_server_never_gave_is_refused() {
    let tree = Tree::new("forged");
    let mut guest = Guest::new(&tree.root);
    for node in [0, 2, 1000, u64::MAX] {
        assert_eq!(guest.call(GETATTR, node, &[0; 16]).0, ESTALE, "{node}");
        assert_eq!(guest.lookup(node, b"hello.txt"), Err(ESTALE));
        assert_eq!(guest.open(node, 0), Err(ESTALE));
    }
    for handle in [0, 1, 1000, u64::MAX] {
        assert_eq!(guest.read(READ, handle, 0, 16).0, EBADF);
        assert_eq!(guest.read(READDIR, handle, 0, 16).0, EBADF);
    }
    assert_eq!(guest.call_share(1, GETATTR, 1, &[0; 16]).0, ENODEV);
    assert_eq!(guest.call(9999, 1, &[]).0, ENOSYS);
}

#[test]
fn a_forgotten_node_is_gone() {
    let tree = Tree::new("forget");
    let mut guest = Guest::new(&tree.root);
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(guest.lookup(1, b"hello.txt").unwrap().0, file);
    // Forgetting gets no answer.
    assert_eq!(guest.call(FORGET, file, &1u64.to_le_bytes()), (0, Vec::new()));
    assert_eq!(guest.call(GETATTR, file, &[0; 16]).0, 0);
    let mut batch = 1u32.to_le_bytes().to_vec();
    batch.extend([0; 4]);
    batch.extend(file.to_le_bytes());
    batch.extend(1u64.to_le_bytes());
    assert_eq!(guest.call(BATCH_FORGET, 0, &batch), (0, Vec::new()));
    assert_eq!(guest.call(GETATTR, file, &[0; 16]).0, ESTALE);
    // The root can't be forgotten.
    guest.call(FORGET, 1, &u64::MAX.to_le_bytes());
    assert_eq!(guest.call(GETATTR, 1, &[0; 16]).0, 0);
}

#[test]
fn a_malformed_request_gets_no_answer() {
    let tree = Tree::new("malformed");
    let mut guest = Guest::new(&tree.root);
    assert!(guest.raw(0, &[]).is_empty());
    assert!(guest.raw(0, &[0; 39]).is_empty());
    // A length past the request's end.
    let mut request = vec![0; 40];
    request[..4].copy_from_slice(&41u32.to_le_bytes());
    assert!(guest.raw(0, &request).is_empty());
    // The server still answers after.
    assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
}

#[test]
fn a_root_must_be_a_directory() {
    let tree = Tree::new("root-file");
    let file = OwnedFd::from(File::open(tree.root.join("hello.txt")).unwrap());
    assert!(Server::new(vec![(file, Mode::ReadOnly)]).is_err());
}
