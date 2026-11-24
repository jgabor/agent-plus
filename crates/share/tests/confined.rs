//! The share server as its own confined process, as `ap` starts it: every
//! change an agent makes in an `rw` share gets past its Landlock rules and
//! its filter, which would kill it, and an `ro` share beside it still
//! refuses every write.

mod guest;

use guest::*;
use std::env;
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// The share server's arguments, one to a line, for the process the test
/// starts.
const ARGS: &str = "AP_SHARE_CONFINED_ARGS";
/// What the server's process does once it has served, as if its own code
/// were turned against it: the number of a descriptor it inherited, the
/// call it opens with, and a path from the descriptor to a file whose mode
/// and times it tries to change.
const AIM: &str = "AP_SHARE_CONFINED_AIM";
const SERVER: &str = "confined_server_process";
/// The server runs on a test's thread here, and `ap` runs it on its main
/// thread. One malloc arena keeps the test's like `ap`'s, which grows with
/// `brk`, where a thread's own arena grows with `mprotect`, which the
/// filter refuses.
const ARENAS: &str = "MALLOC_ARENA_MAX";
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
    // A copy of the aim's descriptor, which the server doesn't close.
    let aim = env::var(AIM).ok().map(|aim| {
        let mut lines = aim.lines();
        let fd = lines.next().unwrap().parse().unwrap();
        // SAFETY: the test passed this descriptor, and it is open.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let call = lines.next().unwrap() == "openat2";
        let path = CString::new(lines.next().unwrap()).unwrap();
        (share::inheritable(fd).unwrap(), call, path)
    });
    let status = share::main(&args);
    if let (0, Some((directory, openat2, path))) = (status, aim) {
        turn(&directory, openat2, &path);
    }
    // The filter allows nothing the test harness does after a test, so
    // the process ends here.
    // SAFETY: `_exit` ends the process at once.
    unsafe { libc::_exit(i32::from(status)) };
}

/// What a server's code could do if it were turned against its rules: open
/// the file at `path` from `directory`, with `openat2` or `openat` and no
/// rule to stay beneath it, and change its mode and times through what it
/// opened. Access mode 3 asks for neither reading nor writing.
fn turn(directory: &OwnedFd, openat2: bool, path: &CStr) {
    #[repr(C)]
    struct How {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    for flags in [libc::O_RDONLY, libc::O_PATH, libc::O_ACCMODE] {
        let flags = flags | libc::O_CLOEXEC;
        let how = How {
            flags: flags as u64,
            mode: 0,
            resolve: 0,
        };
        // SAFETY: `path` and `how` outlive each call, which returns a new
        // descriptor or -1.
        let fd = unsafe {
            match openat2 {
                true => libc::syscall(
                    libc::SYS_openat2,
                    directory.as_raw_fd(),
                    path.as_ptr(),
                    &raw const how,
                    size_of::<How>(),
                ),
                false => libc::syscall(
                    libc::SYS_openat,
                    directory.as_raw_fd(),
                    path.as_ptr(),
                    flags,
                ),
            }
        };
        let Ok(fd) = i32::try_from(fd) else {
            continue;
        };
        if fd < 0 {
            continue;
        }
        let times = [libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }; 2];
        // SAFETY: both calls take a descriptor and plain values, and
        // `times` outlives the call.
        unsafe {
            libc::syscall(libc::SYS_fchmod, fd, 0o777);
            let null = std::ptr::null::<libc::c_char>();
            libc::syscall(libc::SYS_utimensat, fd, null, times.as_ptr(), 0);
        }
    }
}

/// Starts a confined server with one `rw` share at `root`, which, once it
/// has served, turns on `path` from the root with `call`. Returns how it
/// ended.
fn serve_and_turn(root: &Path, call: &str, path: &str) -> ExitStatus {
    let (mine, theirs) = UnixStream::pair().unwrap();
    let inherited = share::inheritable(File::open(root).unwrap().as_fd()).unwrap();
    let fd = inherited.as_raw_fd();
    let mut server = Command::new(env::current_exe().unwrap())
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, format!("rw\n{fd}"))
        .env(AIM, format!("{fd}\n{call}\n{path}"))
        .env(ARENAS, "1")
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(inherited);
    drop(mine);
    server.wait().unwrap()
}

