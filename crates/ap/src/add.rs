//! Turns what an `ADD` copies into a layer, by SPEC.md's `### ADD`: the
//! layer's key, and its EROFS image.
//!
//! Every file is read once, into memory, so the key and the image come from
//! the same bytes. A directory is walked through the handles it opens, one
//! name at a time, and a symlink inside it is never followed.

use std::collections::BTreeMap;
use std::ffi::{CStr, OsStr};
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use agentfile::Add;
use graviola::hashing::sha2::Sha256Context;

use crate::gitignore::Gitignore;
use crate::paths::HostPath;

/// The mode of a directory the guest path needs above the content.
const PARENT: u16 = 0o755;
/// The permission bits a layer keeps. Every file in an image belongs to
/// root, so a setuid bit would give root to whatever the host user wrote.
const PERMISSIONS: u16 = 0o777;

/// What one `ADD` puts in the image.
pub struct Layer {
    /// The SHA-256 of the instruction, its arguments as written, the guest
    /// path, and every name, mode, symlink target, and file hash it read,
    /// in lowercase hex. Times and owners don't count.
    pub key: String,
    /// One line for each special file that was skipped.
    pub warnings: Vec<String>,
    root: BTreeMap<Vec<u8>, Entry>,
}

enum Entry {
    Dir(u16, BTreeMap<Vec<u8>, Entry>),
    File(u16, Vec<u8>),
    Symlink(Vec<u8>),
}

impl Layer {
    /// Reads what `add` copies, from `host`, which the caller resolved from
    /// `add.source`, to land at `guest`, which the parser resolved. Only the
    /// handle that `host` opens is read, so a symlink swapped in after the
    /// check fails instead of being followed.
    pub fn read(add: &Add, host: &HostPath, guest: &str) -> io::Result<Layer> {
        let names: Vec<&str> = guest.split('/').filter(|name| !name.is_empty()).collect();
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an `ADD` can't land at `/`",
            ));
        };
        let mut walk = Walk {
            host,
            ignore: None,
            ignores: Vec::new(),
            warnings: Vec::new(),
        };
        let content = walk.top()?;

        let mut root = BTreeMap::new();
        if let Some(content) = content {
            root.insert(last.as_bytes().to_vec(), content);
        }
        for name in parents.iter().rev() {
            let dir = Entry::Dir(PARENT, root);
            root = BTreeMap::from([(name.as_bytes().to_vec(), dir)]);
        }

        let mut hash = Sha256Context::new();
        field(&mut hash, b"ADD");
        field(&mut hash, add.source.as_bytes());
        match &add.destination {
            Some(destination) => {
                hash.update(&[1]);
                field(&mut hash, destination.as_bytes());
            }
            None => hash.update(&[0]),
        }
        field(&mut hash, guest.as_bytes());
        manifest(&mut hash, &root);
        hash.update(&(walk.ignores.len() as u64).to_le_bytes());
        for (dir, content) in &walk.ignores {
            field(&mut hash, dir);
            field(&mut hash, content);
        }
        let key = hash.finish().iter().map(|b| format!("{b:02x}")).collect();

        Ok(Layer {
            key,
            warnings: walk.warnings,
            root,
        })
    }

    /// The layer as an uncompressed EROFS image.
    pub fn image(self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        erofs::write(dir(PARENT, self.root), &mut out).map_err(|error| match error {
            erofs::Error::Io(error) => error,
            error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })?;
        Ok(out)
    }
}

fn dir(mode: u16, entries: BTreeMap<Vec<u8>, Entry>) -> erofs::Dir {
    let entries = entries
        .into_iter()
        .map(|(name, entry)| {
            let node = match entry {
                Entry::Dir(mode, entries) => erofs::Node::Dir(dir(mode, entries)),
                Entry::File(mode, data) => erofs::Node::File(erofs::File {
                    mode,
                    size: data.len() as u64,
                    content: Box::new(Cursor::new(data)),
                }),
                Entry::Symlink(target) => erofs::Node::Symlink(erofs::Symlink {
                    mode: 0o777,
                    target,
                }),
            };
            (name, node)
        })
        .collect();
    erofs::Dir { mode, entries }
}

