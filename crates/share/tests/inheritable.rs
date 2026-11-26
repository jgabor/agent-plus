//! A copy that [`share::inheritable`] makes reaches only the program it was
//! made for, and no other program that starts while the copy is open, as
//! one that another thread starts would.

#![cfg(target_os = "linux")]

use std::env;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::process::Command;

#[test]
fn a_copy_reaches_only_the_program_it_was_made_for() {
    let path = env::current_exe().unwrap();
    let file = File::open(&path).unwrap();
    let mut made_for = Command::new("sh");
    let copy = share::inheritable(&mut made_for, file.as_fd()).unwrap();
    // Each program checks its own descriptor once it runs, so its exec has
    // closed what closes on exec, and its loader has closed what it opened.
    let holds = format!("test /proc/self/fd/{} -ef \"$0\"", copy.as_raw_fd());
    let mut other = Command::new("sh");
    for command in [&mut made_for, &mut other] {
        command.env("LC_ALL", "C").arg("-c").arg(&holds).arg(&path);
    }

    let other = other.status().unwrap();
    let made_for = made_for.status().unwrap();
    drop(copy);

    assert_eq!((made_for.success(), other.success()), (true, false));
}
