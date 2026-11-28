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
//! | before 0.137.0 | `codex exec` asks no reviewer, so a headless `auto` is denied what it would review |
//! | 0.137.0 | `model_reasoning_effort` takes `low` to `xhigh`, and `approvals_reviewer` takes `auto_review` |
//! | 0.138.0 | `model_reasoning_effort` takes `max` |
//! | 0.145.0 | `[agents]` takes `default_subagent_reasoning_effort` |
//! | 0.147.0 | `codex exec` has `--approve-for-me` |
//! | 0.156.0 | the interactive `codex` has `--no-daemon`, and `[auto_review]` takes `experimental_policy_template` |
//! | 0.157.0 | `[auto_review]` takes `extra_policy` |
//! | 0.160.0 | `[auto_review]` takes `experimental_conversation_history_prompt` |
//! | 0.169.0 | `[auto_review]` no longer takes `extra_policy` or `experimental_policy_template` |
//!
//! A trusted project's `.codex/config.toml` can't select a profile: Codex
//! 0.137.0, 0.147.0, 0.156.0, and 0.169.0 ignore its `profile` and
//! `profiles` keys. Each still applies its
//! `sandbox_workspace_write.writable_roots` despite the other overrides, so
//! the adapter empties that list too.
//!
//! A trusted project's `[auto_review]` text reaches the reviewer's prompt,
//! and can change its verdict. In `auto`, the adapter sets each of those
//! keys the version takes to an empty string, which Codex reads as unset.
//! A trusted project's `model_catalog_json` can still give the reviewer its
//! policy on 0.169.0, because an unset `policy` falls back to the catalog's.
//!
//! A project's `features.guardian_approval = false` also applies. Codex
//! 0.169.0 still sends sandbox escalations and MCP tool calls to the reviewer
//! then, but its source skips the reviewer on other paths, so in `auto` the
//! adapter turns the feature on, as it is by default.
//!
//! A trusted project's `[features.guardianv2]` turns on a classifier, and
//! sets its instructions and the score under which Codex approves an action
//! without asking the reviewer. With `review_threshold = 1.0`, Codex 0.169.0
//! and 0.172.0 applied a patch outside the workspace with no review. So in
//! `auto` the adapter turns the feature off, as it is by default, and the
//! table goes with it. Codex 0.146.0 and later know the feature, and none
//! from 0.137.0 to 0.172.0 warns about the override. A trusted project's
//! `model_catalog_json` can still turn the classifier on, with its own
//! threshold and instructions, because a catalog's `guardian` policy skips
//! the feature.
//!
//! A trusted project's `agents.default_subagent_reasoning_effort` sets the
//! effort a subagent starts with, in place of the parent's, so the adapter
//! sets it to the level of `EFFORT`. Codex 0.144.6 and older read the key as
//! an agent role, and refuse to start. Codex checks that level against the
//! model's metadata, as it does any level asked for a subagent, and refuses
//! the subagent when the level isn't listed, as for a model Codex doesn't
//! know. A role still sets its own effort, from the role's file in the
//! project's `.codex/agents` or `[agents]`, because a role in a higher layer
//! can't take away a lower layer's file. So can the model, when it starts
//! the subagent.
//!
//! An interactive Codex 0.169.0 without `-c` overrides uses a background
//! server, which installs a package of its own and updates it. An
//! interactive run passes `--no-daemon` wherever the version has it.
//!
//! Older versions differ more: before 0.115.0 there is no reviewer, before
//! 0.59.0 no `xhigh`, before 0.39.0 the configuration is refused, and before
//! 0.15.0 `SYSTEM` would go to another file. So the adapter starts at
//! 0.137.0, where every mode means what SPEC.md says.

use agentfile::{Effort, Mode};

use super::{effort_name, version};

/// Where `SYSTEM` goes.
pub const INSTRUCTIONS: &str = "/home/agent/.codex/AGENTS.md";
/// The user configuration.
pub const CONFIGURATION: &str = "/home/agent/.codex/config.toml";
/// Where the harness layer puts Codex's code-mode host, which Codex runs
/// its tools through: beside Codex, where Codex 0.171.0 looks for it.
pub const CODE_MODE_HOST: &str = "/usr/local/bin/codex-code-mode-host";
/// Where the harness layer puts the bubblewrap that Codex's sandbox runs
/// commands in: where Codex 0.171.0 looks for the one bundled with it, when
/// no `bwrap` is on `PATH`.
pub const BWRAP: &str = "/usr/local/bin/codex-resources/bwrap";

/// The variables the adapter sets, besides the certificate variables.
pub const SET: &[&str] = &[];

/// The variables Codex reads for sign-in. Codex 0.169.0 looks for these
/// three before its stored sign-in.
pub const SIGN_IN: &[&str] = &["CODEX_ACCESS_TOKEN", "CODEX_API_KEY", "OPENAI_API_KEY"];

/// The variable that `AUTH key` reads on your host and sets in the guest.
/// `codex exec` 0.169.0 sends it in `Authorization` to `api.openai.com`.
pub const KEY: &str = "CODEX_API_KEY";
/// The hosts the proxy swaps the key's value in on.
pub const KEY_HOSTS: &[&str] = &["api.openai.com"];

/// The request headers that the proxy may put a hidden secret's value in,
/// from SPEC.md's SECRET section. Codex sends its key in `Authorization`,
/// and other clients a key in `X-Api-Key`.
pub const HEADERS: &[&str] = &["Authorization", "X-Api-Key"];

/// The hosts Codex connects to for its model and sign-in. Codex 0.169.0
/// calls the model on `api.openai.com` with an API key, and on `chatgpt.com`
/// with a ChatGPT sign-in, which it gets from `auth.openai.com`.
pub const HOSTS: &[&str] = &["api.openai.com", "auth.openai.com", "chatgpt.com"];

