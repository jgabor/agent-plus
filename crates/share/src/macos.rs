//! The share server's stand-ins on macOS, where it doesn't run yet. On
//! Linux it confines itself with Landlock, a user namespace, and a seccomp
//! filter, and reaches files with calls that only Linux has. macOS has no
//! stand-in for those yet, so each of them fails here, and says so. Each
//! module stands in for the Linux one of the same name.

use std::io;

fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the share server doesn't run on macOS yet",
    )
}

pub mod landlock {
    use std::io;
    use std::os::fd::BorrowedFd;

    /// No share can be confined, so there is no access to grant.
    pub enum Access {}

    pub fn restrict(_roots: &[(BorrowedFd, Access)]) -> io::Result<bool> {
        Err(super::missing())
    }
}

pub mod namespace {
    use std::io;
    use std::process::{Child, Command};

    pub fn spawn(_command: &mut Command) -> io::Result<Child> {
        Err(super::missing())
    }

    pub fn lower() -> io::Result<()> {
        Err(super::missing())
    }
}

pub mod server {
    use std::convert::Infallible;
    use std::io;
    use std::os::fd::{BorrowedFd, RawFd};

    use vmm::confine::Changes;

    use super::landlock::Access;
    use crate::Root;

    /// A server can't start, so none exists.
    pub struct Server(Infallible);

    impl Server {
        pub fn new(_roots: Vec<Root>) -> io::Result<Self> {
            Err(super::missing())
        }

        pub(crate) fn access(&self) -> Vec<(BorrowedFd<'_>, Access)> {
            match self.0 {}
        }

        pub(crate) fn changes(&self) -> Changes {
            match self.0 {}
        }

        pub(crate) fn times(&self) -> Vec<RawFd> {
            match self.0 {}
        }

        pub fn answer(&mut self, _share: u32, _request: &[u8]) -> Vec<u8> {
            match self.0 {}
        }
    }

    /// No server lists a `.git`, so `hook` never runs.
    #[doc(hidden)]
    pub fn when_git_is_listed(_hook: Option<Box<dyn FnMut()>>) {}

    /// No server moves a copy, so `hook` never runs.
    #[doc(hidden)]
    pub fn when_a_copy_moves(_hook: Option<Box<dyn FnMut() -> Result<(), i32>>>) {}
}

pub mod sys {
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, BorrowedFd};

    pub fn fstat(fd: BorrowedFd) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fstat` fills `stat`, which outlives the call.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so it wrote the whole struct.
        Ok(unsafe { stat.assume_init() })
    }
}
