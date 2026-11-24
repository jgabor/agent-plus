//! The branch import of `ap apply`, and the count of commits that `ap diff`
//! prints, for a share whose root holds a `.git` directory, from SPEC.md's
//! `SHARE` section.
//!
//! The agent's `.git` is what it wrote, in the share's records and tree,
//! over `git`, the copy of yours that the share server made when the run
//! started, and over your objects, which it read live. `ap` reads the
//! agent's refs and objects with `crates/git`. It runs your host's `git`
//! only on your repository, and on the agent's objects once it has written
//! them to a quarantine directory.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::ops::Bound;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use git::{Id, Kind, Object, Repository, Target};

use super::*;

/// The commit `HEAD` named when the run started, in hex, or `none`, and
/// the copy of your `.git` that the share server made then.
const HEAD: &str = "head";
const GIT: &str = "git";
/// Where `ap apply` writes the agent's objects before `git fsck` checks
/// them, in the share's pending directory.
const QUARANTINE: &str = "quarantine";
/// How many symbolic refs a ref may go through, as git's `SYMREF_MAXDEPTH`.
const MAX_DEPTH: usize = 5;
const GITLINK: u32 = 0o160000;
const SYMLINK: u32 = 0o120000;
const TREE: u32 = 0o40000;
/// How many paths of the tree of the agent's `HEAD` `ap` reads, at most,
/// and how many bytes of them. What it does with a path takes time, and
/// more time the longer it is.
const MAX_PATHS: usize = 1_000_000;
const MAX_READ: usize = 64 << 20;
/// How long a path of that tree may be, in bytes, at most. Each path is
/// then cheap to read, and a walk of the tree goes no more than 512 deep,
/// which the stack holds.
const MAX_PATH: usize = 1024;
/// How large an object of the agent's, or a pack index, may be, in bytes,
/// at most. Zlib inflates about 1000 to 1, so a small file can say it holds
/// far more than this. It is git's `core.bigFileThreshold`, above which git
/// stores a blob whole.
const MAX_OBJECT: usize = 512 << 20;
/// The author and committer of the last commit.
const IDENTITY: &str = "Agent+ <ap@localhost>";

/// A file in the agent's `.git`: a regular file's contents, or the target
/// of a symlink.
enum Content {
    File(Vec<u8>),
    Link(Vec<u8>),
}

/// The agent's git directory, as the agent's git sees it.
struct Agent<'a> {
    records: &'a Records,
    /// The share's pending directory.
    dir: &'a Path,
    /// The objects the agent wrote, in the tree.
    written: Repository,
    yours: Repository,
    /// The trees and blobs of the last commit, and the commit, which are
    /// nowhere else until they go in quarantine.
    made: RefCell<HashMap<Id, Object>>,
    /// What the last commit leaves out where git-ignored files stay.
    replacing: Replacing,
    /// How many paths of the tree of its `HEAD` have been read, and how
    /// many bytes of them.
    read: Cell<(usize, usize)>,
    /// The entries of each tree of its `HEAD` that has been read, so that
    /// one it names many times is read once.
    trees: RefCell<HashMap<Id, Rc<[git::TreeEntry]>>>,
}

