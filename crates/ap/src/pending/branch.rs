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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
}

impl<'a> Agent<'a> {
    fn new(share: &'a Share, records: &'a Records) -> Agent<'a> {
        Agent {
            records,
            dir: share.dir,
            written: Repository::at(share.dir.join("tree").join(".git")),
            yours: Repository::at(share.root.join(".git")),
        }
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
        let object = match self.written.object(id)? {
            Some(object) => Some(object),
            None => self.yours.object(id)?,
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

    /// The mode and object at `path` in the tree of `commit`, if it has
    /// anything there.
    fn entry(&self, commit: Id, path: &[u8]) -> Result<Option<(u32, Id)>, git::Error> {
        let mut id = self.object(&commit)?.commit()?.tree;
        let mut mode = TREE;
        for name in path.split(|&b| b == b'/') {
            if mode != TREE {
                return Ok(None);
            }
            let entries = self.object(&id)?.entries()?;
            let Some(entry) = entries.into_iter().find(|e| e.name == name) else {
                return Ok(None);
            };
            (mode, id) = (entry.mode, entry.id);
        }
        Ok(Some((mode, id)))
    }

    /// Whether the agent's file at `path` is as `head` holds it.
    fn committed(&self, head: Option<Id>, path: &[u8]) -> Result<bool, git::Error> {
        let theirs = match self.records.view(path) {
            View::Agent(Entry::File { mode }) => {
                let file = self.dir.join("tree").join(bytes_path(path));
                let data = fs::read(file)?;
                let id = Object {
                    kind: Kind::Blob,
                    data,
                }
                .id();
                Some((git_mode(*mode), id))
            }
            View::Agent(Entry::Symlink { target }) => {
                let id = Object {
                    kind: Kind::Blob,
                    data: target.clone(),
                }
                .id();
                Some((SYMLINK, id))
            }
            View::Yours => return Ok(true),
            // A directory holds only what is beneath it, which is checked
            // by itself.
            View::Agent(_) | View::Absent => None,
        };
        let committed = match head {
            Some(head) => self.entry(head, path)?,
            None => None,
        };
        Ok(match theirs {
            Some(_) => committed == theirs,
            // Git keeps no directory, so a tree where the agent has nothing
            // holds only what it has beneath, which is checked by itself.
            None => committed.is_none_or(|(mode, _)| mode == TREE),
        })
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
    let said = String::from_utf8_lossy(&output.stderr);
    refused(format!("git {what} failed: {}", printable(said.trim_end())))
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

    /// Checks a branch import of this share, `branch`, while the run isn't
    /// active, and writes its objects to quarantine, where `git fsck`
    /// checks them. Nothing of yours changes, and nothing stays pending
    /// once it moves in.
    pub fn import<'a>(&'a self, branch: &str) -> io::Result<Import<'a>> {
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
        let agent = Agent::new(self, &records);
        let head = agent.head().map_err(read_failed)?;
        let mut applied = Applied {
            text: Vec::new(),
            ignored: 0,
            pending: 0,
        };
        let landed = BTreeMap::new();
        for path in changed(&records, &host)? {
            match verdict(&records, &landed, &host, &ignore, true, &path)? {
                Verdict::History | Verdict::Landed => {}
                Verdict::Ignored => applied.ignored += 1,
                Verdict::Skipped { why, .. } => line(&mut applied.text, "skipped", &path, &why),
                Verdict::Change => {
                    if !agent.committed(head, &path).map_err(read_failed)? {
                        return Err(refused(
                            "the agent left files it didn't commit, and `ap apply` can't commit them yet",
                        ));
                    }
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
        if let Some(head) = head.filter(|&head| Some(head) != start) {
            let name = format!("refs/heads/{branch}");
            if (agent.yours.find_ref(&name).map_err(read_failed)?).is_some() {
                return Err(refused(format!("the branch {branch} already exists")));
            }
            let quarantine = Quarantine::new(self.dir)?;
            let held = Repository::at(&quarantine.0);
            let mut objects = Vec::new();
            (agent.walk(head, &mut |object| {
                objects.push(held.write(object)?);
                Ok(())
            }))
            .map_err(read_failed)?;
            if !objects.is_empty() {
                fsck(root, &quarantine.0, head, &agent.yours)?;
            }
            import.head = Some(head);
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
        if let Err(error) = import.write_ref() {
            for written in &imports[..at] {
                written.remove_ref();
            }
            return Err((at, error));
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

    /// Makes the branch, which mustn't exist.
    fn write_ref(&self) -> io::Result<()> {
        let Some(head) = self.head else {
            return Ok(());
        };
        let mut update = your_git(self.share.root);
        update
            .args(["update-ref", "-m", "ap apply"])
            .arg(format!("refs/heads/{}", self.branch))
            .arg(head.to_string())
            .arg("");
        let updated = output(update)?;
        if !updated.status.success() {
            return Err(failed_git("update-ref", &updated));
        }
        Ok(())
    }

    /// Removes the branch this made, if it still names what this made it
    /// name.
    fn remove_ref(&self) {
        if let Some(head) = self.head {
            let mut update = your_git(self.share.root);
            update
                .args(["update-ref", "-d"])
                .arg(format!("refs/heads/{}", self.branch))
                .arg(head.to_string());
            let _ = update.output();
        }
    }
}
