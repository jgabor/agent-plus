//! What a run's agent changed in its copy-on-write shares, read from the
//! pending directory that the share server writes, and shown as `ap diff`
//! prints it, from SPEC.md's `SHARE` section.
//!
//! Each share's `records` say what each path the agent changed is now, and
//! its `tree` holds the agent's regular files. The share server's `cow`
//! module describes both. `ap diff` compares that with your files as they
//! are now, and never follows a symlink of the agent's.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::gitignore::Gitignore;
use crate::unified;

/// How much of a file git reads to decide whether it is binary.
const BINARY_PROBE: usize = 8000;
const REGULAR: u32 = 0o100644;
const EXECUTABLE: u32 = 0o100755;

/// What the agent made of a path, as its records leave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File {
        mode: u32,
    },
    Dir {
        mode: u32,
        opaque: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// A FIFO, a socket, or a device, by its record's word.
    Special {
        kind: String,
    },
    Deleted,
}

/// Your file at a path, as it was before the agent first changed it there:
/// its inode, size, modification time, and change time, in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    pub ino: u64,
    pub size: u64,
    pub mtime: i128,
    pub ctime: i128,
}

impl Base {
    /// Your file as `metadata` describes it.
    fn of(metadata: &fs::Metadata) -> Base {
        let nanoseconds = |seconds: i64, nanoseconds: i64| {
            i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
        };
        Base {
            ino: metadata.ino(),
            size: metadata.size(),
            mtime: nanoseconds(metadata.mtime(), metadata.mtime_nsec()),
            ctime: nanoseconds(metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// A share's records, read in order.
#[derive(Debug, Default)]
pub struct Records {
    /// What each path the agent changed is now.
    pub entries: BTreeMap<Vec<u8>, Entry>,
    /// Your file at each path the agent changed, before it did, or `None`
    /// where you had none.
    pub bases: BTreeMap<Vec<u8>, Option<Base>>,
}

/// What the agent sees at a path.
enum View<'a> {
    /// The agent's own entry.
    Agent(&'a Entry),
    /// Your file, which the agent hasn't changed.
    Yours,
    /// Nothing.
    Absent,
}

impl Records {
    /// Reads `text`, a share's `records`. A last line with no newline is
    /// one the share server is still writing, so it is left out.
    pub fn read(text: &[u8]) -> Result<Records, String> {
        let mut records = Records::default();
        let complete = match text.iter().rposition(|&byte| byte == b'\n') {
            Some(end) => &text[..end],
            None => return Ok(records),
        };
        for line in complete.split(|&byte| byte == b'\n') {
            records
                .read_line(line)
                .ok_or_else(|| format!("bad record {:?}", String::from_utf8_lossy(line)))?;
        }
        Ok(records)
    }

    fn read_line(&mut self, line: &[u8]) -> Option<()> {
        let mut words = Words { rest: line };
        let word = words.bare()?;
        let path = words.quoted()?;
        // The share's root is `""`, which the agent can change but not
        // remove or replace.
        let root = path.is_empty() && matches!(word, b"BASE" | b"DIR");
        if !root && !valid_path(&path) {
            return None;
        }
        let entry = match word {
            b"BASE" => {
                let base = match words.bare()? {
                    b"none" => None,
                    ino => Some(Base {
                        ino: number(ino)?,
                        size: number(words.bare()?)?,
                        mtime: time(words.bare()?)?,
                        ctime: time(words.bare()?)?,
                    }),
                };
                words.end()?;
                self.bases.entry(path).or_insert(base);
                return Some(());
            }
            b"FILE" => Entry::File {
                mode: mode(words.bare()?)?,
            },
            b"DIR" => Entry::Dir {
                mode: mode(words.bare()?)?,
                opaque: match words.bare() {
                    None => false,
                    Some(b"opaque") => true,
                    Some(_) => return None,
                },
            },
            b"SYMLINK" => Entry::Symlink {
                target: words.quoted()?,
            },
            b"FIFO" | b"SOCKET" | b"CHAR" | b"BLOCK" => {
                mode(words.bare()?)?;
                if matches!(word, b"CHAR" | b"BLOCK") {
                    number(words.bare()?)?;
                    number(words.bare()?)?;
                }
                Entry::Special {
                    kind: String::from_utf8_lossy(word).into_owned(),
                }
            }
            b"DELETE" => {
                let beneath: Vec<Vec<u8>> = (self.entries.keys())
                    .filter(|other| beneath(other, &path))
                    .cloned()
                    .collect();
                for other in beneath {
                    self.entries.remove(&other);
                }
                Entry::Deleted
            }
            _ => return None,
        };
        words.end()?;
        self.entries.insert(path, entry);
        Some(())
    }

    /// What the agent sees at `path`, as the share server shows it.
    fn view(&self, path: &[u8]) -> View<'_> {
        let mut hidden = false;
        for parent in parents(path) {
            match self.entries.get(parent) {
                None => {}
                Some(Entry::Dir { opaque, .. }) => hidden |= opaque,
                Some(_) => return View::Absent,
            }
        }
        match self.entries.get(path) {
            Some(Entry::Deleted) => View::Absent,
            Some(entry) => View::Agent(entry),
            None if hidden => View::Absent,
            None => View::Yours,
        }
    }
}

/// The words of a record: bare ones, split at a space, and quoted ones.
struct Words<'a> {
    rest: &'a [u8],
}

impl<'a> Words<'a> {
    fn bare(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() || self.rest.first() == Some(&b'"') {
            return None;
        }
        let end = (self.rest.iter())
            .position(|&byte| byte == b' ')
            .unwrap_or(self.rest.len());
        let word = &self.rest[..end];
        self.rest = self.rest.get(end + 1..).unwrap_or_default();
        (!word.is_empty()).then_some(word)
    }

    /// A quoted word, with `\"`, `\\`, and `\xNN` read back.
    fn quoted(&mut self) -> Option<Vec<u8>> {
        let mut bytes = self.rest.strip_prefix(b"\"")?.iter();
        let mut word = Vec::new();
        loop {
            match *bytes.next()? {
                b'"' => break,
                b'\\' => match *bytes.next()? {
                    b'x' => {
                        let high = (*bytes.next()? as char).to_digit(16)?;
                        let low = (*bytes.next()? as char).to_digit(16)?;
                        word.push((high * 16 + low) as u8);
                    }
                    byte @ (b'"' | b'\\') => word.push(byte),
                    _ => return None,
                },
                byte => word.push(byte),
            }
        }
        let rest = bytes.as_slice();
        self.rest = match rest.first() {
            None => rest,
            Some(b' ') => &rest[1..],
            Some(_) => return None,
        };
        Some(word)
    }

    fn end(&self) -> Option<()> {
        self.rest.is_empty().then_some(())
    }
}

fn number(word: &[u8]) -> Option<u64> {
    std::str::from_utf8(word).ok()?.parse().ok()
}

fn mode(word: &[u8]) -> Option<u32> {
    u32::from_str_radix(std::str::from_utf8(word).ok()?, 8)
        .ok()
        .filter(|mode| mode & !0o7777 == 0)
}

/// A time as `<seconds>.<nanoseconds>`, in nanoseconds.
fn time(word: &[u8]) -> Option<i128> {
    let (seconds, nanoseconds) = std::str::from_utf8(word).ok()?.split_once('.')?;
    let seconds: i64 = seconds.parse().ok()?;
    let nanoseconds: u32 = nanoseconds.parse().ok().filter(|&n| n < 1_000_000_000)?;
    Some(i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds))
}

/// Whether `path` names something beneath a share's root: names joined by
/// `/`, none of them empty, `.`, or `..`.
fn valid_path(path: &[u8]) -> bool {
    !path.is_empty()
        && !path.contains(&0)
        && path
            .split(|&byte| byte == b'/')
            .all(|name| !matches!(name, b"" | b"." | b".."))
}

/// Each directory above `path`, from the root's first down.
fn parents(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    (path.iter().enumerate())
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
}

/// Whether `path` is beneath `dir`.
fn beneath(path: &[u8], dir: &[u8]) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// How long `ap diff` keeps reading a share again when a file its records
/// name has gone from the tree, or is no longer a file, as happens while
/// the agent moves it, and how long it waits between reads. The share
/// server changes the tree, then the records, and can be held up between
/// the two on a busy machine.
const PATIENCE: Duration = Duration::from_secs(2);
const PAUSE: Duration = Duration::from_millis(1);

/// `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS`, from
/// `<linux/openat2.h>`.
const NO_SYMLINKS: u64 = 0x04 | 0x02;
/// Those, and `RESOLVE_BENEATH`.
const BENEATH: u64 = NO_SYMLINKS | 0x08;

/// `struct open_how`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Opens `name` from `directory` with `openat2`, resolving it as `resolve`
/// says.
fn openat2(directory: i32, name: &CStr, flags: i32, resolve: u64) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: `name` is NUL-terminated and `how` is a `struct open_how`,
    // both outliving the call, which returns a new descriptor or -1.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            name.as_ptr(),
            &raw const how,
            size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is new, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Your files in a share, each reached beneath the share's root and never
/// through a symlink.
struct Host {
    root: Option<OwnedFd>,
}

impl Host {
    /// Your files beneath `root`, which is reached with no symlink on the
    /// way, as the run that recorded it found it.
    fn new(root: &Path) -> io::Result<Host> {
        let name = CString::new(root.as_os_str().as_bytes()).map_err(io::Error::other)?;
        match openat2(libc::AT_FDCWD, &name, libc::O_DIRECTORY, NO_SYMLINKS) {
            Ok(root) => Ok(Host { root: Some(root) }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Host { root: None }),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Err(io::Error::other(
                "the share's directory is now reached through a symlink",
            )),
            Err(error) => Err(error),
        }
    }

