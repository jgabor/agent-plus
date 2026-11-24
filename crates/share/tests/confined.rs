//! The share server as its own confined process, as `ap` starts it: every
//! change an agent makes in an `rw` share gets past its Landlock rules and
//! its filter, which would kill it, and an `ro` share, which has a server of
//! its own, refuses every write. A copy-on-write share's server can't change
//! your files' modes or times, and a server with an `rw` share holds no
//! other share, even if its own code were turned against it.

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
use std::process::{Child, Command, ExitStatus, Stdio};

/// The share server's arguments, one to a line, for the process the test
/// starts.
const ARGS: &str = "AP_SHARE_CONFINED_ARGS";
/// What the server's process does once it has served, as if its own code
/// were turned against it: the number of a descriptor it inherited, the
/// call it opens with, a path from the descriptor to a file, and the calls
/// it tries on that file, `fchmod`, `futimens`, or both, and `raise` first
/// to take up every capability it holds.
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
        let changes: Vec<_> = lines.next().unwrap().split(' ').collect();
        let changes = ["fchmod", "futimens", "raise"].map(|change| changes.contains(&change));
        (share::inheritable(fd).unwrap(), call, path, changes)
    });
    let status = share::main(&args);
    if let (0, Some((directory, openat2, path, changes))) = (status, aim) {
        turn(&directory, openat2, &path, changes);
    }
    // The filter allows nothing the test harness does after a test, so
    // the process ends here.
    // SAFETY: `_exit` ends the process at once.
    unsafe { libc::_exit(i32::from(status)) };
}

/// What a server's code could do if it were turned against its rules: take
/// up every capability it holds if `raise` says so, open the file at `path`
/// from `directory`, with `openat2` or `openat` and no rule to stay beneath
/// it, and change its mode, its times, or both, as `mode` and `times` say,
/// through what it opened. Access mode 3 asks for neither reading nor
/// writing.
fn turn(directory: &OwnedFd, openat2: bool, path: &CStr, [mode, times, raise]: [bool; 3]) {
    #[repr(C)]
    struct How {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    // `struct __user_cap_header_struct` and `struct __user_cap_data_struct`,
    // as version 3 of capabilities reads them.
    let header = [0x2008_0522_u32, 0];
    let mut data = [0_u32; 6];
    if raise {
        // SAFETY: both calls read `header`, and read or write `data`, which
        // outlive them.
        unsafe {
            libc::syscall(libc::SYS_capget, header.as_ptr(), data.as_mut_ptr());
            data[0] = data[1];
            data[3] = data[4];
            libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr());
        }
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
        let zero = [libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }; 2];
        // SAFETY: both calls take a descriptor and plain values, and
        // `zero` outlives the call.
        unsafe {
            if mode {
                libc::syscall(libc::SYS_fchmod, fd, 0o777);
            }
            if times {
                let null = std::ptr::null::<libc::c_char>();
                libc::syscall(libc::SYS_utimensat, fd, null, zero.as_ptr(), 0);
            }
        }
    }
}

/// Starts a confined server with one `rw` share at `root`, which, once it
/// has served, turns on `path` from the root with `call`, and with every
/// capability it holds if `raise` says so. Returns how it ended.
fn serve_and_turn(root: &Path, call: &str, path: &str, raise: bool) -> ExitStatus {
    let raise = if raise { " raise" } else { "" };
    let aim = format!("{call}\n{path}\nfchmod futimens{raise}");
    let (mut server, socket) = start(&[("rw", root)], Some((0, &aim)));
    drop(socket);
    server.wait().unwrap()
}

/// Starts one share server's process for `roots`, which it serves
/// together, passing each root as `share::start` does: a descriptor it
/// inherits, numbered in its arguments after the words that come before
/// it. With `aim`, once it has served, it turns
/// from the descriptor of root `index` as the rest of the aim says.
/// Returns the process, and the other end of its socket.
fn start(roots: &[(&str, &Path)], aim: Option<(usize, &str)>) -> (Child, OwnedFd) {
    // `ap` starts a server of `rw` shares alone in a user namespace.
    let namespace = roots.iter().all(|(words, _)| words.ends_with("rw"));
    launch(roots, aim, namespace)
}