/// The mode and modification time of the file at `path`.
fn mode_and_time(path: &Path) -> (u32, i64) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.permissions().mode(), metadata.mtime())
}

#[test]
fn a_confined_rw_server_can_t_change_the_mode_or_times_of_a_file_outside_it() {
    let tree = Tree::new("confined-turned");
    let before = mode_and_time(&tree.secret);
    let absolute = tree.secret.to_str().unwrap();
    for call in ["openat", "openat2"] {
        for path in ["../outside/secret", absolute] {
            // The filter kills it at `openat2`, or at access mode 3.
            let status = serve_and_turn(&tree.root, call, path);
            assert_eq!(status.signal(), Some(libc::SIGSYS), "{call} {path}");
            assert_eq!(mode_and_time(&tree.secret), before, "{call} {path}");
        }
    }

    // The same server changes a file of its own share that way.
    serve_and_turn(&tree.root, "openat", "hello.txt");
    let hello = tree.root.join("hello.txt");
    assert_eq!(mode_and_time(&hello), (0o100_777, 0));
}

#[test]
fn a_confined_server_changes_an_rw_share_and_nothing_else() {
    let tree = Tree::new("confined-rw");
    let other = Tree::new("confined-ro");
    let (mine, theirs) = UnixStream::pair().unwrap();
    // Each root reaches the server as `share::start` passes it: a
    // descriptor it inherits, numbered in its arguments.
    let roots = [&tree.root, &other.root].map(|root| File::open(root).unwrap());
    let inherited = roots
        .each_ref()
        .map(|root| share::inheritable(root.as_fd()).unwrap());
    let args = format!(
        "rw\n{}\nro\n{}",
        inherited[0].as_raw_fd(),
        inherited[1].as_raw_fd()
    );
    let mut server = Command::new(env::current_exe().unwrap())
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, args)
        .env(ARENAS, "1")
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    drop(inherited);
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
    // Access mode 3 asks for neither reading nor writing, and opens.
    let neither = guest.open(file, 3).unwrap();
    guest.release(neither);
    assert_eq!(guest.setattr(file, FATTR_SIZE, 4, 0), 0);
    assert_eq!(guest.setattr(file, FATTR_MODE, 0, 0), 0);
    assert_eq!(guest.setattr(file, FATTR_MTIME, 0, 0), 0);
    assert_eq!(guest.setattr(file, FATTR_MODE, 0, 0o640), 0);
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
    // A directory the host moves outside the root is gone, and the server
    // finds that out within its filter.
    let away = guest.mkdir(1, b"away").unwrap();
    fs::rename(root.join("away"), tree.base.join("outside/away")).unwrap();
    assert_eq!(guest.create(away, b"planted", 0), Err(ENOENT));
    assert!(!tree.base.join("outside/away/planted").exists());

    // The `ro` share, beside it, refuses every write, and still locks.
    let (error, entry) = guest.call_share(1, LOOKUP, 1, b"hello.txt\0");
    assert_eq!(error, 0);
    let node = u64::from_le_bytes(entry[..8].try_into().unwrap());
    let mut body = O_WRONLY.to_le_bytes().to_vec();
    body.extend([0; 4]);
    assert_eq!(guest.call_share(1, OPEN, node, &body).0, EROFS);
    let mut neither = 3u32.to_le_bytes().to_vec();
    neither.extend([0; 4]);
    assert_eq!(guest.call_share(1, OPEN, node, &neither).0, EROFS);
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

