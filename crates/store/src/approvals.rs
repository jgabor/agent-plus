//! The grants you approved, each kept with the canonical path of the
//! Agentfile you approved them for, by SPEC.md's Grants section.
//!
//! An approval is a file named after the SHA-256 of that path. Its first line
//! names the path, and the rest is the grants' text form:
//!
//! ```text
//! AGENTFILE /home/me/notes/Agentfile
//! PATH /home/me/shared
//! NETWORK -
//! POLICY -
//! ```
//!
//! An approval appears complete and synced, or not at all. The last writer
//! wins.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentfile::{Grants, print_argument, read_arguments};

use crate::layers::{create_partial, sha256, sync_up_from};

/// The approvals in a store. Creating one touches nothing.
pub struct Approvals {
    dir: PathBuf,
}

impl Approvals {
    /// The approvals under the store root.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Approvals {
            dir: root.as_ref().join("approvals"),
        }
    }

    /// The grants approved for the Agentfile at this canonical path, if any
    /// were.
    pub fn get(&self, agentfile: &Path) -> io::Result<Option<Grants>> {
        let key = key(agentfile)?;
        let text = match fs::read_to_string(self.dir.join(sha256(key.as_bytes()))) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let invalid = |message: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the approval for {key}: {message}"),
            )
        };
        let (first, rest) = text.split_once('\n').unwrap_or((&text, ""));
        let named = first
            .strip_prefix("AGENTFILE ")
            .and_then(|path| read_arguments(path).ok());
        if named.as_deref() != Some(std::slice::from_ref(&key)) {
            return Err(invalid("line 1 doesn't name its Agentfile".to_string()));
        }
        rest.parse()
            .map(Some)
            .map_err(|errors: Vec<agentfile::Error>| {
                let lines: Vec<String> = errors
                    .iter()
                    .map(|error| format!("line {}: {}", error.line + 1, error.message))
                    .collect();
                invalid(lines.join(", "))
            })
    }

    /// Stores `grants` as approved for the Agentfile at this canonical path,
    /// in place of what was approved before.
    pub fn put(&self, agentfile: &Path, grants: &Grants) -> io::Result<()> {
        let key = key(agentfile)?;
        let text = format!("AGENTFILE {}\n{grants}", print_argument(&key));
        fs::create_dir_all(&self.dir)?;
        let name = sha256(key.as_bytes());
        let (partial, mut file) = create_partial(&self.dir, &name)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&partial.0, self.dir.join(name))?;
        sync_up_from(&self.dir)
    }
}

/// The path as text. It must be absolute, since only a canonical path keys
/// an approval.
fn key(agentfile: &Path) -> io::Result<String> {
    match agentfile.to_str() {
        Some(path) if agentfile.is_absolute() => Ok(path.to_string()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} isn't an absolute UTF-8 path, so it can't key an approval",
                agentfile.display()
            ),
        )),
    }
}