impl<'a> Agent<'a> {
    fn new(share: &'a Share, records: &'a Records) -> Agent<'a> {
        Agent {
            records,
            dir: share.dir,
            written: Repository::at(share.dir.join("tree").join(".git")).with_limit(MAX_OBJECT),
            yours: Repository::at(share.root.join(".git")),
            made: RefCell::default(),
            replacing: Replacing::default(),
            read: Cell::new((0, 0)),
            trees: RefCell::default(),
        }
    }

    /// Keeps `object` with the ones the agent's history has, and returns
    /// its name.
    fn make(&self, object: Object) -> Id {
        let id = object.id();
        self.made.borrow_mut().entry(id).or_insert(object);
        id
    }

    /// The agent's file at `path` in its `.git`, or `None` if it has no
    /// regular file or symlink there. Nothing on the way is a symlink.
    fn file(&self, path: &str) -> io::Result<Option<Content>> {
        let inner = format!(".git/{path}");
        match self.records.view(inner.as_bytes()) {
            View::Agent(Entry::File { .. }) => read_beneath(&self.dir.join("tree"), &inner),
            View::Agent(Entry::Symlink { target }) => Ok(Some(Content::Link(target.clone()))),
            View::Agent(_) | View::Absent => Ok(None),
            View::Yours => read_beneath(&self.dir.join(GIT), path),
        }
    }

    /// The commit the agent's `HEAD` names, or `None` if it names none.
    fn head(&self) -> Result<Option<Id>, git::Error> {
        let mut name = "HEAD".to_owned();
        for _ in 0..MAX_DEPTH {
            let target = match self.file(&name)? {
                Some(Content::File(contents)) => git::parse_ref(&name, &contents)?,
                // A symlink is a symbolic ref, as git once wrote them, and
                // must point under `refs/`. It is never followed.
                Some(Content::Link(target)) => {
                    let contents = [&b"ref: "[..], &target].concat();
                    match git::parse_ref(&name, &contents) {
                        Ok(Target::Symbolic(next)) if next != "HEAD" => Target::Symbolic(next),
                        _ => {
                            return Err(git::Error::Corrupt(format!("{name} links outside refs/")));
                        }
                    }
                }
                None => {
                    let packed = match self.file("packed-refs")? {
                        Some(Content::File(contents)) => contents,
                        _ => Vec::new(),
                    };
                    match git::parse_packed_refs(&packed)?.remove(&name) {
                        Some(id) => Target::Object(id),
                        None => return Ok(None),
                    }
                }
            };
            match target {
                Target::Object(id) => return Ok(Some(id)),
                Target::Symbolic(next) => name = next,
            }
        }
        Err(git::Error::Corrupt(
            "HEAD is too deep a chain of symbolic refs".to_owned(),
        ))
    }

    /// The object named `id`, from the agent's objects or yours. Its
    /// contents must have that name.
    fn object(&self, id: &Id) -> Result<Object, git::Error> {
        let made = self.made.borrow().get(id).cloned();
        let object = match made {
            Some(object) => Some(object),
            None => match self.written.object(id)? {
                Some(object) => Some(object),
                None => self.yours.object(id)?,
            },
        };
        let object = object.ok_or_else(|| {
            git::Error::Corrupt(format!("the agent's history is missing object {id}"))
        })?;
        if object.id() != *id {
            return Err(git::Error::Corrupt(format!(
                "the agent's object {id} doesn't hold what its name says"
            )));
        }
        Ok(object)
    }

    /// The entries of the tree `tree`, at `path` in the tree of the agent's
    /// `HEAD`. Each is a path read, which may be no longer than `MAX_PATH`,
    /// and its name one that `git fsck` takes.
    fn entries(&self, path: &[u8], tree: &Id) -> Result<Rc<[git::TreeEntry]>, git::Error> {
        let cached = self.trees.borrow().get(tree).cloned();
        let entries = match &cached {
            Some(entries) => entries.clone(),
            None => {
                let entries: Rc<[git::TreeEntry]> = self.object(tree)?.entries()?.into();
                let named =
                    |name: &[u8]| !matches!(name, b"" | b"." | b"..") && !name.contains(&b'/');
                if !entries.iter().all(|entry| named(&entry.name)) {
                    return Err(git::Error::Corrupt(
                        "the agent's HEAD holds a tree entry with a name that git refuses"
                            .to_owned(),
                    ));
                }
                entries
            }
        };
        let above = path.len() + usize::from(!path.is_empty());
        let bytes: usize = (entries.iter()).map(|entry| above + entry.name.len()).sum();
        let (paths, read) = self.read.get();
        let (paths, read) = (paths + entries.len(), read + bytes);
        if paths > MAX_PATHS {
            return Err(git::Error::Corrupt(
                "the agent's HEAD holds more than the million paths that `ap` reads".to_owned(),
            ));
        }
        if read > MAX_READ {
            return Err(git::Error::Corrupt(
                "the agent's HEAD holds more than the 64 MiB of paths that `ap` reads".to_owned(),
            ));
        }
        self.read.set((paths, read));
        if (entries.iter()).any(|entry| above + entry.name.len() > MAX_PATH) {
            return Err(git::Error::Corrupt(format!(
                "the agent's HEAD holds a path longer than the {MAX_PATH} bytes that `ap` reads"
            )));
        }
        if cached.is_none() {
            self.trees.borrow_mut().insert(*tree, entries.clone());
        }
        Ok(entries)
    }

    /// Walks from `head` to the objects your repository already has, and
    /// gives each other object to `found`, once. Each object must be of the
    /// type that names it, yours too, because `git fsck` doesn't look at
    /// yours. Returns how many of the others are commits.
    fn walk(
        &self,
        head: Id,
        found: &mut dyn FnMut(&Object) -> Result<(), git::Error>,
    ) -> Result<usize, git::Error> {
        let mut seen = HashMap::new();
        let mut stack = vec![(head, Some(Kind::Commit))];
        let mut commits = 0;
        while let Some((id, named)) = stack.pop() {
            if let Some(&kind) = seen.get(&id) {
                check_kind(&id, named, kind)?;
                continue;
            }
            if let Some(kind) = self.yours.kind(&id)? {
                check_kind(&id, named, kind)?;
                seen.insert(id, kind);
                continue;
            }
            let object = self.object(&id)?;
            check_kind(&id, named, object.kind)?;
            seen.insert(id, object.kind);
            match object.kind {
                Kind::Commit => {
                    let commit = object.commit()?;
                    stack.push((commit.tree, Some(Kind::Tree)));
                    stack.extend(
                        commit
                            .parents
                            .into_iter()
                            .map(|id| (id, Some(Kind::Commit))),
                    );
                    commits += 1;
                }
                Kind::Tree => {
                    for entry in object.entries()? {
                        match entry.mode {
                            // A submodule's commit is in its own repository.
                            GITLINK => {}
                            TREE => stack.push((entry.id, Some(Kind::Tree))),
                            _ => stack.push((entry.id, Some(Kind::Blob))),
                        }
                    }
                }
                // A tag says the type of what it names, which `git fsck`
                // checks.
                Kind::Tag => stack.push((object.tag_target()?, None)),
                Kind::Blob => {}
            }
            found(&object)?;
        }
        Ok(commits)
    }

    /// Puts in `found` each file and symlink of the agent's beneath `path`,
    /// where its `HEAD` holds `tree`, that would replace a directory with a
    /// git-ignored file in it, and each directory of the agent's that would
    /// replace a git-ignored file. Git can't hold both, and that file stays.
    fn replacing(
        &self,
        ignore: &Gitignore,
        path: &[u8],
        tree: Id,
        found: &mut Replacing,
    ) -> Result<(), git::Error> {
        for entry in self.entries(path, &tree)?.iter() {
            let inner = beneath(path, &entry.name);
            if entry.mode != TREE {
                // A submodule is as that `HEAD` holds it already, whatever
                // the agent made in its place.
                let dir = matches!(self.records.view(&inner), View::Agent(Entry::Dir { .. }));
                if dir && entry.mode != GITLINK && ignore.is_ignored(bytes_path(&inner), false) {
                    found.dirs.insert(inner);
                }
                continue;
            }
            match self.records.view(&inner) {
                View::Agent(Entry::File { .. } | Entry::Symlink { .. }) => {
                    if self.holds_ignored(ignore, &inner, entry.id, true)? {
                        found.files.insert(inner);
                    }
                }
                _ if self.recorded_beneath(&inner).next().is_some() => {
                    self.replacing(ignore, &inner, entry.id, found)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether the tree `tree` at `path` holds a git-ignored file. Unless
    /// `top` is set, git ignores none of the directories above it.
    fn holds_ignored(
        &self,
        ignore: &Gitignore,
        path: &[u8],
        tree: Id,
        top: bool,
    ) -> Result<bool, git::Error> {
        let entries = self.entries(path, &tree)?;
        if top && !entries.is_empty() && ignore.is_ignored(bytes_path(path), true) {
            return Ok(true);
        }
        for entry in entries.iter() {
            let inner = beneath(path, &entry.name);
            // As `verdict` and `left_at` read it, a submodule is a file.
            if ignore.is_ignored_below(&inner, entry.mode == TREE) {
                return Ok(true);
            }
            if entry.mode == TREE && self.holds_ignored(ignore, &inner, entry.id, false)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// What the last commit leaves out where `tree`, the tree of its
    /// `HEAD`, holds git-ignored files.
    fn replacing_in(&self, ignore: &Gitignore, tree: Option<Id>) -> Result<Replacing, git::Error> {
        let mut found = Replacing::default();
        if let Some(tree) = tree {
            self.replacing(ignore, b"", tree, &mut found)?;
        }
        Ok(found)
    }

    /// The tree of the files as the agent left them: the tree of its `HEAD`,
    /// `tree`, with every file put in, changed, or removed that lands.
    fn left(&self, host: &Host, ignore: &Gitignore, tree: Option<Id>) -> Result<Id, git::Error> {
        match self.left_at(host, ignore, b"", tree.map(|tree| (TREE, tree)), false)? {
            Some((_, tree)) => Ok(tree),
            None => Ok(self.make(Object::tree(Vec::new()))),
        }
    }

    /// Each path the records hold beneath `path`, or beneath the root when
    /// `path` is empty, in order.
    fn recorded_beneath<'b>(&'b self, path: &[u8]) -> impl Iterator<Item = &'b Vec<u8>> {
        self.records
            .entries
            .range(below(path))
            .map(|(path, _)| path)
    }

    /// The mode and object at `path` as the agent left it, if it left
    /// anything there that git keeps, where `head` is what its `HEAD` tree
    /// holds there. A file that doesn't land is as `head` holds it, and so
    /// is one of yours the agent never changed. One the agent deleted, or
    /// deleted a directory above, is there only if you have it now, as the
    /// agent sees it. That is `removed`.
    fn left_at(
        &self,
        host: &Host,
        ignore: &Gitignore,
        path: &[u8],
        head: Option<(u32, Id)>,
        removed: bool,
    ) -> Result<Option<(u32, Id)>, git::Error> {
        let root = path.is_empty();
        let view = match root {
            true => View::Yours,
            false => self.records.view(path),
        };
        let removed = removed || self.records.deleted.contains(path);
        let yours = matches!(view, View::Yours);
        // Only what is yours goes by these, so nothing else looks.
        let recorded = yours && self.recorded_beneath(path).next().is_some();
        // Something the agent deleted beneath it may be one of `head`'s.
        let swept = yours && self.records.deleted.range(below(path)).next().is_some();
        // The agent's directory doesn't land where it would replace a
        // git-ignored file, so that file is as `head` holds it.
        if self.replacing.dirs.contains(path) {
            return Ok(head);
        }
        let head_tree = matches!(head, Some((TREE, _)));
        let dir = root
            || match view {
                View::Agent(Entry::Dir { .. }) => true,
                View::Yours => recorded || head_tree,
                View::Absent => head_tree,
                // The agent's file doesn't land where it would replace a
                // directory with a git-ignored file in it, so the directory
                // is as if the agent deleted it.
                View::Agent(_) => self.replacing.files.contains(path),
            };
        if !dir {
            let there = match view {
                // The walk from the root found no directory above `path`
                // that git ignores.
                View::Yours if !removed || ignore.is_ignored_below(path, false) => {
                    return Ok(head);
                }
                View::Yours => match head {
                    Some((GITLINK, _)) => host.stat(path)?.is_some_and(|m| m.is_dir()),
                    _ => host.stat(path)?.is_some(),
                },
                // What `verdict` says of a path the agent removed, which is
                // under no `.git`, because the walk goes into none.
                View::Absent if ignore.is_ignored_below(path, false) => return Ok(head),
                View::Absent => true,
                _ => {
                    let landed = BTreeMap::new();
                    match verdict(
                        self.records,
                        &landed,
                        host,
                        ignore,
                        Some(&self.replacing),
                        None,
                        path,
                    )? {
                        Verdict::Change => true,
                        _ => return Ok(head),
                    }
                }
            };
            let blob = |data| {
                self.make(Object {
                    kind: Kind::Blob,
                    data,
                })
            };
            return Ok(match view {
                View::Yours => head.filter(|_| there),
                View::Agent(Entry::File { mode }) => {
                    let data = fs::read(self.dir.join("tree").join(bytes_path(path)))?;
                    Some((git_mode(*mode), blob(data)))
                }
                View::Agent(Entry::Symlink { target }) => Some((SYMLINK, blob(target.clone()))),
                _ => None,
            });
        }
        // What the agent did beneath a submodule's directory is in the
        // submodule's own repository.
        if !root && matches!(head, Some((GITLINK, _))) && !matches!(view, View::Absent) {
            return Ok(head);
        }
        let untouched = yours && !recorded && !swept && !removed;
        if !root && (untouched || ignore.is_ignored_below(path, true)) {
            return Ok(head);
        }
        let mut entries: BTreeMap<Vec<u8>, Option<(u32, Id)>> = BTreeMap::new();
        if let Some((TREE, tree)) = head {
            for entry in self.entries(path, &tree)?.iter() {
                entries.insert(entry.name.clone(), Some((entry.mode, entry.id)));
            }
        }
        let start = path.len() + usize::from(!root);
        for other in self.recorded_beneath(path) {
            let name = other[start..].split(|&b| b == b'/').next();
            entries
                .entry(name.unwrap_or_default().to_vec())
                .or_insert(None);
        }
        let mut tree = Vec::new();
        for (name, head) in entries {
            let inner = beneath(path, &name);
            // Git keeps no `.git`, and the agent's history comes as commits.
            let left = match name.eq_ignore_ascii_case(b".git") {
                true => head,
                false => self.left_at(host, ignore, &inner, head, removed)?,
            };
            if let Some((mode, id)) = left {
                tree.push(git::TreeEntry { mode, name, id });
            }
        }
        if tree.is_empty() {
            return Ok(None);
        }
        let tree = Object::tree(tree);
        if head == Some((TREE, tree.id())) {
            return Ok(head);
        }
        Ok(Some((TREE, self.make(tree))))
    }
}

/// The path of `name` in the directory `path`, or at the root when `path`
/// is empty.
fn beneath(path: &[u8], name: &[u8]) -> Vec<u8> {
    match path.is_empty() {
        true => name.to_vec(),
        false => [path, b"/", name].concat(),
    }
}

/// The bounds of the paths beneath `path`, or beneath the root when `path`
/// is empty. The paths beneath `a` sort after `a/` and before `a0`.
fn below(path: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    match path.is_empty() {
        true => (Bound::Excluded(Vec::new()), Bound::Unbounded),
        false => (
            Bound::Excluded([path, b"/"].concat()),
            Bound::Excluded([path, b"0"].concat()),
        ),
    }
}

/// Fails unless `kind`, the type of the object `id`, is the type the object
/// that names it says, if it says one.
fn check_kind(id: &Id, named: Option<Kind>, kind: Kind) -> Result<(), git::Error> {
    match named {
        Some(named) if named != kind => Err(git::Error::Corrupt(format!(
            "the agent's history names object {id} as a {named}, but it is a {kind}"
        ))),
        _ => Ok(()),
    }
}

/// The regular file or symlink at `path` beneath `dir`, reached with no
/// symlink on the way, or `None` if there is neither there.
fn read_beneath(dir: &Path, path: &str) -> io::Result<Option<Content>> {
    let name = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let dir = match openat2(libc::AT_FDCWD, &name, libc::O_DIRECTORY, NO_SYMLINKS) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let name = CString::new(path).map_err(io::Error::other)?;
    // With `O_PATH` and `O_NOFOLLOW`, a symlink at the end opens as itself.
    let flags = libc::O_PATH | libc::O_NOFOLLOW;
    let fd = match openat2(dir.as_raw_fd(), &name, flags, BENEATH) {
        Ok(fd) => fd,
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV) => return Ok(None),
            _ => return Err(error),
        },
    };
    let metadata = File::from(fd.try_clone()?).metadata()?;
    if metadata.is_symlink() {
        let mut target = vec![0; libc::PATH_MAX as usize];
        // SAFETY: the empty name is NUL-terminated, and `target` has room
        // for the length passed.
        let len = unsafe {
            libc::readlinkat(
                fd.as_raw_fd(),
                c"".as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        let len = usize::try_from(len).map_err(|_| io::Error::last_os_error())?;
        target.truncate(len);
        return Ok(Some(Content::Link(target)));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    // The descriptor reads nothing, so the file opens again through it.
    let mut file = File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(Some(Content::File(contents)))
}

/// `text` with each control character escaped, so that nothing an agent
/// named, such as a file, can drive your terminal when it is printed.
pub(super) fn printable(text: &str) -> String {
    let escape = |c: char| match c.is_control() {
        true => c.escape_default().to_string(),
        false => c.to_string(),
    };
    text.chars().map(escape).collect()
}

/// Why a branch import failed, which keeps everything pending.
fn refused(why: impl fmt::Display) -> io::Error {
    io::Error::other(format!("{why}, so everything stays pending"))
}

fn read_failed(error: git::Error) -> io::Error {
    match error {
        git::Error::Io(error) => error,
        git::Error::Corrupt(why) => refused(printable(&why)),
    }
}

/// `git` on your repository at `root`, with nothing from the environment
/// that would point it elsewhere.
fn your_git(root: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .arg("--git-dir")
        .arg(root.join(".git"))
        .stdin(Stdio::null());
    command
}

/// What `command` printed, once it succeeded.
fn output(mut command: Command) -> io::Result<std::process::Output> {
    let output = match command.output() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(refused(
                "`ap apply` needs git on your host to import a branch, and there is none",
            ));
        }
        output => output?,
    };
    Ok(output)
}

fn failed_git(what: &str, output: &std::process::Output) -> io::Error {
    refused(git_failure(what, output))
}

/// What `git <what>` said when it failed.
fn git_failure(what: &str, output: &std::process::Output) -> String {
    let said = String::from_utf8_lossy(&output.stderr);
    format!("git {what} failed: {}", printable(said.trim_end()))
}

/// Writes `path` as an entry of `GIT_ALTERNATE_OBJECT_DIRECTORIES`, quoted
/// as git unquotes it, so a `:` in it doesn't split it.
fn alternate(path: &Path) -> std::ffi::OsString {
    let mut out = vec![b'"'];
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'"' | b'\\' => out.extend([b'\\', byte]),
            0x20..0x7f => out.push(byte),
            _ => out.extend(format!("\\{byte:03o}").bytes()),
        }
    }
    out.push(b'"');
    OsStr::from_bytes(&out).to_owned()
}

