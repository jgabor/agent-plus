//! A share of kept state, against the device's own relay in this process:
//! what the guest writes stays in the share's directory, as a copy-on-write
//! share keeps its changes, and the next server shows it again. It takes
//! what overlayfs needs of an upper layer, which `home_overlay` in T1 shows
//! on a real guest, and keeps each of those as a record too.

#![cfg(target_os = "linux")]

mod guest;

use guest::*;
use share::{Mode, Root, Server};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const SETXATTR: u32 = 21;
const GETXATTR: u32 = 22;
const LISTXATTR: u32 = 23;
const REMOVEXATTR: u32 = 24;
const FATTR_GID: u32 = 4;
const RENAME_WHITEOUT: u32 = 4;
const ERANGE: i32 = 34;
const ENODATA: i32 = 61;
const S_IFCHR: u32 = 0o020_000;
/// The agent's uid and gid in the guest, which own everything in the share.
const AGENT: u32 = 1000;
const OPAQUE: &[u8] = b"trusted.overlay.opaque";

/// An empty directory for a share of kept state, removed when it drops.
struct State(PathBuf);

impl std::ops::Deref for State {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn state(label: &str) -> State {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("share-state-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    State(dir)
}

fn serve(dir: &Path) -> Guest {
    Guest::with(dir, Mode::State)
}

fn setxattr(guest: &mut Guest, node: u64, name: &[u8], value: &[u8], flags: u32) -> i32 {
    let mut body = (value.len() as u32).to_le_bytes().to_vec();
    body.extend(flags.to_le_bytes());
    body.extend(named(name));
    body.extend(value);
    guest.call(SETXATTR, node, &body).0
}

fn getxattr(guest: &mut Guest, node: u64, name: &[u8], size: u32) -> Reply {
    let mut body = size.to_le_bytes().to_vec();
    body.extend([0; 4]);
    body.extend(named(name));
    guest.call(GETXATTR, node, &body)
}

fn listxattr(guest: &mut Guest, node: u64, size: u32) -> Reply {
    let mut body = size.to_le_bytes().to_vec();
    body.extend([0; 4]);
    guest.call(LISTXATTR, node, &body)
}

fn mknod(guest: &mut Guest, parent: u64, name: &[u8], mode: u32) -> i32 {
    let mut body = Vec::new();
    for value in [mode, 0, 0, 0] {
        body.extend(value.to_le_bytes());
    }
    body.extend(named(name));
    guest.call(MKNOD, parent, &body).0
}

/// Gives `node` the owner `uid` and `gid`, as `struct fuse_setattr_in`.
fn chown(guest: &mut Guest, node: u64, uid: u32, gid: u32) -> i32 {
    let mut body = (FATTR_UID | FATTR_GID).to_le_bytes().to_vec();
    body.resize(76, 0);
    body.extend(uid.to_le_bytes());
    body.extend(gid.to_le_bytes());
    body.resize(88, 0);
    guest.call(SETATTR, node, &body).0
}

/// The mode the guest sees at `path`.
fn mode(guest: &mut Guest, path: &str) -> Result<u32, i32> {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = guest.walk(dir)?;
    Ok(guest.lookup(parent, name.as_bytes())?.1)
}

/// Every file beneath `dir`, which nothing in a share of kept state may
/// make anything but a regular file or a directory, or give an extended
/// attribute.
fn plain(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let kind = fs::symlink_metadata(&path).unwrap().file_type();
        assert!(kind.is_file() || kind.is_dir(), "{path:?} is {kind:?}");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is NUL-terminated, and a null buffer of no length
        // asks only how long the list is.
        let len = unsafe { libc::llistxattr(name.as_ptr(), std::ptr::null_mut(), 0) };
        assert_eq!(len, 0, "{path:?} has extended attributes");
        if kind.is_dir() {
            plain(&path);
        }
    }
}

#[test]
fn the_next_server_shows_what_the_guest_wrote() {
    let dir = state("kept");
    let mut guest = serve(&dir);
    let (_, handle) = guest.create(1, b"notes.txt", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"kept"), Ok(4));
    guest.release(handle);
    let made = guest.mkdir(1, b"made").unwrap();
    assert_eq!(setxattr(&mut guest, made, OPAQUE, b"y", 0), 0);
    assert_eq!(setxattr(&mut guest, made, b"user.gone", b"soon", 0), 0);
    assert_eq!(guest.call(REMOVEXATTR, made, &named(b"user.gone")).0, 0);
    guest.symlink(made, b"link", b"../notes.txt").unwrap();
    assert_eq!(mknod(&mut guest, made, b"whiteout", S_IFCHR), 0);
    let notes = guest.walk("notes.txt").unwrap();
    assert_eq!(guest.setattr(notes, FATTR_MODE, 0, 0o600), 0);
    drop(guest);

