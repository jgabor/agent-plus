//! The Claude Code adapter, against the golden files in `golden/claude/`: the
//! files it renders, and the arguments it starts the harness with.
//!
//! `AP_BLESS=1 cargo test --test adapter` writes the golden files from the
//! adapter, for a change that means to change them. Read the diff before you
//! commit it.

use std::fs;
use std::path::Path;

use agentfile::{Effort, Mode};
use ap::adapter::claude;

const EFFORTS: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

const MODES: [Mode; 5] = [Mode::Read, Mode::Ask, Mode::Edit, Mode::Auto, Mode::Full];

/// The current version, and the versions on either side of each change in
/// what Claude Code accepts.
const VERSIONS: [&str; 11] = [
    "2.0.45", "2.0.46", "2.1.22", "2.1.23", "2.1.37", "2.1.38", "2.1.70", "2.1.71", "2.1.110",
    "2.1.111", "2.1.323",
];

/// Fails unless `actual` is what the golden file `name` holds.
fn golden(name: &str, actual: &[u8]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/claude")
        .join(name);
    if std::env::var_os("AP_BLESS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
    }
    let expected = fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert_eq!(
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(&expected),
        "{name} differs from its golden file"
    );
}

/// The file the adapter renders at `path`, for these values and mode.
fn file(system: &[&str], mode: Mode, path: &str) -> Option<Vec<u8>> {
    let system: Vec<Vec<u8>> = system
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect();
    claude::files(&system, mode)
        .into_iter()
        .find(|(at, _)| *at == path)
        .map(|(_, content)| content)
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Read => "read",
        Mode::Ask => "ask",
        Mode::Edit => "edit",
        Mode::Auto => "auto",
        Mode::Full => "full",
    }
}

fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

// spec(SYSTEM): `SYSTEM` sets the harness's global instructions, the equivalent of a global `AGENTS.md`.
// spec(SYSTEM): The adapter writes it to the harness's own location.
// spec(SYSTEM): | `claude` | `~/.claude/CLAUDE.md` |
// spec(SYSTEM): The values concatenate in file order, separated by a blank line.
#[test]
fn system_goes_to_claude_md() {
    assert_eq!(claude::INSTRUCTIONS, "/home/agent/.claude/CLAUDE.md");
    let values = ["Be careful.", "Never push to main.\n\n", "Ask first.\n"];
    let rendered = file(&values, Mode::Auto, claude::INSTRUCTIONS).expect("no CLAUDE.md");
    golden("CLAUDE.md", &rendered);
    assert_eq!(file(&[], Mode::Auto, claude::INSTRUCTIONS), None);
}

// spec(Run): **Updates.** Harness self-updates are disabled.
#[test]
fn the_settings_turn_self_updates_off() {
    for mode in MODES {
        let settings = file(&[], mode, claude::SETTINGS).expect("no settings");
        let text = String::from_utf8(settings.clone()).unwrap();
        assert!(
            text.contains("\"DISABLE_AUTOUPDATER\": \"1\""),
            "{}: {text}",
            mode_name(mode)
        );
        if mode != Mode::Full {
            golden("settings.json", &settings);
        }
    }
}

// spec(MODE): With `full` on Claude Code, the adapter also sets the user-level key that skips the first-run warning (`skipDangerousModePermissionPrompt`).
#[test]
fn full_mode_skips_its_first_run_warning() {
    assert_eq!(claude::SETTINGS, "/home/agent/.claude/settings.json");
    let settings = file(&[], Mode::Full, claude::SETTINGS).expect("no settings");
    golden("settings-full.json", &settings);
    for mode in MODES.into_iter().filter(|mode| *mode != Mode::Full) {
        let settings = file(&[], mode, claude::SETTINGS).unwrap();
        assert!(
            !String::from_utf8(settings)
                .unwrap()
                .contains("skipDangerous"),
            "{}",
            mode_name(mode)
        );
    }
}