/// The line of `ap diff` that says how many commits the branch would
/// carry, or why they can't be counted.
pub fn commits_line(agent: &str, run: &str, commits: &Result<usize, String>) -> String {
    match commits {
        Ok(1) => format!("branch ap/{agent}/{run} would carry 1 commit\n"),
        Ok(n) => format!("branch ap/{agent}/{run} would carry {n} commits\n"),
        Err(why) => format!("branch ap/{agent}/{run}: its commits can't be counted: {why}\n"),
    }
}

/// A branch import, checked and ready to move in.
pub struct Import<'a> {
    share: &'a Share<'a>,
    records: Records,
    branch: String,
    /// The commit the branch names, and the objects it brings, unless the
    /// agent's history is still where it started.
    head: Option<Id>,
    objects: Vec<Id>,
    applied: Applied,
    /// Where the objects wait until they move in.
    quarantine: Option<Quarantine>,
}

/// The quarantine directory, which goes once nothing needs it.
struct Quarantine(PathBuf);

impl Quarantine {
    /// An empty quarantine directory in the share's pending directory `dir`,
    /// in place of any that an `ap apply` that stopped left there.
    fn new(dir: &Path) -> io::Result<Quarantine> {
        let quarantine = Quarantine(dir.join(QUARANTINE));
        quarantine.remove()?;
        Ok(quarantine)
    }

    fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.0) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

impl Drop for Quarantine {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

impl Share<'_> {
    /// How many commits the agent's branch would carry, in a share whose
    /// root holds a `.git` directory: the commits its `HEAD` reaches that
    /// your repository doesn't have.
    pub(super) fn commits(&self, records: &Records) -> Result<usize, git::Error> {
        let agent = Agent::new(self, records);
        match agent.head()? {
            Some(head) => agent.walk(head, &mut |_| Ok(())),
            None => Ok(0),
        }
    }

    /// What a branch import leaves out where the agent's `HEAD` holds
    /// git-ignored files.
    pub(super) fn replacing(
        &self,
        records: &Records,
        ignore: &Gitignore,
    ) -> Result<Replacing, git::Error> {
        let agent = Agent::new(self, records);
        let tree = match agent.head()? {
            Some(head) => Some(agent.object(&head)?.commit()?.tree),
            None => None,
        };
        agent.replacing_in(ignore, tree)
    }

    /// The commit your `HEAD` named when the run started.
    fn start(&self) -> io::Result<Option<Id>> {
        let text = match read_beneath(self.dir, HEAD)? {
            Some(Content::File(text)) => text,
            _ => Vec::new(),
        };
        Ok(std::str::from_utf8(&text)
            .ok()
            .and_then(|text| Id::from_hex(text.trim_end())))
    }