    let mut guest = serve(&dir);
    assert_eq!(guest.read_file("notes.txt").unwrap(), b"kept");
    assert_eq!(mode(&mut guest, "notes.txt"), Ok(S_IFREG | 0o600));
    let link = guest.walk("made/link").unwrap();
    assert_eq!(
        guest.call(READLINK, link, &[]),
        (0, b"../notes.txt".to_vec())
    );
    assert_eq!(mode(&mut guest, "made/whiteout"), Ok(S_IFCHR));
    let made = guest.walk("made").unwrap();
    assert_eq!(
        getxattr(&mut guest, made, OPAQUE, 0),
        (0, vec![1, 0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(getxattr(&mut guest, made, OPAQUE, 1), (0, b"y".to_vec()));
    let names = [OPAQUE, b"\0"].concat();
    assert_eq!(listxattr(&mut guest, made, 64), (0, names));
    assert_eq!(getxattr(&mut guest, made, b"user.gone", 64).0, ENODATA);
    let expected = [".", "..", "made", "notes.txt"].map(String::from);
    assert_eq!(guest.list(1), expected.into());
    drop(guest);
    plain(&dir);
}

#[test]
fn an_extended_attribute_keeps_to_its_limits_and_flags() {
    let dir = state("xattr-flags");
    let mut guest = serve(&dir);
    let made = guest.mkdir(1, b"made").unwrap();
    assert_eq!(setxattr(&mut guest, made, OPAQUE, b"y", 2), ENODATA);
    assert_eq!(setxattr(&mut guest, made, OPAQUE, b"y", 1), 0);
    assert_eq!(setxattr(&mut guest, made, OPAQUE, b"n", 1), EEXIST);
    assert_eq!(setxattr(&mut guest, made, OPAQUE, b"n", 2), 0);
    assert_eq!(getxattr(&mut guest, made, OPAQUE, 0).1[..4], [1, 0, 0, 0]);
    assert_eq!(getxattr(&mut guest, made, OPAQUE, 1), (0, b"n".to_vec()));
    let long = vec![b'a'; 256];
    assert_eq!(setxattr(&mut guest, made, &long, b"y", 0), ERANGE);
    assert_eq!(listxattr(&mut guest, made, 1).0, ERANGE);
    assert_eq!(
        guest.call(REMOVEXATTR, made, &named(b"user.none")).0,
        ENODATA
    );
}

#[test]
fn a_whiteout_rename_leaves_a_whiteout_and_an_exchange_trades_places() {
    let dir = state("renames");
    let mut guest = serve(&dir);
    guest.create(1, b"moved.txt", 0).unwrap();
    assert_eq!(
        guest.rename(1, b"moved.txt", 1, b"there.txt", RENAME_WHITEOUT),
        0
    );
    let left = guest.mkdir(1, b"left").unwrap();
    guest.create(left, b"from-left", 0).unwrap();
    assert_eq!(setxattr(&mut guest, left, OPAQUE, b"y", 0), 0);
    let right = guest.mkdir(1, b"right").unwrap();
    guest.create(right, b"from-right", 0).unwrap();
    assert_eq!(guest.rename(1, b"left", 1, b"right", RENAME_EXCHANGE), 0);
    // The guest's nodes follow what they name.
    assert_eq!(
        guest.list(left),
        [".", "..", "from-left"].map(String::from).into()
    );
    assert_eq!(guest.rename(1, b"left", 1, b"left", RENAME_EXCHANGE), 0);
    let both = RENAME_EXCHANGE | RENAME_NOREPLACE;
    assert_eq!(guest.rename(1, b"left", 1, b"right", both), EINVAL);
    assert_eq!(
        guest.rename(1, b"left", 1, b"none", RENAME_EXCHANGE),
        ENOENT
    );
    drop(guest);

    let mut guest = serve(&dir);
    assert_eq!(mode(&mut guest, "moved.txt"), Ok(S_IFCHR));
    assert_eq!(mode(&mut guest, "there.txt").map(kind), Ok(S_IFREG));
    let left = guest.walk("left").unwrap();
    let right = guest.walk("right").unwrap();
    assert_eq!(
        guest.list(left),
        [".", "..", "from-right"].map(String::from).into()
    );
    assert_eq!(
        guest.list(right),
        [".", "..", "from-left"].map(String::from).into()
    );
    assert_eq!(getxattr(&mut guest, right, OPAQUE, 1), (0, b"y".to_vec()));
    assert_eq!(getxattr(&mut guest, left, OPAQUE, 1).0, ENODATA);
    drop(guest);
    plain(&dir);
}

#[test]
fn a_file_takes_the_owner_it_has_and_no_other() {
    let dir = state("owner");
    let mut guest = serve(&dir);
    let (file, _) = guest.create(1, b"file", 0).unwrap();
    assert_eq!(chown(&mut guest, file, AGENT, AGENT), 0);
    assert_eq!(chown(&mut guest, file, 0, AGENT), EPERM);
    assert_eq!(chown(&mut guest, file, AGENT, 0), EPERM);
}

#[test]
fn a_record_half_written_goes_and_a_bad_one_stops_the_server() {
    let dir = state("records");
    let mut guest = serve(&dir);
    guest.mkdir(1, b"made").unwrap();
    drop(guest);
    let records = dir.join("records");
    let whole = fs::read(&records).unwrap();
    let mut file = OpenOptions::new().append(true).open(&records).unwrap();
    file.write_all(b"FILE \"made/half").unwrap();
    drop(file);

    let mut guest = serve(&dir);
    assert_eq!(fs::read(&records).unwrap(), whole);
    assert_eq!(mode(&mut guest, "made").map(kind), Ok(S_IFDIR));
    assert_eq!(guest.lookup(1, b"half"), Err(ENOENT));
    drop(guest);

    fs::write(&records, b"FILE \"../outside\" 644\n").unwrap();
    let root = OwnedFd::from(File::open(&*dir).unwrap());
    let started = Server::new(vec![Root::new(root, Mode::State)]);
    let error = started.err().map(|error| error.to_string());
    assert_eq!(
        error.as_deref(),
        Some("bad record \"FILE \\\"../outside\\\" 644\"")
    );
}

#[test]
fn a_copy_on_write_share_takes_none_of_what_overlayfs_needs() {
    let tree = Tree::new("cow-overlay");
    let mut guest = Guest::cow(&tree.root, &tree.base.join("pending"));
    let dir = guest.walk("dir").unwrap();
    assert_eq!(setxattr(&mut guest, dir, OPAQUE, b"y", 0), ENOSYS);
    assert_eq!(getxattr(&mut guest, dir, OPAQUE, 0).0, ENOSYS);
    let flags = [RENAME_WHITEOUT, RENAME_EXCHANGE];
    for flag in flags {
        assert_eq!(guest.rename(1, b"hello.txt", 1, b"link", flag), EINVAL);
    }
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(chown(&mut guest, hello, AGENT, AGENT), EPERM);
}
