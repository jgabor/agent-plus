//! The Claude Code adapter.
//!
//! Its files are the global instructions from `SYSTEM`, the user settings
//! that turn self-updates off, and the state that tells Claude Code its
//! onboarding is done. Its command line sets the model, the effort, and the
//! permission mode every time it starts the harness, so neither a project's
//! settings nor an earlier session can pick others. A version older than
//! `--effort` gets the effort from `CLAUDE_CODE_EFFORT_LEVEL`, which it reads
//! before its settings.
//!
//! What each version accepts was read from the published releases, by giving
//! each an option it refuses, or from the code that reads the variable:
//!
//! | Versions | Effort through | Levels |
//! |---|---|---|
//! | before 2.0.46 | nothing | none |
//! | 2.0.46 to 2.1.22 | `CLAUDE_CODE_EFFORT_LEVEL` | `low`, `medium`, `high` |
//! | 2.1.23 to 2.1.37 | `CLAUDE_CODE_EFFORT_LEVEL` | and `max` |
//! | 2.1.38 to 2.1.110 | `--effort` | `low`, `medium`, `high`, `max` |
//! | 2.1.111 on | `--effort` | and `xhigh` |
//!
//! `--permission-mode` takes `auto` from 2.1.71.

use agentfile::{Auth, Effort, Mode};

use super::{effort_name as name, version};

/// Where `SYSTEM` goes.
pub const INSTRUCTIONS: &str = "/home/agent/.claude/CLAUDE.md";
/// The user settings.
pub const SETTINGS: &str = "/home/agent/.claude/settings.json";
/// The state Claude Code keeps for itself, which says whether its onboarding
/// is done.
pub const STATE: &str = "/home/agent/.claude.json";

/// The variable that sets the effort of a version without `--effort`.
pub const EFFORT_VARIABLE: &str = "CLAUDE_CODE_EFFORT_LEVEL";

/// The variables the adapter sets, besides the certificate variables:
/// [`EFFORT_VARIABLE`], the one its settings set, and the one SPEC.md's AUTH
/// section gives `AUTH login`.
pub const SET: &[&str] = &[EFFORT_VARIABLE, "DISABLE_AUTOUPDATER", "CLAUDE_CONFIG_DIR"];

/// The variables Claude Code reads for sign-in. Claude Code 2.1.325 picks its
/// credential from these, in the code that names each source, and reads the
/// refresh token to sign in from it. It also sends the `Authorization` and
/// `X-Api-Key` headers in `ANTHROPIC_CUSTOM_HEADERS` to its model, which
/// signs it in as well.
pub const SIGN_IN: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
];

/// The variable that `AUTH key` reads on your host and sets in the guest.
/// Claude Code 2.1.330 sends it in `X-Api-Key` to `api.anthropic.com`.
pub const KEY: &str = "ANTHROPIC_API_KEY";
/// The hosts the proxy swaps the key's value in on.
pub const KEY_HOSTS: &[&str] = &["api.anthropic.com"];
/// What the key's placeholder ends in. An interactive Claude Code 2.1.330
/// asks whether to use a key from its environment, with "No" first, unless
/// [`STATE`] approves the key's last 20 characters. So the placeholder ends
/// in the same 20 on every run, and [`files`] approves them.
pub const KEY_SUFFIX: &str = "agentplusplaceholder";
/// The variable that `AUTH token` reads on your host and sets in the guest.
pub const TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The request headers that the proxy may put a hidden secret's value in,
/// from SPEC.md's SECRET section. Claude Code sends its key in `X-Api-Key`,
/// and a token in `Authorization`, as most other clients do.
pub const HEADERS: &[&str] = &["Authorization", "X-Api-Key"];

/// The hosts Claude Code connects to for its model and sign-in. Claude Code
/// 2.1.325 calls the model and makes its API key on `api.anthropic.com`,
/// trades a sign-in code for a token on `platform.claude.com`, and has
/// `claude.ai` as its subscription's origin. Versions before it traded the
/// code on `console.anthropic.com`.
pub const HOSTS: &[&str] = &[
    "api.anthropic.com",
    "claude.ai",
    "console.anthropic.com",
    "platform.claude.com",
];

/// The first version that reads [`EFFORT_VARIABLE`].
const VARIABLE_FROM: [u64; 3] = [2, 0, 46];
/// The first version whose variable takes `max`.
const VARIABLE_MAX_FROM: [u64; 3] = [2, 1, 23];
/// The first version that has `--effort`.
const EFFORT_FROM: [u64; 3] = [2, 1, 38];
/// The first version whose `--effort` takes `xhigh`.
const XHIGH_FROM: [u64; 3] = [2, 1, 111];
/// The first version whose `--permission-mode` takes `auto`.
const AUTO_FROM: [u64; 3] = [2, 1, 71];

