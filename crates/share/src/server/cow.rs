//! A copy-on-write share. Not served yet.

use super::*;

pub(super) struct Cow {
    root: OwnedFd,
    pending: OwnedFd,
}

impl Cow {
    pub(super) fn new(root: OwnedFd, pending: OwnedFd) -> io::Result<Self> {
        Ok(Self { root, pending })
    }

    pub(super) fn directories(&self) -> (BorrowedFd<'_>, BorrowedFd<'_>) {
        (self.root.as_fd(), self.pending.as_fd())
    }
}

pub(super) fn dispatch(_share: &mut Cow, request: &Request) -> Option<Answer> {
    Some(match request.opcode {
        INIT => init(request.body),
        _ => Err(libc::ENOSYS),
    })
}
