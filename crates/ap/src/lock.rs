//! `Agentfile.lock`, from SPEC.md's Build section, in the Agentfile's line
//! syntax and with its argument reader, as the store's records are:
//!
//! ```text
//! HARNESS claude 2.1.3 0b1f63c4…
//! ```
//!
//! `HARNESS` holds the harness, its exact version, and the SHA-256 digest of
//! the file it is installed from. It holds nothing else yet, so any other
//! line is an error.
//!
//! [`settle`] resolves what `AGENT` names, downloads the harness into the
//! download cache, and writes the lockfile.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agentfile::{Agent, Harness, read_arguments};

use crate::fetch::{self, Client, Pin};
use crate::store::Store;

/// Where Anthropic publishes Claude Code. `<base>/<channel>` holds a
/// channel's version, such as `2.1.311`, `<base>/<version>/manifest.json` the
/// SHA-256 digest of each platform's file, and
/// `<base>/<version>/<platform>/claude` the file.
pub const ANTHROPIC: &str = "https://downloads.claude.ai/claude-code-releases";

/// The platform the guest runs Claude Code on.
const PLATFORM: &str = "linux-x64";

/// The most a version or a manifest may hold. Both are far smaller.
const MAX_TEXT_BYTES: u64 = 64 * 1024;

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

    /// The text of the lockfile.
    pub fn text(&self) -> String {
        match &self.harness {
            Some(locked) => format!(
                "HARNESS {} {} {}\n",
                name(locked.harness),
                locked.version,
                locked.sha256
            ),
            None => String::new(),
        }
    }
}

/// Settles the harness that `agent` names, and returns whether `lock`
/// changed. A lock that resolves `agent` is kept. Any other resolves again:
/// a channel resolves to the exact version Anthropic publishes for it, and
/// the lock gets that version and the digest Anthropic publishes for its
/// file. The file is downloaded from `base` into `cache`, unless `cache`
/// holds it or `stored` says the store has its layer. A file whose digest
/// isn't the lock's is an error, and isn't stored. Only then does a changed
/// lock replace the lockfile `path`.
pub fn settle(
    path: &Path,
    lock: &mut Lock,
    agent: &Agent,
    client: &Client,
    base: &str,
    cache: &Path,
    stored: impl Fn(&Locked) -> bool,
) -> io::Result<bool> {
    let locked = match &lock.harness {
        Some(locked) if locked.resolves(agent) => locked.clone(),
        _ => resolve(agent, client, base)?,
    };
    let store = Store::flat(cache);
    // Whatever is at the path, the build checks it is the locked file.
    let cached = fs::symlink_metadata(store.path(&locked.sha256)?).is_ok();
    if !cached && !stored(&locked) {
        if locked.harness != Harness::Claude {
            return Err(io::Error::other(format!(
                "`{}` isn't in the download cache, and downloading Codex is not built yet",
                locked.label()
            )));
        }
        check_version(&locked.version)?;
        let url = format!("{base}/{}/{PLATFORM}/claude", locked.version);
        fetch::fetch(
            &store,
            client,
            &Pin {
                url: &url,
                sha256: &locked.sha256,
            },
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("downloading `{}`: {error}", locked.label()),
            )
        })?;
    }
    if lock.harness.as_ref() == Some(&locked) {
        return Ok(false);
    }
    lock.harness = Some(locked);
    write(path, &lock.text())?;
    Ok(true)
}

/// The exact version and digest that Anthropic publishes for what `agent`
/// names.
fn resolve(agent: &Agent, client: &Client, base: &str) -> io::Result<Locked> {
    if agent.harness != Harness::Claude {
        return Err(io::Error::other(format!(
            "resolving `AGENT {}:{}` is not built yet",
            name(agent.harness),
            agent.version
        )));
    }
    let failed = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("resolving `AGENT claude:{}`: {error}", agent.version),
        )
    };
    check_version(&agent.version)?;
    let version = match is_exact(&agent.version) {
        true => agent.version.clone(),
        false => {
            let url = format!("{base}/{}", agent.version);
            let version = fetch::text(client, &url, MAX_TEXT_BYTES).map_err(failed)?;
            let version = version.trim();
            if !is_exact(version) || check_version(version).is_err() {
                return Err(failed(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Anthropic sent a version that isn't one".to_string(),
                )));
            }
            version.to_string()
        }
    };
    let url = format!("{base}/{version}/manifest.json");
    let manifest = fetch::text(client, &url, MAX_TEXT_BYTES).map_err(failed)?;
    let sha256 = checksum(&manifest, PLATFORM).ok_or_else(|| {
        failed(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the manifest of Claude Code {version} has no digest for {PLATFORM}"),
        ))
    })?;
    Ok(Locked {
        harness: Harness::Claude,
        version,
        sha256,
    })
}

/// Whether a version is exact. One that starts with a digit is, and any
/// other names a channel.
fn is_exact(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
}

/// Checks that a version or a channel can go in a URL's path as one segment.
fn check_version(version: &str) -> io::Result<()> {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"._+-".contains(&b);
    if version.is_empty() || version.starts_with('.') || !version.bytes().all(allowed) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{version}` isn't a version or a channel"),
        ));
    }
    Ok(())
}

/// The digest a Claude Code manifest gives for `platform`'s file: the
/// `checksum` in the object that the key `platform` names.
fn checksum(manifest: &str, platform: &str) -> Option<String> {
    let key = format!("\"{platform}\"");
    let (_, rest) = manifest.split_once(&key)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let object = rest.strip_prefix('{')?;
    let object = &object[..object.find(['{', '}'])?];
    let (_, rest) = object.split_once("\"checksum\"")?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let (digest, _) = rest.split_once('"')?;
    is_digest(digest).then(|| digest.to_string())
}

/// Replaces the file at `path` with `text`. The file appears complete, or
/// not at all, and a symlink at `path` is replaced, not followed.
fn write(path: &Path, text: &str) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let shown = path.display();
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (partial, mut file) = loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let partial = dir.join(format!(".{name}.{}.{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
        {
            Ok(file) => break (partial, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io::Error::new(error.kind(), format!("{shown}: {error}"))),
        }
    };
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&partial, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&partial);
        return Err(io::Error::new(error.kind(), format!("{shown}: {error}")));
    }
    Ok(())
}

impl Locked {
    /// Whether this is a resolution of what `AGENT` names: the same harness,
    /// and the same version unless `AGENT` names a channel. A version that
    /// starts with a digit is exact, and any other is a channel.
    pub fn resolves(&self, agent: &Agent) -> bool {
        self.harness == agent.harness
            && (!is_exact(&agent.version) || agent.version == self.version)
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
