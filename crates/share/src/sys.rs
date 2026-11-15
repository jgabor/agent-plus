//! The system calls the share server makes on files, each one exactly the
//! call its filter allows. None takes a path that can leave a descriptor.

use std::ffi::{CStr, c_long};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Resolution that stays beneath the starting descriptor and follows no
/// symlink, from `<linux/openat2.h>`.
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const BENEATH: u64 = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;

/// `struct open_how`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn check(result: c_long) -> io::Result<c_long> {
    match result {
        -1 => Err(io::Error::last_os_error()),
        value => Ok(value),
    }
}

/// Opens `name` beneath `directory`, following no symlink on the way. With
/// `O_PATH | O_NOFOLLOW`, a symlink opens as itself.
pub fn open_beneath(directory: BorrowedFd, name: &CStr, flags: i32) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: BENEATH,
    };
    // SAFETY: `name` is NUL-terminated and `how` is a `struct open_how`,
    // both outliving the call, which returns a new descriptor or -1.
    let fd = check(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            name.as_ptr(),
            &raw const how,
            size_of::<OpenHow>(),
        )
    })?;
    // SAFETY: the descriptor is new, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

pub fn fstat(fd: BorrowedFd) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the kernel fills `stat`, which is its `struct stat` on x86_64.
    check(unsafe { libc::syscall(libc::SYS_fstat, fd.as_raw_fd(), stat.as_mut_ptr()) })?;
    // SAFETY: the call succeeded, so it wrote the whole struct.
    Ok(unsafe { stat.assume_init() })
}

pub fn fstatfs(fd: BorrowedFd) -> io::Result<libc::statfs> {
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the kernel fills `stat`, which is its `struct statfs`.
    check(unsafe { libc::syscall(libc::SYS_fstatfs, fd.as_raw_fd(), stat.as_mut_ptr()) })?;
    // SAFETY: the call succeeded, so it wrote the whole struct.
    Ok(unsafe { stat.assume_init() })
}

/// The target of the symlink that `fd` is, opened with `O_PATH`.
pub fn readlink(fd: BorrowedFd) -> io::Result<Vec<u8>> {
    let mut target = vec![0; libc::PATH_MAX as usize];
    // SAFETY: the empty name is NUL-terminated, and `target` has room for
    // the length passed.
    let len = check(unsafe {
        libc::syscall(
            libc::SYS_readlinkat,
            fd.as_raw_fd(),
            c"".as_ptr(),
            target.as_mut_ptr(),
            target.len(),
        )
    })?;
    target.truncate(len as usize);
    Ok(target)
}

pub fn pread(fd: BorrowedFd, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    let offset = i64::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `buffer` has room for the length passed.
    let len = unsafe {
        libc::pread(
            fd.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            offset,
        )
    };
    check(len as c_long).map(|len| len as usize)
}

/// Reads directory entries from `offset` on, as `getdents64` gives them.
pub fn read_directory(fd: BorrowedFd, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    let offset = i64::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: lseek takes no pointer.
    check(unsafe { libc::lseek(fd.as_raw_fd(), offset, libc::SEEK_SET) } as c_long)?;
    // SAFETY: `buffer` has room for the length passed.
    let len = check(unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd.as_raw_fd(),
            buffer.as_mut_ptr(),
            buffer.len(),
        )
    })?;
    Ok(len as usize)
}
