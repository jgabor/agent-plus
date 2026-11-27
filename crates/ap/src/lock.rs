//! `Agentfile.lock`, from SPEC.md's Build section, in the Agentfile's line
//! syntax and with its argument reader, as the store's records are:
//!
//! ```text
//! HARNESS claude 2.1.3 0b1f63c4…
//! ```
//!
//! `HARNESS` holds the harness, its exact version, and the SHA-256 digest of
//! the file it is installed from. For Codex, it then holds the digest of the
//! file its code-mode host is installed from, and then of the file its
//! bubblewrap is installed from, each `-` for a release that publishes none.
//! A Codex line that ends before either was written before `ap` installed
//! it.
//!
//! ```text
//! SOURCE https://github.com/torvalds/linux v6.12 refs/tags/v6.12 adc218676…
//! ```
//!
//! `SOURCE` holds a remote source's repository, the ref the Agentfile names,
//! or `HEAD` for none, the full name of the ref it resolved through, or `-`
//! for a commit, and the commit. Each repository and ref has one line, in the
//! order the Agentfile first names them. Any other line is an error.
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

use agentfile::{Agent, Harness, print_argument, read_arguments};

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

/// The asset the guest runs Codex's code-mode host from, in one zstd frame.
/// Codex 0.171.0 runs every tool through the host, and releases publish it
/// from 0.143.0.
pub const CODEX_HOST_ASSET: &str = "codex-code-mode-host-x86_64-unknown-linux-musl.zst";

/// The asset the guest runs bubblewrap from, in one zstd frame. Codex
/// 0.171.0's sandbox runs each command in it, and releases publish it from
/// 0.129.0.
pub const CODEX_BWRAP_ASSET: &str = "bwrap-x86_64-unknown-linux-musl.zst";

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
    pub sources: Vec<Source>,
}

/// A remote source a lockfile pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The URL of the repository.
    pub repository: String,
    /// The ref as the Agentfile names it, or `HEAD` for none.
    pub reference: String,
    /// What it resolved to.
    pub resolved: crate::source::Resolved,
}

/// The harness a lockfile pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    pub harness: Harness,
    /// An exact version.
    pub version: String,
    /// The SHA-256 digest of the file the harness is installed from.
    pub sha256: String,
    /// Codex's code-mode host.
    pub code_mode_host: Companion,
    /// The bubblewrap that Codex's sandbox runs commands in.
    pub bwrap: Companion,
}

/// What a lock records of a file that Codex's release publishes, which `ap`
/// installs with Codex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Companion {
    /// The SHA-256 digest of the file it is installed from.
    Digest(String),
    /// The release publishes none, or the harness isn't Codex.
    Unpublished,
    /// The lock is a Codex lock from before `ap` installed it, so it doesn't
    /// say.
    Unrecorded,
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
                "HARNESS" => {
                    let companion = |field: Option<&String>| match field.map(String::as_str) {
                        None => Some(Companion::Unrecorded),
                        Some("-") => Some(Companion::Unpublished),
                        Some(digest) if is_digest(digest) => {
                            Some(Companion::Digest(digest.to_string()))
                        }
                        Some(_) => None,
                    };
                    let companions = match arguments.as_slice() {
                        [harness, _, _] if harness != "codex" => {
                            Some((Companion::Unpublished, Companion::Unpublished))
                        }
                        [harness, _, _, rest @ ..] if harness == "codex" && rest.len() <= 2 => {
                            companion(rest.first()).zip(companion(rest.get(1)))
                        }
                        _ => None,
                    };
                    match (arguments.as_slice(), companions) {
                        ([harness, version, sha256, ..], Some((code_mode_host, bwrap)))
                            if is_digest(sha256) && !version.is_empty() =>
                        {
                            let harness = match harness.as_str() {
                                "claude" => Harness::Claude,
                                "codex" => Harness::Codex,
                                _ => return Err(fail(format!("`{harness}` isn't a harness"))),
                            };
                            lock.harness = Some(Locked {
                                harness,
                                version: version.clone(),
                                sha256: sha256.clone(),
                                code_mode_host,
                                bwrap,
                            });
                        }
                        _ => {
                            return Err(fail(
                                "`HARNESS` takes a harness, a version, and a SHA-256 digest, \
                                 and for Codex then the digests of its code-mode host and its \
                                 bubblewrap, each or `-`"
                                    .to_string(),
                            ));
                        }
                    }
                }
                "SOURCE" => {
                    let source = match arguments.as_slice() {
                        [repository, reference, name, commit] => {
                            let name = match name.as_str() {
                                "-" => Some(None),
                                name if name.starts_with("refs/") => Some(Some(name.to_string())),
                                _ => None,
                            };
                            name.zip(git::Id::from_hex(commit).filter(|_| is_commit(commit)))
                                .map(|(name, commit)| Source {
                                    repository: repository.clone(),
                                    reference: reference.clone(),
                                    resolved: crate::source::Resolved { name, commit },
                                })
                        }
                        _ => None,
                    };
                    let Some(source) = source.filter(|source| {
                        !source.repository.is_empty() && !source.reference.is_empty()
                    }) else {
                        return Err(fail(
                            "`SOURCE` takes a repository, a ref, the full name of the ref it \
                             resolved through or `-`, and a commit"
                                .to_string(),
                        ));
                    };
                    if lock.source(&source.repository, &source.reference).is_some() {
                        return Err(fail(format!(
                            "a second `SOURCE` for `{}` at `{}`",
                            source.repository, source.reference
                        )));
                    }
                    lock.sources.push(source);
                }
                _ => return Err(fail(format!("unknown line `{word}`"))),
            }
        }
        Ok(lock)
    }

    /// What the lock pins for `reference` in `repository`.
    pub fn source(&self, repository: &str, reference: &str) -> Option<&crate::source::Resolved> {
        self.sources
            .iter()
            .find(|source| source.repository == repository && source.reference == reference)
            .map(|source| &source.resolved)
    }

    /// Forgets what `ap update` resolves again: every source, and the
    /// harness when `agent` names a channel. An exact version keeps the
    /// digest its lock records.
    pub fn forget(&mut self, agent: &Agent) {
        self.sources.clear();
        if !is_exact(&agent.version) {
            self.harness = None;
        }
    }

    /// Replaces the lockfile at `path` with this lock.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        write(path, &self.text())
    }

    /// The text of the lockfile.
    pub fn text(&self) -> String {
        let mut text = self.harness_text();
        for source in &self.sources {
            let name = source.resolved.name.as_deref().unwrap_or("-");
            text.push_str(&format!(
                "SOURCE {} {} {} {}\n",
                print_argument(&source.repository),
                print_argument(&source.reference),
                print_argument(name),
                source.resolved.commit
            ));
        }
        text
    }

    fn harness_text(&self) -> String {
        match &self.harness {
            Some(locked) => {
                // A field that a lock doesn't record ends the line.
                let companions: String = match locked.harness {
                    Harness::Claude => String::new(),
                    Harness::Codex => [&locked.code_mode_host, &locked.bwrap]
                        .into_iter()
                        .map_while(|companion| match companion {
                            Companion::Digest(digest) => Some(format!(" {digest}")),
                            Companion::Unpublished => Some(" -".to_string()),
                            Companion::Unrecorded => None,
                        })
                        .collect(),
                };
                format!(
                    "HARNESS {} {} {}{companions}\n",
                    name(locked.harness),
                    locked.version,
                    locked.sha256
                )
            }
            None => String::new(),
        }
    }
}

