//! Reads a git repository the way `git` does: loose objects, and refs with
//! `packed-refs`. It reads only SHA-1 repositories.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use miniz_oxide::inflate::{self, TINFLStatus};

/// An object's SHA-1 name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Id(pub [u8; 20]);

impl Id {
    /// Parses 40 hex digits, in either case.
    pub fn from_hex(hex: &str) -> Option<Id> {
        let hex = hex.as_bytes();
        if hex.len() != 40 {
            return None;
        }
        let mut id = [0; 20];
        for (byte, pair) in id.iter_mut().zip(hex.chunks(2)) {
            *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Some(Id(id))
    }
}

fn hex_id(bytes: &[u8]) -> Option<Id> {
    Id::from_hex(std::str::from_utf8(bytes).ok()?)
}

fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// An object's type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        })
    }
}

/// An object's type and contents.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// What a ref points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Object(Id),
    /// Another ref, by its full name.
    Symbolic(String),
}

/// Why a read failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The repository holds something git wouldn't read.
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(error) => error.fmt(f),
            Error::Corrupt(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

/// A repository, by its git directory.
pub struct Repository {
    git_dir: PathBuf,
}

/// How many symbolic refs `resolve` follows, as git's `SYMREF_MAXDEPTH`.
const MAX_DEPTH: usize = 5;

/// The longest header a loose object can have: `commit `, 20 digits, and NUL.
const MAX_HEADER: usize = 28;

fn corrupt(why: impl Into<String>) -> Error {
    Error::Corrupt(why.into())
}

/// The file at `path`, or `None` if there is none.
fn read(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        // A directory where a ref would be, such as `refs/heads/a` beside
        // `refs/heads/a/b`, means there is no such ref.
        Err(e) if e.kind() == io::ErrorKind::IsADirectory => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Repository {
    /// The repository whose git directory is `git_dir`, such as `.git`.
    pub fn at(git_dir: impl Into<PathBuf>) -> Repository {
        Repository {
            git_dir: git_dir.into(),
        }
    }

    /// The loose object named `id`, or `None` if there is no loose object.
    pub fn loose_object(&self, id: &Id) -> Result<Option<Object>, Error> {
        let hex = id.to_string();
        let path = self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        let Some(compressed) = read(&path)? else {
            return Ok(None);
        };
        // Inflate only the header first, so the size it declares bounds the
        // rest, and a small file can't inflate without limit.
        let prefix = match inflate::decompress_to_vec_zlib_with_limit(&compressed, MAX_HEADER) {
            Ok(all) => all,
            Err(e) if e.status == TINFLStatus::HasMoreOutput => e.output,
            Err(_) => return Err(corrupt(format!("{hex} isn't zlib"))),
        };
        let (kind, size, header) =
            parse_header(&prefix).ok_or_else(|| corrupt(format!("{hex} has a bad header")))?;
        let total = header
            .checked_add(size)
            .ok_or_else(|| corrupt(format!("{hex} is too large")))?;
        let mut data = match inflate::decompress_to_vec_zlib_with_limit(&compressed, total) {
            Ok(all) => all,
            Err(e) if e.status == TINFLStatus::HasMoreOutput => {
                return Err(corrupt(format!("{hex} is longer than its header says")));
            }
            Err(_) => return Err(corrupt(format!("{hex} isn't zlib"))),
        };
        if data.len() != total {
            return Err(corrupt(format!("{hex} is shorter than its header says")));
        }
        data.drain(..header);
        Ok(Some(Object { kind, data }))
    }

    /// The ref named `name`, which is `HEAD` or a full name under `refs/`,
    /// from its loose file or else from `packed-refs`.
    pub fn find_ref(&self, name: &str) -> Result<Option<Target>, Error> {
        check_name(name)?;
        if let Some(target) = self.loose_ref(name)? {
            return Ok(Some(target));
        }
        Ok(self.packed_refs()?.remove(name).map(Target::Object))
    }

    /// The object that `name` points at, through any symbolic refs.
    pub fn resolve(&self, name: &str) -> Result<Option<Id>, Error> {
        let mut name = name.to_string();
        for _ in 0..MAX_DEPTH {
            match self.find_ref(&name)? {
                None => return Ok(None),
                Some(Target::Object(id)) => return Ok(Some(id)),
                Some(Target::Symbolic(next)) => name = next,
            }
        }
        Err(corrupt(format!(
            "{name} is too deep a chain of symbolic refs"
        )))
    }

    /// Every ref under `refs/`, loose and packed, sorted by name. Like `git
    /// for-each-ref`, it leaves out a loose ref it can't read. A ref whose
    /// name isn't UTF-8 is left out too.
    pub fn refs(&self) -> Result<Vec<(String, Target)>, Error> {
        let mut refs: BTreeMap<String, Target> = self
            .packed_refs()?
            .into_iter()
            .map(|(name, id)| (name, Target::Object(id)))
            .collect();
        let mut loose = Vec::new();
        self.loose_refs("refs", &mut loose)?;
        for name in loose {
            match self.loose_ref(&name) {
                Ok(Some(target)) => {
                    refs.insert(name, target);
                }
                // A broken loose ref still hides the packed one.
                Ok(None) | Err(Error::Corrupt(_)) => {
                    refs.remove(&name);
                }
                Err(error) => return Err(error),
            }
        }
        // A ref that leads nowhere, or to the null id, is left out, as git
        // leaves it out.
        let mut listed = Vec::new();
        for (name, target) in refs {
            let id = match &target {
                Target::Object(id) => *id,
                Target::Symbolic(_) => match self.resolve(&name) {
                    Ok(Some(id)) => id,
                    Ok(None) | Err(Error::Corrupt(_)) => continue,
                    Err(error) => return Err(error),
                },
            };
            if id != Id([0; 20]) {
                listed.push((name, target));
            }
        }
        Ok(listed)
    }

    /// The loose ref `name`, or `None` if it has no file. A symlink is a
    /// symbolic ref, as git once wrote them, and must point under `refs/`.
    fn loose_ref(&self, name: &str) -> Result<Option<Target>, Error> {
        let path = self.git_dir.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if metadata.is_symlink() {
            let link = fs::read_link(&path)?;
            let target = link
                .to_str()
                .filter(|target| check_name(target).is_ok() && *target != "HEAD")
                .ok_or_else(|| corrupt(format!("{name} links outside refs/")))?;
            return Ok(Some(Target::Symbolic(target.to_string())));
        }
        match read(&path)? {
            Some(contents) => parse_loose_ref(name, &contents).map(Some),
            None => Ok(None),
        }
    }

    /// The name of every loose ref under `dir`. A file whose name git
    /// wouldn't take as a ref, such as a lock, isn't one.
    fn loose_refs(&self, dir: &str, out: &mut Vec<String>) -> Result<(), Error> {
        let entries = match fs::read_dir(self.git_dir.join(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(file) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let name = format!("{dir}/{file}");
            if entry.file_type()?.is_dir() {
                self.loose_refs(&name, out)?;
            } else if entry.file_type()?.is_file() && check_name(&name).is_ok() {
                // Like `git for-each-ref`, it lists no symlink.
                out.push(name);
            }
        }
        Ok(())
    }

    /// The refs in `packed-refs`, by name. Like git, it refuses a last
    /// line with no newline, and a `^` line that doesn't follow a ref.
    fn packed_refs(&self) -> Result<BTreeMap<String, Id>, Error> {
        let mut refs = BTreeMap::new();
        let Some(contents) = read(&self.git_dir.join("packed-refs"))? else {
            return Ok(refs);
        };
        let Some(body) = contents.strip_suffix(b"\n") else {
            return match contents.is_empty() {
                true => Ok(refs),
                false => Err(corrupt("packed-refs ends without a newline")),
            };
        };
        // Whether the line before was a ref, which a `^` line may peel.
        let mut after_ref = false;
        for (n, line) in body.split(|&b| b == b'\n').enumerate() {
            let bad = || corrupt(format!("packed-refs has a bad line {}", n + 1));
            if n == 0 && line.starts_with(b"# pack-refs with: ") {
                continue;
            }
            if let Some(peeled) = line.strip_prefix(b"^") {
                if !after_ref || hex_id(peeled).is_none() {
                    return Err(bad());
                }
                after_ref = false;
                continue;
            }
            if line.len() < 41 || line[40] != b' ' {
                return Err(bad());
            }
            let id = hex_id(&line[..40]).ok_or_else(bad)?;
            after_ref = true;
            // A name that isn't UTF-8 can't be asked for, so it is left out.
            let Ok(name) = std::str::from_utf8(&line[41..]) else {
                continue;
            };
            check_name(name).map_err(|_| bad())?;
            refs.insert(name.to_string(), id);
        }
        Ok(refs)
    }
}

/// The type, the declared size, and the header's length, NUL included, of
/// `<type> <size>\0`. Like git, it takes no leading zero in the size.
fn parse_header(bytes: &[u8]) -> Option<(Kind, usize, usize)> {
    let nul = bytes.iter().position(|&b| b == 0)?;
    let header = std::str::from_utf8(&bytes[..nul]).ok()?;
    let (kind, size) = header.split_once(' ')?;
    let kind = match kind {
        "commit" => Kind::Commit,
        "tree" => Kind::Tree,
        "blob" => Kind::Blob,
        "tag" => Kind::Tag,
        _ => return None,
    };
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if size.len() > 1 && size.starts_with('0') {
        return None;
    }
    Some((kind, size.parse().ok()?, nul + 1))
}

/// The bytes git's `isspace` takes as whitespace.
fn is_git_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// What a loose ref file says: 40 hex digits, or `ref:` and another ref.
fn parse_loose_ref(name: &str, contents: &[u8]) -> Result<Target, Error> {
    let bad = || corrupt(format!("{name} isn't a ref"));
    // Git reads the file as a C string, so it ends at the first NUL.
    let contents = match contents.iter().position(|&b| b == 0) {
        Some(nul) => &contents[..nul],
        None => contents,
    };
    if let Some(target) = contents.strip_prefix(b"ref:") {
        let start = target.iter().position(|&b| !is_git_space(b));
        let end = target.iter().rposition(|&b| !is_git_space(b));
        let (Some(start), Some(end)) = (start, end) else {
            return Err(bad());
        };
        let target = std::str::from_utf8(&target[start..=end]).map_err(|_| bad())?;
        check_name(target)?;
        return Ok(Target::Symbolic(target.to_string()));
    }
    // Git reads the id, and takes anything after it that starts with
    // whitespace.
    let id = contents.get(..40).and_then(hex_id).ok_or_else(bad)?;
    match contents.get(40) {
        None => Ok(Target::Object(id)),
        Some(&b) if is_git_space(b) => Ok(Target::Object(id)),
        Some(_) => Err(bad()),
    }
}

/// Refuses a name that isn't `HEAD` or a full name under `refs/` that `git
/// check-ref-format` accepts, so no name reaches outside the git directory.
fn check_name(name: &str) -> Result<(), Error> {
    if name == "HEAD" {
        return Ok(());
    }
    let bad = || corrupt(format!("{name:?} isn't a ref name"));
    if !name.starts_with("refs/") || name.ends_with('/') || name.ends_with('.') {
        return Err(bad());
    }
    if name.contains("..") || name.contains("@{") {
        return Err(bad());
    }
    let forbidden = |c: char| c.is_ascii_control() || " ~^:?*[\\".contains(c);
    if name.chars().any(forbidden) {
        return Err(bad());
    }
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(bad());
        }
    }
    Ok(())
}