#[test]
fn a_confined_server_keeps_a_copy_on_write_share_s_changes_pending() {
    let tree = Tree::new("confined-cow");
    let pending = tree.base.join("pending");
    fs::create_dir(&pending).unwrap();
    let (mine, theirs) = UnixStream::pair().unwrap();
    let dirs = [&tree.root, &pending].map(|dir| File::open(dir).unwrap());
    let inherited = dirs
        .each_ref()
        .map(|dir| share::inheritable(dir.as_fd()).unwrap());
    let args = format!(
        "cow\n{}\n{}",
        inherited[0].as_raw_fd(),
        inherited[1].as_raw_fd()
    );
    let mut server = Command::new(env::current_exe().unwrap())
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, args)
        .env(ARENAS, "1")
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    drop(inherited);
    let mut guest = Guest::over(OwnedFd::from(mine));

    guest.overwrite("hello.txt", b"from the agent").unwrap();
    let hello = guest.walk("hello.txt").unwrap();
    let handle = guest.open(hello, O_RDWR).unwrap();
    assert_eq!(guest.lock(SETLK, handle, 7, F_WRLCK, 0).0, 0);
    let mut sync = handle.to_le_bytes().to_vec();
    sync.extend([0; 8]);
    assert_eq!(guest.call(FSYNC, hello, &sync).0, 0);
    guest.release(handle);
    let neither = guest.open(hello, 3).unwrap();
    guest.release(neither);
    assert_eq!(guest.setattr(hello, FATTR_MODE, 0, 0o600), 0);
    assert_eq!(guest.setattr(hello, FATTR_MTIME, 0, 0), 0);
    let made = guest.mkdir(1, b"made").unwrap();
    guest.symlink(made, b"link", b"../hello.txt").unwrap();
    assert!(guest.mknod(made, b"fifo", S_IFIFO | 0o644, 0).is_ok());
    assert_eq!(guest.rename(1, b"made", 1, b"moved", 0), 0);
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), 0);
    assert_eq!(guest.remove(RMDIR, 1, b"dir"), 0);
    assert_eq!(guest.list(1).len(), 8);

    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
    assert!(tree.root.join("dir/nested.txt").exists());
    assert!(!tree.root.join("made").exists() && !tree.root.join("moved").exists());
    assert_eq!(
        fs::read(pending.join("tree/hello.txt")).unwrap(),
        b"from the agent"
    );
    assert!(records(&pending).contains(&r#"DELETE "dir""#.to_owned()));
}

#[test]
fn a_confined_server_serves_a_file_share_s_file_alone() {
    let tree = Tree::new("confined-file");
    let (mine, theirs) = UnixStream::pair().unwrap();
    let root = File::open(&tree.root).unwrap();
    let inherited = [
        share::inheritable(root.as_fd()).unwrap(),
        share::inheritable(root.as_fd()).unwrap(),
    ];
    let args = format!(
        "file\nhello.txt\nrw\n{}\nfile\nhello.txt\nro\n{}",
        inherited[0].as_raw_fd(),
        inherited[1].as_raw_fd()
    );
    let mut server = Command::new(env::current_exe().unwrap())
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, args)
        .env(ARENAS, "1")
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    drop(inherited);
    let mut guest = Guest::over(OwnedFd::from(mine));

    guest.overwrite("hello.txt", b"from the agent").unwrap();
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.setattr(hello, FATTR_SIZE, 4, 0), 0);
    assert_eq!(guest.read_file("dir/nested.txt"), Err(ENOENT));
    assert_eq!(guest.create(1, b"new.txt", 0), Err(EPERM));

    // The `ro` share of the same file reads it, and refuses every write.
    let (error, entry) = guest.call_share(1, LOOKUP, 1, b"hello.txt\0");
    assert_eq!(error, 0);
    let node = u64::from_le_bytes(entry[..8].try_into().unwrap());
    let (error, out) = guest.call_share(1, OPEN, node, &[0; 8]);
    assert_eq!(error, 0);
    let mut read = out[..8].to_vec();
    read.extend(0u64.to_le_bytes());
    read.extend(4096u32.to_le_bytes());
    read.extend([0; 20]);
    assert_eq!(guest.call_share(1, READ, 0, &read), (0, b"from".to_vec()));
    let mut body = O_WRONLY.to_le_bytes().to_vec();
    body.extend([0; 4]);
    assert_eq!(guest.call_share(1, OPEN, node, &body).0, EROFS);
    assert_eq!(guest.call_share(1, LOOKUP, 1, b"dir\0").0, ENOENT);

    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), b"from");
    assert!(!tree.root.join("new.txt").exists());
}