/// Settles the harness that `agent` names, and returns whether `lock`
/// changed. A lock that resolves `agent` is kept. A Codex lock that would,
/// but doesn't record the code-mode host or bubblewrap, resolves the version
/// it locks again, and a digest for Codex's file, or for a code-mode host the
/// lock records, that isn't the lock's is an error. Any other resolves again:
/// a channel resolves to the exact version the harness's publisher
/// publishes for it, and the lock gets that version and the digest the
/// publisher publishes for its file, and for Codex's code-mode host and
/// bubblewrap. Each
/// file is downloaded from `origins` into `cache`, unless `cache` holds it or
/// `stored` says the store has the harness layer. A file whose digest
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
        Some(locked) if locked.pins(agent) => {
            let missing = match locked.code_mode_host {
                Companion::Unrecorded => "the code-mode host",
                _ => "bubblewrap",
            };
            let again = |error: io::Error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "Agentfile.lock doesn't record {missing} of `{}`, \
                         so `ap` resolves it again: {error}",
                        locked.label()
                    ),
                )
            };
            let resolved =
                resolve_codex(&locked.version, client, origins.codex_api).map_err(again)?;
            let changed = |what: &str, given: &str, locked: &str| {
                again(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "GitHub gives {what} the digest {given}, not the {locked} that the lock locks"
                    ),
                ))
            };
            if resolved.sha256 != locked.sha256 {
                return Err(changed("its file", &resolved.sha256, &locked.sha256));
            }
            let shown = |companion: &Companion| match companion {
                Companion::Digest(digest) => digest.clone(),
                _ => "-".to_string(),
            };
            if locked.code_mode_host != Companion::Unrecorded
                && resolved.code_mode_host != locked.code_mode_host
            {
                return Err(changed(
                    "its code-mode host",
                    &shown(&resolved.code_mode_host),
                    &shown(&locked.code_mode_host),
                ));
            }
            resolved
        }
        _ if agent.harness == Harness::Codex => {
            resolve_codex(&agent.version, client, origins.codex_api)?
        }
        _ => resolve(agent, client, origins.claude)?,
    };
    let store = Store::flat(cache);
    let changed = lock.harness.as_ref() != Some(&locked);
    let stored = stored(&locked);
    for (what, sha256, url) in files(&locked, origins) {
        let file = store.path(sha256)?;
        // The build checks that whatever is at the path is the locked file,
        // but only after a changed lock is written.
        let cached = fs::symlink_metadata(&file).is_ok();
        if cached && changed && !stored {
            check_file(&file, sha256, &what)?;
        }
        if !cached && !stored {
            check_version(&locked.version)?;
            fetch::fetch(&store, client, &Pin { url: &url, sha256 }).map_err(|error| {
                io::Error::new(error.kind(), format!("downloading {what}: {error}"))
            })?;
        }
    }
    if !changed {
        return Ok(false);
    }
    lock.harness = Some(locked);
    write(path, &lock.text())?;
    Ok(true)
}