/// The files the adapter puts in the configuration layer, by guest path, in
/// path order: `SYSTEM`'s values in [`INSTRUCTIONS`], if there are any, then
/// [`SETTINGS`] and [`STATE`].
///
/// The settings turn self-updates off through Claude Code's own variable, and
/// with `MODE full` they also skip the warning that full mode shows the first
/// time. The state marks the onboarding done, so a new VM skips the theme and
/// welcome screens. With `AUTH key`, it also approves the key by
/// [`KEY_SUFFIX`].
pub fn files(system: &[Vec<u8>], mode: Mode, auth: Option<Auth>) -> Vec<(&'static str, Vec<u8>)> {
    let mut files = Vec::new();
    if !system.is_empty() {
        files.push((INSTRUCTIONS, super::system(system)));
    }
    let mut settings = String::from("{\n  \"env\": {\n    \"DISABLE_AUTOUPDATER\": \"1\"\n  }");
    if mode == Mode::Full {
        settings.push_str(",\n  \"skipDangerousModePermissionPrompt\": true");
    }
    settings.push_str("\n}\n");
    files.push((SETTINGS, settings.into_bytes()));
    let mut state = String::from("{\n");
    if auth == Some(Auth::Key) {
        state.push_str(&format!(
            "  \"customApiKeyResponses\": {{\n    \"approved\": [\"{KEY_SUFFIX}\"],\n    \"rejected\": []\n  }},\n"
        ));
    }
    state.push_str("  \"hasCompletedOnboarding\": true\n}\n");
    files.push((STATE, state.into_bytes()));
    files
}

/// The level this version of Claude Code gets for `effort`, and a warning
/// when that isn't the level asked for. A level the version doesn't accept
/// becomes the nearest lower one it does. A version with no effort setting
/// gets none.
pub fn effort(version_text: &str, effort: Effort) -> (Option<&'static str>, Option<String>) {
    let asked = name(effort);
    let version = version(version_text);
    let at_least = |from: &[u64]| version.as_slice() >= from;
    if !at_least(&VARIABLE_FROM) {
        let warning = format!(
            "Claude Code {version_text} has no effort setting, so `EFFORT {asked}` does nothing"
        );
        return (None, Some(warning));
    }
    let level = match effort {
        Effort::Max if !at_least(&VARIABLE_MAX_FROM) => Effort::High,
        Effort::Xhigh if !at_least(&XHIGH_FROM) => Effort::High,
        effort => effort,
    };
    let given = name(level);
    let warning = (level != effort).then(|| {
        format!("Claude Code {version_text} doesn't accept `EFFORT {asked}`, so it uses `{given}`")
    });
    (Some(given), warning)
}

/// The variables that set the effort of a version older than `--effort`:
/// [`EFFORT_VARIABLE`], with the level [`effort`] gives. Other versions get
/// none.
pub fn environment(version_text: &str, effort: Option<Effort>) -> Vec<(String, String)> {
    if version(version_text).as_slice() >= EFFORT_FROM.as_slice() {
        return Vec::new();
    }
    match effort.map(|effort| self::effort(version_text, effort)) {
        Some((Some(level), _)) => vec![(EFFORT_VARIABLE.to_owned(), level.to_owned())],
        _ => Vec::new(),
    }
}

/// The arguments that set the model, the effort, and the permission mode, in
/// that order, for this version of Claude Code. The effort is here only from
/// the version that has `--effort`, and in [`environment`] before it. `model` is passed unchanged.
/// Without a model or an effort, Claude Code uses its own default. Where
/// `auto` isn't available, Claude Code starts in its manual mode, `default`.
pub fn arguments(
    version_text: &str,
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Mode,
) -> Vec<String> {
    let mut arguments = Vec::new();
    if let Some(model) = model {
        arguments.extend(["--model".to_owned(), model.to_owned()]);
    }
    if version(version_text).as_slice() >= EFFORT_FROM.as_slice()
        && let Some((Some(level), _)) = effort.map(|effort| self::effort(version_text, effort))
    {
        arguments.extend(["--effort".to_owned(), level.to_owned()]);
    }
    let mode = match mode {
        Mode::Read => "plan",
        Mode::Ask => "default",
        Mode::Edit => "acceptEdits",
        Mode::Auto if version(version_text).as_slice() < AUTO_FROM.as_slice() => "default",
        Mode::Auto => "auto",
        Mode::Full => "bypassPermissions",
    };
    arguments.extend(["--permission-mode".to_owned(), mode.to_owned()]);
    arguments
}

/// The arguments that add `directories`, from [`super::workspace`], to the
/// workspace: `--add-dir` before each, which Claude Code has had since 1.0.18.
pub fn workspace(directories: &[&str]) -> Vec<String> {
    directories
        .iter()
        .flat_map(|directory| ["--add-dir".to_owned(), (*directory).to_owned()])
        .collect()
}