    /// Checks a branch import of this share, `ap/<agent_name>/<run>`, while
    /// the run isn't active, and writes its objects to quarantine, where
    /// `git fsck` checks them. The files the agent didn't commit come as
    /// one last commit. Nothing of yours changes, and nothing stays pending
    /// once it moves in.
    pub fn import<'a>(&'a self, agent_name: &str, run: &str) -> io::Result<Import<'a>> {
        let branch = &format!("ap/{agent_name}/{run}");
        let root = self.root;
        let mut check = your_git(root);
        check.args(["rev-parse", "--show-object-format"]);
        let format = output(check)?;
        if !format.status.success() {
            return Err(failed_git("rev-parse", &format));
        }
        let format = String::from_utf8_lossy(&format.stdout).trim().to_owned();
        if format != "sha1" {
            return Err(refused(format!(
                "the repository's object format is {format}, and `ap apply` imports a branch only into a SHA-1 repository"
            )));
        }
        let records = self.read(RECORDS)?;
        let host = Host::new(root)?;
        let ignore = Gitignore::load(root)?;
        let mut agent = Agent::new(self, &records);
        let head = agent.head().map_err(read_failed)?;
        // The agent's history, checked as it is walked, then the files it
        // didn't commit, as one last commit on it.
        let quarantine = Quarantine::new(self.dir)?;
        let held = Repository::at(&quarantine.0);
        let mut objects = Vec::new();
        let mut tree = None;
        if let Some(head) = head {
            (agent.walk(head, &mut |object| {
                objects.push(held.write(object)?);
                Ok(())
            }))
            .map_err(read_failed)?;
            tree = Some(
                agent
                    .object(&head)
                    .and_then(|commit| commit.commit())
                    .map_err(read_failed)?
                    .tree,
            );
        }
        agent.replacing = agent.replacing_in(&ignore, tree).map_err(read_failed)?;
        let mut applied = Applied {
            text: Vec::new(),
            ignored: 0,
            pending: 0,
        };
        let landed = BTreeMap::new();
        // What the agent left differs from its `HEAD` only where it wrote,
        // and where that hides your files.
        let mut paths = changed(&records, &host)?;
        paths.extend(
            (records.entries.keys())
                .filter(|path| !path.is_empty())
                .cloned(),
        );
        for path in paths {
            match verdict(
                &records,
                &landed,
                &host,
                &ignore,
                Some(&agent.replacing),
                None,
                &path,
            )? {
                Verdict::History | Verdict::Landed => {}
                Verdict::Ignored => applied.ignored += 1,
                Verdict::Skipped { why, .. } => line(&mut applied.text, "skipped", &path, &why),
                Verdict::Change => {}
            }
        }
        // `left` reads what `replacing_in` read again, and has the same
        // limit to itself.
        agent.read.set((0, 0));
        let left = agent.left(&host, &ignore, tree).map_err(read_failed)?;
        let empty = Object::tree(Vec::new()).id();
        let last = (tree.unwrap_or(empty) != left).then(|| {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let time = i64::try_from(time.as_secs()).unwrap_or(i64::MAX);
            let message = format!("Uncommitted changes from {agent_name}/{run}\n");
            let parents: Vec<Id> = head.into_iter().collect();
            agent.make(Object::new_commit(left, &parents, IDENTITY, time, &message))
        });
        if last.is_some() {
            for (id, object) in agent.made.borrow().iter() {
                if !agent.yours.contains(id).map_err(read_failed)? {
                    objects.push(held.write(object).map_err(read_failed)?);
                }
            }
        }
        let mut import = Import {
            share: self,
            records: Records::default(),
            branch: branch.to_owned(),
            head: None,
            objects: Vec::new(),
            applied,
            quarantine: None,
        };
        let start = self.start()?;
        if let Some(tip) = last.or(head.filter(|&head| Some(head) != start)) {
            let name = format!("refs/heads/{branch}");
            if (agent.yours.find_ref(&name).map_err(read_failed)?).is_some() {
                return Err(refused(format!("the branch {branch} already exists")));
            }
            if !objects.is_empty() {
                fsck(root, &quarantine.0, tip, &agent.yours)?;
            }
            import.head = Some(tip);
            import.objects = objects;
            import.quarantine = Some(quarantine);
        }
        drop(agent);
        import.records = records;
        Ok(import)
    }
}

