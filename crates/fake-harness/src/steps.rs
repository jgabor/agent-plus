//! The steps a prompt asks for: `put`, `delete`, and `commit`.

use git::{Id, Kind, Object, Repository, Target, TreeEntry};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Who makes the commits.
const IDENTITY: &str = "Fake Harness <fake-harness@localhost>";

/// Takes each step in `prompt`, one per line, in order, and stops at the
/// first one that fails. A line that starts with no step's word is left
/// alone, so a prompt can say other things too.
pub fn take(prompt: &str) -> Result<(), String> {
    // What changed since the last commit, by path: a file's mode in git and
    // its new contents, or `None` for a deleted file.
    let mut changes: Changes = BTreeMap::new();
    for line in prompt.lines() {
        let line = line.trim_end_matches('\r');
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "put" => {
                let (path, text) = rest
                    .split_once(' ')
                    .filter(|(_, text)| !text.is_empty())
                    .ok_or_else(|| format!("`{line}` needs a path and some text"))?;
                let parts = parts(path)?;
                let contents = format!("{text}\n").into_bytes();
                if let Some(parent) = Path::new(path).parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("couldn't make {}: {e}", parent.display()))?;
                }
                let mode = fs::write(path, &contents)
                    .and_then(|()| fs::metadata(path))
                    .map_err(|e| format!("couldn't put {path}: {e}"))?
                    .permissions()
                    .mode();
                // `put` keeps the mode of a file it replaces, and git keeps
                // only whether it is executable.
                let mode = if mode & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                };
                changes.insert(parts, Some((mode, contents)));
            }
            "delete" => {
                let parts = parts(rest)?;
                fs::remove_file(rest).map_err(|e| format!("couldn't delete {rest}: {e}"))?;
                changes.insert(parts, None);
            }
            "commit" => {
                if rest.is_empty() {
                    return Err(format!("`{line}` needs a message"));
                }
                commit(&changes, rest).map_err(|e| format!("couldn't commit: {e}"))?;
                changes.clear();
            }
            _ => {}
        }
    }
    Ok(())
}

/// The components of a relative path, which stays below the working
/// directory.
fn parts(path: &str) -> Result<Vec<String>, String> {
    let parts: Vec<String> = path.split('/').map(str::to_string).collect();
    if path.is_empty() || parts.iter().any(|p| matches!(p.as_str(), "" | "." | "..")) {
        return Err(format!("{path:?} isn't a path below the working directory"));
    }
    Ok(parts)
}

/// The files changed since the last commit.
type Changes = BTreeMap<Vec<String>, Option<(u32, Vec<u8>)>>;

/// Commits `changes` on top of the commit `HEAD` names, in `.git` in the
/// working directory, and moves `HEAD`, or the branch it names, to it. The
/// index stays as it was, as nothing reads it here.
fn commit(changes: &Changes, message: &str) -> Result<(), String> {
    if changes.is_empty() {
        return Err("nothing changed since the last commit".to_string());
    }
    let git_dir = PathBuf::from(".git");
    let repository = Repository::at(&git_dir);
    let failed = |e: git::Error| e.to_string();
    let (name, parent) = match repository.find_ref("HEAD").map_err(failed)? {
        None => return Err("there is no repository in .git".to_string()),
        Some(Target::Symbolic(name)) => {
            let parent = repository.resolve(&name).map_err(failed)?;
            (name, parent)
        }
        Some(Target::Object(id)) => ("HEAD".to_string(), Some(id)),
    };
    let mut tree = match parent {
        None => None,
        Some(parent) => Some(
            read(&repository, &parent, Kind::Commit)?
                .commit()
                .map_err(failed)?
                .tree,
        ),
    };
    for (parts, contents) in changes {
        let blob = match contents {
            None => None,
            Some((mode, contents)) => {
                let blob = Object {
                    kind: Kind::Blob,
                    data: contents.clone(),
                };
                Some((*mode, write(&git_dir, &repository, &blob)?))
            }
        };
        tree = edit(&git_dir, &repository, tree, parts, blob)?;
    }
    let tree = match tree {
        Some(tree) => tree,
        None => write(&git_dir, &repository, &Object::tree(Vec::new()))?,
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let parents: Vec<Id> = parent.into_iter().collect();
    let commit = Object::new_commit(tree, &parents, IDENTITY, time, &format!("{message}\n"));
    let id = write(&git_dir, &repository, &commit)?;
    replace(&git_dir.join(&name), format!("{id}\n").as_bytes())
        .map_err(|e| format!("couldn't move {name}: {e}"))
}

/// The tree `tree` with the file at `parts` set to `blob`, a mode and an
/// object, or removed for
/// `None`. An empty tree is `None`, because git keeps no empty directory.
fn edit(
    git_dir: &Path,
    repository: &Repository,
    tree: Option<Id>,
    parts: &[String],
    blob: Option<(u32, Id)>,
) -> Result<Option<Id>, String> {
    let mut entries = match tree {
        None => Vec::new(),
        Some(tree) => read(repository, &tree, Kind::Tree)?
            .entries()
            .map_err(|e| e.to_string())?,
    };
    let name = parts[0].as_bytes();
    let old = entries.iter().position(|entry| entry.name == name);
    let old = old.map(|at| entries.remove(at));
    let new = if parts.len() == 1 {
        blob
    } else {
        let below = old.filter(|old| old.mode == 0o40000).map(|old| old.id);
        edit(git_dir, repository, below, &parts[1..], blob)?.map(|id| (0o40000, id))
    };
    if let Some((mode, id)) = new {
        entries.push(TreeEntry {
            mode,
            name: name.to_vec(),
            id,
        });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    write(git_dir, repository, &Object::tree(entries)).map(Some)
}

/// The object `id`, which must be a `kind`.
fn read(repository: &Repository, id: &Id, kind: Kind) -> Result<Object, String> {
    match repository.object(id).map_err(|e| e.to_string())? {
        Some(object) if object.kind == kind => Ok(object),
        Some(object) => Err(format!("{id} is a {}, not a {kind}", object.kind)),
        None => Err(format!("{id} is missing")),
    }
}

/// Writes `object` as a loose object, unless the repository has it. It
/// renames a temporary file into place, because a copy-on-write share
/// refuses the hard link that [`Repository::write`] makes.
fn write(git_dir: &Path, repository: &Repository, object: &Object) -> Result<Id, String> {
    let id = object.id();
    if repository.contains(&id).map_err(|e| e.to_string())? {
        return Ok(id);
    }
    let hex = id.to_string();
    let mut raw = format!("{} {}\0", object.kind, object.data.len()).into_bytes();
    raw.extend_from_slice(&object.data);
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let dir = git_dir.join("objects").join(&hex[..2]);
    fs::create_dir_all(&dir)
        .and_then(|()| replace(&dir.join(&hex[2..]), &compressed))
        .map_err(|e| format!("couldn't write {hex}: {e}"))?;
    Ok(id)
}

/// Writes `contents` beside `path`, then renames it into place.
fn replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".lock");
    fs::write(&temp, contents)?;
    fs::rename(&temp, path)
}
