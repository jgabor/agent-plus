//! Checks the arguments of `ADD`, `REPO`, `PLUGIN`, `SKILL`, and `MCP`, and
//! reads them and their sources into typed [`Sources`]. Nothing is fetched.

use crate::{Error, Instruction};
use std::collections::HashMap;

/// A source, in one of the forms in SPEC.md's Sources section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Remote(Remote),
    /// A host directory, as written. The build resolves it.
    Local(String),
}

/// A git repository, and where in it to look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// The form the source was written in.
    pub form: Form,
    /// The URL to clone, such as `https://github.com/torvalds/linux`.
    pub repository: String,
    /// A path inside the repository, without slashes at either end.
    pub subdirectory: Option<String>,
    /// `None` means the default branch.
    pub reference: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `owner/repo`, with an optional subdirectory and `@ref`.
    Shorthand,
    /// A repository page, such as a GitHub `/tree/<ref>/` URL.
    Forge,
    /// A repository root, with an optional `@ref`.
    Git,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Add {
    pub line: usize,
    /// A host path, as written.
    pub source: String,
    /// A guest path, as written. `None` leaves it to the mirror rule.
    pub destination: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub line: usize,
    pub source: Source,
    /// A guest path, as written.
    pub destination: Option<String>,
    /// Whether `history` asks for the full history.
    pub history: bool,
}

