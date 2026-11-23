//! Reads a git repository the way `git` does: loose and packed objects, and
//! refs with `packed-refs`. It writes loose objects. It reads and writes only
//! SHA-1 repositories.

mod pack;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use miniz_oxide::deflate;
use miniz_oxide::inflate::{self, TINFLStatus};

use pack::Pack;

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

impl Object {
    /// The object's name, the SHA-1 of its header and contents.
    pub fn id(&self) -> Id {
        let mut hash = sha1_smol::Sha1::new();
        hash.update(&self.header());
        hash.update(&self.data);
        Id(hash.digest().bytes())
    }

    /// `<type> <size>\0`, which comes before the contents.
    fn header(&self) -> Vec<u8> {
        format!("{} {}\0", self.kind, self.data.len()).into_bytes()
    }

    /// The tree and parents a commit names, from the lines that start it,
    /// as git's `parse_commit_buffer` reads them.
    pub fn commit(&self) -> Result<Commit, Error> {
        let bad = || corrupt("a commit has a bad header");
        if self.kind != Kind::Commit {
            return Err(corrupt(format!("a {} isn't a commit", self.kind)));
        }
        let mut lines = self.data.split(|&b| b == b'\n');
        let tree = (lines.next().and_then(|line| line.strip_prefix(b"tree ")))
            .and_then(hex_id)
            .ok_or_else(bad)?;
        let mut parents = Vec::new();
        for line in lines {
            match line.strip_prefix(b"parent ") {
                Some(hex) => parents.push(hex_id(hex).ok_or_else(bad)?),
                None => break,
            }
        }
        Ok(Commit { tree, parents })
    }

    /// The entries of a tree, in its order.
    pub fn entries(&self) -> Result<Vec<TreeEntry>, Error> {
        let bad = || corrupt("a tree has a bad entry");
        if self.kind != Kind::Tree {
            return Err(corrupt(format!("a {} isn't a tree", self.kind)));
        }
        let mut entries = Vec::new();
        let mut rest = &self.data[..];
        while !rest.is_empty() {
            let space = rest.iter().position(|&b| b == b' ').ok_or_else(bad)?;
            let mode = &rest[..space];
            if mode.is_empty() || !mode.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return Err(bad());
            }
            let mode = u32::from_str_radix(std::str::from_utf8(mode).map_err(|_| bad())?, 8)
                .map_err(|_| bad())?;
            rest = &rest[space + 1..];
            let nul = rest.iter().position(|&b| b == 0).ok_or_else(bad)?;
            let name = rest[..nul].to_vec();
            let id = rest.get(nul + 1..nul + 21).ok_or_else(bad)?;
            entries.push(TreeEntry {
                mode,
                name,
                id: Id(id.try_into().map_err(|_| bad())?),
            });
            rest = &rest[nul + 21..];
        }
        Ok(entries)
    }

    /// A tree of `entries`, in the order git keeps them: by name, where a
    /// tree's name ends in `/`.
    pub fn tree(mut entries: Vec<TreeEntry>) -> Object {
        entries.sort_by_cached_key(|entry| {
            let mut key = entry.name.clone();
            if entry.mode == 0o40000 {
                key.push(b'/');
            }
            key
        });
        let mut data = Vec::new();
        for entry in entries {
            data.extend(format!("{:o} ", entry.mode).bytes());
            data.extend(&entry.name);
            data.push(0);
            data.extend(entry.id.0);
        }
        Object {
            kind: Kind::Tree,
            data,
        }
    }

    /// A commit of `tree` with `parents`, whose author and committer are
    /// both `identity`, a name and an email in `<>`, at `time`, in seconds
    /// since the epoch in UTC, with `message`.
    pub fn new_commit(
        tree: Id,
        parents: &[Id],
        identity: &str,
        time: i64,
        message: &str,
    ) -> Object {
        let mut data = format!("tree {tree}\n");
        for parent in parents {
            data.push_str(&format!("parent {parent}\n"));
        }
        data.push_str(&format!("author {identity} {time} +0000\n"));
        data.push_str(&format!("committer {identity} {time} +0000\n\n{message}"));
        Object {
            kind: Kind::Commit,
            data: data.into_bytes(),
        }
    }

    /// The object a tag names.
    pub fn tag_target(&self) -> Result<Id, Error> {
        if self.kind != Kind::Tag {
            return Err(corrupt(format!("a {} isn't a tag", self.kind)));
        }
        (self.data.split(|&b| b == b'\n').next())
            .and_then(|line| line.strip_prefix(b"object "))
            .and_then(hex_id)
            .ok_or_else(|| corrupt("a tag has a bad header"))
    }
}

