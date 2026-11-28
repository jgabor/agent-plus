//! A share whose root holds a bind mount. Past the top of a bind mount, the
//! kernel reads a file's path as `/`, so the path of a file the host moved
//! there starts with the path of a root that is `/`.

#![cfg(target_os = "linux")]

mod guest;

use guest::*;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};

/// The directory to bind, and where, for the server's process.
const ARGS: &str = "AP_SHARE_BIND_ARGS";

/// The share server's process, when the test below starts this program
/// again with [`ARGS`] set. It runs before the test harness, so that it is
/// the process's only thread, which a user namespace needs. In a mount
/// namespace of its own, it binds the directory, and serves `/` read-only.
/// Its standard input is the socket.
#[used]
#[unsafe(link_section = ".init_array")]
static SERVER_PROCESS: extern "C" fn() = server_process;

extern "C" fn server_process() {
    let Some(args) = env::var_os(ARGS) else {
        return;
    };
    // As the Rust runtime does before `main`, which hasn't run.
    // SAFETY: ignoring a signal changes no memory.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let args = args.into_string().unwrap();
    let (from, to) = args.split_once('\n').unwrap();
    assert!(alone(), "the server's process has threads it doesn't own");
    // SAFETY: both calls take nothing, and can't fail.
    let me = unsafe { (libc::geteuid(), libc::getegid()) };
    // SAFETY: this call takes only flags.
    let unshared = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
    assert_eq!(unshared, 0, "{}", std::io::Error::last_os_error());
    fs::write("/proc/self/setgroups", "deny").unwrap();
    fs::write("/proc/self/uid_map", format!("{0} {0} 1\n", me.0)).unwrap();
    fs::write("/proc/self/gid_map", format!("{0} {0} 1\n", me.1)).unwrap();
    let path = |path: &str| std::ffi::CString::new(path).unwrap();
    let (from, to) = (path(from), path(to));
    // SAFETY: both paths are valid strings, and a bind takes no data.
    let bound = unsafe {
        libc::mount(
            from.as_ptr(),
            to.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    assert_eq!(bound, 0, "{}", std::io::Error::last_os_error());
    // Opened here, so that it reaches the bind, which is in this namespace
    // alone.
    let root = File::open("/").unwrap().into_raw_fd();
    let status = share::main(&[OsString::from("ro"), OsString::from(root.to_string())]);
    // SAFETY: `_exit` ends the process at once.
    unsafe { libc::_exit(i32::from(status)) };
}

// spec(SHARE): The agent sees only what is under the share's root.
#[test]
fn a_known_file_the_host_moves_past_a_bind_mount_s_top_and_deletes_shows_no_attributes() {
    let tree = Tree::new("bind");
    let base = tree.base.canonicalize().unwrap();
    let (top, mount) = (base.join("top"), base.join("mount"));
    fs::create_dir(&top).unwrap();
    fs::create_dir(&mount).unwrap();
    fs::write(top.join("known.txt"), INSIDE).unwrap();
    let (mine, theirs) = UnixStream::pair().unwrap();
    let args = format!("{}\n{}", top.display(), mount.display());
    let mut server = Command::new(env::current_exe().unwrap())
        .env(ARGS, args)
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut guest = Guest::over(OwnedFd::from(mine));
    let path = |path: &Path| path.to_str().unwrap().to_owned();
    let known = guest.walk(&path(&mount.join("known.txt"))).unwrap();
    let size = |guest: &mut Guest| match guest.call(GETATTR, known, &[0; 16]) {
        (0, out) => Ok(u64::from_le_bytes(out[24..32].try_into().unwrap())),
        (error, _) => Err(error),
    };
    assert_eq!(size(&mut guest), Ok(INSIDE.len() as u64));
    // Out of the bound directory, the file is past the mount's top, where
    // no name beneath the root leads to it.
    let away = base.join("known.txt");
    fs::rename(top.join("known.txt"), &away).unwrap();
    fs::write(&away, CANARY).unwrap();
    assert_eq!(size(&mut guest), Err(ENOENT));
    fs::remove_file(&away).unwrap();
    assert_eq!(size(&mut guest), Err(ENOENT));
    drop(guest);
    assert!(server.wait().unwrap().success());
}
