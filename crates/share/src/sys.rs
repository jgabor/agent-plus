//! The system calls the share server makes on files, each one exactly the
//! call its filter allows. None takes a path that can leave a descriptor,
//! but [`parent`], which goes one directory up and reads nothing.

use std::ffi::{CStr, CString, c_long};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

fn check(result: c_long) -> io::Result<c_long> {
    match result {
        -1 => Err(io::Error::last_os_error()),
        value => Ok(value),
    }
}

/// Opens `name` beneath `directory`, following no symlink on the way. With
/// `O_PATH`, a symlink at the end opens as itself.
pub fn open_beneath(directory: BorrowedFd, name: &CStr, flags: i32) -> io::Result<OwnedFd> {
    let (above, last) = walk(directory, name)?;
    let from = above.as_ref().map_or(directory, AsFd::as_fd);
    openat(from, last.as_c_str(), flags, 0)
}

/// Opens `name` beneath `directory` as [`open_beneath`] does, and with
/// `O_CREAT`, makes it with `mode` if it is missing.
pub fn create_beneath(
    directory: BorrowedFd,
    name: &CStr,
    flags: i32,
    mode: u32,
) -> io::Result<OwnedFd> {
    // A name that ends in a slash can't be made a file, once the way to it
    // opens.
    let bytes = name.to_bytes();
    if let Some(at) = bytes.iter().rposition(|&byte| byte != b'/')
        && at + 1 < bytes.len()
    {
        let way =
            CString::new(&bytes[..=at]).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        walk(directory, &way)?;
        return Err(io::Error::from_raw_os_error(libc::EISDIR));
    }
    let (above, last) = walk(directory, name)?;
    let from = above.as_ref().map_or(directory, AsFd::as_fd);
    openat(from, last.as_c_str(), flags | libc::O_CREAT, mode)
}

/// The directory above `directory`, opened with `O_PATH`, so it reads
/// nothing. Above the top of the tree is the top itself.
pub fn parent(directory: BorrowedFd) -> io::Result<OwnedFd> {
    openat(directory, c"..", libc::O_PATH | libc::O_DIRECTORY, 0)
}

/// Opens each directory on the way to `name`'s last component, one at a
/// time, so no component is a symlink, `..`, or a path from the top. Returns
/// the directory the last component is in, unless it is `directory` itself,
/// and that component. It allocates nothing, so the times process can walk
/// too.
fn walk(directory: BorrowedFd, name: &CStr) -> io::Result<(Option<OwnedFd>, Component)> {
    let name = name.to_bytes();
    if name.starts_with(b"/") {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    // A name that ends in a slash names a directory, as `.` in it does.
    let trailing = name.ends_with(b"/").then_some(&b"."[..]);
    let mut components = (name.split(|&byte| byte == b'/'))
        .filter(|c| !c.is_empty())
        .chain(trailing);
    let mut last = components.next().unwrap_or_default();
    let mut above: Option<OwnedFd> = None;
    for next in components {
        let from = above.as_ref().map_or(directory, AsFd::as_fd);
        let step = component(last)?;
        let flags = libc::O_PATH | libc::O_DIRECTORY;
        above = Some(openat(from, step.as_c_str(), flags, 0)?);
        last = next;
    }
    Ok((above, component(last)?))
}

/// One component of a name, NUL-terminated, and no longer than the kernel
/// takes.
struct Component([u8; libc::NAME_MAX as usize + 1]);

impl Component {
    fn as_c_str(&self) -> &CStr {
        // It holds a NUL, as `component` made it.
        CStr::from_bytes_until_nul(&self.0).unwrap_or_default()
    }
}

fn component(name: &[u8]) -> io::Result<Component> {
    let error = match name {
        b".." => libc::EXDEV,
        // It came from a `CStr`, so it holds no NUL.
        name if name.contains(&0) => libc::EINVAL,
        name if name.len() > libc::NAME_MAX as usize => libc::ENAMETOOLONG,
        name => {
            let mut component = Component([0; libc::NAME_MAX as usize + 1]);
            component.0[..name.len()].copy_from_slice(name);
            return Ok(component);
        }
    };
    Err(io::Error::from_raw_os_error(error))
}

/// Opens the one entry `name` in `directory`, never following it if it is a
/// symlink. The filter reads these flags, and refuses access mode 3, which
/// asks for neither reading nor writing, so Landlock would check nothing.
/// A symlink where a directory is asked for fails with `ELOOP`.
fn openat(directory: BorrowedFd, name: &CStr, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated and outlives the call, which returns
    // a new descriptor or -1.
    let fd = check(unsafe {
        libc::syscall(
            libc::SYS_openat,
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode,
        )
    });
    match fd {
        // SAFETY: the descriptor is new, and nothing else owns it.
        Ok(fd) => Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) }),
        Err(error)
            if error.raw_os_error() == Some(libc::ENOTDIR)
                && flags & libc::O_DIRECTORY != 0
                && is_symlink(directory, name) =>
        {
            Err(io::Error::from_raw_os_error(libc::ELOOP))
        }
        Err(error) => Err(error),
    }
}

