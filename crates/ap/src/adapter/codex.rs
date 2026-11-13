//! The Codex adapter.
//!
//! Its files are the global instructions from `SYSTEM`, and the user
//! configuration, which turns the update check off and records the one-time
//! screen reader probe as done, as Codex does after its first start. Its
//! command line sets the model, the effort, and the mode every time it starts
//! the harness, with flags and `-c` overrides, so neither a project's
//! configuration nor an earlier session can pick others.
//!
//! What each version accepts was read from the published releases, by giving
//! each an option or a value it refuses:
//!
//! | Versions | Changes |
//! |---|---|
//! | before 0.39.0 | refuse the configuration, or read `SYSTEM` from another file |
//! | 0.39.0 | `model_reasoning_effort` takes `low`, `medium`, and `high` |
//! | 0.59.0 | and `xhigh` |
//! | 0.115.0 | `approvals_reviewer`, with `user` and `guardian_subagent` |
//! | 0.124.0 | `approvals_reviewer` takes `auto_review` for `guardian_subagent` |
//! | 0.138.0 | `model_reasoning_effort` takes `max` |
//! | 0.147.0 | `codex exec` has `--approve-for-me` |
//! | 0.156.0 | the interactive `codex` has `--no-daemon` |
//!
//! An interactive Codex 0.169.0 without `-c` overrides uses a background
//! server, which installs a package of its own and updates it. An
//! interactive run passes `--no-daemon` wherever the version has it.
//!
//! A version without `approvals_reviewer` has no reviewer, so `auto` there
//! asks in an interactive run, as `edit` does, and is denied in a headless
//! one.

use agentfile::{Effort, Mode};

use super::{effort_name, version};

/// Where `SYSTEM` goes.
pub const INSTRUCTIONS: &str = "/home/agent/.codex/AGENTS.md";
/// The user configuration.
pub const CONFIGURATION: &str = "/home/agent/.codex/config.toml";

/// The oldest version the adapter configures. Every release from it on
/// takes the configuration and each command line.
pub const OLDEST: &str = "0.39.0";

/// The first version whose `model_reasoning_effort` takes `xhigh`.
const XHIGH_FROM: [u64; 3] = [0, 59, 0];
/// The first version that has `approvals_reviewer`.
const REVIEWER_FROM: [u64; 3] = [0, 115, 0];
/// The first version whose `approvals_reviewer` takes `auto_review`.
const AUTO_REVIEW_FROM: [u64; 3] = [0, 124, 0];
/// The first version whose `model_reasoning_effort` takes `max`.
const MAX_FROM: [u64; 3] = [0, 138, 0];
/// The first version whose `codex exec` has `--approve-for-me`.
const APPROVE_FOR_ME_FROM: [u64; 3] = [0, 147, 0];
/// The first version whose interactive `codex` has `--no-daemon`.
const NO_DAEMON_FROM: [u64; 3] = [0, 156, 0];

/// The user configuration: the update check off, so Codex never offers to
/// install another version, and the screen reader probe done, so a new VM
/// doesn't repeat it.
const CONFIGURATION_TEXT: &str = "\
check_for_update_on_startup = false

[tui]
screen_reader_detection_done = true
";

/// The files the adapter puts in the configuration layer, by guest path, in
/// path order: `SYSTEM`'s values in [`INSTRUCTIONS`], if there are any, then
/// [`CONFIGURATION`].
pub fn files(system: &[Vec<u8>]) -> Vec<(&'static str, Vec<u8>)> {
    let mut files = Vec::new();
    if !system.is_empty() {
        files.push((INSTRUCTIONS, super::system(system)));
    }
    files.push((CONFIGURATION, CONFIGURATION_TEXT.as_bytes().to_vec()));
    files
}

/// Why `ap` can't configure this version of Codex, if it can't.
pub fn unsupported(version_text: &str) -> Option<String> {
    (version(version_text) < version(OLDEST)).then(|| {
        format!("Codex {version_text} is older than {OLDEST}, the oldest version `ap` configures")
    })
}

/// The level this version of Codex gets for `effort`, and a warning when
/// that isn't the level asked for. A level the version doesn't accept
/// becomes the nearest lower one it does.
pub fn effort(version_text: &str, effort: Effort) -> (&'static str, Option<String>) {
    let at_least = |from: &[u64]| version(version_text).as_slice() >= from;
    let level = match effort {
        Effort::Max if at_least(&MAX_FROM) => Effort::Max,
        Effort::Max | Effort::Xhigh if at_least(&XHIGH_FROM) => Effort::Xhigh,
        Effort::Max | Effort::Xhigh => Effort::High,
        effort => effort,
    };
    let (asked, given) = (effort_name(effort), effort_name(level));
    let warning = (level != effort).then(|| {
        format!("Codex {version_text} doesn't accept `EFFORT {asked}`, so it uses `{given}`")
    });
    (given, warning)
}

/// The arguments after `codex` for this version: `exec` for a headless run,
/// then the mode, the model, and the effort. `model` is passed unchanged.
/// Without a model or an effort, Codex uses its own default.
///
/// The mode is the sandbox, the approval policy, and the reviewer where the
/// version has one, and for the `workspace-write` sandbox its network
/// access, each set every time. A
/// headless run in `auto` uses `--approve-for-me`, which sets the same
/// sandbox, approval policy, and reviewer, where the version has it.
pub fn arguments(
    version_text: &str,
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Mode,
    interactive: bool,
) -> Vec<String> {
    let at_least = |from: &[u64]| version(version_text).as_slice() >= from;
    let mut arguments: Vec<String> = Vec::new();
    if !interactive {
        arguments.push("exec".to_owned());
    } else if at_least(&NO_DAEMON_FROM) {
        arguments.push("--no-daemon".to_owned());
    }
    match mode {
        Mode::Full => arguments.push("--dangerously-bypass-approvals-and-sandbox".to_owned()),
        Mode::Auto if !interactive && at_least(&APPROVE_FOR_ME_FROM) => {
            arguments.push("--approve-for-me".to_owned());
        }
        mode => {
            let (sandbox, approval, reviewer) = match mode {
                Mode::Read => ("read-only", "never", "user"),
                Mode::Ask => ("read-only", "on-request", "user"),
                Mode::Edit => ("workspace-write", "on-request", "user"),
                _ if at_least(&AUTO_REVIEW_FROM) => {
                    ("workspace-write", "on-request", "auto_review")
                }
                _ => ("workspace-write", "on-request", "guardian_subagent"),
            };
            let mut settings = vec![("sandbox_mode", sandbox), ("approval_policy", approval)];
            if at_least(&REVIEWER_FROM) {
                settings.push(("approvals_reviewer", reviewer));
            }
            for (key, value) in settings {
                arguments.extend(["-c".to_owned(), format!("{key}=\"{value}\"")]);
            }
        }
    }
    // `NETWORK` already decides what the agent can reach.
    let network = match mode {
        Mode::Edit => Some(false),
        Mode::Auto => Some(true),
        _ => None,
    };
    if let Some(network) = network {
        arguments.extend([
            "-c".to_owned(),
            format!("sandbox_workspace_write.network_access={network}"),
        ]);
    }
    if let Some(model) = model {
        arguments.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = effort {
        let (level, _) = self::effort(version_text, effort);
        arguments.extend([
            "-c".to_owned(),
            format!("model_reasoning_effort=\"{level}\""),
        ]);
    }
    arguments
}