/// The oldest version the adapter configures.
pub const OLDEST: &str = "0.137.0";

/// The first version whose `model_reasoning_effort` takes `max`.
const MAX_FROM: [u64; 3] = [0, 138, 0];
/// The first version whose `[agents]` takes `default_subagent_reasoning_effort`.
const SUBAGENT_EFFORT_FROM: [u64; 3] = [0, 145, 0];
/// The first version whose `codex exec` has `--approve-for-me`.
const APPROVE_FOR_ME_FROM: [u64; 3] = [0, 147, 0];
/// The first version whose interactive `codex` has `--no-daemon`.
const NO_DAEMON_FROM: [u64; 3] = [0, 156, 0];

/// A Codex version, as its three numbers.
type Version = [u64; 3];

/// The `[auto_review]` keys that give the reviewer instructions, each with
/// the first version that takes it and the first that no longer does.
const REVIEWER_TEXT: [(&str, Version, Option<Version>); 4] = [
    ("policy", [0, 137, 0], None),
    (
        "experimental_policy_template",
        [0, 156, 0],
        Some([0, 169, 0]),
    ),
    ("extra_policy", [0, 157, 0], Some([0, 169, 0])),
    (
        "experimental_conversation_history_prompt",
        [0, 160, 0],
        None,
    ),
];

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
/// becomes the nearest lower one it does. The version is one the adapter
/// configures.
pub fn effort(version_text: &str, effort: Effort) -> (&'static str, Option<String>) {
    let at_least = |from: &[u64]| version(version_text).as_slice() >= from;
    let level = match effort {
        Effort::Max if !at_least(&MAX_FROM) => Effort::Xhigh,
        effort => effort,
    };
    let (asked, given) = (effort_name(effort), effort_name(level));
    let warning = (level != effort).then(|| {
        format!("Codex {version_text} doesn't accept `EFFORT {asked}`, so it uses `{given}`")
    });
    (given, warning)
}

/// The arguments after `codex` for this version: `exec` for a headless run,
/// which skips the git repository check, then the mode, the model, and the effort. `model` is passed unchanged.
/// Without a model or an effort, Codex uses its own default. The effort is
/// set for plan mode too, and for the subagents it starts where the version
/// takes that.
///
/// The mode is the sandbox, the approval policy, and the reviewer, and for
/// the `workspace-write` sandbox its network access and an empty list of
/// extra writable roots, each set every time. A
/// headless run in `auto` uses `--approve-for-me`, which sets the same
/// sandbox, approval policy, and reviewer, where the version has it. In
/// `auto`, the reviewer's feature is on, guardian v2 is off, and each
/// [`REVIEWER_TEXT`] key the version takes is set empty.
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
        // `codex exec` refuses a directory that isn't a git repository, such
        // as `/home/agent`, and trusting the directory doesn't change that.
        arguments.extend(["exec".to_owned(), "--skip-git-repo-check".to_owned()]);
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
                _ => ("workspace-write", "on-request", "auto_review"),
            };
            for (key, value) in [
                ("sandbox_mode", sandbox),
                ("approval_policy", approval),
                ("approvals_reviewer", reviewer),
            ] {
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
            // A trusted project's `writable_roots` would otherwise survive
            // the overrides, and widen the sandbox. `--add-dir` still adds
            // the workspace.
            "-c".to_owned(),
            "sandbox_workspace_write.writable_roots=[]".to_owned(),
        ]);
    }
    if mode == Mode::Auto {
        arguments.extend([
            "-c".to_owned(),
            "features.guardian_approval=true".to_owned(),
            "-c".to_owned(),
            "features.guardianv2=false".to_owned(),
        ]);
        for (key, from, until) in REVIEWER_TEXT {
            if at_least(&from) && until.is_none_or(|until| !at_least(&until)) {
                arguments.extend(["-c".to_owned(), format!("auto_review.{key}=\"\"")]);
            }
        }
    }
    if let Some(model) = model {
        arguments.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = effort {
        let (level, _) = self::effort(version_text, effort);
        // Plan mode takes its own effort, from a trusted project or else
        // its preset's `medium`.
        let mut keys = vec!["model_reasoning_effort", "plan_mode_reasoning_effort"];
        if at_least(&SUBAGENT_EFFORT_FROM) {
            keys.push("agents.default_subagent_reasoning_effort");
        }
        for key in keys {
            arguments.extend(["-c".to_owned(), format!("{key}=\"{level}\"")]);
        }
    }
    arguments
}

/// The arguments that add `directories`, from [`super::workspace`], to the
/// workspace, and mark each as trusted, so Codex loads its configuration,
/// hooks, and rules: `--add-dir` before each, and one `-c` override of the
/// `projects` table. The table is written whole, because Codex splits a
/// dotted key at every `.`, including one inside a quoted path.
/// Codex 0.137.0 and 0.169.0 take both, and load a trusted directory's
/// `.codex/config.toml`.
pub fn workspace(directories: &[&str]) -> Vec<String> {
    let mut arguments = Vec::new();
    for directory in directories {
        arguments.extend(["--add-dir".to_owned(), (*directory).to_owned()]);
    }
    if !directories.is_empty() {
        let projects: Vec<String> = (directories.iter())
            .map(|directory| format!("{}={{trust_level=\"trusted\"}}", toml_string(directory)))
            .collect();
        arguments.extend([
            "-c".to_owned(),
            format!("projects={{{}}}", projects.join(",")),
        ]);
    }
    arguments
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    let mut quoted = String::from('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", u32::from(character)));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}
