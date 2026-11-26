//! A share server ends when the `ap run` that started it is killed, even
//! one that a guest took over and that ignores the end of its socket.

#![cfg(target_os = "linux")]

use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

use share::{Mode, Root};

/// Set for the process that stands in for `ap run`, which starts the
/// servers, and hidden from the servers it starts.
const PARENT: &str = "AP_SHARE_DEATH_PARENT";
/// Set for the servers, which stand in for ones a guest took over.
const SERVER: &str = "AP_SHARE_DEATH_SERVER";

/// The processes the test starts, when it starts this program again with
/// [`PARENT`] or [`SERVER`] set. They run before the test harness, so each
/// is its process's only thread, as `ap run` is when it starts the servers.
#[used]
#[unsafe(link_section = ".init_array")]
static STAND_IN: extern "C" fn() = stand_in;

extern "C" fn stand_in() {
    if env::var_os(PARENT).is_some() {
        // A server of `rw` shares, and one of the others.
        let directory = File::open(env::temp_dir()).unwrap();
        let roots = [
            Root::new(directory.as_fd(), Mode::ReadWrite),
            Root::new(directory.as_fd(), Mode::ReadOnly),
        ];
        let exe = env::current_exe().unwrap();
        let servers = share::start(&exe, &roots, &[PARENT.to_string()], None).unwrap();
        let pids: Vec<_> = (servers.started.iter())
            .map(|server| server.process.id().to_string())
            .collect();
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}", pids.join(" ")).unwrap();
        stdout.flush().unwrap();
    } else if env::var_os(SERVER).is_none() {
        return;
    }
    // A server that ignores the end of its socket, or `ap run` waiting on
    // its run, until it is killed.
    loop {
        // SAFETY: `pause` takes nothing, and changes no memory.
        unsafe { libc::pause() };
    }
}

/// A pidfd for `pid`, which needn't be a child of this process.
fn pidfd_open(pid: libc::pid_t) -> OwnedFd {
    // SAFETY: this call takes plain integers.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    // SAFETY: the descriptor is new, and nothing else owns it.
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

/// Whether the process of `pidfd` ends within `millis`.
fn ends_within(pidfd: &OwnedFd, millis: i32) -> bool {
    let mut watched = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `watched` is one valid `pollfd`, which outlives the call.
    unsafe { libc::poll(&mut watched, 1, millis) == 1 }
}

// spec(Run): **End.** A run that `ap run` started ends when that `ap run` exits, for any reason, including a closed terminal or a killed `ap run` process, and its VM stops with it.
#[test]
fn a_share_server_ends_when_the_ap_run_that_started_it_is_killed() {
    let mut parent = Command::new(env::current_exe().unwrap())
        .env(PARENT, "1")
        .env(SERVER, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(parent.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let servers: Vec<OwnedFd> = (line.split_whitespace())
        .map(|pid| pidfd_open(pid.parse().unwrap()))
        .collect();
    assert_eq!(servers.len(), 2, "{line:?}");
    parent.kill().unwrap();
    parent.wait().unwrap();
    let ended: Vec<bool> = servers
        .iter()
        .map(|pidfd| ends_within(pidfd, 10_000))
        .collect();
    for pidfd in &servers {
        // SAFETY: this call takes plain integers and a null `siginfo_t`.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
    }
    assert_eq!(ended, [true, true], "the rw server, then the other");
}
