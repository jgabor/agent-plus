//! The times process, which sets the times of the agent's copies in a
//! copy-on-write share's tree for the share server.
//!
//! Landlock doesn't cover a file's mode or times, and the server of a
//! copy-on-write share holds descriptors beneath your files, so its filter
//! allows it to change neither, even if its own code were turned against
//! it. This process holds the tree alone, and Landlock holds it there too.
//! It sets the times of a file it reaches beneath the tree one name at a
//! time, without following a symlink, or of a file the server passes it
//! that no directory holds, as when the agent removed a file it still has
//! open. Your files are never either one.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use crate::landlock::{self, Access};
use crate::sys;

/// A request is the access and modification times, as two `struct
/// timespec`s of two little-endian words each, what it names, and a path
/// beneath the tree. The answer is an errno, or 0, as one little-endian
/// word.
const TIMES: usize = 32;
/// The file at the path, where `""` is the tree itself.
const NAMED: u8 = 0;
/// The file that comes with the request, with no path.
const UNNAMED: u8 = 1;
const MAX_REQUEST: usize = TIMES + 1 + libc::PATH_MAX as usize;
/// How a file opens to have its times set.
const FLAGS: i32 = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// The server's end of a times process.
pub(crate) struct Times {
    socket: OwnedFd,
}

impl Times {
    /// Starts a times process for `tree`, a directory. Call it before the
    /// server is confined. The process ends once this is dropped.
    pub(crate) fn start(tree: BorrowedFd) -> io::Result<Self> {
        let mut pair = [0; 2];
        let kind = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
        // SAFETY: `pair` has room for the two descriptors the call makes.
        if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, pair.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptors are new, and nothing else owns them.
        let (mine, theirs) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        let (socket, tree) = (theirs.as_raw_fd(), tree.as_raw_fd());
        // Compiled here, because another thread could hold the allocator's
        // lock as this one forks, and the process allocates nothing.
        let filter = vmm::confine::share_times_filter(socket);
        // The process is a grandchild that nothing waits for, so it leaves
        // nothing behind once it ends. The child in between keeps the two
        // descriptors alone at once, because it holds a copy of every other
        // thread's too, then forks.
        // SAFETY: each child makes only system calls, allocates nothing,
        // then ends at once, without running anything this process set up
        // to run.
        let child = unsafe { libc::fork() };
        match child {
            -1 => return Err(io::Error::last_os_error()),
            0 => unsafe {
                let status = match keep_only(socket.min(tree), socket.max(tree)) {
                    false => 1,
                    true => match libc::fork() {
                        0 => serve(socket, tree, &filter),
                        -1 => 1,
                        _ => 0,
                    },
                };
                libc::_exit(status)
            },
            _ => {}
        }
        drop(filter);
        let mut status = 0;
        // SAFETY: `status` outlives the call.
        while unsafe { libc::waitpid(child, &mut status, 0) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        drop(theirs);
        let times = Self { socket: mine };
        // It answers once it is confined.
        match (status, times.answer()) {
            (0, Ok(())) => Ok(times),
            _ => Err(io::Error::other("the times process didn't start")),
        }
    }

    /// The server's end of the process's socket, which it sends requests on
    /// and reads answers from.
    pub(crate) fn socket(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Sets the times of the file at `path` beneath the tree, as `futimens`
    /// takes them.
    pub(crate) fn set(&self, path: &[u8], times: &[libc::timespec; 2]) -> Result<(), i32> {
        self.ask(NAMED, path, None, times)
    }

    /// Sets the times of `file`, a regular file that no directory holds.
    pub(crate) fn set_unnamed(
        &self,
        file: BorrowedFd,
        times: &[libc::timespec; 2],
    ) -> Result<(), i32> {
        self.ask(UNNAMED, b"", Some(file), times)
    }

    fn ask(
        &self,
        kind: u8,
        path: &[u8],
        file: Option<BorrowedFd>,
        times: &[libc::timespec; 2],
    ) -> Result<(), i32> {
        let mut request = Vec::with_capacity(TIMES + 1 + path.len());
        for time in times {
            request.extend(time.tv_sec.to_le_bytes());
            request.extend(time.tv_nsec.to_le_bytes());
        }
        request.push(kind);
        request.extend(path);
        if request.len() > MAX_REQUEST {
            return Err(libc::ENAMETOOLONG);
        }
        let mut iov = libc::iovec {
            iov_base: request.as_mut_ptr().cast(),
            iov_len: request.len(),
        };
        // Room for one descriptor, aligned as a `struct cmsghdr`.
        let mut control = [0u64; 3];
        // SAFETY: an all-zero `struct msghdr` names nothing.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        if let Some(file) = file {
            message.msg_control = control.as_mut_ptr().cast();
            // SAFETY: these macros only compute sizes and offsets.
            unsafe {
                message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
                libc::CMSG_DATA(header)
                    .cast::<RawFd>()
                    .write_unaligned(file.as_raw_fd());
            }
        }
        // SAFETY: `message` and everything it points to outlive the call.
        let sent = again(|| unsafe { libc::sendmsg(self.socket(), &message, libc::MSG_NOSIGNAL) });
        if sent != request.len() as isize {
            return Err(libc::EIO);
        }
        self.answer()
    }

    /// Reads the process's answer to a request.
    fn answer(&self) -> Result<(), i32> {
        let mut answer = [0; 4];
        // SAFETY: `answer` has room for the length passed.
        let len = again(|| unsafe {
            libc::read(self.socket(), answer.as_mut_ptr().cast(), answer.len())
        });
        match (len, i32::from_le_bytes(answer)) {
            (4, 0) => Ok(()),
            (4, error) => Err(error),
            _ => Err(libc::EIO),
        }
    }
}

/// The times process, which holds `socket` and `tree` alone: it confines
/// itself with `filter`, says so, then answers each request until the
/// server hangs up. Returns the status to exit with.
fn serve(socket: RawFd, tree: RawFd, filter: &vmm::confine::Filter) -> i32 {
    // SAFETY: both descriptors are open, and nothing else here closes them.
    let (socket, tree) = unsafe { (BorrowedFd::borrow_raw(socket), BorrowedFd::borrow_raw(tree)) };
    let confined = landlock::restrict(&[(tree, Access::Read)]).and_then(|_| filter.install());
    if confined.is_err() {
        return 1;
    }
    if !reply(socket, 0) {
        return 1;
    }
    let mut request = [0u8; MAX_REQUEST];
    loop {
        let result = match receive(socket, &mut request) {
            Some(Received::HungUp) => return 0,
            Some(Received::Whole(len, file)) => set(tree, &request[..len], file),
            Some(Received::Cut) => Err(libc::EINVAL),
            None => return 1,
        };
        if !reply(socket, result.err().unwrap_or(0)) {
            return 1;
        }
    }
}

/// A request as it came.
enum Received {
    /// The server hung up.
    HungUp,
    /// A request of this length, and the file that came with it, if one
    /// did.
    Whole(usize, Option<OwnedFd>),
    /// A request too long, or with more than one file, which names none.
    Cut,
}

/// Closes every descriptor but `low` and `high`, and says whether it did.
fn keep_only(low: RawFd, high: RawFd) -> bool {
    let (low, high) = (i64::from(low), i64::from(high));
    let ranges = [
        (0, low - 1),
        (low + 1, high - 1),
        (high + 1, i64::from(u32::MAX)),
    ];
    // SAFETY: this call takes no pointer.
    let closed =
        |(first, last): (i64, i64)| unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    (ranges.into_iter())
        .filter(|(first, last)| first <= last)
        .all(|range| closed(range) == 0)
}

/// Makes `call` again while a signal interrupts it.
fn again(mut call: impl FnMut() -> isize) -> isize {
    loop {
        let result = call();
        if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return result;
        }
    }
}

