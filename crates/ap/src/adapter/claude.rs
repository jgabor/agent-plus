//! The Claude Code adapter.
//!
//! Its files are the global instructions from `SYSTEM`, the user settings
//! that turn self-updates off, and the state that tells Claude Code its
//! onboarding is done. Its command line sets the model, the effort, and the
//! permission mode every time it starts the harness, so neither a project's
//! settings nor an earlier session can pick others.
//!
//! What each version accepts was read from the published releases: `--effort`
//! takes `low`, `medium`, `high`, and `max` from 2.1.38, and `xhigh` too from
//! 2.1.111, and `--permission-mode` takes `auto` from 2.1.91.

use agentfile::{Effort, Mode};

use super::version;

/// Where `SYSTEM` goes.
pub const INSTRUCTIONS: &str = "/home/agent/.claude/CLAUDE.md";
/// The user settings.
pub const SETTINGS: &str = "/home/agent/.claude/settings.json";
/// The state Claude Code keeps for itself, which says whether its onboarding
/// is done.
pub const STATE: &str = "/home/agent/.claude.json";

/// The first version that has `--effort`.
const EFFORT_FROM: [u64; 3] = [2, 1, 38];
/// The first version whose `--effort` takes `xhigh`.
const XHIGH_FROM: [u64; 3] = [2, 1, 111];
/// The first version whose `--permission-mode` takes `auto`.
const AUTO_FROM: [u64; 3] = [2, 1, 91];

/// The files the adapter puts in the configuration layer, by guest path, in
/// path order: `SYSTEM`'s values in [`INSTRUCTIONS`], if there are any, then
/// [`SETTINGS`] and [`STATE`].
///
/// The settings turn self-updates off through Claude Code's own variable, and
/// with `MODE full` they also skip the warning that full mode shows the first
/// time. The state marks the onboarding done, so a new VM skips the theme and
/// welcome screens.
pub fn files(system: &[Vec<u8>], mode: Mode) -> Vec<(&'static str, Vec<u8>)> {
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
    files.push((STATE, b"{\n  \"hasCompletedOnboarding\": true\n}\n".to_vec()));
    files
}

/// The level `--effort` gets for `effort` in this version of Claude Code, and
/// a warning when that isn't the level asked for. A level the version doesn't
/// accept becomes the nearest lower one it does. A version without `--effort`
/// gets none.
pub fn effort(version_text: &str, effort: Effort) -> (Option<&'static str>, Option<String>) {
    let asked = name(effort);
    let version = version(version_text);
    if version.as_slice() < EFFORT_FROM.as_slice() {
        let warning = format!(
            "Claude Code {version_text} has no effort setting, so `EFFORT {asked}` does nothing"
        );
        return (None, Some(warning));
    }
    let level = match effort {
        Effort::Xhigh if version.as_slice() < XHIGH_FROM.as_slice() => Effort::High,
        effort => effort,
    };
    let given = name(level);
    let warning = (level != effort).then(|| {
        format!(
            "Claude Code {version_text} doesn't accept `EFFORT {asked}`, so it uses `{given}`"
        )
    });
    (Some(given), warning)
}

/// The arguments that set the model, the effort, and the permission mode, in
/// that order, for this version of Claude Code. `model` is passed unchanged.
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
    if let Some((Some(level), _)) = effort.map(|effort| self::effort(version_text, effort)) {
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

/// The level as `EFFORT` and `--effort` write it.
fn name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}