/// Adds a field that can't run into the next one.
fn field(hash: &mut Sha256Context, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn manifest(hash: &mut Sha256Context, entries: &BTreeMap<Vec<u8>, Entry>) {
    hash.update(&(entries.len() as u64).to_le_bytes());
    for (name, entry) in entries {
        field(hash, name);
        match entry {
            Entry::Dir(mode, entries) => {
                hash.update(b"d");
                hash.update(&mode.to_le_bytes());
                manifest(hash, entries);
            }
            Entry::File(mode, data) => {
                hash.update(b"f");
                hash.update(&mode.to_le_bytes());
                let mut file = Sha256Context::new();
                file.update(data);
                hash.update(&file.finish());
            }
            Entry::Symlink(target) => {
                hash.update(b"l");
                field(hash, target);
            }
        }
    }
}

struct Walk<'a> {
    /// The `<src>`, whose canonical path is also for messages.
    host: &'a HostPath,
    ignore: Option<Gitignore>,
    /// Each `.gitignore` read, by the directory that holds it, so the key
    /// covers one that ignores itself.
    ignores: Vec<(Vec<u8>, Vec<u8>)>,
    warnings: Vec<String>,
}

impl Walk<'_> {
    /// What the `<src>` itself holds. A special file is skipped before it is
    /// opened, because a socket can't be, and opening a device can do
    /// something.
    fn top(&mut self) -> io::Result<Option<Entry>> {
        let kind = fs::symlink_metadata(self.host.path())
            .map_err(|error| self.error(b"", error))?
            .file_type();
        // A symlink here was swapped in after the check, and `open` refuses
        // it.
        if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() {
            self.skip(b"");
            return Ok(None);
        }
        let file = self.host.open()?;
        let stat = fstat(&file).map_err(|error| self.error(b"", error))?;
        let content = match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                let data = read_all(file, &stat).map_err(|error| self.error(b"", error))?;
                Some(Entry::File(permissions(&stat), data))
            }
            libc::S_IFDIR => {
                // A file named explicitly is copied, so only a directory
                // reads the `.gitignore` files.
                self.ignore = Some(Gitignore::default());
                Some(self.dir(&file, permissions(&stat), &mut Vec::new(), false)?)
            }
            _ => {
                self.skip(b"");
                None
            }
        };
        Ok(content)
    }

    /// The entries of the directory open as `dir`, which is `rel` below the
    /// top. Like git, it reads no `.gitignore` inside a `.git`.
    fn dir(&mut self, dir: &File, mode: u16, rel: &mut Vec<u8>, in_git: bool) -> io::Result<Entry> {
        if !in_git && self.ignore.is_some() {
            self.gitignore(dir, rel)?;
        }
        let mut entries = BTreeMap::new();
        for name in list(dir).map_err(|error| self.error(rel, error))? {
            let len = rel.len();
            if !rel.is_empty() {
                rel.push(b'/');
            }
            rel.extend_from_slice(name.to_bytes());
            // An error carries the path where it happened.
            let entry = self.entry(dir, &name, rel, in_git);
            rel.truncate(len);
            if let Some(entry) = entry? {
                entries.insert(name.to_bytes().to_vec(), entry);
            }
        }
        Ok(Entry::Dir(mode, entries))
    }

    fn entry(
        &mut self,
        dir: &File,
        name: &CStr,
        rel: &mut Vec<u8>,
        in_git: bool,
    ) -> io::Result<Option<Entry>> {
        let stat = stat_at(dir, name).map_err(|error| self.error(rel, error))?;
        let kind = stat.st_mode & libc::S_IFMT;
        let is_dir = kind == libc::S_IFDIR;
        if let Some(ignore) = &self.ignore
            && ignore.is_ignored(Path::new(OsStr::from_bytes(rel)), is_dir)
        {
            return Ok(None);
        }
        match kind {
            libc::S_IFDIR => {
                let child = open_at(dir, name, libc::O_DIRECTORY)
                    .and_then(|child| Ok((same(&child, &stat)?, child)));
                let (stat, child) = child.map_err(|error| self.error(rel, error))?;
                let in_git = in_git || name.to_bytes() == b".git";
                self.dir(&child, permissions(&stat), rel, in_git).map(Some)
            }
            libc::S_IFREG => {
                let read = || {
                    let child = open_at(dir, name, libc::O_NONBLOCK | libc::O_NOCTTY)?;
                    let stat = same(&child, &stat)?;
                    let data = read_all(child, &stat)?;
                    Ok(Entry::File(permissions(&stat), data))
                };
                read().map(Some).map_err(|error| self.error(rel, error))
            }
            libc::S_IFLNK => read_link(dir, name, &stat)
                .map(|target| Some(Entry::Symlink(target)))
                .map_err(|error| self.error(rel, error)),
            _ => {
                self.skip(rel);
                Ok(None)
            }
        }
    }

    /// Reads the `.gitignore` in `dir`, if it is a regular file.
    fn gitignore(&mut self, dir: &File, rel: &[u8]) -> io::Result<()> {
        let name = c".gitignore";
        let read = || {
            let stat = match stat_at(dir, name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                stat => stat?,
            };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Ok(None);
            }
            let file = open_at(dir, name, libc::O_NONBLOCK | libc::O_NOCTTY)?;
            let stat = same(&file, &stat)?;
            read_all(file, &stat).map(Some)
        };
        let mut path = rel.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name.to_bytes());
        let content = read().map_err(|error| self.error(&path, error))?;
        if let Some(content) = content {
            if let Some(ignore) = &mut self.ignore {
                ignore.add(Path::new(OsStr::from_bytes(rel)), &content);
            }
            self.ignores.push((rel.to_vec(), content));
        }
        Ok(())
    }

    fn skip(&mut self, rel: &[u8]) {
        self.warnings.push(format!(
            "skipped {}, which is a special file",
            self.show(rel)
        ));
    }

    fn error(&self, rel: &[u8], error: io::Error) -> io::Error {
        io::Error::new(error.kind(), format!("{}: {error}", self.show(rel)))
    }

    fn show(&self, rel: &[u8]) -> String {
        if rel.is_empty() {
            self.host.path().display().to_string()
        } else {
            self.host
                .path()
                .join(OsStr::from_bytes(rel))
                .display()
                .to_string()
        }
    }
}