/// Reads one request into `request`, or returns `None` if it can't. It
/// allocates nothing, because the filter allows no allocation.
fn receive(socket: BorrowedFd, request: &mut [u8]) -> Option<Received> {
    let mut iov = libc::iovec {
        iov_base: request.as_mut_ptr().cast(),
        iov_len: request.len(),
    };
    // Room for a `struct cmsghdr` and two descriptors, so a second one that
    // comes is seen, and refused.
    let mut control = [0u64; 3];
    // SAFETY: an all-zero `struct msghdr` names nothing.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as _;
    // SAFETY: `message` and everything it points to outlive the call.
    let len = again(|| unsafe {
        libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC)
    });
    let len = usize::try_from(len).ok()?;
    // Each descriptor that came is owned here, so it closes.
    let mut files: [Option<OwnedFd>; 2] = [None, None];
    let mut count = 0;
    // SAFETY: the kernel filled `control` as far as `msg_controllen` says,
    // and these macros walk only that.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let room = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for at in 0..room / size_of::<RawFd>() {
                    // SAFETY: the kernel made this descriptor for this
                    // process, and nothing else owns it.
                    let file = OwnedFd::from_raw_fd(data.add(at).read_unaligned());
                    if let Some(slot) = files.get_mut(count) {
                        *slot = Some(file);
                    }
                    count += 1;
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    let cut = message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    Some(match (len, cut || count > 1) {
        (_, true) => Received::Cut,
        (0, false) => Received::HungUp,
        (len, false) => Received::Whole(len, files[0].take()),
    })
}

