//! A static program for T1, kept in an agent's top layer: it prints what it
//! sees of the guest's root, one `name=value` line each, and leaves a file
//! behind for the next boot to look for. It also does what a harness does to
//! the root: it replaces a directory from a lower layer, runs a `#!` script,
//! and opens an inotify instance, as a file watcher does.

use std::env;
use std::ffi::c_int;
use std::fs::{self, File};
use std::os::fd::FromRawFd;
use std::process::Command;

const LEFT_BEHIND: &str = "/left-behind";
const REPLACED: &str = "/srv/replaced";
const SCRIPT: &str = "/bin/script";

unsafe extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("unreadable: {error}"))
}

/// `rm -r dir && mkdir dir`, on a directory from a lower layer.
fn remake() -> String {
    match fs::remove_dir_all(REPLACED).and_then(|()| fs::create_dir(REPLACED)) {
        Ok(()) => "true".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// Runs the script, whose `#!` line runs this program again.
fn script() -> String {
    match Command::new(SCRIPT).output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).into(),
        Ok(output) => format!("failed: {}", output.status),
        Err(error) => error.to_string(),
    }
}

fn inotify() -> String {
    // SAFETY: inotify_init1 takes no pointers.
    let fd = unsafe { inotify_init1(0) };
    if fd < 0 {
        return std::io::Error::last_os_error().to_string();
    }
    // SAFETY: the kernel just returned this descriptor, and nothing else owns
    // it.
    drop(unsafe { File::from_raw_fd(fd) });
    "true".to_owned()
}

fn main() {
    // The kernel runs a script's interpreter with the script's path after it.
    if env::args().nth(1).as_deref() == Some(SCRIPT) {
        print!("ran");
        return;
    }
    let root = read("/proc/mounts")
        .lines()
        .find(|line| line.split(' ').nth(1) == Some("/"))
        .unwrap_or_default()
        .to_owned();
    let exe = fs::read_link("/proc/self/exe")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unreadable: {error}"));
    let found = fs::exists(LEFT_BEHIND).unwrap_or(false);
    let wrote = fs::write(LEFT_BEHIND, "from an earlier boot").is_ok();
    println!("greeting={}", read("/etc/greeting"));
    println!("kept={}", read("/etc/kept"));
    println!("root={root}");
    println!("exe={exe}");
    println!("found={found}");
    println!("wrote={wrote}");
    println!("remade={}", remake());
    println!("script={}", script());
    println!("inotify={}", inotify());
}
