//! Resolves the host paths of an Agentfile, by the host side of SPEC.md's
//! Paths section, and opens them without following a symlink swapped in after
//! the check. The guest side is the `agentfile` crate's.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Opens a directory only to walk through it.
#[cfg(target_os = "linux")]
const SEARCH: libc::c_int = libc::O_PATH | libc::O_DIRECTORY;
#[cfg(target_os = "macos")]
const SEARCH: libc::c_int = libc::O_SEARCH;

/// A host path from an Agentfile, canonicalized and checked against the
/// build context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    path: PathBuf,
    outside: bool,
}

impl HostPath {
    /// Resolves `text` from an Agentfile. A relative path resolves against
    /// `context`, the directory that holds the Agentfile, `~/` against
    /// `home`, and an absolute path is used as it is. The result is
    /// canonical: `.` and `..` are resolved, and every symlink is followed,
    /// so the path must exist.
    pub fn resolve(text: &str, context: &Path, home: &Path) -> io::Result<HostPath> {
        let context = canonical(context)?;
        let joined = if text == "~" {
            home.to_path_buf()
        } else if let Some(rest) = text.strip_prefix("~/") {
            // `~//etc` stays under home.
            home.join(rest.trim_start_matches('/'))
        } else {
            // An absolute `text` replaces `context`.
            context.join(text)
        };
        let path = canonical(&joined)?;
        let outside = !path.starts_with(&context);
        Ok(HostPath { path, outside })
    }

    /// The canonical path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the canonical path is outside the build context. A symlink
    /// inside the build context that points outside it counts as outside.
    pub fn outside(&self) -> bool {
        self.outside
    }

    /// Opens the canonical path one component at a time from `/`, refusing
    /// a symlink at any of them. If one was swapped in after `resolve`, this
    /// fails instead of following it. Read the path only through the handle.
    /// The handle is nonblocking, which matters only for a special file.
    pub fn open(&self) -> io::Result<File> {
        let (parent, name) = self.open_parent()?;
        // Nonblocking, so a FIFO can't hang it.
        self.open_at(&parent, &name, libc::O_RDONLY | libc::O_NONBLOCK)
    }

    /// Opens the directory that holds the canonical path the way `open`
    /// does, with only search permission, and returns it with the last
    /// name, so a caller can look at what is there before opening it. The
    /// root's name is `.`.
    pub fn open_parent(&self) -> io::Result<(File, CString)> {
        let mut names = self
            .path
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.as_bytes()),
                // A canonical path holds only the root and names.
                _ => None,
            });
        let last = names.next_back().unwrap_or(b".");
        let mut dir = File::open("/")?;
        for name in names {
            // A directory on the way needs only search permission.
            dir = self.open_at(&dir, &c_name(name)?, SEARCH)?;
        }
        Ok((dir, c_name(last)?))
    }

    fn open_at(&self, dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        // SAFETY: the descriptor is open, and the name is a valid C string
        // that outlives the call.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => io::Error::new(
                    error.kind(),
                    format!(
                        "{} changed after it was checked: {error}",
                        self.path.display()
                    ),
                ),
                _ => with_path(&self.path, error),
            });
        }
        // SAFETY: `openat` returned a new descriptor that nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn c_name(name: &[u8]) -> io::Result<CString> {
    CString::new(name).map_err(io::Error::other)
}

fn canonical(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().map_err(|error| with_path(path, error))
}

fn with_path(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}
