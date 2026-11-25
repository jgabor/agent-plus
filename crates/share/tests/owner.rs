//! Who owns a shared file, as the agent sees it. The agent runs as uid and
//! gid 1000, and the guest's kernel checks each access against the owner
//! the server reports, so the host user's files must appear as the agent's
//! whatever that user's uid and gid are.

#![cfg(target_os = "linux")]

mod guest;

use guest::*;
use std::env;
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

const AGENT: (u32, u32) = (1000, 1000);
const NOBODY: (u32, u32) = (65534, 65534);
/// The uid and gid the server runs as below, in a user namespace of its own.
const OTHER: u32 = 1001;
const ARGS: &str = "AP_SHARE_OWNER_ARGS";

/// The owner in the `struct fuse_attr` at `at` in a reply.
fn owner_at(reply: &[u8], at: usize) -> (u32, u32) {
    let word =
        |offset: usize| u32::from_le_bytes(reply[at + offset..at + offset + 4].try_into().unwrap());
    (word(68), word(72))
}

impl Guest {
    /// The owner and permission bits of `path`, as the guest sees them.
    fn owner(&mut self, path: &str) -> ((u32, u32), u32) {
        let node = self.walk(path).unwrap();
        let (error, reply) = self.call(GETATTR, node, &[0; 16]);
        assert_eq!(error, 0, "{path}");
        let mode = u32::from_le_bytes(reply[16 + 60..16 + 64].try_into().unwrap());
        (owner_at(&reply, 16), mode & 0o7777)
    }
}

/// The share server's process, when the test below starts this program
/// again as another user, with [`ARGS`] set. It runs before the test
/// harness, so that it is the process's only thread, as it is in `ap`. Its
/// standard input is the socket.
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
    let args: Vec<OsString> = args.to_str().unwrap().lines().map(OsString::from).collect();
    assert!(alone(), "the server's process has threads it doesn't own");
    let status = share::main(&args);
    // SAFETY: `_exit` ends the process at once.
    unsafe { libc::_exit(i32::from(status)) };
}

/// Writes `data` to `path` without allocating, as a forked child must.
fn write_proc(path: &CString, data: &[u8]) -> io::Result<()> {
    // SAFETY: both pointers are valid for the lengths given, and the
    // descriptor is closed before returning.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, data.as_ptr().cast(), data.len());
        let error = io::Error::last_os_error();
        libc::close(fd);
        if written != data.len() as isize {
            return Err(error);
        }
    }
    Ok(())
}

// spec(SHARE): In a share, what your user owns on your host appears owned by the agent's user, and what your primary group owns appears owned by the agent's group.
// spec(SHARE): A share never changes who owns a file on your host.
#[test]
fn a_host_user_whose_uid_isn_t_1000_reads_and_writes_its_own_files() {
    let tree = Tree::new("owner-rw");
    let other = Tree::new("owner-ro");
    let root = tree.root.clone();
    fs::create_dir(root.join("open")).unwrap();
    fs::set_permissions(root.join("open"), fs::Permissions::from_mode(0o755)).unwrap();
    for dir in [&root, &other.root] {
        fs::write(dir.join("private.txt"), b"mine alone").unwrap();
        fs::set_permissions(dir.join("private.txt"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let host = fs::metadata(&root).unwrap();
    let host = (host.uid(), host.gid());
    // SAFETY: both calls take nothing, and can't fail.
    let me = unsafe { (libc::geteuid(), libc::getegid()) };
    // A tree in a setgid directory takes that directory's group, which
    // isn't the server's, so it appears as `nobody`'s.
    let agent = (AGENT.0, if host.1 == me.1 { AGENT.1 } else { NOBODY.1 });

    let (mut server, mut guest) = start_as_other("rw", &tree.root);

    // The server's own files appear as the agent's, with their modes.
    assert_eq!(guest.owner("").0, agent);
    assert_eq!(guest.owner("open"), (agent, 0o755));
    assert_eq!(guest.owner("private.txt"), (agent, 0o700));

    // So the agent reads a mode-0700 file and writes in a mode-0755
    // directory, and the server does both as this user.
    assert_eq!(guest.read_file("private.txt").unwrap(), b"mine alone");
    let open = guest.walk("open").unwrap();
    let (_, handle) = guest.create(open, b"new.txt", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"from the agent"), Ok(14));
    guest.release(handle);
    assert_eq!(guest.owner("open/new.txt").0, agent);
    guest.overwrite("private.txt", b"changed").unwrap();
    assert_eq!(fs::read(root.join("private.txt")).unwrap(), b"changed");
    drop(guest);
    assert!(server.wait().unwrap().success());

    // An `ro` share, from a server of its own, shows the same owner.
    let (mut server, mut guest) = start_as_other("ro", &other.root);
    assert_eq!(guest.owner("private.txt"), (agent, 0o700));
    drop(guest);
    assert!(server.wait().unwrap().success());

    // Nothing changed hands on the host.
    for path in ["private.txt", "open", "open/new.txt"] {
        let found = fs::metadata(root.join(path)).unwrap();
        assert_eq!((found.uid(), found.gid()), host, "{path}");
    }
    assert_eq!(
        fs::read(root.join("open/new.txt")).unwrap(),
        b"from the agent"
    );
}

/// Starts the share server's process for a `mode` share of `root`, as uid
/// and gid 1001 in a user namespace that maps them to this user, so it owns
/// the tree as 1001.
fn start_as_other(mode: &str, root: &Path) -> (Child, Guest) {
    // SAFETY: both calls take nothing, and can't fail.
    let me = unsafe { (libc::geteuid(), libc::getegid()) };
    let (mine, theirs) = UnixStream::pair().unwrap();
    let inherited = share::inheritable(File::open(root).unwrap().as_fd()).unwrap();
    let args = format!("{mode}\n{}", inherited.as_raw_fd());
    let setgroups = CString::new("/proc/self/setgroups").unwrap();
    let uid_map = CString::new("/proc/self/uid_map").unwrap();
    let gid_map = CString::new("/proc/self/gid_map").unwrap();
    let uid_line = format!("{OTHER} {} 1\n", me.0);
    let gid_line = format!("{OTHER} {} 1\n", me.1);
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .env(ARGS, args)
        .stdin(OwnedFd::from(theirs))
        .stdout(Stdio::null());
    // SAFETY: the closure runs in the forked child, and makes only system
    // calls, on memory made before the fork.
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                return Err(io::Error::last_os_error());
            }
            write_proc(&setgroups, b"deny")?;
            write_proc(&uid_map, uid_line.as_bytes())?;
            write_proc(&gid_map, gid_line.as_bytes())
        });
    }
    let server = command.spawn().unwrap();
    // The command holds the server's end of the socket.
    drop(command);
    drop(inherited);
    (server, Guest::over(OwnedFd::from(mine)))
}

// spec(SHARE): Any other owner appears as uid or gid 65534, `nobody`.
#[test]
fn a_file_someone_else_owns_appears_owned_by_nobody() {
    let root = Path::new("/");
    let found = fs::metadata(root.join("usr")).unwrap();
    // SAFETY: both calls take nothing, and can't fail.
    let me = unsafe { (libc::geteuid(), libc::getegid()) };
    let expected = (
        if found.uid() == me.0 {
            AGENT.0
        } else {
            NOBODY.0
        },
        if found.gid() == me.1 {
            AGENT.1
        } else {
            NOBODY.1
        },
    );
    let mut guest = Guest::new(root);
    assert_eq!(guest.owner("usr").0, expected);
}