/// What a commit names in the lines that start it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Commit {
    pub tree: Id,
    pub parents: Vec<Id>,
}

/// One entry of a tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEntry {
    /// Its mode, such as `0o100644`, `0o40000` for a tree, or `0o160000`
    /// for a submodule's commit.
    pub mode: u32,
    pub name: Vec<u8>,
    pub id: Id,
}

/// What a ref points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Object(Id),
    /// Another ref, by its full name.
    Symbolic(String),
}

/// Why a read or a write failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The repository holds something git wouldn't read, or an object
    /// larger than the limit.
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
    /// The packs, found when an object is first looked for in one.
    packs: OnceLock<Vec<Pack>>,
    /// The most bytes of contents an object may hold.
    limit: usize,
}

/// How many symbolic refs `resolve` follows, as git's `SYMREF_MAXDEPTH`.
const MAX_DEPTH: usize = 5;

/// The longest header a loose object can have: `commit `, 20 digits, and NUL.
const MAX_HEADER: usize = 28;

fn corrupt(why: impl Into<String>) -> Error {
    Error::Corrupt(why.into())
}

/// Whether an error opening a path means nothing is there.
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory
    )
}

/// The type of the loose object at `path`, named `hex`, or `None` if there
/// is none. Only as much of the file as holds the header is read, a little
/// more each time, so a large object is never read whole.
fn loose_kind(path: &Path, hex: &str) -> Result<Option<Kind>, Error> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut compressed = Vec::new();
    let mut chunk = 256;
    loop {
        let len = match (&mut file).take(chunk).read_to_end(&mut compressed) {
            Ok(len) => len,
            Err(e) if is_missing(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let prefix = match inflate::decompress_to_vec_zlib_with_limit(&compressed, MAX_HEADER) {
            Ok(all) => all,
            Err(e) => e.output,
        };
        if let Some((kind, _, _)) = parse_header(&prefix) {
            return Ok(Some(kind));
        }
        if len == 0 {
            return Err(corrupt(format!("{hex} has a bad header")));
        }
        chunk *= 2;
    }
}

/// The file at `path`, or `None` if there is none.
fn read(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        // A directory where a ref would be, such as `refs/heads/a` beside
        // `refs/heads/a/b`, or a file above one, means there is no such ref.
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Repository {
    /// The repository whose git directory is `git_dir`, such as `.git`.
    pub fn at(git_dir: impl Into<PathBuf>) -> Repository {
        Repository {
            git_dir: git_dir.into(),
            packs: OnceLock::new(),
            limit: usize::MAX,
        }
    }

    /// The same repository, refusing as corrupt an object whose contents are
    /// larger than `limit` bytes. A loose object or a pack entry larger than
    /// that is refused before it is inflated past its header, and a delta
    /// whose result is larger is refused before it is applied. A chain of
    /// deltas is applied one delta at a time, so reading an object holds no
    /// more than its file or pack entry, its base, one delta, and itself.
    /// Without a limit, an object can be as large as its header says.
    pub fn with_limit(self, limit: usize) -> Repository {
        Repository { limit, ..self }
    }

    /// The object named `id`, loose or packed, or `None` if there is none.
    pub fn object(&self, id: &Id) -> Result<Option<Object>, Error> {
        match self.loose_object(id)? {
            Some(object) => Ok(Some(object)),
            None => self.packed_object(id),
        }
    }

    /// The object named `id` from the first pack that holds it, or `None`
    /// if no pack does. It finds the packs once, so a pack written after
    /// the first lookup needs a new `Repository`. A bad index or pack is an
    /// error, where git would skip it.
    pub fn packed_object(&self, id: &Id) -> Result<Option<Object>, Error> {
        for pack in self.packs()? {
            if let Some(offset) = pack.find(id)? {
                return pack.read(offset, self.limit).map(Some);
            }
        }
        Ok(None)
    }

    /// The type of the object named `id`, loose or packed, or `None` if
    /// there is none. It reads only a loose object's header, and only the
    /// headers of a packed object's chain of deltas.
    pub fn kind(&self, id: &Id) -> Result<Option<Kind>, Error> {
        let hex = id.to_string();
        let path = self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        if let Some(kind) = loose_kind(&path, &hex)? {
            return Ok(Some(kind));
        }
        for pack in self.packs()? {
            if let Some(offset) = pack.find(id)? {
                return pack.kind(offset).map(Some);
            }
        }
        Ok(None)
    }

    /// Whether the repository has the object named `id`, loose or packed,
    /// without reading it.
    pub fn contains(&self, id: &Id) -> Result<bool, Error> {
        let hex = id.to_string();
        let path = self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        if fs::symlink_metadata(&path).is_ok() {
            return Ok(true);
        }
        self.is_packed(id)
    }

    /// Whether a pack holds the object named `id`.
    fn is_packed(&self, id: &Id) -> Result<bool, Error> {
        for pack in self.packs()? {
            if pack.find(id)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every pack with an index in `objects/pack`, in the order of their
    /// names. An index with no pack is left out, as git leaves it out.
    fn packs(&self) -> Result<&[Pack], Error> {
        if let Some(packs) = self.packs.get() {
            return Ok(packs);
        }
        let dir = self.git_dir.join("objects").join("pack");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if is_missing(&e) => return Ok(self.packs.get_or_init(Vec::new)),
            Err(e) => return Err(e.into()),
        };
        let mut indexes = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "idx") {
                indexes.push(path);
            }
        }
        indexes.sort();
        let mut packs = Vec::new();
        for index in indexes {
            packs.extend(Pack::open(&index)?);
        }
        Ok(self.packs.get_or_init(|| packs))
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
        if size > self.limit {
            return Err(corrupt(format!("{hex} is larger than the limit")));
        }
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

    /// Writes `object` as a loose object, and returns its name. It writes a
    /// temporary file beside the object and links it into place, as git does,
    /// so a reader never sees half an object. An object that already exists,
    /// loose or packed, is left as it was, even one another writer puts there
    /// first.
    pub fn write(&self, object: &Object) -> Result<Id, Error> {
        let id = object.id();
        let hex = id.to_string();
        let dir = self.git_dir.join("objects").join(&hex[..2]);
        let path = dir.join(&hex[2..]);
        if fs::symlink_metadata(&path).is_ok() || self.is_packed(&id)? {
            return Ok(id);
        }
        fs::create_dir_all(&dir)?;
        let mut raw = object.header();
        raw.extend_from_slice(&object.data);
        let compressed = deflate::compress_to_vec_zlib(&raw, 6);
        let (temp, mut file) = temp_file(&dir)?;
        let written = file
            .write_all(&compressed)
            .and_then(|()| file.sync_all())
            .and_then(|()| match fs::hard_link(&temp, &path) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                linked => linked,
            });
        let removed = fs::remove_file(&temp);
        written.and(removed)?;
        Ok(id)
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

    /// Every ref under `refs/`, loose and packed, sorted by name, each once.
    /// Like `git for-each-ref`, it leaves out a ref it can't read. A ref
    /// whose name isn't UTF-8 is left out too. Git writes `packed-refs`
    /// sorted and with each name once; given one that isn't, git lists it in
    /// file order, and this lists the last record of each name, in order.
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
            Err(e) if is_missing(&e) => return Ok(None),
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

    /// The refs in `packed-refs` under `refs/`, by name, as
    /// [`parse_packed_refs`] reads them.
    fn packed_refs(&self) -> Result<BTreeMap<String, Id>, Error> {
        match read(&self.git_dir.join("packed-refs"))? {
            Some(contents) => parse_packed_refs(&contents),
            None => Ok(BTreeMap::new()),
        }
    }
}

/// The refs under `refs/` that `contents`, a `packed-refs` file, holds, by
/// name. It parses records as git's `next_record` does: an id, one
/// whitespace byte other than a newline, and a name up to a newline, with an
/// optional `^` line that peels it. A name git calls dangerous refuses the
/// file, and any other bad name is left out.
pub fn parse_packed_refs(contents: &[u8]) -> Result<BTreeMap<String, Id>, Error> {
    let mut refs = BTreeMap::new();
    let mut rest = contents;
    let mut sorted = false;
    if let Some(traits) = rest.strip_prefix(b"# pack-refs with: ") {
        let eol = traits.iter().position(|&b| b == b'\n');
        let eol = eol.ok_or_else(|| corrupt("packed-refs has an unterminated header"))?;
        sorted = traits[..eol].split(|&b| b == b' ').any(|t| t == b"sorted");
        rest = &traits[eol + 1..];
    }
    // A file that doesn't say it is sorted, git sorts first, and that
    // takes every line but a `^` one to be long enough for a record.
    if !sorted {
        let mut lines = rest.split(|&b| b == b'\n');
        while let Some(line) = lines.next() {
            if line.is_empty() && lines.clone().next().is_none() {
                break;
            }
            if line.len() < 42 {
                return Err(corrupt("packed-refs has a short line"));
            }
            if lines
                .clone()
                .next()
                .is_some_and(|next| next.starts_with(b"^"))
            {
                lines.next();
            }
        }
    }
    while !rest.is_empty() {
        let bad = || corrupt("packed-refs has a bad line");
        // Git finds records by line, so the space can't be a newline.
        if rest.len() < 42 || !is_git_space(rest[40]) || rest[40] == b'\n' {
            return Err(bad());
        }
        let id = hex_id(&rest[..40]).ok_or_else(bad)?;
        rest = &rest[41..];
        let eol = rest.iter().position(|&b| b == b'\n');
        let eol = eol.ok_or_else(|| corrupt("packed-refs ends without a newline"))?;
        let name = &rest[..eol];
        rest = &rest[eol + 1..];
        if name.contains(&0) || (!format_ok(name) && !is_safe(name)) {
            return Err(corrupt("packed-refs holds a dangerous name"));
        }
        if let Some(peeled) = rest.strip_prefix(b"^") {
            if peeled.len() < 41 || hex_id(&peeled[..40]).is_none() || peeled[40] != b'\n' {
                return Err(bad());
            }
            rest = &peeled[41..];
        }
        // A name that isn't UTF-8 can't be asked for, so it is left out.
        if let Ok(name) = std::str::from_utf8(name)
            && name.starts_with("refs/")
            && format_ok(name.as_bytes())
        {
            refs.insert(name.to_string(), id);
        }
    }
    Ok(refs)
}

/// A new file in `dir`, named as git names its temporary objects, and
/// read-only once closed, as git leaves an object.
fn temp_file(dir: &Path) -> io::Result<(PathBuf, fs::File)> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("tmp_obj_{}_{n}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
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

/// Whether git's `refname_is_safe` takes `name`: under `refs/` with no
/// empty, `.`, or `..` component, or else only capitals and underscores.
fn is_safe(name: &[u8]) -> bool {
    match name.strip_prefix(b"refs/") {
        Some(rest) => rest
            .split(|&b| b == b'/')
            .all(|component| !matches!(component, b"" | b"." | b"..")),
        None => !name.is_empty() && name.iter().all(|&b| b.is_ascii_uppercase() || b == b'_'),
    }
}

/// The bytes git's `isspace` takes as whitespace.
fn is_git_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// What `contents`, the file of the loose ref `name`, says, as
/// [`Repository::find_ref`] reads it.
pub fn parse_ref(name: &str, contents: &[u8]) -> Result<Target, Error> {
    check_name(name)?;
    parse_loose_ref(name, contents)
}

/// What a loose ref file says: 40 hex digits, or `ref:` and another ref.
fn parse_loose_ref(name: &str, contents: &[u8]) -> Result<Target, Error> {
    let bad = || corrupt(format!("{name} isn't a ref"));
    // As git's `refs_read_raw_ref` does: trim whitespace from the end of
    // the file, then read what is left as a C string, to its first NUL.
    let end = contents.iter().rposition(|&b| !is_git_space(b));
    let contents = &contents[..end.map_or(0, |end| end + 1)];
    let contents = match contents.iter().position(|&b| b == 0) {
        Some(nul) => &contents[..nul],
        None => contents,
    };
    if let Some(target) = contents.strip_prefix(b"ref:") {
        let start = target.iter().position(|&b| !is_git_space(b));
        let target = &target[start.unwrap_or(target.len())..];
        let target = std::str::from_utf8(target).map_err(|_| bad())?;
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
    if name == "HEAD" || (name.starts_with("refs/") && format_ok(name.as_bytes())) {
        return Ok(());
    }
    Err(corrupt(format!("{name:?} isn't a ref name")))
}

/// Whether git's `check_refname_format` takes `name`, allowing one level.
fn format_ok(name: &[u8]) -> bool {
    if name == b"@" || name.ends_with(b".") {
        return false;
    }
    if name.windows(2).any(|pair| pair == b".." || pair == b"@{") {
        return false;
    }
    let forbidden = |b: u8| b < 0x20 || b == 0x7f || b" ~^:?*[\\".contains(&b);
    if name.iter().any(|&b| forbidden(b)) {
        return false;
    }
    name.split(|&b| b == b'/').all(|component| {
        !component.is_empty() && !component.starts_with(b".") && !component.ends_with(b".lock")
    })
}