fn permissions(stat: &libc::stat) -> u16 {
    stat.st_mode as u16 & PERMISSIONS
}

/// Opens `name` in `dir` for reading, refusing a symlink.
fn open_at(dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let flags = flags | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: the descriptor is open, and the name is a C string.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat_at(dir: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is open, the name is a C string, and `stat`
    // has room for the result.
    let done = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatat` succeeded, so it filled `stat`.
    Ok(unsafe { stat.assume_init() })
}

fn fstat(file: &File) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is open, and `stat` has room for the result.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded, so it filled `stat`.
    Ok(unsafe { stat.assume_init() })
}

/// The status of `file`, if it is still the entry `before` described.
fn same(file: &File, before: &libc::stat) -> io::Result<libc::stat> {
    let stat = fstat(file)?;
    if (stat.st_dev, stat.st_ino) != (before.st_dev, before.st_ino) {
        return Err(io::Error::other("it changed while it was read"));
    }
    Ok(stat)
}

/// The whole content of a regular file, which must not change size while
/// it is read.
fn read_all(mut file: File, stat: &libc::stat) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    if data.len() as u64 != stat.st_size as u64 {
        return Err(io::Error::other("it changed while it was read"));
    }
    Ok(data)
}

fn read_link(dir: &File, name: &CStr, stat: &libc::stat) -> io::Result<Vec<u8>> {
    // One byte more than the size, to tell a target that grew.
    let mut target = vec![0u8; stat.st_size as usize + 1];
    // SAFETY: the descriptor is open, the name is a C string, and the
    // buffer holds `target.len()` bytes.
    let len = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != stat.st_size as usize {
        return Err(io::Error::other("it changed while it was read"));
    }
    target.truncate(len as usize);
    Ok(target)
}

/// The names in `dir`, without `.` and `..`.
fn list(dir: &File) -> io::Result<Vec<std::ffi::CString>> {
    // SAFETY: the descriptor is open. `fdopendir` takes the copy.
    let fd = unsafe { libc::dup(dir.as_raw_fd()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new descriptor of a directory that nothing else owns.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let mut names = Vec::new();
    let result = loop {
        // SAFETY: `errno` is this thread's, and `readdir` sets it only on
        // an error.
        unsafe { *errno() = 0 };
        // SAFETY: `stream` is open until `closedir` below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            // SAFETY: as above.
            let code = unsafe { *errno() };
            break if code == 0 {
                Ok(names)
            } else {
                Err(io::Error::from_raw_os_error(code))
            };
        }
        // SAFETY: `readdir` returned an entry with a NUL-terminated name,
        // valid until the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    };
    // SAFETY: `stream` is open, and isn't used again.
    unsafe { libc::closedir(stream) };
    result
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: always safe to call.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: always safe to call.
    unsafe { libc::__error() }
}
