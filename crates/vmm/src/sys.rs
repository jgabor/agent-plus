//! The C library calls the VMM needs, declared by hand.

use std::ffi::{c_int, c_ulong, c_void};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const MAP_PRIVATE: c_int = 2;
const MAP_ANONYMOUS: c_int = 0x20;

/// The value of `SIG_ERR`.
pub const SIGNAL_FAILED: usize = usize::MAX;

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    pub fn pthread_self() -> usize;
    pub fn pthread_kill(thread: usize, signum: c_int) -> c_int;
}

/// A readable and writable memory mapping, unmapped on drop.
pub struct Mapping {
    start: *mut u8,
    len: usize,
}

impl Mapping {
    /// Maps `len` bytes of zeroed private memory.
    pub fn anonymous(len: usize) -> io::Result<Self> {
        Self::map(len, MAP_PRIVATE | MAP_ANONYMOUS, -1)
    }

    /// Maps the first `len` bytes of `fd`, shared with the kernel.
    pub fn shared(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::map(len, MAP_SHARED, fd.as_raw_fd())
    }

    fn map(len: usize, flags: c_int, fd: c_int) -> io::Result<Self> {
        // SAFETY: a null hint lets the kernel pick an unused range, so this
        // can't replace an existing mapping.
        let start = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, flags, fd, 0) };
        // MAP_FAILED is -1.
        if start as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            start: start.cast(),
            len,
        })
    }

    pub fn start(&self) -> *mut u8 {
        self.start
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `start` and `len` are the range `mmap` returned, and this
        // value owns it. Unmapping a valid range can't fail.
        unsafe { munmap(self.start.cast(), self.len) };
    }
}
