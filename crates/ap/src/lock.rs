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
//! download cache, and writes the lockfile, from the [`ORIGINS`] each
//! harness is published at.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agentfile::{Agent, Harness, read_arguments};

use graviola::hashing::sha2::Sha256Context;

use crate::fetch::{self, Client, Pin};
use crate::json::{self, Value};
use crate::store::Store;

/// Where Anthropic publishes Claude Code. `<base>/<channel>` holds a
/// channel's version, such as `2.1.311`, `<base>/<version>/manifest.json` the
/// SHA-256 digest of each platform's file, and
/// `<base>/<version>/<platform>/claude` the file.
pub const ANTHROPIC: &str = "https://downloads.claude.ai/claude-code-releases";

/// The platform the guest runs Claude Code on.
const PLATFORM: &str = "linux-x64";

/// Where OpenAI publishes Codex's releases in GitHub's API.
/// `<api>/releases/latest` is the newest release, and
/// `<api>/releases/tags/rust-v<version>` a version's. Each lists its assets,
/// with the SHA-256 digest of each.
pub const OPENAI_API: &str = "https://api.github.com/repos/openai/codex";

/// Where OpenAI publishes Codex's files.
/// `<base>/releases/download/rust-v<version>/<asset>` is a release's asset.
pub const OPENAI: &str = "https://github.com/openai/codex";

/// The asset the guest runs Codex from: the static Linux x86_64 binary, in
/// one zstd frame.
pub const CODEX_ASSET: &str = "codex-x86_64-unknown-linux-musl.zst";

/// The prefix of a Codex release's tag, before its version.
const CODEX_TAG: &str = "rust-v";

/// The most a GitHub release may hold. Codex's are about 350 KiB, most of
/// it the list of assets.
const MAX_RELEASE_BYTES: u64 = 4 * 1024 * 1024;

/// Where [`settle`] resolves and downloads each harness.
pub struct Origins<'a> {
    /// Claude Code's releases, as [`ANTHROPIC`].
    pub claude: &'a str,
    /// Codex's releases in GitHub's API, as [`OPENAI_API`].
    pub codex_api: &'a str,
    /// Codex's files, as [`OPENAI`].
    pub codex: &'a str,
}

/// Where each harness is published.
pub const ORIGINS: Origins<'static> = Origins {
    claude: ANTHROPIC,
    codex_api: OPENAI_API,
    codex: OPENAI,
};

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
/// a channel resolves to the exact version the harness's publisher
/// publishes for it, and the lock gets that version and the digest the
/// publisher publishes for its file. The file is downloaded from `origins`
/// into `cache`, unless `cache` holds it or `stored` says the store has its
/// layer. A file whose digest
/// isn't the lock's is an error, and isn't stored. A changed lock checks a
/// file the cache already holds, and only then replaces the lockfile `path`.
pub fn settle(
    path: &Path,
    lock: &mut Lock,
    agent: &Agent,
    client: &Client,
    origins: &Origins,
    cache: &Path,
    stored: impl Fn(&Locked) -> bool,
) -> io::Result<bool> {
    let locked = match &lock.harness {
        Some(locked) if locked.resolves(agent) => locked.clone(),
        _ if agent.harness == Harness::Codex => resolve_codex(agent, client, origins.codex_api)?,
        _ => resolve(agent, client, origins.claude)?,
    };
    let store = Store::flat(cache);
    let file = store.path(&locked.sha256)?;
    let changed = lock.harness.as_ref() != Some(&locked);
    // The build checks that whatever is at the path is the locked file, but
    // only after a changed lock is written.
    let cached = fs::symlink_metadata(&file).is_ok();
    if cached && changed && !stored(&locked) {
        check_file(&file, &locked)?;
    }
    if !cached && !stored(&locked) {
        check_version(&locked.version)?;
        let url = match locked.harness {
            Harness::Claude => format!("{}/{}/{PLATFORM}/claude", origins.claude, locked.version),
            Harness::Codex => format!(
                "{}/releases/download/{CODEX_TAG}{}/{CODEX_ASSET}",
                origins.codex, locked.version
            ),
        };
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
    if !changed {
        return Ok(false);
    }
    lock.harness = Some(locked);
    write(path, &lock.text())?;
    Ok(true)
}

/// Checks that `path` is a file with the digest `locked` names.
fn check_file(path: &Path, locked: &Locked) -> io::Result<()> {
    let shown = path.display();
    let failed = |error: io::Error| io::Error::new(error.kind(), format!("{shown}: {error}"));
    // Nonblocking, so that a FIFO fails the check instead of waiting.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(failed)?;
    if !file.metadata().map_err(failed)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{shown} isn't a file"),
        ));
    }
    let mut hash = Sha256Context::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(failed)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let actual: String = hash.finish().iter().map(|b| format!("{b:02x}")).collect();
    if actual != locked.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{shown} has digest {actual}, not the {} of `{}`",
                locked.sha256,
                locked.label()
            ),
        ));
    }
    Ok(())
}

/// The exact version and digest that Anthropic publishes for what `agent`
/// names.
fn resolve(agent: &Agent, client: &Client, base: &str) -> io::Result<Locked> {
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

/// The exact version and digest that GitHub's API at `api` publishes for
/// the Codex that `agent` names. Codex publishes one channel, `latest`, its
/// newest release.
fn resolve_codex(agent: &Agent, client: &Client, api: &str) -> io::Result<Locked> {
    let failed = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("resolving `AGENT codex:{}`: {error}", agent.version),
        )
    };
    let invalid = |what: String| failed(io::Error::new(io::ErrorKind::InvalidData, what));
    check_version(&agent.version)?;
    let url = match agent.version.as_str() {
        version if is_exact(version) => format!("{api}/releases/tags/{CODEX_TAG}{version}"),
        "latest" => format!("{api}/releases/latest"),
        _ => {
            return Err(failed(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Codex publishes no channel but `latest`".to_string(),
            )));
        }
    };
    let release = fetch::text(client, &url, MAX_RELEASE_BYTES).map_err(failed)?;
    let release = json::parse(&release)
        .ok_or_else(|| invalid("GitHub sent a release that isn't JSON".to_string()))?;
    let version = (release.get("tag_name").and_then(Value::as_str))
        .and_then(|tag| tag.strip_prefix(CODEX_TAG))
        .filter(|version| is_exact(version) && check_version(version).is_ok())
        .filter(|version| !is_exact(&agent.version) || *version == agent.version)
        .ok_or_else(|| invalid("GitHub sent a release that isn't the one asked for".to_string()))?
        .to_string();
    let sha256 = (release.get("assets").and_then(Value::as_array))
        .unwrap_or_default()
        .iter()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(CODEX_ASSET))
        .and_then(|asset| asset.get("digest")?.as_str()?.strip_prefix("sha256:"))
        .filter(|digest| is_digest(digest))
        .ok_or_else(|| {
            invalid(format!(
                "Codex {version} has no SHA-256 digest for {CODEX_ASSET}"
            ))
        })?
        .to_string();
    Ok(Locked {
        harness: Harness::Codex,
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