// spec(AGENT): which first-run state it prepares, so a new VM doesn't repeat the harness's setup
#[test]
fn the_state_marks_onboarding_done() {
    assert_eq!(claude::STATE, "/home/agent/.claude.json");
    for mode in MODES {
        let state = file(&["Be careful."], mode, claude::STATE).expect("no state");
        golden("claude.json", &state);
    }
}

// spec(MODEL): `<model>` is passed to the harness unchanged, so it accepts whatever names or aliases the harness accepts.
// spec(MODEL): Without `MODEL`, the harness uses its default model.
// spec(EFFORT): Without `EFFORT`, the harness uses its default effort.
#[test]
fn the_model_passes_unchanged() {
    let mut lines = String::new();
    for model in [
        None,
        Some("opus"),
        Some("claude-opus-5-5"),
        Some("claude-opus-5-5[1m]"),
        Some("-weird name"),
    ] {
        let arguments = claude::arguments("2.1.323", model, None, Mode::Auto);
        lines.push_str(&format!("{model:?}: {arguments:?}\n"));
    }
    golden("model.txt", lines.as_bytes());
}

// spec(EFFORT): The adapter maps the level to the harness's own setting.
// spec(EFFORT): The adapter knows which levels each harness version accepts.
// spec(EFFORT): A level the harness doesn't accept is a build warning, and the adapter uses the nearest lower level, or the harness's lowest level if none is lower.
#[test]
fn each_effort_maps_to_a_level_the_version_accepts() {
    let mut lines = String::new();
    for version in VERSIONS {
        for effort in EFFORTS {
            let (level, warning) = claude::effort(version, effort);
            let arguments = claude::arguments(version, None, Some(effort), Mode::Auto);
            let environment = claude::environment(version, Some(effort));
            lines.push_str(&format!(
                "{version} {}: {level:?} {arguments:?} {environment:?}{}\n",
                effort_name(effort),
                warning.map_or(String::new(), |warning| format!(", warning: {warning}"))
            ));
        }
    }
    golden("effort.txt", lines.as_bytes());
}

// spec(MODE): | `read` | reads and explores, and doesn't edit. In an interactive Claude Code run, approving its plan lets it start editing | `plan` | `read-only` sandbox, approval `never` |
// spec(MODE): | `ask` | reads, and asks before anything else | `default` | `read-only` sandbox, approval `on-request` |
// spec(MODE): | `edit` | also edits files in its workspace. Claude Code asks before commands other than common file operations. Codex runs commands inside its sandbox and asks before leaving it | `acceptEdits` | `workspace-write` sandbox, approval `on-request` |
// spec(MODE): | `auto` | runs without routine prompts. The harness's own reviewer checks actions beyond reads and workspace edits (Claude Code) or beyond the sandbox (Codex) | `auto` | `workspace-write` sandbox, approval `on-request`, reviewer `auto_review` |
// spec(MODE): | `full` | skips permission prompts and the harness's checks, except the few actions the harness never auto-approves. The VM is the boundary | `bypassPermissions` | `--dangerously-bypass-approvals-and-sandbox` |
// spec(MODE): The adapter sets the mode on the harness's command line, with flags or Codex's `-c` overrides, every time it starts the harness, including on `--resume`, so a project's settings or an earlier session can't pick a different one.
#[test]
fn each_mode_is_set_on_the_command_line() {
    let mut lines = String::new();
    for version in VERSIONS {
        for mode in MODES {
            let arguments = claude::arguments(version, Some("opus"), Some(Effort::High), mode);
            lines.push_str(&format!("{version} {}: {arguments:?}\n", mode_name(mode)));
        }
    }
    golden("mode.txt", lines.as_bytes());
}

// spec(MODE): The default is `auto`.
#[test]
fn an_agentfile_without_mode_starts_in_auto() {
    let instructions = agentfile::parse("AGENT claude\nOS minimal\n").unwrap();
    let settings = agentfile::Settings::read(&instructions).unwrap();
    let arguments = claude::arguments("2.1.323", None, None, settings.mode);
    assert_eq!(arguments, ["--permission-mode", "auto"]);
}
