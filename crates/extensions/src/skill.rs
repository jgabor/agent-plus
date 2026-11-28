//! Finds the Agent Skills in a tree, and reads each one's `SKILL.md`, as
//! SPEC.md's `SKILL` and the Agent Skills specification say.
//!
//! The specification is <https://agentskills.io/specification>, as read on
//! 2026-11-28. It has no version number. Its reference validator, skills-ref,
//! was read at commit f130f348f502d9804278a617f86929846896d2e9.
//!
//! Every byte comes from a stranger, so the reader looks at no more than
//! [`MAX_FRONT_MATTER`] bytes of each `SKILL.md`, and never at its body.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

/// The most bytes of a `SKILL.md` that its front matter may take, with both
/// of its `---` lines. A reader needs no more of the file than this.
pub const MAX_FRONT_MATTER: usize = 64 * 1024;

/// The file that makes a directory a skill.
const SKILL_MD: &str = "SKILL.md";

/// The most characters each value may have, by the specification.
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
const MAX_COMPATIBILITY: usize = 500;

/// One skill, with the front matter keys that the specification defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// The directory that holds it, relative to the tree's root, with `/`
    /// between its parts. The root itself is empty.
    pub directory: String,
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// The specification's experimental `allowed-tools`, as written.
    pub allowed_tools: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    /// No directory in the tree holds a `SKILL.md`.
    NoSkills,
    /// A name was asked for, and no skill has it.
    NoSkill(String),
    /// A `SKILL.md` couldn't be read.
    Read { path: String, error: io::Error },
    /// A `SKILL.md` breaks the specification.
    Invalid { path: String, problem: Problem },
}

