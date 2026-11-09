//! A static `/init` for T1: it proves it ran, and powers off.

use guest::linux::Linux;
use guest::{MOUNTS, System};
use std::fs::OpenOptions;
use std::io::Write;

fn main() {
    let mut system = Linux;
    for mount in &MOUNTS {
        system.mount(mount).expect("the init should mount");
    }
    // The kernel's log is polled out to the serial port, which has no
    // interrupts for a write to its own device to wait on.
    OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .and_then(|mut log| log.write_all(b"test-init reached\n"))
        .expect("the init should log");
    system.power_off().expect("the init should power off");
}