    /// Opens `path` with `flags`, or `None` if you have nothing there that
    /// a path with no symlink in it reaches.
    fn open(&self, path: &[u8], flags: i32) -> io::Result<Option<OwnedFd>> {
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let name = CString::new(path).map_err(io::Error::other)?;
        match openat2(root.as_raw_fd(), &name, flags | libc::O_NOFOLLOW, BENEATH) {
            Ok(fd) => Ok(Some(fd)),
            Err(error) => match error.raw_os_error() {
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV) => Ok(None),
                _ => Err(error),
            },
        }
    }

    /// What you have at `path`, as itself if it is a symlink.
    fn stat(&self, path: &[u8]) -> io::Result<Option<fs::Metadata>> {
        match self.open(path, libc::O_PATH)? {
            Some(fd) => Ok(Some(File::from(fd).metadata()?)),
            None => Ok(None),
        }
    }

    /// Your file at `path` now.
    fn yours(&self, path: &[u8]) -> io::Result<Yours> {
        let Some(fd) = self.open(path, libc::O_PATH)? else {
            return Ok(Yours::None);
        };
        let metadata = File::from(fd.try_clone()?).metadata()?;
        if metadata.is_symlink() {
            let mut target = vec![0; libc::PATH_MAX as usize];
            // SAFETY: the empty name is NUL-terminated, and `target` has
            // room for the length passed.
            let len = unsafe {
                libc::readlinkat(
                    fd.as_raw_fd(),
                    c"".as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if len < 0 {
                return Err(io::Error::last_os_error());
            }
            target.truncate(len as usize);
            return Ok(Yours::Symlink(target));
        }
        if !metadata.is_file() {
            return Ok(Yours::Other);
        }
        let Some(file) = self.open(path, libc::O_RDONLY | libc::O_NONBLOCK)? else {
            return Ok(Yours::None);
        };
        let mut data = Vec::new();
        io::Read::read_to_end(&mut File::from(file), &mut data)?;
        Ok(Yours::File {
            mode: metadata.mode(),
            data,
        })
    }

    /// The names in your directory at `path`, in order.
    fn names(&self, path: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let Some(dir) = self.open(path, libc::O_RDONLY | libc::O_DIRECTORY)? else {
            return Ok(Vec::new());
        };
        // The descriptor is the directory, opened with no symlink followed.
        let mut names = Vec::new();
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            names.push(entry?.file_name().as_bytes().to_vec());
        }
        names.sort();
        Ok(names)
    }

    /// Calls `found` with each of your files at or beneath `path` that
    /// isn't a directory.
    fn beneath(&self, path: &[u8], found: &mut dyn FnMut(Vec<u8>)) -> io::Result<()> {
        let Some(metadata) = self.stat(path)? else {
            return Ok(());
        };
        if !metadata.is_dir() {
            found(path.to_vec());
            return Ok(());
        }
        for name in self.names(path)? {
            self.beneath(&[path, b"/", &name].concat(), found)?;
        }
        Ok(())
    }

    /// Your directory at `path`, if a path with no symlink in it reaches one.
    fn dir(&self, path: &[u8]) -> io::Result<Option<OwnedFd>> {
        let path = if path.is_empty() { b"." } else { path };
        self.open(path, libc::O_RDONLY | libc::O_DIRECTORY)
    }

    /// Opens your directory at `path`, through no symlink, and makes each
    /// one that is missing on the way with the mode the agent gave it.
    fn make_dir(&self, records: &Records, path: &[u8]) -> io::Result<OwnedFd> {
        let root = (self.root.as_ref()).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut dir = root.try_clone()?;
        if path.is_empty() {
            return Ok(dir);
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let mut end = 0;
        for name in path.split(|&byte| byte == b'/') {
            end += name.len();
            let at = &path[..end];
            end += 1;
            let name = CString::new(name).map_err(io::Error::other)?;
            dir = match openat2(dir.as_raw_fd(), &name, flags, BENEATH) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // You can write in it until what goes in it is written,
                    // and [`Host::seal`] gives it the agent's mode.
                    let mode = match records.view(at) {
                        View::Agent(Entry::Dir { mode, .. }) => *mode | 0o700,
                        _ => 0o755,
                    };
                    // SAFETY: `name` is NUL-terminated, and outlives the call.
                    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode) } < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                    openat2(dir.as_raw_fd(), &name, flags, BENEATH)?
                }
                // A symlink there is never followed, and says so.
                Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                    let found = openat2(
                        dir.as_raw_fd(),
                        &name,
                        libc::O_PATH | libc::O_NOFOLLOW,
                        BENEATH,
                    );
                    return match found.and_then(|fd| File::from(fd).metadata()) {
                        Ok(metadata) if metadata.is_symlink() => {
                            Err(io::Error::from_raw_os_error(libc::ELOOP))
                        }
                        _ => Err(error),
                    };
                }
                opened => opened?,
            };
        }
        Ok(dir)
    }

    /// Removes your file at `path`, if a path with no symlink in it reaches
    /// one.
    fn remove(&self, path: &[u8]) -> io::Result<()> {
        let (parent, name) = split(path);
        let Some(dir) = self.dir(parent)? else {
            return Ok(());
        };
        match unlink(&dir, name, 0) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        }
    }

    /// Removes each empty directory of yours at or beneath `path` that the
    /// agent's view doesn't have, deepest first. One that isn't empty
    /// stays, holding files that weren't applied, such as git-ignored ones.
    fn prune(&self, records: &Records, path: &[u8]) {
        // Beneath a directory the agent sees as yours, only paths it has
        // records of differ, and each of those is pruned on its own.
        // Nothing is ever applied under `.git`, and that includes removing
        // a repository's empty directories.
        let view = records.view(path);
        let git = (path.split(|&byte| byte == b'/')).any(|name| name.eq_ignore_ascii_case(b".git"));
        if path.is_empty()
            || git
            || matches!(view, View::Yours)
            || !matches!(self.stat(path), Ok(Some(m)) if m.is_dir())
        {
            return;
        }
        for name in self.names(path).unwrap_or_default() {
            self.prune(records, &[path, b"/", &name].concat());
        }
        if let View::Agent(Entry::Dir { .. }) = view {
            return;
        }
        let (parent, name) = split(path);
        if let Ok(Some(dir)) = self.dir(parent) {
            let _ = unlink(&dir, name, libc::AT_REMOVEDIR);
        }
    }

    /// Puts the agent's file at `path`, with this mode and the contents of
    /// `from`, unless yours is the same already, and says whether it did.
    fn write_file(
        &self,
        records: &Records,
        path: &[u8],
        mode: u32,
        from: &Path,
    ) -> io::Result<bool> {
        let mut source = File::open(from)?;
        if let Yours::File { mode: yours, data } = self.yours(path)?
            && yours & 0o7777 == mode
            && fs::read(from)? == data
        {
            return Ok(false);
        }
        self.replace(records, path, |dir, temp| {
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
            // SAFETY: `temp` is NUL-terminated, and outlives the call,
            // which returns a new descriptor or -1.
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    temp.as_ptr(),
                    flags | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the descriptor is new, and nothing else owns it.
            let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            io::copy(&mut source, &mut file)?;
            file.set_permissions(fs::Permissions::from_mode(mode))
        })
        .map(|()| true)
    }

    /// Puts the agent's symlink at `path`, unless yours is the same, and
    /// says whether it did.
    fn write_symlink(&self, records: &Records, path: &[u8], target: &[u8]) -> io::Result<bool> {
        if matches!(self.yours(path)?, Yours::Symlink(yours) if yours == target) {
            return Ok(false);
        }
        let target = CString::new(target).map_err(io::Error::other)?;
        self.replace(records, path, |dir, temp| {
            // SAFETY: both names are NUL-terminated, and outlive the call.
            match unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), temp.as_ptr()) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        })
        .map(|()| true)
    }

    /// Puts what `make` makes at a new name beside `path` in its place, in
    /// one rename, so whatever happens, your file at `path` is either as it
    /// was or the agent's. An empty directory of yours there makes way.
    fn replace(
        &self,
        records: &Records,
        path: &[u8],
        make: impl FnOnce(&OwnedFd, &CStr) -> io::Result<()>,
    ) -> io::Result<()> {
        let (parent, name) = split(path);
        let dir = self.make_dir(records, parent)?;
        let name = CString::new(name).map_err(io::Error::other)?;
        let temp =
            CString::new(format!(".ap-{}", store::random_name()?)).map_err(io::Error::other)?;
        let rename = || {
            // SAFETY: both names are NUL-terminated, and outlive the call.
            match unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temp.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                )
            } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        };
        let made = make(&dir, &temp).and_then(|()| match rename() {
            Err(error) if error.raw_os_error() == Some(libc::EISDIR) => {
                unlink(&dir, name.as_bytes(), libc::AT_REMOVEDIR)?;
                rename()
            }
            renamed => renamed,
        });
        if made.is_err() {
            let _ = unlink(&dir, temp.as_bytes(), 0);
        }
        made
    }

    /// Gives each directory the agent made where you had nothing the mode
    /// the agent gave it, deepest first, once nothing beneath it stays
    /// pending. Until then it stays one you can write in, and what stays
    /// pending keeps its record.
    fn seal(&self, records: &Records, kept: &BTreeMap<Vec<u8>, Option<Option<Base>>>) {
        for path in records.entries.keys().rev() {
            let View::Agent(Entry::Dir { mode, .. }) = records.view(path) else {
                continue;
            };
            if records.bases.get(path) != Some(&None)
                || kept.keys().any(|other| beneath(other, path))
                || !matches!(self.stat(path), Ok(Some(m)) if m.is_dir())
            {
                continue;
            }
            if let Ok(Some(dir)) = self.dir(path) {
                let _ = File::from(dir).set_permissions(fs::Permissions::from_mode(*mode));
            }
        }
    }
}

