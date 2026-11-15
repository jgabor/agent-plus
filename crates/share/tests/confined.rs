//! The share server as its own confined process, as `ap` starts it: every
//! change an agent makes in an `rw` share gets past its Landlock rules and
//! its filter, which would kill it, and an `ro` share beside it still
//! refuses every write.

mod guest;

use guest::*;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

/// The share server's arguments, one to a line, for the process the test
/// starts.
const ARGS: &str = "AP_SHARE_CONFINED_ARGS";
const SERVER: &str = "confined_server_process";
const FALLOCATE: u32 = 43;
const FSYNC: u32 = 20;

/// The share server's process, when the test below starts it. Its standard
/// input is the socket.
#[test]
fn confined_server_process() {
    let Some(args) = env::var_os(ARGS) else {
        return;
    };
    let args: Vec<OsString> = args.to_str().unwrap().lines().map(OsString::from).collect();
    let status = share::main(&args);
    // The filter allows nothing the test harness does after a test, so
    // the process ends here.
    // SAFETY: `_exit` ends the process at once.
    unsafe { libc::_exit(i32::from(status)) };
}

#[test]
fn a_confined_server_changes_an_rw_share_and_nothing_else() {
    let tree = Tree::new("confined-rw");
    let other = Tree::new("confined-ro");
    let (mine, theirs) = UnixStream::pair().unwrap();
    let args = format!("rw\n{}\nro\n{}", tree.root.display(), other.root.display());
    let mut server = Command::new(env::current_exe().unwrap())
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, args)
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut guest = Guest::over(OwnedFd::from(mine));
    let root = tree.root.clone();

    let (file, handle) = guest.create(1, b"new.txt", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"from the agent"), Ok(14));
    let mut range = handle.to_le_bytes().to_vec();
    for value in [0u64, 4096] {
        range.extend(value.to_le_bytes());
    }
    range.extend([0; 8]);
    assert_eq!(guest.call(FALLOCATE, file, &range).0, 0);
    let mut sync = handle.to_le_bytes().to_vec();
    sync.extend([0; 8]);
    assert_eq!(guest.call(FSYNC, file, &sync).0, 0);
    assert_eq!(guest.lock(SETLK, handle, 7, F_WRLCK, 0).0, 0);
    assert_eq!(guest.lock(GETLK, handle, 8, F_WRLCK, 0).0, 0);
    assert_eq!(guest.lock(SETLK, handle, 7, F_WRLCK, LK_FLOCK).0, 0);
    let mut flush = handle.to_le_bytes().to_vec();
    flush.extend([0; 8]);
    flush.extend(7u64.to_le_bytes());
    assert_eq!(guest.call(FLUSH, file, &flush).0, 0);
    guest.release(handle);
    assert_eq!(guest.setattr(file, FATTR_SIZE, 4, 0), 0);
    assert_eq!(guest.setattr(file, FATTR_MODE, 0, 0o640), 0);
    assert_eq!(guest.setattr(file, FATTR_MTIME, 0, 0), 0);
    let made = guest.mkdir(1, b"made").unwrap();
    guest.symlink(made, b"link", b"../new.txt").unwrap();
    guest.link(file, made, b"hard").unwrap();
    assert_eq!(guest.rename(1, b"new.txt", made, b"moved", 0), 0);
    assert_eq!(
        guest.rename(made, b"moved", 1, b"hello.txt", RENAME_EXCHANGE),
        0
    );
    assert_eq!(fs::read(root.join("hello.txt")).unwrap(), b"from");
    assert_eq!(fs::read(root.join("made/hard")).unwrap(), b"from");
    for name in [&b"link"[..], b"hard", b"moved"] {
        assert_eq!(guest.remove(UNLINK, made, name), 0);
    }
    assert_eq!(guest.remove(RMDIR, 1, b"made"), 0);
    assert_eq!(guest.create(1, b"Agentfile", 0), Err(EPERM));

    // The `ro` share, beside it, refuses every write, and still locks.
    let (error, entry) = guest.call_share(1, LOOKUP, 1, b"hello.txt\0");
    assert_eq!(error, 0);
    let node = u64::from_le_bytes(entry[..8].try_into().unwrap());
    let mut body = O_WRONLY.to_le_bytes().to_vec();
    body.extend([0; 4]);
    assert_eq!(guest.call_share(1, OPEN, node, &body).0, EROFS);
    assert_eq!(guest.call_share(1, MKDIR, 1, &[0; 16]).0, EROFS);
    let (error, out) = guest.call_share(1, OPEN, node, &[0; 8]);
    assert_eq!(error, 0);
    let handle = u64::from_le_bytes(out[..8].try_into().unwrap());
    let mut lock = handle.to_le_bytes().to_vec();
    lock.extend(7u64.to_le_bytes());
    lock.extend(0u64.to_le_bytes());
    lock.extend(i64::MAX.to_le_bytes());
    lock.extend(F_RDLCK.to_le_bytes());
    lock.extend([0; 12]);
    assert_eq!(guest.call_share(1, SETLK, 0, &lock).0, 0);
    assert_eq!(fs::read(other.root.join("hello.txt")).unwrap(), INSIDE);

    // Closing the socket ends the server, which lived through all of it.
    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
}
