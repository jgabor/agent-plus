//! Built agents, each kept under its name as one record, and the layers they
//! use.
//!
//! A record is a file named after the agent, in the Agentfile's line syntax
//! and with its argument reader, as SPEC.md's Build section and the grants'
//! text form use them:
//!
//! ```text
//! DIRECTORY /home/me/notes
//! AGENTFILE /home/me/notes/Agentfile
//! LAYER 0b1f63c4… 9829b1f2… "ADD ./src /work"
//! AGENT claude
//! SHARE ./out /work/out rw
//! ```
//!
//! `DIRECTORY` is the directory the agent was built from, and `AGENTFILE` is
//! the canonical path of the Agentfile that the build read, which keys its
//! approval. Each `LAYER` holds
//! a layer's key and its digest, shortened here, and the instruction that
//! made it, in the agent's order. Every other line is an instruction the Agentfile declared,
//! with its arguments.
//!
//! A record appears under its name complete and synced, or not at all.
//! A record names only layers that are synced. Writers hold a lock on the
//! store's agents while they change one, and free a layer only when no
//! record still names it. A layer that a build has put
//! and not yet recorded can be freed in that time, so recording it is then
//! an error, and the build puts it again.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentfile::{INSTRUCTIONS, print_argument, read_arguments};

use crate::Layers;
use crate::layers::{create_partial, is_digest, sync_up_from};

/// A built agent, as the store keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// A name, as [`valid_name`] says.
    pub name: String,
    /// The canonical directory the agent was built from.
    pub directory: String,
    /// The canonical path of the Agentfile the build read. Its approval is
    /// kept with this path.
    pub agentfile: String,
    /// The agent's layers, in order.
    pub layers: Vec<Layer>,
    /// The instructions the Agentfile declared, in order.
    pub declared: Vec<Declared>,
}

/// One of an agent's layers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// What the layer was built from: its instruction, arguments, and inputs.
    pub key: String,
    /// The SHA-256 of the layer's uncompressed content.
    pub digest: String,
    /// The instruction that made the layer, as `ap build` reports it, such
    /// as `ADD ./src /work` or `harness configuration`.
    pub instruction: String,
}

/// An instruction that an Agentfile declared. `SYSTEM` and `PROMPT` take
/// text instead of arguments, so a record can't hold them. Their text goes
/// into the harness configuration layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The instruction's name, in uppercase.
    pub name: String,
    pub arguments: Vec<String>,
}

/// Whether `name` can name an agent: 1 to 63 lowercase letters, digits, and
/// `-`, starting and ending with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let letter_or_digit = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.iter().all(|b| letter_or_digit(b) || *b == b'-')
        && bytes.first().is_some_and(letter_or_digit)
        && bytes.last().is_some_and(letter_or_digit)
}

/// The agents in a store. Creating one touches nothing.
pub struct Agents {
    dir: PathBuf,
    layers: Layers,
}

impl Agents {
    /// The agents under the store root, and the layers they use.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Agents {
            dir: root.as_ref().join("agents"),
            layers: Layers::new(root),
        }
    }

    /// Records `agent` under its name, and frees the layers that only the
    /// record it replaces used. Every layer must already be in the store, or
    /// the error is `NotFound`. A record under the same name from another
    /// directory is an `AlreadyExists` error, unless `replace` is set.
    pub fn record(&self, agent: &Agent, replace: bool) -> io::Result<()> {
        let text = record_text(agent)?;
        let _lock = self.lock()?;
        let old = self.read(&agent.name)?;
        if let Some(old) = &old
            && old.directory != agent.directory
            && !replace
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "agent {} was built from {}, not {}",
                    agent.name, old.directory, agent.directory
                ),
            ));
        }
        for layer in &agent.layers {
            if !self.layers.path(&layer.digest).try_exists()? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("the store has no layer {}", layer.digest),
                ));
            }
        }
        if !agent.layers.is_empty() {
            self.layers.sync()?;
        }
        let (partial, mut file) = create_partial(&self.dir, &agent.name)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&partial.0, self.dir.join(&agent.name))?;
        sync_up_from(&self.dir)?;
        if let Some(old) = old {
            self.free(old.layers)?;
        }
        Ok(())
    }

    /// The agent with this name, if the store has one.
    pub fn get(&self, name: &str) -> io::Result<Option<Agent>> {
        check_name(name)?;
        self.read(name)
    }

    /// Every agent in the store, by name.
    pub fn list(&self) -> io::Result<Vec<Agent>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(name) = name.to_str().filter(|name| valid_name(name)) else {
                // The lock, and unfinished records, start with a dot.
                if name.as_encoded_bytes().starts_with(b".") {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} isn't an agent's name", name.display()),
                ));
            };
            names.push(name.to_string());
        }
        names.sort();
        let mut agents = Vec::new();
        for name in names {
            // Another writer may have removed it since.
            agents.extend(self.read(&name)?);
        }
        Ok(agents)
    }

    /// Removes the agent with this name, and frees the layers no other agent
    /// uses. It returns whether there was one.
    pub fn remove(&self, name: &str) -> io::Result<bool> {
        check_name(name)?;
        let _lock = self.lock()?;
        let Some(agent) = self.read(name)? else {
            return Ok(false);
        };
        fs::remove_file(self.dir.join(name))?;
        File::open(&self.dir)?.sync_all()?;
        self.free(agent.layers)?;
        Ok(true)
    }

    /// The digest of a layer with this key that an agent recorded, so that a
    /// build can reuse it.
    pub fn layer(&self, key: &str) -> io::Result<Option<String>> {
        Ok(self
            .list()?
            .into_iter()
            .flat_map(|agent| agent.layers)
            .find(|layer| layer.key == key)
            .map(|layer| layer.digest))
    }

    /// Removes each of these layers that no record names. The caller holds
    /// the lock.
    fn free(&self, layers: Vec<Layer>) -> io::Result<()> {
        let used: HashSet<String> = self
            .list()?
            .into_iter()
            .flat_map(|agent| agent.layers)
            .map(|layer| layer.digest)
            .collect();
        for layer in layers {
            if used.contains(&layer.digest) {
                continue;
            }
            match fs::remove_file(self.layers.path(&layer.digest)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Takes the lock on the store's agents, which lasts until the file
    /// closes.
    fn lock(&self) -> io::Result<File> {
        fs::create_dir_all(&self.dir)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(".lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Reads the record with this name, which the caller has checked.
    fn read(&self, name: &str) -> io::Result<Option<Agent>> {
        let text = match fs::read_to_string(self.dir.join(name)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("agent {name}: {error}"),
                ));
            }
        };
        read_record(name, &text).map(Some).map_err(|message| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("agent {name}: {message}"),
            )
        })
    }
}

