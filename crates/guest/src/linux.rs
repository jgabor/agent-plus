//! The real system calls.

use crate::{Mount, System};
use std::ffi::{CStr, OsStr, c_char, c_int, c_ulong, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::{fs, io, ptr};

const WNOHANG: c_int = 1;
const ECHILD: i32 = 10;
const RB_POWER_OFF: c_int = 0x4321_FEDC;

unsafe extern "C" {
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
            reboot(RB_POWER_OFF);
        }
        Err(io::Error::last_os_error())
    }
}

fn path(target: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(target.to_bytes()))
}