/// Removes `name` from the directory `dir`, as `unlinkat` does with `flags`.
fn unlink(dir: &OwnedFd, name: &[u8], flags: i32) -> io::Result<()> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: `name` is NUL-terminated, and outlives the call.
    match unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The directory above `path`, `""` for the share's root, and its name.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (b"", path),
    }
}

fn bytes_path(path: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(path))
}

/// Your file at a path.
enum Yours {
    None,
    File {
        mode: u32,
        data: Vec<u8>,
    },
    Symlink(Vec<u8>),
    /// A directory or a special file, which no diff shows.
    Other,
}

/// The agent's file at a path.
enum Theirs {
    None,
    File { mode: u32, data: Vec<u8> },
    Symlink(Vec<u8>),
}

/// A file's mode as a git diff's header gives it: executable by you or
/// not.
fn git_mode(mode: u32) -> u32 {
    match mode & 0o100 {
        0 => REGULAR,
        _ => EXECUTABLE,
    }
}

/// In a share's pending directory, the share server's record of changes,
/// and the journal of what an `ap apply` landed: a `BASE` record of each of
/// your files once it landed it, until it rewrites the records.
const RECORDS: &str = "records";
const LANDED: &str = "landed";

/// One copy-on-write share of a run, as `ap diff` reads it.
pub struct Share<'a> {
    /// The share's directory on your host.
    pub root: &'a Path,
    /// The directory that keeps its changes.
    pub dir: &'a Path,
}

