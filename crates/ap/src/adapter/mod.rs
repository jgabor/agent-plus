//! The harness adapters, from SPEC.md's AGENT section. An adapter says where
//! the harness's configuration goes, which first-run state it prepares, and
//! what its command line holds. `ap build` puts the files it renders in the
//! agent's final layer, and `ap run` starts the harness with its arguments.

pub mod claude;
pub mod codex;

use agentfile::{Harness, Reserved};

/// The variables that point tools at the run's certificate authority, from
/// SPEC.md's SECRET section. Every harness gets them.
pub const CERTIFICATES: [&str; 5] = [
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
];

/// What the adapter for `harness` keeps for itself: the variables it sets,
/// the ones the harness reads for sign-in, and the harness's own hosts.
pub fn reserved(harness: Harness) -> Reserved<'static> {
    let (name, set, sign_in, hosts) = match harness {
        Harness::Claude => ("Claude Code", claude::SET, claude::SIGN_IN, claude::HOSTS),
        Harness::Codex => ("Codex", codex::SET, codex::SIGN_IN, codex::HOSTS),
    };
    Reserved {
        harness: name,
        set: CERTIFICATES.iter().chain(set).copied().collect(),
        sign_in,
        hosts,
    }
}

/// `SYSTEM`'s values as one text: in file order, separated by a blank line,
/// and ending in a newline. A value's own blank lines are kept.
pub fn system(values: &[Vec<u8>]) -> Vec<u8> {
    let mut text: Vec<u8> = Vec::new();
    for value in values {
        if !text.is_empty() {
            if !text.ends_with(b"\n") {
                text.push(b'\n');
            }
            text.push(b'\n');
        }
        text.extend_from_slice(value);
    }
    if !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    text
}

/// An exact version, as numbers to compare. A part that isn't a number, such
/// as a pre-release suffix, ends it.
fn version(text: &str) -> Vec<u64> {
    text.split('.')
        .map_while(|part| part.parse().ok())
        .collect()
}

/// The level as `EFFORT` writes it, which is also how both harnesses write
/// it.
fn effort_name(effort: agentfile::Effort) -> &'static str {
    use agentfile::Effort;
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The guest home directory, which a file share never adds to the workspace.
const HOME: &str = "/home/agent";

/// The directories that shares add to the harness's workspace, from SPEC.md's
/// MODE section, in share order and without repeats: each directory share,
/// and the directory that holds each file share, unless that is the home
/// directory. Each share is its guest path, and whether it is a directory.
pub fn workspace<'a>(shares: impl IntoIterator<Item = (&'a str, bool)>) -> Vec<&'a str> {
    let mut directories: Vec<&str> = Vec::new();
    for (path, directory) in shares {
        let added = match (directory, path.rsplit_once('/')) {
            (true, _) => path,
            (false, Some((HOME, _)) | None) => continue,
            (false, Some(("", _))) => "/",
            (false, Some((parent, _))) => parent,
        };
        if !directories.contains(&added) {
            directories.push(added);
        }
    }
    directories
}
