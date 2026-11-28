//! Stub.
use yaml_rust2 as _;
use std::collections::BTreeMap;
use std::fmt;
use std::io;

pub const MAX_FRONT_MATTER: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub directory: String,
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    NoSkills,
    NoSkill(String),
    Read { path: String, error: io::Error },
    Invalid { path: String, problem: Problem },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    NoFrontMatter,
    Unclosed,
    TooLong,
    NotUtf8,
    Yaml(String),
    NotMapping,
    Missing(&'static str),
    Repeated(String),
    NotText(&'static str),
    Metadata,
    Name(String),
    Directory { name: String, directory: String },
    Length { key: &'static str, max: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub fn find<'a>(
    _root: &str,
    _paths: impl IntoIterator<Item = &'a str>,
    _names: &[&str],
    _read: impl FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<Vec<Skill>, Error> {
    Err(Error::NoSkills)
}
