//! Reads a remote source, by SPEC.md's Sources section, and turns a `REPO`
//! of one into a layer, by its `### REPO`.
//!
//! The `git` on your host does what touches the network. [`resolve`] asks
//! the remote which commit a ref names with `git ls-remote`, and [`fetch`]
//! runs `git fetch` for that commit into an empty bare repository in the
//! download cache, under `git/<commit>`, or `git/<commit>.history` for the
//! whole history. Each runs with credentials off: no configuration but
//! git's defaults, no credential helper, no `.netrc`, no ssh key or agent,
//! and no prompt. `git` may use only the `https` and `ssh` transports, checks
//! every object it receives, checks nothing out, and runs no hook.
//!
//! [`Repo::image`] reads the fetched objects with `crates/git`, and writes
//! the clone: the commit's tree, and a `.git` that holds the fetched packs,
//! the refs, and an index that the host's `git read-tree` writes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Cursor};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use git::{Id, Kind, Repository};
use graviola::hashing::sha2::Sha256Context;

use crate::printable;

/// The ref a source without one names: the remote's default branch.
pub const DEFAULT: &str = "HEAD";

/// How deep a tree may nest, as git's default `core.maxTreeDepth`.
const MAX_DEPTH: usize = 2048;

/// What a ref resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The full name of the ref it resolved through, such as
    /// `refs/heads/main`, or `None` for a commit.
    pub name: Option<String>,
    pub commit: Id,
}

/// The commit that `reference` names in the repository at `url`: the
/// default branch for [`DEFAULT`], the commit itself for 40 hex digits,
/// and otherwise the first of `refs/<ref>`, `refs/tags/<ref>`, and
/// `refs/heads/<ref>` that the remote has, or `<ref>` alone when it starts
/// with `refs/`, as `git fetch` picks. A tag resolves to the commit it
/// names. `cache` is the download cache, which holds what `git` works in.
pub fn resolve(url: &str, reference: &str, cache: &Path) -> io::Result<Resolved> {
    if let Some(commit) = Id::from_hex(reference) {
        return Ok(Resolved { name: None, commit });
    }
    let work = Work::new(cache)?;
    let mut command = work.git();
    // An annotated tag is listed again, peeled, as `<tag>^{}`, which only
    // its own pattern matches.
    let peeled = format!("{reference}^{{}}");
    command.args(["ls-remote", "--symref", "--", url, reference, &peeled]);
    let output = run(command, "ls-remote")?;
    let listed = String::from_utf8_lossy(&output.stdout);
    let mut symbolic = None;
    let mut refs = BTreeMap::new();
    for line in listed.lines() {
        let Some((value, name)) = line.split_once('\t') else {
            continue;
        };
        match value.strip_prefix("ref: ") {
            Some(target) if name == DEFAULT => symbolic = Some(target.to_string()),
            Some(_) => {}
            None => {
                if let Some(id) = Id::from_hex(value) {
                    refs.insert(name.to_string(), id);
                }
            }
        }
    }
    let candidates = match reference {
        DEFAULT => vec![DEFAULT.to_string()],
        _ if reference.starts_with("refs/") => vec![reference.to_string()],
        _ => ["refs/", "refs/tags/", "refs/heads/"]
            .iter()
            .map(|prefix| format!("{prefix}{reference}"))
            .collect(),
    };
    for candidate in candidates {
        let Some(id) = refs.get(&candidate) else {
            continue;
        };
        let commit = *refs.get(&format!("{candidate}^{{}}")).unwrap_or(id);
        let name = match candidate.as_str() {
            DEFAULT => symbolic.clone().filter(|name| check_ref(name)),
            _ => Some(candidate).filter(|name| check_ref(name)),
        };
        return Ok(Resolved { name, commit });
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("`{}` names no branch, tag, or commit", printable(reference)),
    ))
}