fn is_symlink(directory: BorrowedFd, name: &CStr) -> bool {
    let found = openat(directory, name, libc::O_PATH, 0);
    found
        .and_then(|fd| fstat(fd.as_fd()))
        .is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK)
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

/// The target of the symlink `name` in `directory`, or with an empty
/// name, of the symlink that `directory` is, opened with `O_PATH`.
pub fn readlink(directory: BorrowedFd, name: &CStr) -> io::Result<Vec<u8>> {
    let mut target = vec![0; libc::PATH_MAX as usize];
    // SAFETY: the name is NUL-terminated, and `target` has room for the
    // length passed.
    let len = check(unsafe {
        libc::syscall(
            libc::SYS_readlinkat,
            directory.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr(),
            target.len(),
        )
    })? as usize;
    // A target as long as the buffer may have been cut short.
    if len == target.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    target.truncate(len);
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

pub fn pwrite(fd: BorrowedFd, data: &[u8], offset: u64) -> io::Result<usize> {
    let offset = i64::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `data` holds the length passed.
    let len = check(unsafe {
        libc::syscall(
            libc::SYS_pwrite64,
            fd.as_raw_fd(),
            data.as_ptr(),
            data.len(),
            offset,
        )
    })?;
    Ok(len as usize)
}

pub fn ftruncate(fd: BorrowedFd, size: u64) -> io::Result<()> {
    let size = i64::try_from(size).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: this call takes no pointer.
    check(unsafe { libc::syscall(libc::SYS_ftruncate, fd.as_raw_fd(), size) }).map(drop)
}

pub fn fchmod(fd: BorrowedFd, mode: u32) -> io::Result<()> {
    // SAFETY: this call takes no pointer.
    check(unsafe { libc::syscall(libc::SYS_fchmod, fd.as_raw_fd(), mode) }).map(drop)
}

/// Sets the access and change times of the file `fd` is, and never of a
/// path: the name it passes is always null.
pub fn futimens(fd: BorrowedFd, times: &[libc::timespec; 2]) -> io::Result<()> {
    // SAFETY: `times` is two `struct timespec`s, which outlive the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_utimensat,
            fd.as_raw_fd(),
            std::ptr::null::<libc::c_char>(),
            times.as_ptr(),
            0,
        )
    })
    .map(drop)
}

pub fn fallocate(fd: BorrowedFd, mode: i32, offset: u64, len: u64) -> io::Result<()> {
    let bad = || io::Error::from_raw_os_error(libc::EINVAL);
    let offset = i64::try_from(offset).map_err(|_| bad())?;
    let len = i64::try_from(len).map_err(|_| bad())?;
    // SAFETY: this call takes no pointer.
    check(unsafe { libc::syscall(libc::SYS_fallocate, fd.as_raw_fd(), mode, offset, len) })
        .map(drop)
}

pub fn fsync(fd: BorrowedFd, data_only: bool) -> io::Result<()> {
    let call = match data_only {
        true => libc::SYS_fdatasync,
        false => libc::SYS_fsync,
    };
    // SAFETY: this call takes no pointer.
    check(unsafe { libc::syscall(call, fd.as_raw_fd()) }).map(drop)
}

/// Makes the directory `name` in `directory`. A name is one entry, so
/// nothing is made anywhere else.
pub fn mkdirat(directory: BorrowedFd, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_mkdirat,
            directory.as_raw_fd(),
            name.as_ptr(),
            mode,
        )
    })
    .map(drop)
}

pub fn unlinkat(directory: BorrowedFd, name: &CStr, flags: i32) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_unlinkat,
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
        )
    })
    .map(drop)
}

/// Makes the special file `name` in `directory`, of `mode`'s kind.
pub fn mknodat(directory: BorrowedFd, name: &CStr, mode: u32, rdev: u64) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_mknodat,
            directory.as_raw_fd(),
            name.as_ptr(),
            mode,
            rdev,
        )
    })
    .map(drop)
}

pub fn symlinkat(target: &CStr, directory: BorrowedFd, name: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and outlive the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_symlinkat,
            target.as_ptr(),
            directory.as_raw_fd(),
            name.as_ptr(),
        )
    })
    .map(drop)
}

/// Links `old` in `from` as `new` in `to`. A symlink is linked as itself.
pub fn linkat(from: BorrowedFd, old: &CStr, to: BorrowedFd, new: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and outlive the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_linkat,
            from.as_raw_fd(),
            old.as_ptr(),
            to.as_raw_fd(),
            new.as_ptr(),
            0,
        )
    })
    .map(drop)
}

