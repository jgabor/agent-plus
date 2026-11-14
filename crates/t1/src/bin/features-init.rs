//! A static `/init` for T1: it logs what the kernel offers runs and harnesses,
//! and powers off.

use guest::linux::Linux;
use guest::{MOUNTS, System};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

const FLUSH: Duration = Duration::from_secs(1);

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
