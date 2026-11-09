//! A static `/init` for T1: it connects to the host over vsock, sends back
//! every byte until the host closes, and powers off.

use guest::linux::Linux;
use guest::{MOUNTS, System};
use std::ffi::c_int;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::FromRawFd;

const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const HOST_CID: u32 = 2;
const PORT: u32 = 1024;

/// `struct sockaddr_vm`.
#[repr(C)]
struct Address {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    flags: u8,
    zero: [u8; 3],
}

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const Address, len: u32) -> c_int;
}

fn connect_to_host() -> io::Result<File> {
    // SAFETY: this call takes no pointers.
    let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a new descriptor that nothing else owns.
    let stream = unsafe { File::from_raw_fd(fd) };
    let address = Address {
        family: AF_VSOCK as u16,
        reserved: 0,
        port: PORT,
        cid: HOST_CID,
        flags: 0,
        zero: [0; 3],
    };
    // SAFETY: `address` is a `sockaddr_vm` of the size given, and lives
    // through the call.
    if unsafe { connect(fd, &raw const address, size_of::<Address>() as u32) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

fn echo() -> io::Result<u64> {
    let mut stream = connect_to_host()?;
    let mut buffer = [0; 8192];
    let mut total = 0;
    loop {
        let len = stream.read(&mut buffer)?;
        if len == 0 {
            return Ok(total);
        }
        stream.write_all(&buffer[..len])?;
        total += len as u64;
    }
}

fn main() {
    let mut system = Linux;
    for mount in &MOUNTS {
        system.mount(mount).expect("the init should mount");
    }
    let result = echo();
    OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .and_then(|mut log| log.write_all(format!("vsock-echo: {result:?}\n").as_bytes()))
        .expect("the init should log");
    system.power_off().expect("the init should power off");
}