pub fn renameat2(
    from: BorrowedFd,
    old: &CStr,
    to: BorrowedFd,
    new: &CStr,
    flags: u32,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and outlive the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from.as_raw_fd(),
            old.as_ptr(),
            to.as_raw_fd(),
            new.as_ptr(),
            flags,
        )
    })
    .map(drop)
}

/// An open file description lock: `F_OFD_SETLK` or `F_OFD_GETLK`, which
/// never waits.
pub fn ofd_lock(fd: BorrowedFd, command: i32, lock: &mut libc::flock) -> io::Result<()> {
    // SAFETY: `lock` is a `struct flock`, which outlives the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_fcntl,
            fd.as_raw_fd(),
            command,
            lock as *mut libc::flock,
        )
    })
    .map(drop)
}

pub fn flock(fd: BorrowedFd, operation: i32) -> io::Result<()> {
    // SAFETY: this call takes no pointer.
    check(unsafe { libc::syscall(libc::SYS_flock, fd.as_raw_fd(), operation) }).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// The kernel's own `openat2`, staying beneath `directory` and
    /// following no symlink, which the walk must match.
    fn oracle(directory: BorrowedFd, name: &CStr, flags: i32) -> io::Result<OwnedFd> {
        let how = [(flags | libc::O_CLOEXEC) as u64, 0, 0x02 | 0x04 | 0x08];
        // SAFETY: `name` and `how`, a `struct open_how`, outlive the call.
        let fd = check(unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory.as_raw_fd(),
                name.as_ptr(),
                how.as_ptr(),
                size_of_val(&how),
            )
        })?;
        // SAFETY: the descriptor is new, and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    fn outcome(opened: io::Result<OwnedFd>) -> Result<(u64, u64), i32> {
        match opened {
            Ok(fd) => {
                let stat = fstat(fd.as_fd()).unwrap();
                Ok((stat.st_dev, stat.st_ino))
            }
            Err(error) => Err(error.raw_os_error().unwrap()),
        }
    }

    #[test]
    fn opening_beneath_a_directory_matches_openat2() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sys-test")
            .join(std::process::id().to_string());
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root/dir")).unwrap();
        fs::write(base.join("root/hello.txt"), "hello").unwrap();
        fs::write(base.join("root/dir/nested.txt"), "nested").unwrap();
        fs::write(base.join("outside"), "outside").unwrap();
        symlink("hello.txt", base.join("root/link")).unwrap();
        symlink("dir", base.join("root/dirlink")).unwrap();
        symlink("../outside", base.join("root/escape")).unwrap();
        let root = File::open(base.join("root")).unwrap();
        let names = [
            ".",
            "hello.txt",
            "dir",
            "dir/nested.txt",
            "dir//nested.txt",
            "./dir/nested.txt",
            "link",
            "dirlink",
            "dirlink/nested.txt",
            "escape",
            "hello.txt/",
            "hello.txt/x",
            "dir/",
            "link/",
            "dirlink/",
            "missing",
            "missing/x",
            "..",
            "../outside",
            "/etc/hostname",
            "",
        ];
        let flags = [
            libc::O_RDONLY,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            libc::O_RDONLY | libc::O_DIRECTORY,
            libc::O_PATH | libc::O_NOFOLLOW,
            libc::O_PATH | libc::O_DIRECTORY,
        ];
        // A component as long as the kernel takes, and one longer.
        let long = ["x".repeat(255), "x".repeat(256)];
        let long = long
            .iter()
            .flat_map(|x| [x.clone(), format!("dir/{x}"), format!("{x}/x")]);
        let mut checked = 0;
        for name in names.map(str::to_owned).into_iter().chain(long) {
            let name = CString::new(name).unwrap();
            for flags in flags {
                let walked = outcome(open_beneath(root.as_fd(), &name, flags));
                let expected = outcome(oracle(root.as_fd(), &name, flags));
                assert_eq!(walked, expected, "{name:?} {flags:#o}");
                checked += 1;
            }
        }
        // Making a file beneath it, too.
        for name in [
            "made",
            "dir/made",
            "link",
            "dirlink/made",
            "made/",
            "dir/made/",
            "missing/made/",
            "dir/",
            "../made",
        ] {
            let name = CString::new(name).unwrap();
            let flags = libc::O_WRONLY | libc::O_EXCL;
            let walked = outcome(create_beneath(root.as_fd(), &name, flags, 0o600));
            if walked.is_ok() {
                fs::remove_file(base.join("root").join(name.to_str().unwrap())).unwrap();
            }
            let how = flags | libc::O_CREAT;
            let expected = outcome(oracle(root.as_fd(), &name, how)).map(|_| ());
            assert_eq!(walked.map(|_| ()), expected, "{name:?}");
            if expected.is_ok() {
                fs::remove_file(base.join("root").join(name.to_str().unwrap())).unwrap();
            }
            checked += 1;
        }
        let _ = fs::remove_dir_all(&base);
        assert_eq!(checked, 144);
    }
}
