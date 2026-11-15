//! Reads a git repository the way `git` does: loose objects, and refs with
//! `packed-refs`. It reads only SHA-1 repositories.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// An object's SHA-1 name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Id(pub [u8; 20]);

impl Id {
    /// Parses 40 hex digits, in either case.
    pub fn from_hex(_hex: &str) -> Option<Id> {
        None
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// An object's type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        })
    }
}

/// An object's type and contents.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// What a ref points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Object(Id),
    /// Another ref, by its full name.
    Symbolic(String),
}

/// Why a read failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The repository holds something git wouldn't read.
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(error) => error.fmt(f),
            Error::Corrupt(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

/// A repository, by its git directory.
pub struct Repository {
    #[allow(dead_code)]
    git_dir: PathBuf,
}

impl Repository {
    /// The repository whose git directory is `git_dir`, such as `.git`.
    pub fn at(git_dir: impl Into<PathBuf>) -> Repository {
        Repository {
            git_dir: git_dir.into(),
        }
    }

    /// The loose object named `id`, or `None` if there is no loose object.
    pub fn loose_object(&self, _id: &Id) -> Result<Option<Object>, Error> {
        let _ = miniz_oxide::inflate::decompress_to_vec_zlib;
        Err(Error::Corrupt("unimplemented".into()))
    }

    /// The ref named `name`, which is `HEAD` or a full name under `refs/`,
    /// from its loose file or else from `packed-refs`.
    pub fn find_ref(&self, _name: &str) -> Result<Option<Target>, Error> {
        Err(Error::Corrupt("unimplemented".into()))
    }

    /// The object that `name` points at, through any symbolic refs.
    pub fn resolve(&self, _name: &str) -> Result<Option<Id>, Error> {
        Err(Error::Corrupt("unimplemented".into()))
    }

    /// Every ref under `refs/`, loose and packed, sorted by name.
    pub fn refs(&self) -> Result<Vec<(String, Target)>, Error> {
        Err(Error::Corrupt("unimplemented".into()))
    }
}
