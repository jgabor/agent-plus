//! The real system calls.

use crate::{Mount, System};
use std::ffi::{CStr, OsStr, c_char, c_int, c_short, c_ulong, c_void};
use std::fs::File;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::{fs, io, ptr};

const WNOHANG: c_int = 1;
const ECHILD: i32 = 10;
/// A restart, which ends the VM. `RB_POWER_OFF` would only halt the CPU,
/// because the kernel has no ACPI, and the host would never find out.
const RB_AUTOBOOT: c_int = 0x0123_4567;
const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const HOST_CID: u32 = 2;
const POLLIN: c_short = 1;
const FIONREAD: c_ulong = 0x541B;
const FOREVER: c_int = -1;

/// `struct sockaddr_vm`.
#[repr(C)]
struct VsockAddress {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    flags: u8,
    zero: [u8; 3],
}

/// `struct pollfd`.
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn sync();
    fn reboot(command: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const VsockAddress, len: u32) -> c_int;
}

/// The running Linux kernel.
pub struct Linux;

impl System for Linux {
    fn mount(&mut self, wanted: &Mount) -> io::Result<()> {
        // The initramfs holds only the init, so mount points may not exist.
        match fs::create_dir(path(wanted.target)) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        // SAFETY: every pointer comes from a `CStr` that outlives the call.
        let result = unsafe {
            mount(
                wanted.source.as_ptr(),
                wanted.target.as_ptr(),
                wanted.fstype.as_ptr(),
                wanted.flags,
                wanted.data.as_ptr().cast(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn reap(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: a null status pointer asks the kernel not to write one.
        match unsafe { waitpid(-1, ptr::null_mut(), WNOHANG) } {
            0 => Ok(None),
            -1 => {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ECHILD) {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
            pid => Ok(Some(pid)),
        }
    }

    fn power_off(&mut self) -> io::Result<()> {
        // SAFETY: neither call takes a pointer.
        unsafe {
            sync();
            reboot(RB_AUTOBOOT);
        }
        Err(io::Error::last_os_error())
    }
}

/// Waits until `pipe` can be read without blocking, which includes having
/// closed, or until `ended` closes. Returns whether `ended` has closed.
pub fn program_has_ended(pipe: BorrowedFd, ended: BorrowedFd) -> io::Result<bool> {
    let mut fds = [waiting_on(pipe), waiting_on(ended)];
    poll_retrying(&mut fds)?;
    Ok(fds[1].revents != 0)
}

/// How many bytes `pipe` holds, which can be more than its nominal size.
pub fn bytes_waiting(pipe: BorrowedFd) -> io::Result<usize> {
    let mut count: c_int = 0;
    // SAFETY: `FIONREAD` writes one `int` through this pointer, which is valid
    // for the whole call.
    if unsafe { ioctl(pipe.as_raw_fd(), FIONREAD, &mut count as *mut c_int) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count as usize)
}

fn waiting_on(fd: BorrowedFd) -> PollFd {
    PollFd {
        fd: fd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    }
}

fn poll_retrying(fds: &mut [PollFd]) -> io::Result<()> {
    loop {
        // SAFETY: the pointer and count describe `fds`, which outlives the call.
        if unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, FOREVER) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Connects to `port` on the host over vsock.
pub fn connect_to_host(port: u32) -> io::Result<File> {
    // SAFETY: this call takes no pointers.
    let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a new descriptor that nothing else owns.
    let stream = unsafe { File::from_raw_fd(fd) };
    let address = VsockAddress {
        family: AF_VSOCK as u16,
        reserved: 0,
        port,
        cid: HOST_CID,
        flags: 0,
        zero: [0; 3],
    };
    let length = size_of::<VsockAddress>() as u32;
    // SAFETY: `address` outlives the call, and `length` is its size.
    if unsafe { connect(fd, &address, length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

fn path(target: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(target.to_bytes()))
}