impl Repo {
    /// The repository's name, without a trailing `.git`. `None` when only the
    /// host knows it, as for `REPO .`.
    pub fn name(&self) -> Option<&str> {
        let path = match &self.source {
            Source::Remote(remote) => remote.repository.as_str(),
            Source::Local(path) => path.as_str(),
        };
        let last = path.trim_end_matches('/').rsplit('/').next()?;
        let name = last.strip_suffix(".git").unwrap_or(last);
        match name {
            "" | "." | ".." | "~" => None,
            name => Some(name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub line: usize,
    pub source: Source,
    /// The plugins to pick from a marketplace. Empty when the source is one
    /// plugin.
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub line: usize,
    pub source: Source,
    /// The skills to pick. Empty means every skill under the source.
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mcp {
    /// `MCP <source>`: the servers that the source's `.mcp.json` or
    /// `mcp.json` declares.
    Declared { line: usize, source: Source },
    /// `MCP <name> <url>`: a remote server over HTTP.
    Remote {
        line: usize,
        name: String,
        url: String,
    },
}

/// The `ADD`, `REPO`, `PLUGIN`, `SKILL`, and `MCP` lines of an Agentfile,
/// each in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    pub add: Vec<Add>,
    pub repo: Vec<Repo>,
    pub plugin: Vec<Plugin>,
    pub skill: Vec<Skill>,
    pub mcp: Vec<Mcp>,
}

impl Sources {
    /// Reads the sources from the instructions that [`crate::parse`]
    /// returned. It skips the other instructions, and returns every error it
    /// finds, in line order.
    pub fn read(instructions: &[Instruction]) -> Result<Sources, Vec<Error>> {
        let mut sources = Sources::default();
        let mut errors = Vec::new();
        for instruction in instructions {
            let line = instruction.line;
            match read(instruction) {
                None => {}
                Some(Err(message)) => errors.push(Error { line, message }),
                Some(Ok(Read::Add(add))) => sources.add.push(add),
                Some(Ok(Read::Repo(repo))) => sources.repo.push(repo),
                Some(Ok(Read::Plugin(plugin))) => sources.plugin.push(plugin),
                Some(Ok(Read::Skill(skill))) => sources.skill.push(skill),
                Some(Ok(Read::Mcp(mcp))) => sources.mcp.push(mcp),
            }
        }
        // The line of the first `REPO` without a destination, by name.
        let mut named: HashMap<&str, usize> = HashMap::new();
        for repo in &sources.repo {
            let Some(name) = repo.name().filter(|_| repo.destination.is_none()) else {
                continue;
            };
            if let Some(first) = named.get(name) {
                errors.push(Error {
                    line: repo.line,
                    message: format!(
                        "line {first} already clones a repository named `{name}` into `~/git/{name}`. Give one a destination"
                    ),
                });
            } else {
                named.insert(name, repo.line);
            }
        }
        if errors.is_empty() {
            Ok(sources)
        } else {
            errors.sort_by_key(|error| error.line);
            Err(errors)
        }
    }
}

enum Read {
    Add(Add),
    Repo(Repo),
    Plugin(Plugin),
    Skill(Skill),
    Mcp(Mcp),
}

pub(crate) fn check(instruction: &Instruction) -> Result<(), String> {
    match read(instruction) {
        Some(Err(message)) => Err(message),
        _ => Ok(()),
    }
}

/// Reads one instruction, or returns `None` for one this module doesn't own.
fn read(instruction: &Instruction) -> Option<Result<Read, String>> {
    let line = instruction.line;
    let arguments: Vec<&str> = instruction.arguments.iter().map(String::as_str).collect();
    let result = match instruction.name.as_str() {
        "ADD" => add(line, &arguments).map(Read::Add),
        "REPO" => repo(line, &arguments).map(Read::Repo),
        "PLUGIN" => named("PLUGIN", &arguments).map(|(source, names)| {
            Read::Plugin(Plugin {
                line,
                source,
                names,
            })
        }),
        "SKILL" => named("SKILL", &arguments).map(|(source, names)| {
            Read::Skill(Skill {
                line,
                source,
                names,
            })
        }),
        "MCP" => mcp(line, &arguments).map(Read::Mcp),
        _ => return None,
    };
    Some(result)
}

fn add(line: usize, arguments: &[&str]) -> Result<Add, String> {
    let (source, destination) = match arguments {
        [source] => (*source, None),
        [source, destination] => (*source, Some(*destination)),
        _ => {
            return Err(format!(
                "`ADD` takes a host path and an optional destination, but found {} arguments",
                arguments.len()
            ));
        }
    };
    if source.is_empty() {
        return Err("`ADD` needs a host path".to_string());
    }
    Ok(Add {
        line,
        source: source.to_string(),
        destination: destination
            .map(|value| self::destination("ADD", value))
            .transpose()?,
    })
}

fn repo(line: usize, arguments: &[&str]) -> Result<Repo, String> {
    let (history, rest) = match arguments {
        [rest @ .., "history"] if !rest.is_empty() => (true, rest),
        _ => (false, arguments),
    };
    let (text, destination) = match rest {
        [text] => (*text, None),
        [text, destination] => (*text, Some(*destination)),
        _ => {
            return Err(format!(
                "`REPO` takes a source, an optional destination, and an optional `history`, but found {} arguments",
                arguments.len()
            ));
        }
    };
    Ok(Repo {
        line,
        source: source("REPO", text)?,
        destination: destination
            .map(|value| self::destination("REPO", value))
            .transpose()?,
        history,
    })
}

/// Reads `<source> [<name>...]`, for `PLUGIN` and `SKILL`.
fn named(name: &str, arguments: &[&str]) -> Result<(Source, Vec<String>), String> {
    let [text, names @ ..] = arguments else {
        return Err(format!("`{name}` needs a source"));
    };
    if names.iter().any(|name| name.is_empty()) {
        return Err(format!("`{name}` can't take an empty name"));
    }
    Ok((
        source(name, text)?,
        names.iter().map(|name| name.to_string()).collect(),
    ))
}

fn mcp(line: usize, arguments: &[&str]) -> Result<Mcp, String> {
    match arguments {
        [text] => Ok(Mcp::Declared {
            line,
            source: source("MCP", text)?,
        }),
        [name, url] => {
            if name.is_empty() {
                return Err("`MCP` needs a server name before its URL".to_string());
            }
            let host = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"));
            if host.is_none_or(|host| host.split('/').next().unwrap_or("").is_empty()) {
                return Err(format!(
                    "`MCP <name> <url>` takes an `https` or `http` URL, but found `{url}`"
                ));
            }
            Ok(Mcp::Remote {
                line,
                name: name.to_string(),
                url: url.to_string(),
            })
        }
        _ => Err(format!(
            "`MCP` takes a source, or a name and a URL, but found {} arguments",
            arguments.len()
        )),
    }
}

fn destination(name: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("`{name}` can't take an empty destination"));
    }
    Ok(value.to_string())
}

/// Reads a source in any of its four forms.
fn source(name: &str, text: &str) -> Result<Source, String> {
    if text.is_empty() {
        return Err(format!("`{name}` needs a source"));
    }
    let is_local = matches!(text, "." | ".." | "~")
        || ["./", "../", "/", "~/"]
            .iter()
            .any(|prefix| text.starts_with(prefix));
    if is_local {
        return Ok(Source::Local(text.to_string()));
    }
    let remote = match text.split_once("://") {
        Some((scheme, rest)) => url(text, scheme, rest),
        None => shorthand(text),
    };
    remote.map(Source::Remote)
}

/// Reads `owner/repo[/<subdirectory>][@<ref>]`.
fn shorthand(text: &str) -> Result<Remote, String> {
    let (path, reference) = split_reference(text)?;
    let path = path.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let wrong = || {
        format!(
            "a source is `owner/repo`, a URL, or a host path that starts with `./`, `../`, `/`, or `~/`, but found `{text}`"
        )
    };
    let [owner, repository, subdirectory @ ..] = parts.as_slice() else {
        return Err(wrong());
    };
    if owner.is_empty() || repository.is_empty() || owner.contains(':') {
        return Err(wrong());
    }
    Ok(Remote {
        form: Form::Shorthand,
        repository: format!("https://github.com/{owner}/{repository}"),
        subdirectory: subdirectory_of(text, subdirectory)?,
        reference,
    })
}

/// Reads a forge URL or a Git URL, which is `<scheme>://<rest>`.
fn url(text: &str, scheme: &str, rest: &str) -> Result<Remote, String> {
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return Err(format!(
            "a Git URL names a repository after its host, but found `{text}`"
        ));
    }
    if scheme == "https" && matches!(host, "github.com" | "www.github.com") {
        return github(text, host, path);
    }
    let (path, reference) = split_reference(path).map_err(|_| empty_reference(text))?;
    Ok(Remote {
        form: Form::Git,
        repository: format!("{scheme}://{authority}/{path}"),
        subdirectory: None,
        reference,
    })
}

