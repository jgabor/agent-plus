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

use std::collections::HashSet;
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
    /// regular file there. Nothing on the way is a symlink.
    fn file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        let inner = format!(".git/{path}");
        match self.records.view(inner.as_bytes()) {
            View::Agent(Entry::File { .. }) => read_beneath(&self.dir.join("tree"), &inner),
            View::Agent(_) | View::Absent => Ok(None),
            View::Yours => read_beneath(&self.dir.join(GIT), path),
        }
    }

    /// The commit the agent's `HEAD` names, or `None` if it names none.
    fn head(&self) -> Result<Option<Id>, git::Error> {
        let mut name = "HEAD".to_owned();
        for _ in 0..MAX_DEPTH {
            let target = match self.file(&name)? {
                Some(contents) => git::parse_ref(&name, &contents)?,
                None => {
                    let packed = self.file("packed-refs")?.unwrap_or_default();
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
    /// gives each other object to `found`, once. Returns how many of them
    /// are commits.
    fn walk(
        &self,
        head: Id,
        found: &mut dyn FnMut(&Object) -> Result<(), git::Error>,
    ) -> Result<usize, git::Error> {
        let mut seen = HashSet::new();
        let mut stack = vec![head];
        let mut commits = 0;
        while let Some(id) = stack.pop() {
            if !seen.insert(id) || self.yours.contains(&id)? {
                continue;
            }
            let object = self.object(&id)?;
            match object.kind {
                Kind::Commit => {
                    let commit = object.commit()?;
                    stack.push(commit.tree);
                    stack.extend(commit.parents);
                    commits += 1;
                }
                Kind::Tree => {
                    let entries = object.entries()?;
                    // A submodule's commit is in its own repository.
                    stack.extend((entries.iter().filter(|e| e.mode != GITLINK)).map(|e| e.id));
                }
                Kind::Tag => stack.push(object.tag_target()?),
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
            View::Agent(Entry::Dir { .. }) | View::Yours => return Ok(true),
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

/// The file at `path` beneath `dir`, reached with no symlink, or `None` if
/// there is no regular file there.
fn read_beneath(dir: &Path, path: &str) -> io::Result<Option<Vec<u8>>> {
    let name = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let dir = match openat2(libc::AT_FDCWD, &name, libc::O_DIRECTORY, NO_SYMLINKS) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let name = CString::new(path).map_err(io::Error::other)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = match openat2(dir.as_raw_fd(), &name, flags, BENEATH) {
        Ok(fd) => fd,
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV) => return Ok(None),
            _ => return Err(error),
        },
    };
    let mut file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(Some(contents))
}

/// Why a branch import failed, which keeps everything pending.
fn refused(why: impl fmt::Display) -> io::Error {
    io::Error::other(format!("{why}, so everything stays pending"))
}

fn read_failed(error: git::Error) -> io::Error {
    match error {
        git::Error::Io(error) => error,
        git::Error::Corrupt(why) => refused(why),
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
    refused(format!("git {what} failed: {}", said.trim_end()))
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

/// How many commits the branch would carry, as `ap diff` counts them.
pub fn commits_line(agent: &str, run: &str, commits: usize) -> String {
    let commits = match commits {
        1 => "1 commit".to_owned(),
        n => format!("{n} commits"),
    };
    format!("branch ap/{agent}/{run} would carry {commits}\n")
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
        let text = read_beneath(self.dir, HEAD)?.unwrap_or_default();
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
            if agent.object(&head).map_err(read_failed)?.kind != Kind::Commit {
                return Err(refused("the agent's HEAD doesn't name a commit"));
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
        .collect::<Vec<_>>()
        .join("\n");
    Err(io::Error::other(format!(
        "git fsck --strict refused the agent's objects, so everything stays pending:\n{said}"
    )))
}

impl Import<'_> {
    /// Moves the agent's objects and the branch into your repository, and
    /// then keeps nothing of the share pending. Returns what it skipped.
    pub fn land(self) -> io::Result<Applied> {
        let share = self.share;
        if let Some(head) = self.head {
            let Some(quarantine) = &self.quarantine else {
                return Err(io::Error::other("the quarantine directory is gone"));
            };
            let held = Repository::at(&quarantine.0);
            let yours = Repository::at(share.root.join(".git"));
            for id in &self.objects {
                let object = held.loose_object(id).map_err(read_failed)?;
                let object = object.ok_or_else(|| refused(format!("object {id} is gone")))?;
                yours.write(&object).map_err(read_failed)?;
            }
            let mut update = your_git(share.root);
            update
                .args(["update-ref", "-m", "ap apply"])
                .arg(format!("refs/heads/{}", self.branch))
                .arg(head.to_string())
                .arg("");
            let updated = output(update)?;
            if !updated.status.success() {
                return Err(failed_git("update-ref", &updated));
            }
        }
        let landed = share.read(LANDED)?.bases;
        share.keep(&self.records, &landed, &BTreeMap::new())?;
        Ok(self.applied)
    }
}
