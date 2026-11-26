//! A static `/init` for T1: it logs what the kernel offers runs and harnesses,
//! and powers off.

use guest::linux::Linux;
use guest::{MOUNTS, System};
use std::ffi::{c_int, c_ulong};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

const FLUSH: Duration = Duration::from_secs(1);

/// `nobody`, which can make the namespaces only if the kernel lets anyone.
const NOBODY: u32 = 65534;
const CLONE_NEWUSER: c_int = 0x1000_0000;
const NAMESPACES: [(&str, c_int); 4] = [
    ("user_ns", 0),
    ("pid_ns", 0x2000_0000),
    ("ipc_ns", 0x0800_0000),
    ("net_ns", 0x4000_0000),
];
const PR_SET_SECCOMP: c_int = 22;
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const SECCOMP_MODE_FILTER: c_ulong = 2;
/// `BPF_RET | BPF_K` returning `SECCOMP_RET_ALLOW`: a filter that allows
/// every system call.
const ALLOW: SockFilter = SockFilter {
    code: 0x06,
    jt: 0,
    jf: 0,
    k: 0x7fff_0000,
};

/// `struct sock_filter`.
#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

unsafe extern "C" {
    fn fork() -> c_int;
    fn _exit(status: c_int) -> !;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn setgid(gid: u32) -> c_int;
    fn setuid(uid: u32) -> c_int;
    fn unshare(flags: c_int) -> c_int;
    fn prctl(option: c_int, ...) -> c_int;
}

fn main() {
    let mut system = Linux;
    for mount in &MOUNTS {
        system.mount(mount).expect("the init should mount");
    }
    // One record, so that the report reaches the serial port whole or not
    // at all.
    let mut report = String::from("features:");
    let filesystems = fs::read_to_string("/proc/filesystems").unwrap_or_default();
    for line in filesystems.lines() {
        if let Some(name) = line.split_whitespace().last() {
            report.push_str(&format!(" {name}"));
        }
    }
    if Path::new("/sys/bus/virtio/drivers/virtio_blk").is_dir() {
        report.push_str(" virtio_blk");
    }
    // Tokio's signal driver, which Codex starts with, makes this pair.
    if UnixStream::pair().is_ok() {
        report.push_str(" unix");
    }
    // Codex's sandbox, unprivileged as the agent is: bubblewrap puts a
    // command in new user, PID, IPC, and network namespaces, and Codex
    // filters its system calls with seccomp.
    for (name, flag) in NAMESPACES {
        // SAFETY: `unshare` takes only flags, and changes only the child that
        // `as_nobody` runs it in.
        if as_nobody(|| unsafe { unshare(CLONE_NEWUSER | flag) } == 0) {
            report.push_str(&format!(" {name}"));
        }
    }
    let filter = SockFprog {
        len: 1,
        filter: &ALLOW,
    };
    // `prctl` reads every argument after the option as an `unsigned long`.
    let (on, unused): (c_ulong, c_ulong) = (1, 0);
    // SAFETY: each argument has the type `prctl` reads, and `filter` points
    // to a `struct sock_fprog` whose one filter is static, and which outlives
    // the call.
    let filters = || unsafe {
        prctl(PR_SET_NO_NEW_PRIVS, on, unused, unused, unused) == 0
            && prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &raw const filter) == 0
    };
    if as_nobody(filters) {
        report.push_str(" seccomp_filter");
    }
    report.push_str(" end");
    OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .and_then(|mut log| log.write_all(report.as_bytes()))
        .expect("the init should log");
    // The kernel's log is polled out to the serial port by a worker of its
    // own, which power off can cut short.
    thread::sleep(FLUSH);
    system.power_off().expect("the init should power off");
}

/// Whether `probe` succeeds in a child that runs as `nobody`, so that what it
/// changes leaves the init as it was. The init has one thread, so the child
/// that `fork` makes is in a consistent state.
fn as_nobody(probe: impl Fn() -> bool) -> bool {
    // SAFETY: the child of a process with one thread can make any call, and
    // it leaves only through `_exit`. `status` outlives the `waitpid` that
    // writes it.
    unsafe {
        let pid = fork();
        if pid == 0 {
            let dropped = setgid(NOBODY) == 0 && setuid(NOBODY) == 0;
            _exit(if dropped && probe() { 0 } else { 1 });
        }
        let mut status = 0;
        pid > 0 && waitpid(pid, &mut status, 0) == pid && status == 0
    }
}