/// Answers with `error`, or 0, and says whether the answer went.
fn reply(socket: BorrowedFd, error: i32) -> bool {
    let answer = error.to_le_bytes();
    // SAFETY: `answer` holds the length passed.
    let len =
        again(|| unsafe { libc::write(socket.as_raw_fd(), answer.as_ptr().cast(), answer.len()) });
    len == answer.len() as isize
}

/// Sets the times that `request` names.
fn set(tree: BorrowedFd, request: &[u8], file: Option<OwnedFd>) -> Result<(), i32> {
    let (times, rest) = request.split_at_checked(TIMES).ok_or(libc::EINVAL)?;
    let word = |at: usize| i64::from_le_bytes(times[at..at + 8].try_into().unwrap_or_default());
    let times = [0, 16].map(|at| libc::timespec {
        tv_sec: word(at),
        tv_nsec: word(at + 8),
    });
    match (rest.split_first(), file) {
        (Some((&NAMED, path)), None) => {
            let mut name = [0u8; libc::PATH_MAX as usize + 2];
            let path: &[u8] = match path.is_empty() {
                true => b".",
                false => path,
            };
            if path.contains(&0) {
                return Err(libc::EINVAL);
            }
            name[..path.len()].copy_from_slice(path);
            let name = CStr::from_bytes_until_nul(&name).map_err(|_| libc::EINVAL)?;
            let file = sys::open_beneath(tree, name, FLAGS).map_err(errno)?;
            sys::futimens(file.as_fd(), &times).map_err(errno)
        }
        (Some((&UNNAMED, [])), Some(file)) => {
            let stat = sys::fstat(file.as_fd()).map_err(errno)?;
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 0 {
                return Err(libc::EPERM);
            }
            sys::futimens(file.as_fd(), &times).map_err(errno)
        }
        _ => Err(libc::EINVAL),
    }
}

fn errno(error: io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::{Path, PathBuf};

    const ZERO: [libc::timespec; 2] = [libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }; 2];

    /// A directory with a tree in it, and a file of yours beside the tree.
    fn base(name: &str) -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/times-test")
            .join(format!("{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree/dir")).unwrap();
        fs::write(base.join("tree/dir/copy"), "copy").unwrap();
        fs::write(base.join("yours"), "yours").unwrap();
        symlink("../yours", base.join("tree/link")).unwrap();
        base
    }

    fn mtime(path: &Path) -> i64 {
        fs::symlink_metadata(path).unwrap().mtime()
    }

    #[test]
    fn the_times_process_sets_the_times_of_a_copy_beneath_the_tree() {
        let base = base("named");
        let tree = File::open(base.join("tree")).unwrap();
        let times = Times::start(tree.as_fd()).unwrap();
        assert_eq!(times.set(b"dir/copy", &ZERO), Ok(()));
        assert_eq!(times.set(b"dir", &ZERO), Ok(()));
        assert_eq!(times.set(b"", &ZERO), Ok(()));
        for path in ["tree/dir/copy", "tree/dir", "tree"] {
            assert_eq!(mtime(&base.join(path)), 0, "{path}");
        }
        assert_eq!(times.set(b"missing", &ZERO), Err(libc::ENOENT));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_times_process_never_reaches_past_the_tree() {
        let base = base("past");
        let yours = base.join("yours");
        let before = mtime(&yours);
        let tree = File::open(base.join("tree")).unwrap();
        let times = Times::start(tree.as_fd()).unwrap();
        let absolute = yours.as_os_str().as_encoded_bytes().to_vec();
        for path in [&b"../yours"[..], b"dir/../../yours", &absolute] {
            assert_eq!(times.set(path, &ZERO), Err(libc::EXDEV));
        }
        assert_eq!(times.set(b"link", &ZERO), Err(libc::ELOOP));
        assert_eq!(times.set(b"dir\0copy", &ZERO), Err(libc::EINVAL));
        // Your file, passed with no name, still has a name.
        let file = File::open(&yours).unwrap();
        assert_eq!(times.set_unnamed(file.as_fd(), &ZERO), Err(libc::EPERM));
        assert_eq!(mtime(&yours), before);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_times_process_sets_the_times_of_a_file_no_directory_holds() {
        let base = base("unnamed");
        let tree = File::open(base.join("tree")).unwrap();
        let times = Times::start(tree.as_fd()).unwrap();
        let copy = File::open(base.join("tree/dir/copy")).unwrap();
        fs::remove_file(base.join("tree/dir/copy")).unwrap();
        assert_eq!(times.set_unnamed(copy.as_fd(), &ZERO), Ok(()));
        assert_eq!(copy.metadata().unwrap().mtime(), 0);
        let _ = fs::remove_dir_all(&base);
    }
}