/// The bare repository in the download cache `cache` that holds `commit`
/// from the repository at `url`, with its whole history if `history`. One
/// that the cache doesn't hold yet is fetched.
pub fn fetch(url: &str, commit: &Id, history: bool, cache: &Path) -> io::Result<PathBuf> {
    let dir = cache.join("git");
    let name = match history {
        true => format!("{commit}.history"),
        false => commit.to_string(),
    };
    let fetched = dir.join(name);
    if fs::symlink_metadata(&fetched).is_ok() {
        return Ok(fetched);
    }
    let work = Work::new(cache)?;
    let repository = work.dir.join("repository");
    let mut init = work.git();
    init.args(["init", "--quiet", "--bare", "--template="])
        .arg(&repository);
    run(init, "init")?;
    let mut command = work.git();
    command
        .arg("--git-dir")
        .arg(&repository)
        // Everything stays in packs, so a clone can copy them as they are.
        .args(["-c", "fetch.unpackLimit=1", "-c", "transfer.unpackLimit=1"])
        .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
        .arg("--no-recurse-submodules");
    if !history {
        command.arg("--depth=1");
    }
    command.args(["--", url, &commit.to_string()]);
    run(command, "fetch")?;
    match Repository::at(&repository).kind(commit) {
        Ok(Some(Kind::Commit)) => {}
        Ok(Some(kind)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{commit} in {} is a {kind}, not a commit", printable(url)),
            ));
        }
        Ok(None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} didn't send {commit}", printable(url)),
            ));
        }
        Err(error) => return Err(read_failed(error)),
    }
    match fs::rename(&repository, &fetched) {
        Ok(()) => Ok(fetched),
        // Another `ap` fetched it first.
        Err(_) if fs::symlink_metadata(&fetched).is_ok() => Ok(fetched),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", fetched.display()),
        )),
    }
}

/// A directory of its own in the download cache, which holds what `git`
/// works in and an empty home for it, and which goes when this does.
struct Work {
    dir: PathBuf,
}

impl Work {
    fn new(cache: &Path) -> io::Result<Work> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = cache.join("git");
        let failed = |error: io::Error| {
            io::Error::new(error.kind(), format!("{}: {error}", parent.display()))
        };
        fs::create_dir_all(&parent).map_err(failed)?;
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = parent.join(format!(".partial.{}.{id}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    fs::create_dir(dir.join("home")).map_err(failed)?;
                    return Ok(Work { dir });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(failed(error)),
            }
        }
    }

    /// `git`, with credentials off, as this module says. Of the environment
    /// that names git, only `GIT_SSL_CAINFO`, the certificates you trust,
    /// stays.
    fn git(&self) -> Command {
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            let bytes = name.as_bytes();
            if bytes.starts_with(b"GIT_") && bytes != b"GIT_SSL_CAINFO" {
                command.env_remove(&name);
            }
        }
        for name in ["SSH_ASKPASS", "SSH_AUTH_SOCK"] {
            command.env_remove(name);
        }
        let config = [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "protocol.ssh.allow=always",
            "transfer.fsckObjects=true",
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "credential.helper=",
            "gc.auto=0",
            "maintenance.auto=false",
            "fetch.writeCommitGraph=false",
            "init.defaultBranch=main",
        ];
        for setting in config {
            command.args(["-c", setting]);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_VARIANT", "ssh")
            .env(
                "GIT_SSH_COMMAND",
                "ssh -F /dev/null -o BatchMode=yes -o IdentityAgent=none \
                 -o PubkeyAuthentication=no -o PasswordAuthentication=no \
                 -o KbdInteractiveAuthentication=no -o GSSAPIAuthentication=no \
                 -o HostbasedAuthentication=no",
            )
            // An empty home, so no `.netrc` gives git a password.
            .env("HOME", self.dir.join("home"))
            // No repository around the directory it works in, such as your
            // project's, gives git its configuration.
            .env(
                "GIT_CEILING_DIRECTORIES",
                self.dir.parent().unwrap_or(&self.dir),
            )
            .current_dir(&self.dir)
            .stdin(Stdio::null());
        command
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What `git <what>` printed, once it succeeded.
fn run(mut command: Command, what: &str) -> io::Result<Output> {
    let output = match command.output() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                error.kind(),
                "`ap build` needs `git` on your host to fetch a remote source, and there is none",
            ));
        }
        output => output?,
    };
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "git {what} failed: {}",
            printable(said.trim_end())
        )));
    }
    Ok(output)
}

fn read_failed(error: git::Error) -> io::Error {
    match error {
        git::Error::Io(error) => error,
        git::Error::Corrupt(why) => io::Error::new(io::ErrorKind::InvalidData, printable(&why)),
    }
}