/// Runs `git fsck --strict` on the objects in `quarantine`, against your
/// repository at `root`, from `head`. With `--no-full`, it checks only
/// those objects, and stops at yours. It reports a loose one of yours that
/// they name as missing, because it doesn't look for loose objects in an
/// alternate. The walk already found each object they name, so a missing
/// object that you have is no failure.
fn fsck(root: &Path, quarantine: &Path, head: Id, yours: &Repository) -> io::Result<()> {
    let mut fsck = your_git(root);
    fsck.args(["fsck", "--strict", "--no-full", "--no-dangling"])
        .args(["--no-reflogs", "--no-progress"])
        .arg(head.to_string())
        .env("GIT_OBJECT_DIRECTORY", quarantine.join("objects"))
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            alternate(&root.join(".git").join("objects")),
        );
    let output = output(fsck)?;
    let yours_missing = |line: &str| {
        let words: Vec<&str> = line.split(' ').collect();
        matches!(words[..], ["missing", _, hex] if Id::from_hex(hex)
            .is_some_and(|id| yours.contains(&id).unwrap_or(false)))
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let passed = match output.status.code() {
        Some(0) => true,
        // Only unreachable or missing objects.
        Some(2) => output.stderr.is_empty() && stdout.lines().all(yours_missing),
        _ => false,
    };
    if passed {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = (said.lines().chain(stdout.lines()))
        .filter(|line| !yours_missing(line))
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n");
    Err(io::Error::other(format!(
        "git fsck --strict refused the agent's objects, so everything stays pending:\n{said}"
    )))
}

/// Moves the agent's objects and each branch into your repositories, and
/// then keeps nothing of their shares pending. If a branch can't be
/// written, the ones written before it go again, so everything stays
/// pending. Returns what each import skipped, or which one failed, and why.
pub fn land(imports: Vec<Import<'_>>) -> Result<Vec<Applied>, (usize, io::Error)> {
    for (at, import) in imports.iter().enumerate() {
        import.move_objects().map_err(|error| (at, error))?;
    }
    for (at, import) in imports.iter().enumerate() {
        if let Err(mut why) = import.write_ref() {
            for written in &imports[..at] {
                if let Err(cause) = written.remove_ref() {
                    why.push_str(&format!(
                        ", and the branch {} stays in {}, because git update-ref couldn't remove it: {cause}",
                        written.branch,
                        written.share.root.display()
                    ));
                }
            }
            return Err((at, refused(why)));
        }
    }
    let mut applied = Vec::new();
    for (at, import) in imports.into_iter().enumerate() {
        let share = import.share;
        let landed = share.read(LANDED).map_err(|error| (at, error))?.bases;
        (share.keep(&import.records, &landed, &BTreeMap::new())).map_err(|error| (at, error))?;
        applied.push(import.applied);
    }
    Ok(applied)
}

impl Import<'_> {
    /// Writes the agent's objects into your repository, where nothing
    /// names them until the branch does.
    fn move_objects(&self) -> io::Result<()> {
        let Some(quarantine) = &self.quarantine else {
            return Ok(());
        };
        let held = Repository::at(&quarantine.0);
        let yours = Repository::at(self.share.root.join(".git"));
        for id in &self.objects {
            let object = held.loose_object(id).map_err(read_failed)?;
            let object = object.ok_or_else(|| refused(format!("object {id} is gone")))?;
            yours.write(&object).map_err(read_failed)?;
        }
        Ok(())
    }

    /// Makes the branch, which mustn't exist, or says why it couldn't.
    fn write_ref(&self) -> Result<(), String> {
        let Some(head) = self.head else {
            return Ok(());
        };
        let mut update = your_git(self.share.root);
        update
            .args(["update-ref", "-m", "ap apply"])
            .arg(format!("refs/heads/{}", self.branch))
            .arg(head.to_string())
            .arg("");
        let updated = update.output().map_err(|error| error.to_string())?;
        match updated.status.success() {
            true => Ok(()),
            false => Err(git_failure("update-ref", &updated)),
        }
    }

    /// Removes the branch this made, if it still names what this made it
    /// name, or says why it couldn't.
    fn remove_ref(&self) -> Result<(), String> {
        let Some(head) = self.head else {
            return Ok(());
        };
        let mut update = your_git(self.share.root);
        update
            .args(["update-ref", "-d"])
            .arg(format!("refs/heads/{}", self.branch))
            .arg(head.to_string());
        let updated = update.output().map_err(|error| error.to_string())?;
        match updated.status.success() {
            true => Ok(()),
            false => Err(printable(
                String::from_utf8_lossy(&updated.stderr).trim_end(),
            )),
        }
    }
}
