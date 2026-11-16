//! What a run's agent changed in its copy-on-write shares, read from the
//! pending directory that the share server writes, and shown as `ap diff`
//! prints it, from SPEC.md's `SHARE` section.
//!
//! Each share's `records` say what each path the agent changed is now, and
//! its `tree` holds the agent's regular files. The share server's `cow`
//! module describes both. `ap diff` compares that with your files as they
//! are now, and never follows a symlink of the agent's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

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
        if !valid_path(&path) {
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
    Special(String),
}

/// A file's mode as git records it: executable by you or not.
fn git_mode(mode: u32) -> u32 {
    match mode & 0o100 {
        0 => REGULAR,
        _ => EXECUTABLE,
    }
}

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
    pub fn diff(&self) -> io::Result<Shown> {
        let text = match fs::read(self.dir.join("records")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let records = Records::read(&text).map_err(io::Error::other)?;
        let ignore = match fs::symlink_metadata(self.root) {
            Ok(metadata) if metadata.is_dir() => Gitignore::load(self.root)?,
            _ => Gitignore::default(),
        };
        let repository = fs::symlink_metadata(self.root.join(".git")).is_ok_and(|m| m.is_dir());
        let mut shown = Shown {
            text: Vec::new(),
            ignored: 0,
        };
        for path in self.changed(&records)? {
            let names: Vec<&[u8]> = path.split(|&byte| byte == b'/').collect();
            // The repository's own history, which the branch carries.
            if repository && names[0] == b".git" {
                continue;
            }
            let yours = self.yours(&path)?;
            let theirs = match records.view(&path) {
                View::Agent(Entry::File { mode }) => Theirs::File {
                    mode: *mode,
                    data: fs::read(self.dir.join("tree").join(bytes_path(&path)))?,
                },
                View::Agent(Entry::Symlink { target }) => Theirs::Symlink(target.clone()),
                View::Agent(Entry::Special { kind }) => Theirs::Special(kind.clone()),
                _ => Theirs::None,
            };
            if ignore.is_ignored(bytes_path(&path), false) {
                shown.ignored += 1;
                continue;
            }
            let out = &mut shown.text;
            let skip = |out: &mut Vec<u8>, why: &str| {
                out.extend(b"skipped ");
                quote(out, b"", &path);
                out.extend(format!(": {why}\n").bytes());
            };
            if names.iter().any(|name| name.eq_ignore_ascii_case(b".git")) {
                skip(out, "it is under .git");
                continue;
            }
            if self.changed_on_your_side(&records, &path)? {
                skip(
                    out,
                    "it changed on your side after the agent first changed it",
                );
                continue;
            }
            match theirs {
                Theirs::Special(kind) => {
                    let kind = kind.to_ascii_lowercase();
                    skip(
                        out,
                        &format!("it is a {kind}, and special files are never applied"),
                    );
                    continue;
                }
                Theirs::Symlink(target) if !inside(&path, &target) => {
                    skip(out, "it is a symlink whose target leaves the share");
                    continue;
                }
                _ => {}
            }
            render(out, &path, yours, theirs);
        }
        Ok(shown)
    }

    /// Every path whose file differs between yours and the agent's view, in
    /// order.
    fn changed(&self, records: &Records) -> io::Result<BTreeSet<Vec<u8>>> {
        let mut paths = BTreeSet::new();
        for (path, entry) in &records.entries {
            match entry {
                Entry::File { .. } | Entry::Symlink { .. } | Entry::Special { .. } => {
                    if matches!(records.view(path), View::Agent(_)) {
                        paths.insert(path.clone());
                    }
                }
                Entry::Deleted | Entry::Dir { opaque: true, .. } => {
                    self.yours_beneath(path, &mut |yours| {
                        if matches!(records.view(&yours), View::Absent) {
                            paths.insert(yours);
                        }
                    })?;
                }
                Entry::Dir { .. } => {}
            }
        }
        Ok(paths)
    }

    /// Calls `found` with the path of each of your files at or beneath
    /// `path` that isn't a directory, without following a symlink.
    fn yours_beneath(&self, path: &[u8], found: &mut dyn FnMut(Vec<u8>)) -> io::Result<()> {
        let host = self.root.join(bytes_path(path));
        let metadata = match fs::symlink_metadata(&host) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            found(path.to_vec());
            return Ok(());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&host)? {
            names.push(entry?.file_name().as_bytes().to_vec());
        }
        names.sort();
        for name in names {
            self.yours_beneath(&[path, b"/", &name].concat(), found)?;
        }
        Ok(())
    }

    /// Your file at `path` now.
    fn yours(&self, path: &[u8]) -> io::Result<Yours> {
        let host = self.root.join(bytes_path(path));
        let metadata = match fs::symlink_metadata(&host) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Yours::None),
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => return Ok(Yours::None),
            Err(error) => return Err(error),
        };
        Ok(if metadata.is_symlink() {
            Yours::Symlink(fs::read_link(&host)?.as_os_str().as_bytes().to_vec())
        } else if metadata.is_file() {
            Yours::File {
                mode: metadata.mode(),
                data: fs::read(&host)?,
            }
        } else {
            Yours::Other
        })
    }

    /// Whether your file at `path` changed since the agent first changed
    /// it, by the record the share server kept of it then.
    fn changed_on_your_side(&self, records: &Records, path: &[u8]) -> io::Result<bool> {
        let Some(base) = records.bases.get(path) else {
            return Ok(false);
        };
        let now = match fs::symlink_metadata(self.root.join(bytes_path(path))) {
            Ok(metadata) => Some(Base {
                ino: metadata.ino(),
                size: metadata.size(),
                mtime: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
                ctime: i128::from(metadata.ctime()) * 1_000_000_000
                    + i128::from(metadata.ctime_nsec()),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => None,
            Err(error) => return Err(error),
        };
        Ok(now != *base)
    }
}

/// Whether a symlink at `path` with this target stays inside the share,
/// read as the guest would read it, with no symlink beneath it followed.
fn inside(path: &[u8], target: &[u8]) -> bool {
    if target.starts_with(b"/") || target.is_empty() {
        return false;
    }
    let mut depth = path.split(|&byte| byte == b'/').count() - 1;
    for name in target.split(|&byte| byte == b'/') {
        match name {
            b"" | b"." => {}
            b".." if depth == 0 => return false,
            b".." => depth -= 1,
            _ => depth += 1,
        }
    }
    true
}

/// Prints one path's change: a unified diff for a text file, and one line
/// for a binary file, a mode change, or a symlink.
fn render(out: &mut Vec<u8>, path: &[u8], yours: Yours, theirs: Theirs) {
    let yours = match yours {
        Yours::Symlink(target) => {
            if !matches!(&theirs, Theirs::Symlink(new) if *new == target) {
                out.extend(b"delete symlink ");
                quote(out, b"", path);
                out.push(b'\n');
            }
            None
        }
        Yours::File { mode, data } => Some((git_mode(mode), data)),
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
        Theirs::File { mode, data } => Some((git_mode(mode), data)),
        Theirs::None | Theirs::Special(_) => None,
    };
    let (old, new) = match (yours, theirs) {
        (None, None) => return,
        (Some((old_mode, old)), Some((new_mode, new))) => {
            if old_mode != new_mode {
                out.extend(format!("mode change {old_mode:o} => {new_mode:o} ").bytes());
                quote(out, b"", path);
                out.push(b'\n');
            }
            if old == new {
                return;
            }
            (Some((old_mode, old)), Some((new_mode, new)))
        }
        (old, new) => (old, new),
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
    fn a_symlink_stays_inside_only_if_its_target_does() {
        assert!(inside(b"latest", b"build/out"));
        assert!(inside(b"a/b/link", b"../../c"));
        assert!(inside(b"a/link", b"./x/../y"));
        assert!(!inside(b"a/b/link", b"../../../c"));
        assert!(!inside(b"link", b"/etc/passwd"));
        assert!(!inside(b"link", b".."));
        assert!(!inside(b"link", b""));
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
