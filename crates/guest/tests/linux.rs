//! The real system calls, from a process without privileges, so that each one
//! fails where the guest's would succeed.
//!
//! No test here starts a child, because `reap_until` collects any child of the
//! process.

use guest::linux::{Linux, bytes_waiting, program_has_ended, reap_until};
use guest::{Mount, System};
use std::ffi::{CString, c_int};
use std::fs::{self, File};
use std::io::{Write, pipe};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::thread::{JoinHandleExt, RawPthread};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const EPERM: i32 = 1;
const ECHILD: i32 = 10;
const EACCES: i32 = 13;
const SIGUSR1: c_int = 10;

unsafe extern "C" {
    fn signal(signal: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn pthread_kill(thread: RawPthread, signal: c_int) -> c_int;
}

/// Panics unless the process has no capabilities, because these tests call
/// `mount` and `reboot`, and count on both being refused.
fn unprivileged() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .unwrap();
    let capabilities = u64::from_str_radix(effective.trim(), 16).unwrap();
    assert_eq!(capabilities, 0, "these tests must run without privileges");
}

/// An empty directory of its own for each test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("linux")
        .join(name);
    if dir.exists() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn tmpfs_at(target: &Path) -> Mount {
    let target = CString::new(target.as_os_str().as_bytes()).unwrap();
    Mount {
        source: c"tmpfs",
        target: Box::leak(target.into_boxed_c_str()),
        fstype: c"tmpfs",
        flags: 0,
        data: c"",
    }
}

#[test]
fn a_refused_mount_is_an_error_after_its_mount_point_is_made() {
    unprivileged();
    let target = scratch("refused").join("mnt");

    let error = Linux.mount(&tmpfs_at(&target)).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(EPERM));
    assert!(target.is_dir());
}

#[test]
fn an_existing_mount_point_is_used() {
    unprivileged();
    let target = scratch("existing").join("mnt");
    fs::create_dir(&target).unwrap();

    let error = Linux.mount(&tmpfs_at(&target)).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(EPERM));
}

#[test]
fn a_mount_point_that_cannot_be_made_is_an_error() {
    unprivileged();
    let parent = scratch("unwritable");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();

    let error = Linux.mount(&tmpfs_at(&parent.join("mnt"))).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(EACCES));
}

#[test]
fn a_refused_power_off_is_an_error() {
    unprivileged();
    let error = Linux.power_off().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(EPERM));
}

#[test]
fn waiting_without_children_is_an_error() {
    let error = reap_until(u32::MAX).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ECHILD));
}

#[test]
fn the_bytes_in_a_pipe_are_counted() {
    let (reader, mut writer) = pipe().unwrap();
    writer.write_all(b"12345").unwrap();
    assert_eq!(bytes_waiting(reader.as_fd()).unwrap(), 5);
}

#[test]
fn counting_what_is_not_a_pipe_is_an_error() {
    let null = File::open("/dev/null").unwrap();
    assert!(bytes_waiting(null.as_fd()).is_err());
}

extern "C" fn ignore(_signal: c_int) {}

/// A signal interrupts `poll`, and the wait must go on through it.
#[test]
fn a_signal_does_not_end_the_wait_for_a_program() {
    // SAFETY: the handler does nothing, so it is safe in any context.
    unsafe { signal(SIGUSR1, ignore) };
    let (output, _output_writer) = pipe().unwrap();
    let (ended, ended_writer) = pipe().unwrap();
    let waiter = thread::spawn(move || program_has_ended(output.as_fd(), ended.as_fd()));

    for _ in 0..10 {
        thread::sleep(Duration::from_millis(20));
        // SAFETY: the thread can't be joined before this loop ends.
        assert_eq!(unsafe { pthread_kill(waiter.as_pthread_t(), SIGUSR1) }, 0);
    }
    drop(ended_writer);

    assert!(waiter.join().unwrap().unwrap());
}