/// What is wrong with a `SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Its first line isn't `---`.
    NoFrontMatter,
    /// No `---` line ends its front matter.
    Unclosed,
    /// Its front matter is longer than [`MAX_FRONT_MATTER`].
    TooLong,
    NotUtf8,
    /// The YAML parser's message.
    Yaml(String),
    /// The front matter isn't one YAML mapping.
    NotMapping,
    Missing(&'static str),
    /// A key appears twice, at the top level or, after `metadata.`, in
    /// `metadata`.
    Repeated(String),
    /// A key that the specification defines has a value that isn't a
    /// scalar.
    NotText(&'static str),
    /// `metadata` isn't a mapping from scalars to scalars.
    Metadata,
    /// A top-level key that YAML readers resolve in different ways: an
    /// alias, the merge key `<<`, or a key with a tag, which can make any
    /// key a merge key. Each could give a key that the specification
    /// defines a value this reader never sees.
    Key(&'static str),
    /// `name` breaks the specification's rules for one.
    Name(String),
    /// `name` isn't the name of the skill's directory.
    Directory {
        name: String,
        directory: String,
    },
    /// A value is empty, or longer than `max` characters.
    Length {
        key: &'static str,
        max: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSkills => write!(f, "no directory holds a `{SKILL_MD}`"),
            Error::NoSkill(name) => write!(f, "no skill is named `{name}`"),
            Error::Read { path, error } => write!(f, "couldn't read {path}: {error}"),
            Error::Invalid { path, problem } => write!(f, "{path}: {problem}"),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::NoFrontMatter => write!(
                f,
                "has no front matter, which starts on its first line with `---`"
            ),
            Problem::Unclosed => write!(f, "its front matter has no closing `---` line"),
            Problem::TooLong => write!(
                f,
                "its front matter is longer than {MAX_FRONT_MATTER} bytes"
            ),
            Problem::NotUtf8 => write!(f, "its front matter isn't UTF-8"),
            Problem::Yaml(message) => write!(f, "its front matter isn't valid YAML: {message}"),
            Problem::NotMapping => write!(f, "its front matter isn't one YAML mapping"),
            Problem::Missing(key) => write!(f, "its front matter has no `{key}`"),
            Problem::Repeated(key) => write!(f, "`{key}` appears twice"),
            Problem::NotText(key) => write!(f, "`{key}` isn't text"),
            Problem::Metadata => write!(f, "`metadata` isn't a mapping from text to text"),
            Problem::Key(what) => write!(
                f,
                "its front matter has {what}, which YAML readers read in different ways"
            ),
            Problem::Name(name) => write!(
                f,
                "`name` is `{name}`, but a name is 1 to {MAX_NAME} of the letters a to z, \
                 digits, and single hyphens, with no hyphen at either end"
            ),
            Problem::Directory { name, directory } => write!(
                f,
                "`name` is `{name}`, but the skill's directory is `{directory}`"
            ),
            Problem::Length { key, max } => write!(f, "`{key}` must be 1 to {max} characters"),
        }
    }
}

/// The skills in a tree whose root is named `root`, and whose files are
/// `paths`, relative to the root, with `/` between their parts. When the
/// root holds a `SKILL.md`, that is the one skill. Otherwise each directory
/// with a `SKILL.md` is one, except a directory inside another skill, which
/// belongs to that skill. Skills come in the order of their directories.
///
/// With `names`, only the skills with those names are read, and a name that
/// no skill has is an error. Each skill's name is its directory's, so a
/// skill that isn't asked for is never read. `read` returns a file's bytes.
/// It may stop after `MAX_FRONT_MATTER + 1` of them, but no sooner, so a cut
/// line is never taken for the closing `---`.
pub fn find<'a>(
    root: &str,
    paths: impl IntoIterator<Item = &'a str>,
    names: &[&str],
    mut read: impl FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<Vec<Skill>, Error> {
    let directories: BTreeSet<&str> = paths
        .into_iter()
        .filter_map(|path| match path.strip_suffix(SKILL_MD)? {
            "" => Some(""),
            parent => parent.strip_suffix('/'),
        })
        .collect();
    let skills: Vec<&str> = match directories.contains("") {
        true => vec![""],
        false => directories
            .iter()
            .copied()
            .filter(|directory| {
                !ancestors(directory).any(|ancestor| directories.contains(ancestor))
            })
            .collect(),
    };
    if skills.is_empty() {
        return Err(Error::NoSkills);
    }
    let named = |directory: &str| match directory.rsplit_once('/') {
        Some((_, last)) => last.to_string(),
        None if directory.is_empty() => root.to_string(),
        None => directory.to_string(),
    };
    if let Some(missing) = names
        .iter()
        .find(|name| !skills.iter().any(|directory| named(directory) == **name))
    {
        return Err(Error::NoSkill(missing.to_string()));
    }
    skills
        .into_iter()
        .filter(|directory| names.is_empty() || names.contains(&named(directory).as_str()))
        .map(|directory| {
            let path = match directory {
                "" => SKILL_MD.to_string(),
                directory => format!("{directory}/{SKILL_MD}"),
            };
            let bytes = match read(&path) {
                Ok(bytes) => bytes,
                Err(error) => return Err(Error::Read { path, error }),
            };
            match parse(directory, &named(directory), &bytes) {
                Ok(skill) => Ok(skill),
                Err(problem) => Err(Error::Invalid { path, problem }),
            }
        })
        .collect()
}

/// The directories that hold `directory`, but not the root.
fn ancestors(directory: &str) -> impl Iterator<Item = &str> {
    directory
        .char_indices()
        .filter(|(_, c)| *c == '/')
        .map(|(at, _)| &directory[..at])
}

/// Reads the `SKILL.md` of the skill in `directory`, whose last part is
/// `name`, from its first bytes.
fn parse(directory: &str, name: &str, bytes: &[u8]) -> Result<Skill, Problem> {
    let yaml = front_matter(bytes)?;
    let yaml = std::str::from_utf8(yaml).map_err(|_| Problem::NotUtf8)?;
    let keys = Keys::read(yaml)?;
    let skill = Skill {
        directory: directory.to_string(),
        name: keys.name.ok_or(Problem::Missing("name"))?,
        description: keys.description.ok_or(Problem::Missing("description"))?,
        license: keys.license,
        compatibility: keys.compatibility,
        metadata: keys.metadata,
        allowed_tools: keys.allowed_tools,
    };
    if !valid_name(&skill.name) {
        return Err(Problem::Name(skill.name));
    }
    if skill.name != name {
        return Err(Problem::Directory {
            name: skill.name,
            directory: name.to_string(),
        });
    }
    within("description", Some(&skill.description), MAX_DESCRIPTION)?;
    within(
        "compatibility",
        skill.compatibility.as_deref(),
        MAX_COMPATIBILITY,
    )?;
    Ok(skill)
}

/// The YAML between a first line of `---` and the next line of `---`. A
/// fence line may end in spaces or tabs, and a line may end in `\r\n`.
fn front_matter(bytes: &[u8]) -> Result<&[u8], Problem> {
    let scanned = &bytes[..bytes.len().min(MAX_FRONT_MATTER)];
    let mut lines = scanned.split_inclusive(|byte| *byte == b'\n');
    let first = lines.next().ok_or(Problem::NoFrontMatter)?;
    if !is_fence(first) {
        return Err(Problem::NoFrontMatter);
    }
    let start = first.len();
    let mut end = start;
    for line in lines {
        // A last line cut off by the bound may not be the whole line.
        let whole = line.ends_with(b"\n") || scanned.len() == bytes.len();
        if whole && is_fence(line) {
            return Ok(&scanned[start..end]);
        }
        end += line.len();
    }
    match scanned.len() < bytes.len() {
        true => Err(Problem::TooLong),
        false => Err(Problem::Unclosed),
    }
}

fn is_fence(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.strip_prefix(b"---")
        .is_some_and(|rest| rest.iter().all(|byte| matches!(byte, b' ' | b'\t')))
}

/// Lowercase ASCII letters and digits, in runs joined by single hyphens.
/// The specification lists `a-z` and `0-9`, so no other letter counts.
fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name.split('-').all(|run| {
            !run.is_empty()
                && run
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn within(key: &'static str, value: Option<&str>, max: usize) -> Result<(), Problem> {
    match value {
        Some(value) if value.trim().is_empty() || value.chars().count() > max => {
            Err(Problem::Length { key, max })
        }
        _ => Ok(()),
    }
}

/// The keys of the front matter that the specification defines. Any other
/// key is a harness's own, and its value is skipped, whatever it is.
#[derive(Default)]
struct Keys {
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: BTreeMap<String, String>,
    allowed_tools: Option<String>,
}

impl Keys {
    /// Every value is its scalar's text, as skills-ref's strictyaml reads
    /// it, so `version: 1.0` is the text `1.0`. The parser hands over events,
    /// so an alias is never expanded.
    fn read(yaml: &str) -> Result<Keys, Problem> {
        let mut events = Events(Parser::new_from_str(yaml));
        events.expect(|event| matches!(event, Event::StreamStart))?;
        if !matches!(events.next()?, Event::DocumentStart) {
            return Err(Problem::NotMapping);
        }
        if !matches!(events.next()?, Event::MappingStart(..)) {
            return Err(Problem::NotMapping);
        }
        let mut keys = Keys::default();
        // YAML allows a key once in a mapping. A key that is a collection
        // is skipped with its value, and so are the mappings inside a value
        // that is skipped.
        let mut seen = BTreeSet::new();
        loop {
            let key = match events.next()? {
                Event::MappingEnd => break,
                Event::Alias(_) => return Err(Problem::Key("an alias as a key")),
                Event::Scalar(_, _, _, Some(_)) => return Err(Problem::Key("a key with a tag")),
                Event::Scalar(key, TScalarStyle::Plain, ..) if key == "<<" => {
                    return Err(Problem::Key("`<<` as a key"));
                }
                Event::Scalar(key, ..) if seen.contains(&key) => {
                    return Err(Problem::Repeated(key));
                }
                Event::Scalar(key, ..) => {
                    seen.insert(key.clone());
                    key
                }
                other => {
                    // A key that is a collection, which no reader takes
                    // for one the specification defines.
                    events.skip(other)?;
                    events.value()?;
                    continue;
                }
            };
            let (known, slot) = match key.as_str() {
                "name" => ("name", &mut keys.name),
                "description" => ("description", &mut keys.description),
                "license" => ("license", &mut keys.license),
                "compatibility" => ("compatibility", &mut keys.compatibility),
                "allowed-tools" => ("allowed-tools", &mut keys.allowed_tools),
                "metadata" => {
                    keys.metadata = events.metadata()?;
                    continue;
                }
                _ => {
                    events.value()?;
                    continue;
                }
            };
            match events.next()? {
                Event::Scalar(value, ..) => *slot = Some(value),
                other => {
                    events.skip(other)?;
                    return Err(Problem::NotText(known));
                }
            }
        }
        events.expect(|event| matches!(event, Event::DocumentEnd))?;
        match events.next()? {
            Event::StreamEnd => Ok(keys),
            _ => Err(Problem::NotMapping),
        }
    }
}

struct Events<'a>(Parser<std::str::Chars<'a>>);

impl Events<'_> {
    fn next(&mut self) -> Result<Event, Problem> {
        match self.0.next_token() {
            Ok((event, _)) => Ok(event),
            Err(error) => Err(Problem::Yaml(error.to_string())),
        }
    }

    fn expect(&mut self, wanted: impl Fn(&Event) -> bool) -> Result<(), Problem> {
        match wanted(&self.next()?) {
            true => Ok(()),
            false => Err(Problem::NotMapping),
        }
    }

    /// Skips the next node.
    fn value(&mut self) -> Result<(), Problem> {
        let first = self.next()?;
        self.skip(first)
    }

    /// Skips the rest of the node that `first` starts. It counts how deep
    /// it is, so nesting costs no stack. Only a node can start here, so
    /// anything else is the end of the mapping, or of the document.
    fn skip(&mut self, first: Event) -> Result<(), Problem> {
        let mut depth = 0usize;
        let mut event = first;
        loop {
            match event {
                Event::SequenceStart(..) | Event::MappingStart(..) => depth += 1,
                Event::SequenceEnd | Event::MappingEnd if depth > 0 => depth -= 1,
                Event::Scalar(..) | Event::Alias(_) => {}
                _ => return Err(Problem::NotMapping),
            }
            if depth == 0 {
                return Ok(());
            }
            event = self.next()?;
        }
    }

    /// The value of `metadata`: a mapping from scalars to scalars.
    fn metadata(&mut self) -> Result<BTreeMap<String, String>, Problem> {
        if !matches!(self.next()?, Event::MappingStart(..)) {
            return Err(Problem::Metadata);
        }
        let mut metadata = BTreeMap::new();
        loop {
            let key = match self.next()? {
                Event::MappingEnd => return Ok(metadata),
                Event::Scalar(key, ..) => key,
                _ => return Err(Problem::Metadata),
            };
            let Event::Scalar(value, ..) = self.next()? else {
                return Err(Problem::Metadata);
            };
            if metadata.contains_key(&key) {
                return Err(Problem::Repeated(format!("metadata.{key}")));
            }
            metadata.insert(key, value);
        }
    }
}
