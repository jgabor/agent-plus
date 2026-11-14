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
//! with, before it lets go of the lock.
//!
//! While a run is active, its run process holds an exclusive lock on its
//! record. The kernel drops the lock when that process exits, for
//! any reason, so a run whose process is gone never reads as active.
//!
//! A run's log is kept apart from its record, under `logs` with the run's
//! name, and only `ap run` writes it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::agents::valid_name;
use crate::layers::{create_partial, sync_up_from};

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

/// The runs in a store. Creating one touches nothing.
pub struct Runs {
    dir: PathBuf,
    logs: PathBuf,
}

impl Runs {
    /// The runs under the store root.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Runs {
            dir: root.as_ref().join("runs"),
            logs: root.as_ref().join("logs"),
        }
    }

    /// Records a new run of `agent`, which started at `started`, under a name
    /// picked at random. It picks again if the store already has a run with
    /// that name.
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
            match fs::hard_link(&partial.0, self.dir.join(&name)) {
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
    /// which marks the run active until the returned file closes. The file
    /// can add to the record, as [`record_exit`] does.
    pub fn hold(&self, name: &str) -> io::Result<File> {
        check_run_name(name)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.dir.join(name))
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => {
                    io::Error::new(error.kind(), format!("no run is named {name}"))
                }
                _ => io::Error::new(error.kind(), format!("run {name}: {error}")),
            })?;
        file.lock()
            .map_err(|error| io::Error::new(error.kind(), format!("run {name}: {error}")))?;
        Ok(file)
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