/// The files `locked` is installed from: what each is, its digest, and the
/// URL `origins` publishes it at.
fn files<'a>(locked: &'a Locked, origins: &Origins) -> Vec<(String, &'a str, String)> {
    let version = &locked.version;
    let label = locked.label();
    match locked.harness {
        Harness::Claude => vec![(
            format!("`{label}`"),
            &locked.sha256,
            format!("{}/{version}/{PLATFORM}/claude", origins.claude),
        )],
        Harness::Codex => {
            let url = |asset: &str| {
                format!(
                    "{}/releases/download/{CODEX_TAG}{version}/{asset}",
                    origins.codex
                )
            };
            let mut files = vec![(format!("`{label}`"), &*locked.sha256, url(CODEX_ASSET))];
            if let Companion::Digest(host) = &locked.code_mode_host {
                files.push((
                    format!("the code-mode host of `{label}`"),
                    host,
                    url(CODEX_HOST_ASSET),
                ));
            }
            if let Companion::Digest(bwrap) = &locked.bwrap {
                files.push((
                    format!("bubblewrap of `{label}`"),
                    bwrap,
                    url(CODEX_BWRAP_ASSET),
                ));
            }
            files
        }
    }
}

/// Checks that `path` is a file with the digest `sha256`, which is `what`'s.
fn check_file(path: &Path, sha256: &str, what: &str) -> io::Result<()> {
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
    if actual != sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{shown} has digest {actual}, not the {sha256} of {what}"),
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
        code_mode_host: Companion::Unpublished,
        bwrap: Companion::Unpublished,
    })
}

/// The exact version and digests that GitHub's API at `api` publishes for
/// the Codex that `asked` names, an exact version or a channel. Codex publishes
/// one channel, `latest`, its newest release.
fn resolve_codex(asked: &str, client: &Client, api: &str) -> io::Result<Locked> {
    let failed = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("resolving `AGENT codex:{asked}`: {error}"),
        )
    };
    let invalid = |what: String| failed(io::Error::new(io::ErrorKind::InvalidData, what));
    check_version(asked)?;
    let url = match asked {
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
        .filter(|version| !is_exact(asked) || *version == asked)
        .ok_or_else(|| invalid("GitHub sent a release that isn't the one asked for".to_string()))?
        .to_string();
    let assets = (release.get("assets").and_then(Value::as_array)).unwrap_or_default();
    let asset = |name: &str| {
        assets
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
    };
    let digest = |name: &str| {
        asset(name)
            .and_then(|asset| asset.get("digest")?.as_str()?.strip_prefix("sha256:"))
            .filter(|digest| is_digest(digest))
            .map(str::to_string)
            .ok_or_else(|| invalid(format!("Codex {version} has no SHA-256 digest for {name}")))
    };
    let sha256 = digest(CODEX_ASSET)?;
    let companion = |name: &str| match asset(name) {
        Some(_) => digest(name).map(Companion::Digest),
        None => Ok(Companion::Unpublished),
    };
    let (code_mode_host, bwrap) = (companion(CODEX_HOST_ASSET)?, companion(CODEX_BWRAP_ASSET)?);
    Ok(Locked {
        harness: Harness::Codex,
        version,
        sha256,
        code_mode_host,
        bwrap,
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
    /// A Codex lock that doesn't record the code-mode host or bubblewrap
    /// resolves nothing.
    pub fn resolves(&self, agent: &Agent) -> bool {
        self.pins(agent)
            && self.code_mode_host != Companion::Unrecorded
            && self.bwrap != Companion::Unrecorded
    }

    /// Whether this locks the harness and version that `AGENT` names, as
    /// [`Locked::resolves`] says, whatever it records of the code-mode host
    /// and bubblewrap.
    fn pins(&self, agent: &Agent) -> bool {
        self.harness == agent.harness
            && (!is_exact(&agent.version) || agent.version == self.version)
    }

    /// The `AGENT` line this resolves to, such as `AGENT claude:2.1.3`.
    pub fn label(&self) -> String {
        format!("AGENT {}:{}", name(self.harness), self.version)
    }
}

/// The version in a [`Locked::label`] for `harness`, or `None` if `label`
/// isn't one.
pub fn labeled_version(label: &str, harness: Harness) -> Option<&str> {
    label
        .strip_prefix("AGENT ")?
        .strip_prefix(name(harness))?
        .strip_prefix(':')
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

/// Whether `text` is a SHA-1 commit in lowercase hex, as the lock writes it.
fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