/// Whether `name` is a ref name `git check-ref-format` accepts, so that it
/// can go in a `.git` as it is.
pub(crate) fn check_ref(name: &str) -> bool {
    let bad = |c: char| c.is_ascii_control() || " ~^:?*[\\".contains(c);
    name.contains('/')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with('.')
        && !name.chars().any(bad)
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// One `REPO` line, resolved, which [`Repo::image`] turns into a layer.
#[derive(Clone, Debug)]
pub struct Repo {
    /// The URL it was fetched from, which the clone's `origin` names.
    pub url: String,
    pub resolved: Resolved,
    /// The absolute guest path it lands at.
    pub guest: String,
    pub history: bool,
}

impl Repo {
    /// The layer's key: the SHA-256 of everything the image is made from,
    /// in lowercase hex.
    pub fn key(&self) -> String {
        let mut hash = Sha256Context::new();
        let mut field = |bytes: &[u8]| {
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        };
        field(b"REPO");
        field(self.url.as_bytes());
        field(&self.resolved.commit.0);
        field(self.resolved.name.as_deref().unwrap_or("").as_bytes());
        field(self.guest.as_bytes());
        field(&[u8::from(self.history)]);
        hash.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The layer as an uncompressed EROFS image, from the bare repository
    /// `fetched`: the commit's tree at the guest path, with a `.git` beside
    /// it, all owned by `agent`.
    /// `cache` is the download cache, where `git` writes the index.
    pub fn image(&self, fetched: &Path, cache: &Path) -> io::Result<Vec<u8>> {
        let repository = Repository::at(fetched);
        let commit = self.resolved.commit;
        let tree = (repository.object(&commit).map_err(read_failed)?)
            .ok_or_else(|| missing(&commit))?
            .commit()
            .map_err(read_failed)?
            .tree;
        let mut root = worktree(&repository, &tree, 0)?;
        root.entries.insert(
            b".git".to_vec(),
            erofs::Node::Dir(self.git_dir(fetched, cache)?),
        );

        let names: Vec<&str> = (self.guest.split('/'))
            .filter(|name| !name.is_empty())
            .collect();
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a `REPO` can't land at `/`",
            ));
        };
        let mut top = BTreeMap::from([(last.as_bytes().to_vec(), erofs::Node::Dir(root))]);
        for name in parents.iter().rev() {
            let dir = erofs::Node::Dir(erofs::Dir {
                mode: 0o755,
                entries: top,
            });
            top = BTreeMap::from([(name.as_bytes().to_vec(), dir)]);
        }
        let content = names.join("/").into_bytes();
        let owner = |path: &[u8]| crate::add::entry_owner(&content, path);
        let mut image = Vec::new();
        let root = erofs::Dir {
            mode: 0o755,
            entries: top,
        };
        erofs::write_owned(root, &owner, &mut image).map_err(|error| match error {
            erofs::Error::Io(error) => error,
            error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })?;
        Ok(image)
    }

    /// The clone's `.git`: `HEAD`, a branch for a branch and a detached
    /// `HEAD` otherwise, `origin` at the URL, the fetched packs, `shallow`
    /// as the fetch wrote it, and the commit's index.
    fn git_dir(&self, fetched: &Path, cache: &Path) -> io::Result<erofs::Dir> {
        let commit = self.resolved.commit;
        let branch =
            (self.resolved.name.as_deref()).and_then(|name| name.strip_prefix("refs/heads/"));
        let mut config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\
                          \tbare = false\n\tlogallrefupdates = true\n"
            .to_string();
        config.push_str(&format!(
            "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            quoted(&self.url)
        ));
        let (head, refs) = match branch {
            Some(branch) => {
                config.push_str(&format!(
                    "[branch {}]\n\tremote = origin\n\tmerge = refs/heads/{branch}\n",
                    quoted(branch)
                ));
                (
                    format!("ref: refs/heads/{branch}\n"),
                    format!(
                        "# pack-refs with: peeled fully-peeled sorted \n\
                         {commit} refs/heads/{branch}\n{commit} refs/remotes/origin/{branch}\n"
                    ),
                )
            }
            None => (format!("{commit}\n"), String::new()),
        };
        let mut git = dir(BTreeMap::new());
        let mut put = |name: &str, data: Vec<u8>| {
            git.entries
                .insert(name.as_bytes().to_vec(), file(0o644, data));
        };
        put("HEAD", head.into_bytes());
        put("config", config.into_bytes());
        if !refs.is_empty() {
            put("packed-refs", refs.into_bytes());
        }
        match fs::read(fetched.join("shallow")) {
            Ok(shallow) => put("shallow", shallow),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        put("index", index(fetched, &commit, cache)?);
        let refs = BTreeMap::from([
            (b"heads".to_vec(), erofs::Node::Dir(dir(BTreeMap::new()))),
            (b"tags".to_vec(), erofs::Node::Dir(dir(BTreeMap::new()))),
        ]);
        git.entries
            .insert(b"refs".to_vec(), erofs::Node::Dir(dir(refs)));
        let mut packs = BTreeMap::new();
        for entry in fs::read_dir(fetched.join("objects/pack"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let data = fs::read(entry.path())?;
                packs.insert(entry.file_name().as_bytes().to_vec(), file(0o444, data));
            }
        }
        let objects = BTreeMap::from([(b"pack".to_vec(), erofs::Node::Dir(dir(packs)))]);
        git.entries
            .insert(b"objects".to_vec(), erofs::Node::Dir(dir(objects)));
        Ok(git)
    }
}

