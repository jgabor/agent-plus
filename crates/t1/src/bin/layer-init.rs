//! A static `/init` for T1: it mounts the first virtio-blk disk as EROFS,
//! logs what it finds there, and powers off.

use guest::linux::Linux;
use guest::{MOUNTS, Mount, System};
use std::ffi::c_ulong;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

const MS_RDONLY: c_ulong = 1;
const FLUSH: Duration = Duration::from_secs(1);
const LAYER: Mount = Mount {
    source: c"/dev/vda",
    target: c"/layer",
    fstype: c"erofs",
    flags: MS_RDONLY,
    data: c"",
};

/// Every fact on one line, split by `|`, so that the host can tell a missing
/// one.
fn report(system: &mut Linux) -> io::Result<String> {
    system.mount(&LAYER)?;
    let mount = fs::read_to_string("/proc/mounts")?
        .lines()
        .find(|line| line.contains(" /layer "))
        .unwrap_or_default()
        .to_owned();
    let read_only = fs::read_to_string("/sys/block/vda/ro")?;
    let serial = fs::read_to_string("/sys/block/vda/serial")?;
    let mut entries = fs::read_dir("/layer")?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    let file = fs::read("/layer/dir/greeting")?;
    Ok([
        mount,
        read_only.trim().to_owned(),
        serial.trim().to_owned(),
        entries.join(","),
        String::from_utf8_lossy(&file).into_owned(),
    ]
    .join("|"))
}

fn main() {
    let mut system = Linux;
    for mount in &MOUNTS {
        system.mount(mount).expect("the init should mount");
    }
    let line = match report(&mut system) {
        Ok(report) => format!("layer: ok|{report}\n"),
        Err(error) => format!("layer: error {error}\n"),
    };
    OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .and_then(|mut log| log.write_all(line.as_bytes()))
        .expect("the init should log");
    // The kernel's log is polled out to the serial port by a worker of its
    // own, which power off can cut short.
    thread::sleep(FLUSH);
    system.power_off().expect("the init should power off");
}