/// What `ap diff` prints for one share, and how many git-ignored files it
/// left out.
pub struct Shown {
    pub text: Vec<u8>,
    pub ignored: usize,
}

impl Share<'_> {
    /// The changes the agent made to this share, as `ap diff` prints them.
    /// While the run is active, the agent can move a file between reading
    /// the records and reading the file, so it reads the share again.
    pub fn diff(&self) -> io::Result<Shown> {
        let start = Instant::now();
        loop {
            if let Some(shown) = self.attempt()? {
                return Ok(shown);
            }
            if start.elapsed() > PATIENCE {
                return Err(io::Error::other(
                    "the agent kept changing files while they were read, so try again",
                ));
            }
            std::thread::sleep(PAUSE);
        }
    }

    /// The share's records, or its journal, by its name in the share's
    /// pending directory.
    fn read(&self, name: &str) -> io::Result<Records> {
        let text = match fs::read(self.dir.join(name)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Records::read(&text).map_err(io::Error::other)
    }

    /// The share's changes, or `None` if a file its records name was gone.
    fn attempt(&self) -> io::Result<Option<Shown>> {
        let records = self.read(RECORDS)?;
        let landed = self.read(LANDED)?.bases;
        let host = Host::new(self.root)?;
        let ignore = match host.stat(b".")? {
            Some(metadata) if metadata.is_dir() => Gitignore::load(self.root)?,
            _ => Gitignore::default(),
        };
        let repository = host.stat(b".git")?.is_some_and(|m| m.is_dir());
        let mut shown = Shown {
            text: Vec::new(),
            ignored: 0,
        };
        for path in changed(&records, &host)? {
            match verdict(&records, &landed, &host, &ignore, repository, &path)? {
                Verdict::History => continue,
                Verdict::Ignored => {
                    shown.ignored += 1;
                    continue;
                }
                Verdict::Skipped { why, .. } => {
                    line(&mut shown.text, "skipped", &path, &why);
                    continue;
                }
                Verdict::Change => {}
            }
            let theirs = match records.view(&path) {
                View::Agent(Entry::File { mode }) => {
                    match fs::read(self.dir.join("tree").join(bytes_path(&path))) {
                        Ok(data) => Theirs::File { mode: *mode, data },
                        // The agent moved or replaced it since.
                        Err(error)
                            if matches!(
                                error.raw_os_error(),
                                Some(libc::ENOENT | libc::EISDIR | libc::ENOTDIR)
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(error) => return Err(error),
                    }
                }
                View::Agent(Entry::Symlink { target }) => Theirs::Symlink(target.clone()),
                _ => Theirs::None,
            };
            render(&mut shown.text, &path, host.yours(&path)?, theirs);
        }
        Ok(Some(shown))
    }

    /// Whether the share's root on your host holds a `.git` directory, so
    /// that its changes come back as a branch.
    pub fn is_repository(&self) -> io::Result<bool> {
        Ok((Host::new(self.root)?.stat(b".git")?).is_some_and(|m| m.is_dir()))
    }

    /// Applies the agent's changes to your files in this share, which holds
    /// no repository, while the run isn't active. Then its records keep
    /// only the files it couldn't apply. Each file is written beside its
    /// target and renamed into place, so your file is either as it was or
    /// the agent's.
    pub fn apply(&self) -> io::Result<Applied> {
        let records = self.read(RECORDS)?;
        let landed = self.read(LANDED)?.bases;
        let host = Host::new(self.root)?;
        if host.root.is_none() {
            return Err(io::Error::other("the share's directory is gone"));
        }
        let ignore = Gitignore::load(self.root)?;
        let mut applied = Applied {
            text: Vec::new(),
            ignored: 0,
            pending: 0,
        };
        // Each line of the report by its path, so they print in order.
        let mut report = BTreeMap::new();
        // Each path that stays pending, and the `BASE` record it keeps.
        let mut kept = BTreeMap::new();
        let mut changes = Vec::new();
        for path in changed(&records, &host)? {
            match verdict(&records, &landed, &host, &ignore, false, &path)? {
                Verdict::History => {}
                Verdict::Ignored => applied.ignored += 1,
                Verdict::Skipped { why, conflict } => {
                    let mut out = Vec::new();
                    line(&mut out, "skipped", &path, &why);
                    if conflict {
                        let base = recorded_base(&records, &path).map(|(_, base)| *base);
                        kept.insert(path.clone(), base);
                    }
                    report.insert(path, out);
                }
                Verdict::Change => changes.push(path),
            }
        }
        // Each file this lands, as it lands it, so that one stopped before
        // it rewrites the records doesn't find them changed on your side.
        let mut journal = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.dir.join(LANDED))?;
        // From here on, a failure keeps its one file pending, and nothing
        // stops the rest, so the records always say what stays.
        let mut failed = BTreeMap::new();
        // Your files the agent's go in place of, deepest first.
        for path in changes.iter().rev() {
            let replaced = matches!(
                records.view(path),
                View::Agent(Entry::File { .. } | Entry::Symlink { .. })
            );
            if !replaced && let Err(error) = host.remove(path) {
                failed.insert(path.clone(), error);
            }
        }
        for path in records.entries.keys().rev() {
            host.prune(&records, path);
        }
        for path in &changes {
            if failed.contains_key(path) {
                continue;
            }
            let written = match records.view(path) {
                View::Agent(Entry::File { mode }) => {
                    let from = self.dir.join("tree").join(bytes_path(path));
                    host.write_file(&records, path, *mode, &from)
                }
                View::Agent(Entry::Symlink { target }) => {
                    host.write_symlink(&records, path, target)
                }
                View::Agent(Entry::Dir { .. }) => host.make_dir(&records, path).map(|_| false),
                _ => Ok(false),
            };
            match written {
                Ok(true) => {
                    if let Ok(Some(now)) = host.stat(path) {
                        let mut out = Vec::new();
                        base_record(&mut out, path, Some(&Base::of(&now)));
                        let _ = io::Write::write_all(&mut journal, &out);
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    failed.insert(path.clone(), error);
                }
            }
        }
        for (path, error) in failed {
            let why = match error.raw_os_error() {
                Some(libc::ELOOP) => "a directory on its way is a symlink on your side".to_owned(),
                _ => error.to_string(),
            };
            let mut out = Vec::new();
            line(&mut out, "failed", &path, &why);
            report.insert(path.clone(), out);
            // Your file is as it was, so a later `ap apply` can try again.
            let now = host.stat(&path).ok().flatten();
            kept.insert(path, Some(now.as_ref().map(Base::of)));
        }
        host.seal(&records, &kept);
        applied.text = report.into_values().flatten().collect();
        applied.pending = kept.len();
        self.keep(&records, &kept)?;
        Ok(applied)
    }

    /// Rewrites the share's records to hold only the paths in `kept`, each
    /// with the `BASE` record given, and removes the rest of the tree.
    fn keep(
        &self,
        records: &Records,
        kept: &BTreeMap<Vec<u8>, Option<Option<Base>>>,
    ) -> io::Result<()> {
        let mut out = Vec::new();
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        for (path, base) in kept {
            // The directories the agent made above it, so that a later
            // `ap apply` makes them as the agent did.
            for dir in parents(path) {
                if let View::Agent(Entry::Dir { mode, .. }) = records.view(dir)
                    && dirs.insert(dir)
                {
                    if let Some(base) = records.bases.get(dir) {
                        base_record(&mut out, dir, base.as_ref());
                    }
                    let entry = Entry::Dir {
                        mode: *mode,
                        opaque: false,
                    };
                    entry_record(&mut out, dir, &entry);
                }
            }
            if let Some(base) = base {
                base_record(&mut out, path, base.as_ref());
            }
            let entry = match records.view(path) {
                View::Agent(entry) => entry.clone(),
                _ => Entry::Deleted,
            };
            if let Entry::File { .. } = entry {
                files.insert(path.clone());
            }
            entry_record(&mut out, path, &entry);
        }
        let new = self.dir.join(format!("{RECORDS}.new"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&new)?;
        io::Write::write_all(&mut file, &out)?;
        file.sync_all()?;
        fs::rename(&new, self.dir.join(RECORDS))?;
        match fs::remove_file(self.dir.join(LANDED)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        match prune_tree(&self.dir.join("tree"), b"", &files) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// What `ap apply` did with one share.
pub struct Applied {
    /// A line for each file it skipped or couldn't write, in order.
    pub text: Vec<u8>,
    /// How many git-ignored files it left out.
    pub ignored: usize,
    /// How many files stay pending.
    pub pending: usize,
}

/// What becomes of a path whose file the agent changed.
enum Verdict {
    /// The repository's own history, which the branch carries.
    History,
    Ignored,
    /// It is left out, and why. A file that changed on your side too stays
    /// pending, and anything else left out never applies.
    Skipped {
        why: String,
        conflict: bool,
    },
    Change,
}

fn verdict(
    records: &Records,
    landed: &BTreeMap<Vec<u8>, Option<Base>>,
    host: &Host,
    ignore: &Gitignore,
    repository: bool,
    path: &[u8],
) -> io::Result<Verdict> {
    let names: Vec<&[u8]> = path.split(|&byte| byte == b'/').collect();
    if repository && names[0] == b".git" {
        return Ok(Verdict::History);
    }
    if ignore.is_ignored(bytes_path(path), false) {
        return Ok(Verdict::Ignored);
    }
    let skipped = |why: &str| {
        Ok(Verdict::Skipped {
            why: why.to_owned(),
            conflict: false,
        })
    };
    if names.iter().any(|name| name.eq_ignore_ascii_case(b".git")) {
        return skipped("it is under .git");
    }
    match records.view(path) {
        View::Agent(Entry::Special { kind }) => {
            let kind = kind.to_ascii_lowercase();
            return skipped(&format!(
                "it is a {kind}, and special files are never applied"
            ));
        }
        View::Agent(Entry::Symlink { target }) => {
            if let Some(why) = leaves(records, host, path, target)? {
                return skipped(why);
            }
        }
        _ => {}
    }
    // A branch import leaves your working tree as it is, so a file you
    // changed too isn't skipped there.
    if !repository
        && changed_on_your_side(records, host, path)?
        && !landed_by_apply(landed, host, path)?
    {
        return Ok(Verdict::Skipped {
            why: "it changed on your side after the agent first changed it".to_owned(),
            conflict: true,
        });
    }
    Ok(Verdict::Change)
}

/// Whether your file at `path` is one an earlier `ap apply` landed, with
/// nothing changed since, by the journal it left when it was stopped before
/// it rewrote the records.
fn landed_by_apply(
    landed: &BTreeMap<Vec<u8>, Option<Base>>,
    host: &Host,
    path: &[u8],
) -> io::Result<bool> {
    let Some(base) = landed.get(path) else {
        return Ok(false);
    };
    Ok(host.stat(path)?.as_ref().map(Base::of) == *base)
}

/// Prints a line of the report for `path`: what happened to it, and why.
fn line(out: &mut Vec<u8>, what: &str, path: &[u8], why: &str) {
    out.extend(what.bytes());
    out.push(b' ');
    quote(out, b"", path);
    out.extend(format!(": {why}\n").bytes());
}

/// Removes every file in the tree at or beneath `path` but those in `keep`,
/// and every directory left empty, and returns whether anything is left.
fn prune_tree(tree: &Path, path: &[u8], keep: &BTreeSet<Vec<u8>>) -> io::Result<bool> {
    let mut left = false;
    for entry in fs::read_dir(tree.join(bytes_path(path)))? {
        let entry = entry?;
        let name = entry.file_name();
        let child = match path.is_empty() {
            true => name.as_bytes().to_vec(),
            false => [path, b"/", name.as_bytes()].concat(),
        };
        let at = tree.join(bytes_path(&child));
        if entry.file_type()?.is_dir() {
            match prune_tree(tree, &child, keep)? {
                true => left = true,
                false => fs::remove_dir(at)?,
            }
        } else if keep.contains(&child) {
            left = true;
        } else {
            fs::remove_file(at)?;
        }
    }
    Ok(left)
}

/// Quotes a record's word, as the share server does.
fn record_word(out: &mut Vec<u8>, word: &[u8]) {
    out.extend(b" \"");
    for &byte in word {
        match byte {
            b'"' => out.extend(b"\\\""),
            b'\\' => out.extend(b"\\\\"),
            0x20..0x7f => out.push(byte),
            _ => out.extend(format!("\\x{byte:02x}").bytes()),
        }
    }
    out.push(b'"');
}

/// The `BASE` record of `path`.
fn base_record(out: &mut Vec<u8>, path: &[u8], base: Option<&Base>) {
    out.extend(b"BASE");
    record_word(out, path);
    let time = |time: i128| {
        let second = 1_000_000_000;
        format!("{}.{:09}", time.div_euclid(second), time.rem_euclid(second))
    };
    match base {
        None => out.extend(b" none"),
        Some(base) => out.extend(
            format!(
                " {} {} {} {}",
                base.ino,
                base.size,
                time(base.mtime),
                time(base.ctime)
            )
            .bytes(),
        ),
    }
    out.push(b'\n');
}

/// The record that sets `path` to `entry`. A special file keeps its kind,
/// and nothing else, because it is never applied.
fn entry_record(out: &mut Vec<u8>, path: &[u8], entry: &Entry) {
    let word = match entry {
        Entry::File { .. } => "FILE",
        Entry::Dir { .. } => "DIR",
        Entry::Symlink { .. } => "SYMLINK",
        Entry::Special { kind } => kind,
        Entry::Deleted => "DELETE",
    };
    out.extend(word.bytes());
    record_word(out, path);
    match entry {
        Entry::File { mode } => out.extend(format!(" {mode:o}").bytes()),
        Entry::Dir { mode, opaque } => {
            out.extend(format!(" {mode:o}").bytes());
            if *opaque {
                out.extend(b" opaque");
            }
        }
        Entry::Symlink { target } => record_word(out, target),
        Entry::Special { kind } => match kind.as_str() {
            "CHAR" | "BLOCK" => out.extend(b" 0 0 0"),
            _ => out.extend(b" 0"),
        },
        Entry::Deleted => {}
    }
    out.push(b'\n');
}

/// Every path whose file differs between yours and the agent's view, in
/// order.
fn changed(records: &Records, host: &Host) -> io::Result<BTreeSet<Vec<u8>>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in &records.entries {
        if let Entry::Dir { opaque: false, .. } = entry {
            // A directory where you have a file replaces it.
            if host.stat(path)?.is_some_and(|m| !m.is_dir()) {
                paths.insert(path.clone());
            }
            continue;
        }
        if let View::Agent(entry) = records.view(path)
            && !matches!(entry, Entry::Dir { .. })
        {
            paths.insert(path.clone());
        }
        // Whatever the agent put here hides what you have at and beneath
        // it, unless it made a directory that shows yours.
        host.beneath(path, &mut |yours| {
            if !matches!(records.view(&yours), View::Yours) {
                paths.insert(yours);
            }
        })?;
    }
    Ok(paths)
}

/// Whether your file at `path` changed since the agent first changed it,
/// by the record the share server kept of it then. A file of yours that
/// the agent never changed itself, but removed with a directory above it,
/// goes by the record of the nearest such directory.
fn changed_on_your_side(records: &Records, host: &Host, path: &[u8]) -> io::Result<bool> {
    let Some((path, base)) = recorded_base(records, path) else {
        return Ok(false);
    };
    Ok(host.stat(path)?.as_ref().map(Base::of) != *base)
}

/// The record the share server kept of your file at `path`, or at the
/// nearest directory above it that has one, and that record's path.
fn recorded_base<'a>(records: &'a Records, path: &'a [u8]) -> Option<(&'a [u8], &'a Option<Base>)> {
    let mut recorded = parents(path).chain([path]).collect::<Vec<_>>();
    recorded.reverse();
    (recorded.into_iter()).find_map(|path| Some((path, records.bases.get(path)?)))
}

/// Why a symlink at `path` with this target would leave the share, if it
/// would. The target is read as your host would read it once applied, and
/// a symlink on the way, yours or the agent's, could lead anywhere. One of
/// yours counts even where the agent changed it, because that change may
/// be skipped.
fn leaves(
    records: &Records,
    host: &Host,
    path: &[u8],
    target: &[u8],
) -> io::Result<Option<&'static str>> {
    const OUTSIDE: Option<&str> = Some("it is a symlink whose target leaves the share");
    const THROUGH: Option<&str> = Some("it is a symlink whose target goes through a symlink");
    if target.starts_with(b"/") || target.is_empty() {
        return Ok(OUTSIDE);
    }
    let mut at: Vec<&[u8]> = path.split(|&byte| byte == b'/').collect();
    at.pop();
    for name in target.split(|&byte| byte == b'/') {
        match name {
            b"" | b"." => continue,
            b".." => {
                if at.pop().is_none() {
                    return Ok(OUTSIDE);
                }
                continue;
            }
            _ => at.push(name),
        }
        let reached = at.join(&b'/');
        // Yours counts too, because the agent's change there may not land.
        let agent = matches!(records.view(&reached), View::Agent(Entry::Symlink { .. }));
        if agent || host.stat(&reached)?.is_some_and(|m| m.is_symlink()) {
            return Ok(THROUGH);
        }
    }
    Ok(None)
}

/// Prints one path's change: a unified diff for a text file, and one line
/// for a binary file, a mode change, or a symlink.
fn render(out: &mut Vec<u8>, path: &[u8], yours: Yours, theirs: Theirs) {
    let yours = match yours {
        Yours::Symlink(old) => {
            match &theirs {
                Theirs::Symlink(new) if *new == old => return,
                Theirs::Symlink(_) => {}
                _ => {
                    out.extend(b"delete symlink ");
                    quote(out, b"", path);
                    out.push(b'\n');
                }
            }
            None
        }
        Yours::File { mode, data } => Some((mode, data)),
        Yours::None | Yours::Other => None,
    };
    let theirs = match theirs {
        Theirs::Symlink(target) => {
            out.extend(b"symlink ");
            quote(out, b"", path);
            out.extend(b" -> ");
            quote(out, b"", &target);
            out.push(b'\n');
            None
        }
        Theirs::File { mode, data } => Some((mode, data)),
        Theirs::None => None,
    };
    let (old, new) = match (yours, theirs) {
        (None, None) => return,
        (Some((old_mode, old)), Some((new_mode, new))) => {
            let (old_mode, new_mode) = (old_mode & 0o7777, new_mode & 0o7777);
            if old_mode != new_mode {
                let file = libc::S_IFREG;
                let line = format!(
                    "mode change {:o} => {:o} ",
                    file | old_mode,
                    file | new_mode
                );
                out.extend(line.bytes());
                quote(out, b"", path);
                out.push(b'\n');
            }
            if old == new {
                return;
            }
            (
                Some((git_mode(old_mode), old)),
                Some((git_mode(new_mode), new)),
            )
        }
        (old, new) => (
            old.map(|(mode, data)| (git_mode(mode), data)),
            new.map(|(mode, data)| (git_mode(mode), data)),
        ),
    };
    let binary = |data: &[u8]| data[..data.len().min(BINARY_PROBE)].contains(&0);
    let empty = Vec::new();
    let old_data = old.as_ref().map_or(&empty, |(_, data)| data);
    let new_data = new.as_ref().map_or(&empty, |(_, data)| data);
    let side = |out: &mut Vec<u8>, prefix: &[u8], present: bool| match present {
        true => quote(out, prefix, path),
        false => out.extend(b"/dev/null"),
    };
    if binary(old_data) || binary(new_data) {
        out.extend(b"Binary files ");
        side(out, b"a/", old.is_some());
        out.extend(b" and ");
        side(out, b"b/", new.is_some());
        out.extend(b" differ\n");
        return;
    }
    out.extend(b"diff --git ");
    quote(out, b"a/", path);
    out.push(b' ');
    quote(out, b"b/", path);
    out.push(b'\n');
    match (&old, &new) {
        (None, Some((mode, _))) => out.extend(format!("new file mode {mode:o}\n").bytes()),
        (Some((mode, _)), None) => out.extend(format!("deleted file mode {mode:o}\n").bytes()),
        _ => {}
    }
    let hunks = unified::hunks(old_data, new_data);
    if hunks.is_empty() {
        return;
    }
    out.extend(b"--- ");
    side(out, b"a/", old.is_some());
    out.extend(b"\n+++ ");
    side(out, b"b/", new.is_some());
    out.push(b'\n');
    out.extend(hunks);
}

/// Prints `prefix` and `path` as git does, in double quotes with C escapes
/// when the path holds a control character, `"`, `\`, or a byte that isn't
/// ASCII.
pub fn quote(out: &mut Vec<u8>, prefix: &[u8], path: &[u8]) {
    let plain = |byte: u8| (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\';
    if prefix.iter().chain(path).all(|&byte| plain(byte)) {
        out.extend(prefix);
        out.extend(path);
        return;
    }
    out.push(b'"');
    for &byte in prefix.iter().chain(path) {
        match byte {
            b'\x07' => out.extend(b"\\a"),
            b'\x08' => out.extend(b"\\b"),
            b'\t' => out.extend(b"\\t"),
            b'\n' => out.extend(b"\\n"),
            b'\x0b' => out.extend(b"\\v"),
            b'\x0c' => out.extend(b"\\f"),
            b'\r' => out.extend(b"\\r"),
            b'"' => out.extend(b"\\\""),
            b'\\' => out.extend(b"\\\\"),
            byte if plain(byte) => out.push(byte),
            byte => out.extend(format!("\\{byte:03o}").bytes()),
        }
    }
    out.push(b'"');
}

/// The share's directory, for the line that names it.
pub fn root_line(root: &Path) -> Vec<u8> {
    let mut out = b"share ".to_vec();
    quote(&mut out, b"", root.as_os_str().as_bytes());
    out.push(b'\n');
    out
}

/// The line that counts the git-ignored files `ap diff` left out.
pub fn ignored_line(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("skipped 1 git-ignored file\n".to_owned()),
        n => Some(format!("skipped {n} git-ignored files\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_back_in_order() {
        let text = concat!(
            "BASE \"src/main.rs\" 1234 210 1794567729.123456789 1794567729.000000001\n",
            "FILE \"src/main.rs\" 644\n",
            "DIR \"src/new\" 755\n",
            "DIR \"build\" 755 opaque\n",
            "SYMLINK \"latest\" \"build/out\"\n",
            "FIFO \"pipe\" 644\n",
            "CHAR \"null\" 666 1 3\n",
            "BASE \"notes.txt\" none\n",
            "FILE \"notes.txt\" 600\n",
            "FILE \"src/new/a\" 600\n",
            "DELETE \"src/new\"\n",
            "FILE \"say \\\"hi\\\"\\\\\\x0a\\xff\" 600\n",
            "FILE \"half",
        );
        let records = Records::read(text.as_bytes()).unwrap();
        let entries: Vec<(&[u8], &Entry)> = (records.entries.iter())
            .map(|(path, entry)| (path.as_slice(), entry))
            .collect();
        assert_eq!(
            entries,
            [
                (
                    &b"build"[..],
                    &Entry::Dir {
                        mode: 0o755,
                        opaque: true
                    }
                ),
                (
                    b"latest",
                    &Entry::Symlink {
                        target: b"build/out".to_vec()
                    }
                ),
                (b"notes.txt", &Entry::File { mode: 0o600 }),
                (
                    b"null",
                    &Entry::Special {
                        kind: "CHAR".to_owned()
                    }
                ),
                (
                    b"pipe",
                    &Entry::Special {
                        kind: "FIFO".to_owned()
                    }
                ),
                (b"say \"hi\"\\\n\xff", &Entry::File { mode: 0o600 }),
                (b"src/main.rs", &Entry::File { mode: 0o644 }),
                (b"src/new", &Entry::Deleted),
            ]
        );
        assert_eq!(
            records.bases[&b"src/main.rs"[..]],
            Some(Base {
                ino: 1234,
                size: 210,
                mtime: 1794567729123456789,
                ctime: 1794567729000000001,
            })
        );
        assert_eq!(records.bases[&b"notes.txt"[..]], None);
        for bad in [
            "FILE \"a\"\n",
            "FILE \"a\" 9\n",
            "FILE \"../a\" 644\n",
            "FILE \"a//b\" 644\n",
            "FILE \"/a\" 644\n",
            "FILE \"a\" 644 more\n",
            "DIR \"a\" 755 shut\n",
            "MOVE \"a\" 644\n",
            "BASE \"a\" 1 2 3 4\n",
            "SYMLINK \"a\" \"b\\q\"\n",
        ] {
            assert!(Records::read(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn records_that_apply_keeps_read_back_as_they_were() {
        let base = Base {
            ino: 7,
            size: 3,
            mtime: -4_999_999_900,
            ctime: 1_794_567_729_000_000_001,
        };
        let entries = [
            (&b"a file"[..], Entry::File { mode: 0o755 }),
            (
                b"dir",
                Entry::Dir {
                    mode: 0o700,
                    opaque: true,
                },
            ),
            (
                b"say \"hi\"\\\n\xff",
                Entry::Symlink {
                    target: b"t\x01\"".to_vec(),
                },
            ),
            (
                b"null",
                Entry::Special {
                    kind: "CHAR".to_owned(),
                },
            ),
            (
                b"pipe",
                Entry::Special {
                    kind: "FIFO".to_owned(),
                },
            ),
            (b"gone", Entry::Deleted),
        ];
        let mut out = Vec::new();
        base_record(&mut out, b"a file", Some(&base));
        base_record(&mut out, b"gone", None);
        for (path, entry) in &entries {
            entry_record(&mut out, path, entry);
        }
        let records = Records::read(&out).unwrap();
        for (path, entry) in &entries {
            assert_eq!(records.entries.get(*path), Some(entry), "{path:?}");
        }
        assert_eq!(records.entries.len(), entries.len());
        assert_eq!(records.bases[&b"a file"[..]], Some(base));
        assert_eq!(records.bases[&b"gone"[..]], None);
    }

    #[test]
    fn a_symlink_stays_inside_only_if_its_target_does() {
        let mut records = Records::default();
        records.entries.insert(
            b"hop".to_vec(),
            Entry::Symlink {
                target: b"/".to_vec(),
            },
        );
        let host = Host { root: None };
        let inside =
            |path: &[u8], target: &[u8]| leaves(&records, &host, path, target).unwrap().is_none();
        assert!(inside(b"latest", b"build/out"));
        assert!(inside(b"a/b/link", b"../../c"));
        assert!(inside(b"a/link", b"./x/../y"));
        assert!(!inside(b"a/b/link", b"../../../c"));
        assert!(!inside(b"link", b"/etc/passwd"));
        assert!(!inside(b"link", b".."));
        assert!(!inside(b"link", b""));
        // Through the agent's own symlink, which could lead anywhere.
        assert!(!inside(b"link", b"hop/etc"));
        assert!(!inside(b"link", b"hop/../etc"));
    }

    #[test]
    fn a_path_is_quoted_as_git_quotes_it() {
        let quoted = |path: &[u8]| {
            let mut out = Vec::new();
            quote(&mut out, b"a/", path);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(quoted(b"src/main.rs"), "a/src/main.rs");
        assert_eq!(quoted(b"a b"), "a/a b");
        assert_eq!(quoted(b"tab\there"), "\"a/tab\\there\"");
        assert_eq!(quoted("é".as_bytes()), "\"a/\\303\\251\"");
        assert_eq!(quoted(b"q\"\\"), "\"a/q\\\"\\\\\"");
    }
}