/// Starts a share server as [`start`] does, in a user namespace as
/// `namespace` says.
fn launch(
    roots: &[(&str, &Path)],
    aim: Option<(usize, &str)>,
    namespace: bool,
) -> (Child, OwnedFd) {
    let (mine, theirs) = UnixStream::pair().unwrap();
    let inherited: Vec<OwnedFd> = (roots.iter())
        .map(|(_, root)| share::inheritable(File::open(root).unwrap().as_fd()).unwrap())
        .collect();
    let mut args = Vec::new();
    for ((words, _), fd) in roots.iter().zip(&inherited) {
        if !words.is_empty() {
            args.push(words.to_string());
        }
        args.push(fd.as_raw_fd().to_string());
    }
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([SERVER, "--exact", "--test-threads=1", "--quiet"])
        .env(ARGS, args.join("\n"))
        .env(ARENAS, "1")
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some((index, rest)) = aim {
        command.env(AIM, format!("{}\n{rest}", inherited[index].as_raw_fd()));
    }
    let server = match namespace {
        true => share::spawn_read_write(&mut command).unwrap(),
        false => command.spawn().unwrap(),
    };
    // The command holds the server's end of the socket.
    drop(command);
    drop(inherited);
    (server, OwnedFd::from(mine))
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
            let status = serve_and_turn(&tree.root, call, path, false);
            assert_eq!(status.signal(), Some(libc::SIGSYS), "{call} {path}");
            assert_eq!(mode_and_time(&tree.secret), before, "{call} {path}");
        }
    }

    // The same server changes a file of its own share that way.
    serve_and_turn(&tree.root, "openat", "hello.txt", false);
    let hello = tree.root.join("hello.txt");
    assert_eq!(mode_and_time(&hello), (0o100_777, 0));
}

// #697: a server that may open a file past its mode, to change the mode,
// still opens nothing outside its roots.
#[test]
fn a_confined_rw_server_can_t_change_the_mode_of_an_unreadable_file_outside_it() {
    let tree = Tree::new("confined-turned-unreadable");
    fs::set_permissions(&tree.secret, fs::Permissions::from_mode(0o0)).unwrap();
    let before = mode_and_time(&tree.secret);
    for path in ["../outside/secret", tree.secret.to_str().unwrap()] {
        let status = serve_and_turn(&tree.root, "openat", path, true);
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{path}");
        assert_eq!(mode_and_time(&tree.secret), before, "{path}");
    }
    // The same server changes a file of its own share that way.
    let hello = tree.root.join("hello.txt");
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o0)).unwrap();
    serve_and_turn(&tree.root, "openat", "hello.txt", true);
    assert_eq!(mode_and_time(&hello), (0o100_777, 0));
}

// spec(SHARE): In an `rw` share, the agent can change the mode and times of a file or a directory it can't read, if you own it and its group is your primary group, as you can on your host.
// spec(SHARE): On a host that doesn't let you make a user namespace, it can't.
#[test]
fn a_confined_rw_server_changes_the_mode_of_what_it_can_t_read() {
    let tree = Tree::new("confined-unreadable");
    fs::create_dir(tree.root.join("shut")).unwrap();
    for path in ["hello.txt", "shut"] {
        fs::set_permissions(tree.root.join(path), fs::Permissions::from_mode(0o0)).unwrap();
    }
    let (mut server, socket) = start(&[("rw", &tree.root)], None);
    let mut guest = Guest::over(socket);
    for (path, kind, mode) in [("hello.txt", 0o100_000, 0o600), ("shut", 0o040_000, 0o700)] {
        let node = guest.walk(path).unwrap();
        assert_eq!(guest.setattr(node, FATTR_MTIME, 0, 0), 0, "{path}");
        assert_eq!(guest.setattr(node, FATTR_MODE, 0, mode), 0, "{path}");
        assert_eq!(
            mode_and_time(&tree.root.join(path)),
            (kind | mode, 0),
            "{path}"
        );
        // And again once the guest has forgotten it.
        assert_eq!(guest.setattr(node, FATTR_MODE, 0, 0), 0, "{path}");
        guest.call(FORGET, node, &u64::MAX.to_le_bytes());
        let node = guest.walk(path).unwrap();
        assert_eq!(guest.setattr(node, FATTR_MODE, 0, mode), 0, "{path}");
        assert_eq!(
            mode_and_time(&tree.root.join(path)).0,
            kind | mode,
            "{path}"
        );
    }
    // What it can't write, it still can't truncate.
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.setattr(hello, FATTR_MODE, 0, 0o400), 0);
    assert_eq!(guest.setattr(hello, FATTR_SIZE, 0, 0), EACCES);
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
    drop(guest);
    assert!(server.wait().unwrap().success());

    // Where the host makes no user namespace, `ap` starts the server
    // outside one, as here.
    fs::set_permissions(tree.root.join("hello.txt"), fs::Permissions::from_mode(0o0)).unwrap();
    let (mut server, socket) = launch(&[("rw", &tree.root)], None, false);
    let mut guest = Guest::over(socket);
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.setattr(hello, FATTR_MODE, 0, 0o600), EACCES);
    assert_eq!(mode_and_time(&tree.root.join("hello.txt")).0, 0o100_000);
    drop(guest);
    assert!(server.wait().unwrap().success());
}

