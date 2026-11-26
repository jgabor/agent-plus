//! A copy that [`share::inheritable`] makes reaches only the program it was
//! made for, and no other program that starts while the copy is open, as
//! one that another thread starts would.

#![cfg(target_os = "linux")]

use std::env;
use std::fs::{self, File};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command};

/// A program that holds still until the test kills it.
fn waiting() -> Command {
    let mut command = Command::new("sleep");
    command.arg("60");
    command
}

/// What `process`'s descriptor `fd` is open to, if it is open.
fn opened(process: &Child, fd: RawFd) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{}/fd/{fd}", process.id())).ok()
}

#[test]
fn a_copy_reaches_only_the_program_it_was_made_for() {
    let path = env::current_exe().unwrap();
    let file = File::open(&path).unwrap();
    let mut command = waiting();
    let copy = share::inheritable(&mut command, file.as_fd()).unwrap();
    let number = copy.as_raw_fd();

    // `spawn` returns once each program has replaced the process it forked.
    let mut other = waiting().spawn().unwrap();
    let mut made_for = command.spawn().unwrap();
    drop(copy);
    let held = (opened(&made_for, number), opened(&other, number));
    for process in [&mut other, &mut made_for] {
        process.kill().unwrap();
        process.wait().unwrap();
    }

    assert_eq!(held, (Some(path), None));
}