fn check_name(name: &str) -> io::Result<()> {
    if valid_name(name) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{name:?} isn't a name: a name is 1 to 63 lowercase letters, digits, and `-`, starting and ending with a letter or digit"
        ),
    ))
}

/// Whether a record can hold this instruction: one that takes arguments.
fn declarable(name: &str) -> bool {
    name != "SYSTEM" && name != "PROMPT" && INSTRUCTIONS.iter().any(|d| d.name == name)
}

/// The text of `agent`'s record, after checking each part of it.
fn record_text(agent: &Agent) -> io::Result<String> {
    let invalid = |message: String| Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    check_name(&agent.name)?;
    if !agent.directory.starts_with('/') {
        return invalid(format!(
            "an agent's directory is a canonical path, but found {:?}",
            agent.directory
        ));
    }
    if !agent.agentfile.starts_with('/') {
        return invalid(format!(
            "an agent's Agentfile is a canonical path, but found {:?}",
            agent.agentfile
        ));
    }
    let mut text = String::new();
    let mut line = |name: &str, arguments: &[&str]| {
        text.push_str(name);
        for argument in arguments {
            text.push(' ');
            text.push_str(&print_argument(argument));
        }
        text.push('\n');
    };
    line("DIRECTORY", &[&agent.directory]);
    line("AGENTFILE", &[&agent.agentfile]);
    for layer in &agent.layers {
        if layer.key.is_empty() || layer.instruction.is_empty() {
            return invalid("a layer's key and instruction can't be empty".to_string());
        }
        if !is_digest(&layer.digest) {
            return invalid(format!("{:?} isn't a SHA-256 digest", layer.digest));
        }
        line("LAYER", &[&layer.key, &layer.digest, &layer.instruction]);
    }
    for declared in &agent.declared {
        if !declarable(&declared.name) {
            return invalid(format!("a record can't declare `{}`", declared.name));
        }
        let arguments: Vec<&str> = declared.arguments.iter().map(String::as_str).collect();
        line(&declared.name, &arguments);
    }
    Ok(text)
}

/// Reads the text of the record named `name`.
fn read_record(name: &str, text: &str) -> Result<Agent, String> {
    let mut directory = None;
    let mut agentfile = None;
    let mut layers = Vec::new();
    let mut declared = Vec::new();
    for (index, content) in text.split('\n').enumerate() {
        let fail = |message: String| format!("line {}: {message}", index + 1);
        let content = content.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let (word, rest) = content
            .split_once(char::is_whitespace)
            .unwrap_or((content, ""));
        let word = word.to_ascii_uppercase();
        let mut arguments = read_arguments(rest).map_err(fail)?;
        match word.as_str() {
            "DIRECTORY" => match arguments.as_slice() {
                _ if directory.is_some() => return Err(fail("a second `DIRECTORY`".to_string())),
                [path] if path.starts_with('/') => directory = arguments.pop(),
                _ => return Err(fail("`DIRECTORY` takes a canonical path".to_string())),
            },
            "AGENTFILE" => match arguments.as_slice() {
                _ if agentfile.is_some() => return Err(fail("a second `AGENTFILE`".to_string())),
                [path] if path.starts_with('/') => agentfile = arguments.pop(),
                _ => return Err(fail("`AGENTFILE` takes a canonical path".to_string())),
            },
            "LAYER" => match arguments.as_slice() {
                [key, digest, instruction]
                    if !key.is_empty() && is_digest(digest) && !instruction.is_empty() =>
                {
                    let instruction = arguments.pop().unwrap_or_default();
                    let digest = arguments.pop().unwrap_or_default();
                    let key = arguments.pop().unwrap_or_default();
                    layers.push(Layer {
                        key,
                        digest,
                        instruction,
                    });
                }
                _ => {
                    return Err(fail(
                        "`LAYER` takes a key, a SHA-256 digest, and an instruction".to_string(),
                    ));
                }
            },
            _ if declarable(&word) => declared.push(Declared {
                name: word,
                arguments,
            }),
            _ => return Err(fail(format!("unknown line `{word}`"))),
        }
    }
    let Some(directory) = directory else {
        return Err("the record has no `DIRECTORY`".to_string());
    };
    let Some(agentfile) = agentfile else {
        return Err("the record has no `AGENTFILE`".to_string());
    };
    Ok(Agent {
        name: name.to_string(),
        directory,
        agentfile,
        layers,
        declared,
    })
}
