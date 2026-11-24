//! Runs, each kept under its name as one record:
//!
//! ```text
//! AGENT notes
//! STARTED 1794567729123456789
//! ```
//!
//! `AGENT` is the agent the run started, and `STARTED` is when, in
//! nanoseconds since the Unix epoch. A record appears under its name complete
//! and synced, or not at all, and never replaces another run's. When the run
//! ends, its run process adds `EXITED <status>`, the status `ap run` exits
//! with, before it exits.
//!
//! While a run is active, an exclusive lock on its record is held. `ap run`
//! takes it with [`Runs::lock`], and its run process and share servers each
//! hold a copy of what it opened. The kernel drops the lock when the last of
//! them exits, for any reason, so a run whose processes are gone never reads
//! as active.
//!
//! A run's log is kept apart from its record, under `logs` with the run's
//! name, and only `ap run` writes it. The changes a run's agent made to its
//! copy-on-write shares wait under `pending` with the run's name, until you
//! apply or discard them. That directory holds one directory for each
//! copy-on-write share, numbered from 1 in share order:
//!
//! ```text
//! pending/k3x9q2mf/1/root      the share's directory on your host
//! pending/k3x9q2mf/1/tree      what the share server keeps, as
//! pending/k3x9q2mf/1/records   `share` says
//! ```
//!
//! `root` holds the directory's absolute path, as bytes, with no newline.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::ops::Deref;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use std::os::unix::ffi::OsStringExt;

use crate::agents::{Agents, valid_name};
use crate::layers::{create_partial, sync_up_from};

/// In a share's pending directory, the file that names its root.
const ROOT: &str = "root";
/// In a share's pending directory, the share server's record of changes.
const RECORDS: &str = "records";

/// A copy-on-write share of a run, as its pending directory keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingShare {
    /// The share's directory on your host.
    pub root: PathBuf,
    /// The directory that holds `tree` and `records`.
    pub dir: PathBuf,
}

/// The characters of a run's name.
pub const NAME_ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
/// How many characters a new run's name has.
pub const NAME_LEN: usize = 8;

/// The smallest mask of low bits that covers every index into the alphabet.
const MASK: usize = (2 << (usize::BITS - 1 - (NAME_ALPHABET.len() - 1).leading_zeros())) - 1;
/// How many random bytes to draw at a time: nanoid's estimate of what one
/// name needs, given how many bytes the mask throws away.
const STEP: usize = (16 * MASK * NAME_LEN).div_ceil(10 * NAME_ALPHABET.len());

/// A run, as the store keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub name: String,
    /// The agent the run started.
    pub agent: String,
    /// When the run started, in nanoseconds since the Unix epoch.
    pub started: u128,
    /// The status the run ended with, once it has, if its run process could
    /// record it.
    pub exited: Option<u8>,
}

/// Whether `name` can name a run: one or more characters of the alphabet.
/// New names have [`NAME_LEN`] of them, and older or newer ones may differ.
pub fn valid_run_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| NAME_ALPHABET.contains(&byte))
}

/// A new name, picked at random with nanoid's algorithm: random bytes, each
/// masked to an index, and those past the alphabet thrown away, so that every
/// character is equally likely.
pub fn random_name() -> io::Result<String> {
    let mut name = String::with_capacity(NAME_LEN);
    let mut bytes = [0; STEP];
    loop {
        getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        for byte in bytes {
            if let Some(&character) = NAME_ALPHABET.get(usize::from(byte) & MASK) {
                name.push(char::from(character));
                if name.len() == NAME_LEN {
                    return Ok(name);
                }
            }
        }
    }
}

/// Adds the status the run ended with to its record, which [`Runs::hold`]
/// opened. It is one write, so a reader sees the whole line or none of it.
pub fn record_exit(mut held: &File, status: u8) -> io::Result<()> {
    held.write_all(format!("EXITED {status}\n").as_bytes())
}

/// The lock on a run's record, which [`Runs::hold`] takes. The run is active
/// until this drops.
#[derive(Debug)]
pub struct Held(File);