// #440: Landlock doesn't cover a file's mode or times, and a server with
// an `rw` share may change those, so it holds no other share's root.
#[test]
fn a_server_that_changes_modes_and_times_serves_only_rw_shares() {
    for mode in ["ro", "cow"] {
        let tree = Tree::new(&format!("confined-mixed-{mode}-rw"));
        let other = Tree::new(&format!("confined-mixed-{mode}"));
        let pending = other.base.join("pending");
        fs::create_dir(&pending).unwrap();
        let hello = other.root.join("hello.txt");
        let before = mode_and_time(&hello);
        let mut roots = vec![("rw", tree.root.as_path()), (mode, &other.root)];
        if mode == "cow" {
            roots.push(("", &pending));
        }
        let aim = "openat\nhello.txt\nfchmod futimens";
        let (mut server, socket) = start(&roots, Some((1, aim)));
        drop(socket);
        let status = server.wait().unwrap();
        assert_eq!(mode_and_time(&hello), before, "{mode}");
        // It refuses the roots before it serves, and exits with 1.
        assert_eq!(status.code(), Some(1), "{mode} {status}");
    }
}

#[test]
fn a_confined_server_changes_an_rw_share_and_nothing_else() {
    let tree = Tree::new("confined-rw");
    let (mut server, socket) = start(&[("rw", &tree.root)], None);
    let mut guest = Guest::over(socket);
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

    // Closing the socket ends the server, which lived through all of it.
    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
}

#[test]
fn a_confined_ro_server_refuses_every_write_and_still_locks() {
    let tree = Tree::new("confined-ro");
    let (mut server, socket) = start(&[("ro", &tree.root)], None);
    let mut guest = Guest::over(socket);

    let node = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.open(node, O_WRONLY), Err(EROFS));
    assert_eq!(guest.open(node, 3), Err(EROFS));
    assert_eq!(guest.call(MKDIR, 1, &[0; 16]).0, EROFS);
    let handle = guest.open(node, 0).unwrap();
    assert_eq!(guest.lock(SETLK, handle, 7, F_RDLCK, 0).0, 0);

    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
}

#[test]
fn a_confined_server_keeps_a_copy_on_write_share_s_changes_pending() {
    let tree = Tree::new("confined-cow");
    let pending = tree.base.join("pending");
    fs::create_dir(&pending).unwrap();
    let (mut server, socket) = start(&[("cow", &tree.root), ("", &pending)], None);
    let mut guest = Guest::over(socket);

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
    // A file the agent removed takes its times through its handle.
    let (file, handle) = guest.create(1, b"temp", 0).unwrap();
    assert_eq!(guest.remove(UNLINK, 1, b"temp"), 0);
    let mut body = (FATTR_MTIME | 64).to_le_bytes().to_vec();
    body.extend([0; 4]);
    body.extend(handle.to_le_bytes());
    body.resize(88, 0);
    assert_eq!(guest.call(SETATTR, file, &body).0, 0);
    guest.release(handle);
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
fn a_copy_on_write_server_can_t_change_the_mode_or_times_of_your_file() {
    for call in ["fchmod", "futimens"] {
        let tree = Tree::new(&format!("confined-cow-turned-{call}"));
        let pending = tree.base.join("pending");
        fs::create_dir(&pending).unwrap();
        let hello = tree.root.join("hello.txt");
        let before = mode_and_time(&hello);
        let roots = [("cow", tree.root.as_path()), ("", &pending)];
        let aim = format!("openat\nhello.txt\n{call}");
        let (mut server, socket) = start(&roots, Some((0, &aim)));
        let mut guest = Guest::over(socket);
        // The agent changes its own copy's mode and times first.
        let file = guest.walk("hello.txt").unwrap();
        assert_eq!(guest.setattr(file, FATTR_MODE, 0, 0o600), 0);
        assert_eq!(guest.setattr(file, FATTR_MTIME, 0, 0), 0);

        drop(guest);
        let status = server.wait().unwrap();
        assert_eq!(mode_and_time(&hello), before, "{call}");
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{call} {status}");
    }
}

#[test]
fn a_confined_server_serves_a_file_share_s_file_alone() {
    let tree = Tree::new("confined-file");
    let (mut server, socket) = start(&[("file\nhello.txt\nrw", &tree.root)], None);
    let mut guest = Guest::over(socket);

    guest.overwrite("hello.txt", b"from the agent").unwrap();
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.setattr(hello, FATTR_SIZE, 4, 0), 0);
    assert_eq!(guest.read_file("dir/nested.txt"), Err(ENOENT));
    assert_eq!(guest.create(1, b"new.txt", 0), Err(EPERM));
    drop(guest);
    assert!(server.wait().unwrap().success());

    // An `ro` share of the same file, from a server of its own, reads it,
    // and refuses every write.
    let (mut server, socket) = start(&[("file\nhello.txt\nro", &tree.root)], None);
    let mut guest = Guest::over(socket);
    assert_eq!(guest.read_file("hello.txt"), Ok(b"from".to_vec()));
    let node = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.open(node, O_WRONLY), Err(EROFS));
    assert_eq!(guest.lookup(1, b"dir"), Err(ENOENT));

    drop(guest);
    assert!(server.wait().unwrap().success());
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), b"from");
    assert!(!tree.root.join("new.txt").exists());
}
