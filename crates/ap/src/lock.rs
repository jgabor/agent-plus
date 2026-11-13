//! `Agentfile.lock`, from SPEC.md's Build section, in the Agentfile's line
//! syntax and with its argument reader, as the store's records are:
//!
//! ```text
//! HARNESS claude 2.1.3 0b1f63c4…
//! ```
//!
//! `HARNESS` holds the harness, its exact version, and the SHA-256 digest of
//! the file it is installed from. Nothing writes a lockfile yet, and it holds
//! nothing else yet, so any other line is an error.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use agentfile::{Agent, Harness, read_arguments};

/// What a lockfile pins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    pub harness: Option<Locked>,
}

/// The harness a lockfile pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    pub harness: Harness,
    /// An exact version.
    pub version: String,
    /// The SHA-256 digest of the file the harness is installed from.
    pub sha256: String,
}

/// A lockfile's error, on the line it is on, from 1.
#[derive(Debug, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl Lock {
    /// Reads the text of a lockfile.
    pub fn read(text: &str) -> Result<Lock, Error> {
        let mut lock = Lock::default();
        for (index, content) in text.split('\n').enumerate() {
            let fail = |message: String| Error {
                line: index + 1,
                message,
            };
            let content = content.trim();
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            let (word, rest) = content
                .split_once(char::is_whitespace)
                .unwrap_or((content, ""));
            let arguments = read_arguments(rest).map_err(fail)?;
            match word.to_ascii_uppercase().as_str() {
                "HARNESS" if lock.harness.is_some() => {
                    return Err(fail("a second `HARNESS`".to_string()));
                }
                "HARNESS" => match arguments.as_slice() {
                    [harness, version, sha256] if is_digest(sha256) && !version.is_empty() => {
                        let harness = match harness.as_str() {
                            "claude" => Harness::Claude,
                            "codex" => Harness::Codex,
                            _ => return Err(fail(format!("`{harness}` isn't a harness"))),
                        };
                        lock.harness = Some(Locked {
                            harness,
                            version: version.clone(),
                            sha256: sha256.clone(),
                        });
                    }
                    _ => {
                        return Err(fail(
                            "`HARNESS` takes a harness, a version, and a SHA-256 digest"
                                .to_string(),
                        ));
                    }
                },
                _ => return Err(fail(format!("unknown line `{word}`"))),
            }
        }
        Ok(lock)
    }
}

impl Locked {
    /// Whether this is a resolution of what `AGENT` names: the same harness,
    /// and the same version unless `AGENT` names a channel. A version that
    /// starts with a digit is exact, and any other is a channel.
    pub fn resolves(&self, agent: &Agent) -> bool {
        let exact = agent.version.starts_with(|c: char| c.is_ascii_digit());
        self.harness == agent.harness && (!exact || agent.version == self.version)
    }

    /// The `AGENT` line this resolves to, such as `AGENT claude:2.1.3`.
    pub fn label(&self) -> String {
        format!("AGENT {}:{}", name(self.harness), self.version)
    }
}

/// The harness's name, as `AGENT` writes it.
pub fn name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    }
}

/// The download cache for these values of `XDG_CACHE_HOME` and `HOME`, or
/// `None` if neither gives an absolute path. The XDG spec treats an empty or
/// relative `XDG_CACHE_HOME` as unset.
pub fn cache(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    fn absolute(value: Option<&OsStr>) -> Option<&Path> {
        value.map(Path::new).filter(|path| path.is_absolute())
    }
    match absolute(xdg_cache_home) {
        Some(cache) => Some(cache.join("ap")),
        None => Some(absolute(home)?.join(".cache/ap")),
    }
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
