//! A static `/init` for `floor_guest_host`. It runs as root in a real guest,
//! and does what the files beside it say, so that every attack is written
//! down in the test.
//!
//! - `/before` holds raw bytes to send as soon as the channel is up.
//! - `/await-run`, if present, waits for the host's request after that.
//! - `/after` holds raw bytes to send next.
//! - `/scan`, if present, sends everything the guest can see of its world, so
//!   that the host can search it. The guest never learns what is searched for.
//! - `/stop-early`, if present, powers off without waiting for the host, for
//!   a frame the host is still waiting to see the end of.

use guest::linux::{Linux, connect_to_host};
use guest::protocol::{MAX_OUTPUT, PORT, ToGuest, ToHost};
use guest::{Channel, Event};
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

const O_NONBLOCK: i32 = 0o4000;
const FLUSH: Duration = Duration::from_secs(1);
/// What the walk of the root skips, because `/proc` is read on purpose and
/// the rest is the kernel's own.
const NOT_WALKED: [&str; 3] = ["/proc", "/sys", "/dev"];

struct Probe;

impl Channel for Probe {
    fn next_event(&mut self) -> io::Result<Event> {
        let mut host = connect_to_host(PORT)?;
        // Any write can fail once the host has refused what came before. That
        // is a result the host reports, so the probe carries on to the end.
        let _ = host.write_all(&fs::read("/before").unwrap_or_default());
        if Path::new("/await-run").exists() {
            let _ = ToGuest::read(&mut host);
        }
        let _ = host.write_all(&fs::read("/after").unwrap_or_default());
        if Path::new("/scan").exists() {
            let _ = send_scan(&mut host);
        }
        if Path::new("/stop-early").exists() {
            // The kernel sends what was written from a worker of its own.
            thread::sleep(FLUSH);
        } else {
            drain(&mut host);
        }
        Ok(Event::PowerOff)
    }
}

/// Waits for the host to hang up, so that nothing sent is lost to a power-off.
fn drain(host: &mut File) {
    let mut buffer = [0; 4096];
    while matches!(host.read(&mut buffer), Ok(len) if len > 0) {}
}

fn send_scan(host: &mut File) -> io::Result<()> {
    let failed = |error| io::Error::other(format!("{error:?}"));
    for chunk in scan().chunks(MAX_OUTPUT) {
        ToHost::Stdout(chunk.to_vec()).write(host).map_err(failed)?;
    }
    ToHost::Exited(0).write(host).map_err(failed)
}

/// The environment, command lines, mounts, descriptors, paths, and kernel log.
fn scan() -> Vec<u8> {
    let mut seen = Vec::new();
    for (name, value) in env::vars_os() {
        seen.extend_from_slice(name.as_encoded_bytes());
        seen.push(b'=');
        seen.extend_from_slice(value.as_encoded_bytes());
        seen.push(b'\n');
    }
    if let Ok(directory) = env::current_dir() {
        seen.extend_from_slice(format!("cwd {}\n", directory.display()).as_bytes());
    }
    for file in ["/proc/cmdline", "/proc/mounts", "/proc/self/mountinfo"] {
        append_file(&mut seen, file);
    }
    for entry in entries("/proc") {
        let is_process = entry
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()));
        if is_process {
            append_file(&mut seen, &entry.join("cmdline").to_string_lossy());
            append_file(&mut seen, &entry.join("environ").to_string_lossy());
        }
    }
    let mut links = String::new();
    for link in entries("/proc/self/fd")
        .into_iter()
        .chain(["cwd", "exe", "root"].map(|name| Path::new("/proc/self").join(name)))
    {
        if let Ok(target) = fs::read_link(&link) {
            let _ = writeln!(links, "{} -> {}", link.display(), target.display());
        }
    }
    seen.extend_from_slice(links.as_bytes());
    walk(Path::new("/"), &mut seen);
    append_kernel_log(&mut seen);
    seen
}

fn entries(directory: &str) -> Vec<std::path::PathBuf> {
    fs::read_dir(directory)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

fn append_file(seen: &mut Vec<u8>, path: &str) {
    seen.extend_from_slice(format!("--- {path}\n").as_bytes());
    if let Ok(contents) = fs::read(path) {
        seen.extend_from_slice(&contents);
    }
    seen.push(b'\n');
}

/// Every name in the root filesystem.
fn walk(directory: &Path, seen: &mut Vec<u8>) {
    for path in entries(&directory.to_string_lossy()) {
        seen.extend_from_slice(format!("{}\n", path.display()).as_bytes());
        let walked = !NOT_WALKED.iter().any(|skip| path == Path::new(skip));
        if walked && path.is_dir() && !path.is_symlink() {
            walk(&path, seen);
        }
    }
}

/// Reading `/dev/kmsg` without blocking stops at the end of the log.
fn append_kernel_log(seen: &mut Vec<u8>) {
    let Ok(mut log) = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return;
    };
    let mut record = [0; 8192];
    while let Ok(len) = log.read(&mut record) {
        if len == 0 {
            break;
        }
        seen.extend_from_slice(&record[..len]);
    }
}

fn main() {
    let _ = guest::run(&mut Linux, &mut Probe);
}