/// Reads a GitHub URL: a repository root, or a `/tree/<ref>/` page in one.
fn github(text: &str, host: &str, path: &str) -> Result<Remote, String> {
    let parts: Vec<&str> = path.split('/').collect();
    let (owner, repository, rest) = match parts.as_slice() {
        [owner, repository, rest @ ..] if !owner.is_empty() && !repository.is_empty() => {
            (owner, repository, rest)
        }
        _ => return Err(not_github(text)),
    };
    let root = format!("https://{host}/{owner}/{repository}");
    match rest {
        [] => {
            let (path, reference) = split_reference(path).map_err(|_| empty_reference(text))?;
            Ok(Remote {
                form: Form::Git,
                repository: format!("https://{host}/{path}"),
                subdirectory: None,
                reference,
            })
        }
        ["tree", reference, subdirectory @ ..] if !reference.is_empty() => Ok(Remote {
            form: Form::Forge,
            repository: root,
            subdirectory: subdirectory_of(text, subdirectory)?,
            reference: Some(reference.to_string()),
        }),
        _ => Err(not_github(text)),
    }
}

fn not_github(text: &str) -> String {
    format!(
        "a GitHub URL is `https://github.com/<owner>/<repo>`, or a directory in one under `/tree/<ref>/`, but found `{text}`"
    )
}

fn empty_reference(text: &str) -> String {
    format!("a source needs a ref after `@`, but found `{text}`")
}

/// Splits a path at its first `@`. What follows is the ref.
fn split_reference(text: &str) -> Result<(&str, Option<String>), String> {
    match text.split_once('@') {
        None => Ok((text, None)),
        Some((_, "")) => Err(empty_reference(text)),
        Some((path, reference)) => Ok((path, Some(reference.to_string()))),
    }
}

/// Joins the parts of a subdirectory. Each part must name a directory, so a
/// subdirectory can't leave its repository.
fn subdirectory_of(text: &str, parts: &[&str]) -> Result<Option<String>, String> {
    if parts.is_empty() {
        return Ok(None);
    }
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(format!(
            "a subdirectory in a source can't have empty, `.`, or `..` parts, but found `{text}`"
        ));
    }
    Ok(Some(parts.join("/")))
}