/// The index of `commit`'s tree, as the host's `git read-tree` writes it
/// from the bare repository `fetched`, in the download cache `cache`.
fn index(fetched: &Path, commit: &Id, cache: &Path) -> io::Result<Vec<u8>> {
    let work = Work::new(cache)?;
    let output = work.dir.join("index");
    let mut command = work.git();
    command
        .arg("--git-dir")
        .arg(fetched)
        .arg("read-tree")
        .arg(OsStr::from_bytes(
            &[b"--index-output=", output.as_os_str().as_bytes()].concat(),
        ))
        .arg(commit.to_string());
    run(command, "read-tree")?;
    fs::read(&output)
}

/// Quotes a value of git's configuration, so that it reads back as it is.
fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn missing(id: &Id) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the fetched repository has no object {id}"),
    )
}

fn dir(entries: BTreeMap<Vec<u8>, erofs::Node>) -> erofs::Dir {
    erofs::Dir {
        mode: 0o755,
        entries,
    }
}

fn file(mode: u16, data: Vec<u8>) -> erofs::Node {
    erofs::Node::File(erofs::File {
        mode,
        size: data.len() as u64,
        content: Box::new(Cursor::new(data)),
    })
}

/// The working tree of the tree `id`, as `git checkout` writes it. A
/// submodule is an empty directory, as git leaves one it doesn't fetch. A
/// name that git wouldn't check out, such as `.git`, is an error.
fn worktree(repository: &Repository, id: &Id, depth: usize) -> io::Result<erofs::Dir> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the tree nests deeper than {MAX_DEPTH} directories"),
        ));
    }
    let tree = repository
        .object(id)
        .map_err(read_failed)?
        .ok_or_else(|| missing(id))?;
    let mut entries = BTreeMap::new();
    for entry in tree.entries().map_err(read_failed)? {
        let name = entry.name;
        if matches!(&name[..], b"" | b"." | b"..")
            || name.eq_ignore_ascii_case(b".git")
            || name.contains(&b'/')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the tree has an entry git won't check out, `{}`",
                    printable(&String::from_utf8_lossy(&name))
                ),
            ));
        }
        let blob = |id: &Id| -> io::Result<Vec<u8>> {
            let object = repository
                .object(id)
                .map_err(read_failed)?
                .ok_or_else(|| missing(id))?;
            if object.kind != Kind::Blob {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{id} is a {}, not a blob", object.kind),
                ));
            }
            Ok(object.data)
        };
        let node = match entry.mode {
            0o40000 => erofs::Node::Dir(worktree(repository, &entry.id, depth + 1)?),
            0o160000 => erofs::Node::Dir(dir(BTreeMap::new())),
            0o120000 => erofs::Node::Symlink(erofs::Symlink {
                mode: 0o777,
                target: blob(&entry.id)?,
            }),
            0o100755 => file(0o755, blob(&entry.id)?),
            0o100644 | 0o100664 => file(0o644, blob(&entry.id)?),
            mode => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the tree has an entry with mode {mode:o}"),
                ));
            }
        };
        entries.insert(name, node);
    }
    Ok(dir(entries))
}