impl Deref for Held {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Closing the file isn't enough: a child that another thread forked
        // has a copy of it until its exec, and that copy holds the lock as
        // well. Unlocking releases it for every copy.
        let _ = self.0.unlock();
    }
}

/// The runs in a store. Creating one touches nothing.
pub struct Runs {
    agents: Agents,
    dir: PathBuf,
    logs: PathBuf,
    pending: PathBuf,
}

impl Runs {
    /// The runs under the store root.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Runs {
            agents: Agents::new(&root),
            dir: root.as_ref().join("runs"),
            logs: root.as_ref().join("logs"),
            pending: root.as_ref().join("pending"),
        }
    }

    /// Records a new run of `agent`, which started at `started`, under a name
    /// picked at random. It picks again if the store already has a run or an
    /// agent with that name, so a name never means both.
    pub fn create(&self, agent: &str, started: u128) -> io::Result<Run> {
        self.create_with(agent, started, random_name)
    }

    /// Like [`Runs::create`], with names from `pick` instead.
    pub fn create_with(
        &self,
        agent: &str,
        started: u128,
        mut pick: impl FnMut() -> io::Result<String>,
    ) -> io::Result<Run> {
        if !valid_name(agent) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{agent:?} isn't an agent's name"),
            ));
        }
        fs::create_dir_all(&self.dir)?;
        let (partial, mut file) = create_partial(&self.dir, "run")?;
        file.write_all(format!("AGENT {agent}\nSTARTED {started}\n").as_bytes())?;
        file.sync_all()?;
        // A link, unlike a rename, fails instead of replacing a run that has
        // the name already.
        let name = loop {
            let name = pick()?;
            let record = self.dir.join(&name);
            match fs::hard_link(&partial.0, &record) {
                // Checked once the name is taken, so an agent recorded
                // before the run's name appeared is never missed.
                Ok(()) if self.agents.has(&name)? => fs::remove_file(&record)?,
                Ok(()) => break name,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        drop(partial);
        sync_up_from(&self.dir)?;
        drop(file);
        let run = Run {
            name,
            agent: agent.to_owned(),
            started,
            exited: None,
        };
        Ok(run)
    }

    /// The run with this name, if the store has one.
    pub fn get(&self, name: &str) -> io::Result<Option<Run>> {
        let Some(file) = self.open(name)? else {
            return Ok(None);
        };
        let text = io::read_to_string(file)
            .map_err(|error| io::Error::new(error.kind(), format!("run {name}: {error}")))?;
        read_record(name, &text).map(Some).map_err(|message| {
            io::Error::new(io::ErrorKind::InvalidData, format!("run {name}: {message}"))
        })
    }

    /// Every run the store has, in no order.
    pub fn list(&self) -> io::Result<Vec<Run>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut runs = Vec::new();
        for entry in entries {
            let file = entry?.file_name();
            // A partial record starts with a dot, which no name has.
            let Some(name) = file.to_str().filter(|name| valid_run_name(name)) else {
                continue;
            };
            // `ap rm` may have removed it since.
            if let Some(run) = self.get(name)? {
                runs.push(run);
            }
        }
        Ok(runs)
    }

    /// Creates the log of the run with this name, which must have none.
    pub fn create_log(&self, name: &str) -> io::Result<File> {
        check_run_name(name)?;
        fs::create_dir_all(&self.logs)?;
        // A log can hold whatever the agent printed, so it is yours alone.
        OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(self.logs.join(name))
    }

    /// The log of the run with this name, if the store has it.
    pub fn log(&self, name: &str) -> io::Result<Option<File>> {
        check_run_name(name)?;
        match File::open(self.logs.join(name)) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("the log of run {name}: {error}"),
            )),
        }
    }

    /// Makes the directory that holds the pending changes of the run with
    /// this name, which must have none, so a run never starts from another
    /// run's changes. It holds what the agent wrote, so it is yours alone.
    pub fn create_pending(&self, name: &str) -> io::Result<PathBuf> {
        check_run_name(name)?;
        if let Some(root) = self.pending.parent() {
            fs::create_dir_all(root)?;
        }
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&self.pending) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        let dir = self.pending.join(name);
        builder.create(&dir)?;
        Ok(dir)
    }

    /// Adds a directory for the next copy-on-write share of the run with this
    /// name, beneath the one [`Runs::create_pending`] made, and records that
    /// the share's directory on your host is `root`. The share server keeps
    /// the share's changes in the directory it returns.
    pub fn create_pending_share(&self, name: &str, root: &Path) -> io::Result<PathBuf> {
        check_run_name(name)?;
        let dir = self.pending.join(name);
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        let share = (1..)
            .map(|number: u32| dir.join(number.to_string()))
            .find(|share| !share.exists())
            .unwrap_or_default();
        builder.create(&share)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(share.join(ROOT))?;
        file.write_all(root.as_os_str().as_encoded_bytes())?;
        Ok(share)
    }

    /// The names of the runs whose pending directory is in the store, in no
    /// order. A run's record may be gone.
    pub fn list_pending(&self) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(&self.pending) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries {
            if let Some(name) = entry?
                .file_name()
                .to_str()
                .filter(|name| valid_run_name(name))
            {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }

    /// The copy-on-write shares of the run with this name, in share order:
    /// each one's directory on your host, and the directory that keeps its
    /// changes. A run with no pending directory has none.
    pub fn pending_shares(&self, name: &str) -> io::Result<Vec<PendingShare>> {
        check_run_name(name)?;
        let dir = self.pending.join(name);
        let mut shares = Vec::new();
        for number in 1.. {
            let share = dir.join(number.to_string());
            let root = match fs::read(share.join(ROOT)) {
                Ok(root) => PathBuf::from(std::ffi::OsString::from_vec(root)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error),
            };
            shares.push(PendingShare { root, dir: share });
        }
        Ok(shares)
    }

    /// Whether the run with this name has pending changes: whether the share
    /// server recorded a change in any of its copy-on-write shares.
    pub fn has_pending(&self, name: &str) -> io::Result<bool> {
        for share in self.pending_shares(name)? {
            match fs::metadata(share.dir.join(RECORDS)) {
                Ok(metadata) if metadata.len() > 0 => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    /// Throws away the pending changes of the run with this name, if it has
    /// any.
    /// A run's copy of your `.git` keeps your modes, so a directory you
    /// can't write to is made writable first.
    pub fn discard(&self, name: &str) -> io::Result<()> {
        check_run_name(name)?;
        let dir = self.pending.join(name);
        let removed = match fs::remove_dir_all(&dir) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                make_writable(&dir)?;
                fs::remove_dir_all(&dir)
            }
            removed => removed,
        };
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Removes the run with this name and its log. The log goes first, so a
    /// run is never left with a log and no record.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        check_run_name(name)?;
        for path in [self.logs.join(name), self.dir.join(name)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Takes the exclusive lock on the record of the run with this name,
    /// which marks the run active until the returned hold drops. Its file
    /// can add to the record, as [`record_exit`] does.
    pub fn hold(&self, name: &str) -> io::Result<Held> {
        let file = self.appending(name)?;
        lock(name, &file)?;
        Ok(Held(file))
    }

    /// Opens the record of the run with this name to add to it, as
    /// [`record_exit`] does, and takes no lock.
    pub fn appending(&self, name: &str) -> io::Result<File> {
        self.open_record(name, OpenOptions::new().read(true).append(true))
    }

    /// Opens the record of the run with this name to read, and takes the
    /// exclusive lock on what it opened. The lock belongs to the open file,
    /// not to this process: each copy of the file, in any process, holds it
    /// until the last copy closes, so the run is active until then.
    pub fn lock(&self, name: &str) -> io::Result<File> {
        let file = self.open_record(name, OpenOptions::new().read(true))?;
        lock(name, &file)?;
        Ok(file)
    }

    fn open_record(&self, name: &str, options: &OpenOptions) -> io::Result<File> {
        check_run_name(name)?;
        options
            .open(self.dir.join(name))
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => {
                    io::Error::new(error.kind(), format!("no run is named {name}"))
                }
                _ => io::Error::new(error.kind(), format!("run {name}: {error}")),
            })
    }

    /// Whether the run with this name is active: whether a process holds the
    /// lock on its record. A run the store doesn't have isn't.
    pub fn active(&self, name: &str) -> io::Result<bool> {
        let Some(file) = self.open(name)? else {
            return Ok(false);
        };
        match file.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    fn open(&self, name: &str) -> io::Result<Option<File>> {
        check_run_name(name)?;
        match File::open(self.dir.join(name)) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io::Error::new(error.kind(), format!("run {name}: {error}"))),
        }
    }
}

