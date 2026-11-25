//! The harness adapters, from SPEC.md's AGENT section. An adapter says where
//! the harness's configuration goes, which first-run state it prepares, and
//! what its command line holds. `ap build` puts the files it renders in the
//! agent's final layer, and `ap run` starts the harness with its arguments.

pub mod claude;
pub mod codex;

use agentfile::{Auth, Harness, Reserved};

/// The variables that point tools at the run's certificate authority, from
/// SPEC.md's SECRET section. Every harness gets them.
pub const CERTIFICATES: [&str; 5] = [
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
];

/// The variables `ap` sets for every run, after `ENV`, from SPEC.md's ENV
/// section.
pub const RUN: [&str; 3] = ["HOME", "USER", "LOGNAME"];

/// What the adapter for `harness` keeps for itself: the variables `ap` and
/// it set, the ones the harness reads for sign-in, and the harness's own
/// hosts.
pub fn reserved(harness: Harness) -> Reserved<'static> {
    let (name, set, sign_in, hosts) = match harness {
        Harness::Claude => ("Claude Code", claude::SET, claude::SIGN_IN, claude::HOSTS),
        Harness::Codex => ("Codex", codex::SET, codex::SIGN_IN, codex::HOSTS),
    };
    Reserved {
        harness: name,
        set: RUN
            .iter()
            .chain(&CERTIFICATES)
            .chain(set)
            .copied()
            .collect(),
        sign_in,
        hosts,
    }
}

/// The request headers that the proxy may put a hidden secret's value in,
/// for a run of `harness`.
pub fn headers(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => claude::HEADERS,
        Harness::Codex => codex::HEADERS,
    }
}

/// How `AUTH key` or `AUTH token` signs a harness in, from SPEC.md's AUTH
/// section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// The variable that `ap` reads on your host, and sets in the guest.
    pub variable: &'static str,
    /// The hosts the proxy swaps a key's value in on. A token has none, and
    /// the guest gets its value.
    pub hosts: &'static [&'static str],
}

/// How `auth` signs `harness` in, or `None` for `AUTH login`, which isn't
/// built yet, and for a method the harness doesn't have.
pub fn sign_in(harness: Harness, auth: Auth) -> Option<SignIn> {
    match (harness, auth) {
        (_, Auth::Login) | (Harness::Codex, Auth::Token) => None,
        (Harness::Claude, Auth::Key) => Some(SignIn {
            variable: claude::KEY,
            hosts: claude::KEY_HOSTS,
        }),
        (Harness::Claude, Auth::Token) => Some(SignIn {
            variable: claude::TOKEN,
            hosts: &[],
        }),
        (Harness::Codex, Auth::Key) => Some(SignIn {
            variable: codex::KEY,
            hosts: codex::KEY_HOSTS,
        }),
    }
}

/// The files a run writes in the guest for `harness`, by guest path, when
/// it starts in `directory`, and `AUTH key` gives it `placeholder`, if it
/// does: Claude Code's [`claude::STATE`], which trusts `directory` and
/// approves the placeholder. The placeholder is hex, so it needs no escape.
pub fn run_files(
    harness: Harness,
    directory: &str,
    placeholder: Option<&[u8]>,
) -> Vec<(&'static str, Vec<u8>)> {
    match harness {
        Harness::Claude => vec![(claude::STATE, claude::state(directory, placeholder))],
        Harness::Codex => Vec::new(),
    }
}

/// The secrets that git uses for HTTPS to `github.com`, from SPEC.md's
/// SECRET section.
pub const GIT_TOKENS: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];

/// Git's system configuration in the guest. No layer writes it, and the run's
/// root is new each run, so it holds only what the run puts there.
pub const GIT_CONFIG: &str = "/etc/gitconfig";

/// The [`GIT_CONFIG`] that has git sign in to `https://github.com` with
/// `placeholder`, which the proxy replaces in `Authorization: Basic`. Any
/// username works with a token, and the placeholder is hex, so it needs no
/// escape. The helper answers any request, and git reads only the answer to
/// `get`.
pub fn git_config(placeholder: &[u8]) -> Vec<u8> {
    let mut text = b"[credential \"https://github.com\"]\n\
        \thelper = \"!f() { echo username=x-access-token; echo password="
        .to_vec();
    text.extend_from_slice(placeholder);
    text.extend_from_slice(b"; }; f\"\n");
    text
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