fn lock(name: &str, file: &File) -> io::Result<()> {
    file.lock()
        .map_err(|error| io::Error::new(error.kind(), format!("run {name}: {error}")))
}

fn check_run_name(name: &str) -> io::Result<()> {
    if valid_run_name(name) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name:?} isn't a run's name"),
    ))
}

fn read_record(name: &str, text: &str) -> Result<Run, String> {
    let mut agent = None;
    let mut started = None;
    let mut exited = None;
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("AGENT", value)) if agent.is_none() && valid_name(value) => {
                agent = Some(value.to_owned());
            }
            Some(("STARTED", value)) if started.is_none() => {
                started = Some(value.parse().map_err(|_| format!("bad line {line:?}"))?);
            }
            Some(("EXITED", value)) if exited.is_none() => {
                exited = Some(value.parse().map_err(|_| format!("bad line {line:?}"))?);
            }
            _ => return Err(format!("bad line {line:?}")),
        }
    }
    Ok(Run {
        name: name.to_owned(),
        agent: agent.ok_or("no AGENT line")?,
        started: started.ok_or("no STARTED line")?,
        exited,
    })
}

/// Lets you read, write, and search `dir` and each directory beneath it,
/// following no symlink.
fn make_writable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700))?;
    for entry in fs::read_dir(dir)? {
        make_writable(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanoid_s_mask_and_step_for_this_alphabet() {
        assert_eq!(MASK, 63);
        // ceil(1.6 * 63 * 8 / 36), as nanoid computes it.
        assert_eq!(STEP, 23);
    }

    // spec(Run): **Run name.** Every run has a name of 8 characters from `0123456789abcdefghijklmnopqrstuvwxyz`, such as `k3x9q2mf`.
    #[test]
    fn a_new_name_has_8_characters_from_the_alphabet() {
        let names: Vec<String> = (0..1000).map(|_| random_name().unwrap()).collect();
        for name in &names {
            assert_eq!(name.len(), 8, "{name}");
            assert!(valid_run_name(name), "{name}");
        }
        // Every character turns up, so none of the alphabet is cut off.
        for character in NAME_ALPHABET {
            assert!(
                names.iter().any(|name| name.as_bytes().contains(character)),
                "{} never turned up",
                char::from(*character)
            );
        }
        assert!(valid_run_name("k3x9q2mf"));
        for bad in ["", "K3X9Q2MF", "k3x9-q2m", ".run.1.0"] {
            assert!(!valid_run_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_record_reads_back_and_a_bad_one_is_an_error() {
        let run = read_record("k3x9q2mf", "AGENT notes\nSTARTED 12\n").unwrap();
        assert_eq!(run.agent, "notes");
        assert_eq!(run.started, 12);
        assert_eq!(run.exited, None);
        let run = read_record("k3x9q2mf", "AGENT notes\nSTARTED 12\nEXITED 137\n").unwrap();
        assert_eq!(run.exited, Some(137));
        for bad in [
            "",
            "AGENT notes\n",
            "STARTED 12\n",
            "AGENT Notes\nSTARTED 12\n",
            "AGENT notes\nSTARTED soon\n",
            "AGENT notes\nAGENT notes\nSTARTED 12\n",
            "AGENT notes\nSTARTED 12\nPORT 80\n",
            "AGENT notes\nSTARTED 12\nEXITED 256\n",
            "AGENT notes\nSTARTED 12\nEXITED 1\nEXITED 2\n",
        ] {
            assert!(read_record("k3x9q2mf", bad).is_err(), "{bad:?}");
        }
    }
}
