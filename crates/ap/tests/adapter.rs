//! The Claude Code and Codex adapters, against the golden files in
//! `golden/claude/` and `golden/codex/`: the files each renders, the
//! arguments it starts the harness with, and the variables it keeps.
//!
//! `AP_BLESS=1 cargo test --test adapter` writes the golden files from the
//! adapter, for a change that means to change them. Read the diff before you
//! commit it.

use std::fs;
use std::path::Path;

use agentfile::{Auth, Effort, Mode};
use ap::adapter::{claude, codex};

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

/// The current version, and the versions on either side of each change in
/// what Codex accepts.
const CODEX_VERSIONS: [&str; 11] = [
    "0.137.0", "0.138.0", "0.146.1", "0.147.0", "0.155.1", "0.156.0", "0.157.0", "0.159.3",
    "0.160.0", "0.168.0", "0.169.0",
];

/// Fails unless `actual` is what the Claude Code golden file `name` holds.
fn golden(name: &str, actual: &[u8]) {
    golden_in("claude", name, actual);
}

/// Fails unless `actual` is what the golden file `name` in `golden/<dir>/`
/// holds.
fn golden_in(dir: &str, name: &str, actual: &[u8]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(dir)
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

// spec(Run): The Claude Code adapter marks `/` as trusted (`projects["/"].hasTrustDialogAccepted` in `~/.claude.json`), so an interactive run whose working directory isn't in a git repository doesn't ask whether to trust it.
#[test]
fn the_state_trusts_the_root_directory() {
    for mode in MODES {
        let state = String::from_utf8(file(&[], mode, claude::STATE).unwrap()).unwrap();
        assert!(
            state.contains(
                "\"projects\": {\n    \"/\": {\n      \"hasTrustDialogAccepted\": true\n    }\n  }"
            ),
            "{}: {state}",
            mode_name(mode)
        );
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

#[test]
fn codex_older_than_the_oldest_it_configures_is_unsupported() {
    assert_eq!(codex::OLDEST, "0.137.0");
    assert_eq!(
        codex::unsupported("0.136.0").as_deref(),
        Some("Codex 0.136.0 is older than 0.137.0, the oldest version `ap` configures")
    );
    for version in CODEX_VERSIONS {
        assert_eq!(codex::unsupported(version), None, "{version}");
    }
}

/// The file the Codex adapter renders at `path`, for these values.
fn codex_file(system: &[&str], path: &str) -> Option<Vec<u8>> {
    let system: Vec<Vec<u8>> = system
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect();
    codex::files(&system)
        .into_iter()
        .find(|(at, _)| *at == path)
        .map(|(_, content)| content)
}

// spec(SYSTEM): `SYSTEM` sets the harness's global instructions, the equivalent of a global `AGENTS.md`.
// spec(SYSTEM): | `codex` | `~/.codex/AGENTS.md` |
// spec(SYSTEM): The values concatenate in file order, separated by a blank line.
#[test]
fn system_goes_to_codex_agents_md() {
    assert_eq!(codex::INSTRUCTIONS, "/home/agent/.codex/AGENTS.md");
    let values = ["Be careful.", "Never push to main.\n\n", "Ask first.\n"];
    let rendered = codex_file(&values, codex::INSTRUCTIONS).expect("no AGENTS.md");
    golden_in("codex", "AGENTS.md", &rendered);
    assert_eq!(codex_file(&[], codex::INSTRUCTIONS), None);
}

// spec(Run): **Updates.** Harness self-updates are disabled.
// spec(AGENT): which first-run state it prepares, so a new VM doesn't repeat the harness's setup
#[test]
fn the_codex_configuration_turns_updates_off_and_holds_first_run_state() {
    assert_eq!(codex::CONFIGURATION, "/home/agent/.codex/config.toml");
    for system in [&[][..], &["Be careful."][..]] {
        let configuration = codex_file(system, codex::CONFIGURATION).expect("no config.toml");
        golden_in("codex", "config.toml", &configuration);
    }
    let text = String::from_utf8(codex_file(&[], codex::CONFIGURATION).unwrap()).unwrap();
    assert!(
        text.contains("check_for_update_on_startup = false"),
        "{text}"
    );
    assert!(
        text.contains("screen_reader_detection_done = true"),
        "{text}"
    );
}

// spec(MODEL): `<model>` is passed to the harness unchanged, so it accepts whatever names or aliases the harness accepts.
// spec(MODEL): Without `MODEL`, the harness uses its default model.
// spec(EFFORT): Without `EFFORT`, the harness uses its default effort.
#[test]
fn the_codex_model_passes_unchanged() {
    let mut lines = String::new();
    for model in [
        None,
        Some("gpt-5.5-codex"),
        Some("o3"),
        Some("-weird name"),
        Some("model=\"quoted\""),
    ] {
        let arguments = codex::arguments("0.169.0", model, None, Mode::Edit, false);
        lines.push_str(&format!("{model:?}: {arguments:?}\n"));
    }
    golden_in("codex", "model.txt", lines.as_bytes());
}

// spec(EFFORT): The adapter maps the level to the harness's own setting.
// spec(EFFORT): The adapter knows which levels each harness version accepts.
// spec(EFFORT): A level the harness doesn't accept is a build warning, and the adapter uses the nearest lower level, or the harness's lowest level if none is lower.
#[test]
fn each_codex_effort_maps_to_a_level_the_version_accepts() {
    let mut lines = String::new();
    for version in CODEX_VERSIONS {
        for effort in EFFORTS {
            let (level, warning) = codex::effort(version, effort);
            let arguments = codex::arguments(version, None, Some(effort), Mode::Full, false);
            lines.push_str(&format!(
                "{version} {}: {level:?} {arguments:?}{}\n",
                effort_name(effort),
                warning.map_or(String::new(), |warning| format!(", warning: {warning}"))
            ));
        }
    }
    golden_in("codex", "effort.txt", lines.as_bytes());
}

// spec(EFFORT): With Codex, the adapter also sets the effort of plan mode (`plan_mode_reasoning_effort`), so plan mode uses the same level, and a project's settings can't pick another.
#[test]
fn codex_plan_mode_gets_the_effort() {
    for version in CODEX_VERSIONS {
        for interactive in [false, true] {
            for mode in MODES {
                for effort in [None].into_iter().chain(EFFORTS.map(Some)) {
                    let arguments = codex::arguments(version, None, effort, mode, interactive);
                    let efforts: Vec<&str> = (arguments.windows(2))
                        .filter(|pair| pair[0] == "-c" && pair[1].contains("reasoning_effort="))
                        .map(|pair| pair[1].as_str())
                        .collect();
                    let run = format!("{version} {mode:?} {interactive} {effort:?}");
                    let Some(effort) = effort else {
                        assert!(efforts.is_empty(), "{run} {efforts:?}");
                        continue;
                    };
                    let (level, _) = codex::effort(version, effort);
                    assert_eq!(
                        efforts,
                        [
                            format!("model_reasoning_effort=\"{level}\""),
                            format!("plan_mode_reasoning_effort=\"{level}\""),
                        ],
                        "{run}"
                    );
                }
            }
        }
    }
}

// spec(MODE): | `read` | reads and explores, and doesn't edit. In an interactive Claude Code run, approving its plan lets it start editing | `plan` | `read-only` sandbox, approval `never` |
// spec(MODE): | `ask` | reads, and asks before anything else | `default` | `read-only` sandbox, approval `on-request` |
// spec(MODE): | `edit` | also edits files in its workspace. Claude Code asks before commands other than common file operations. Codex runs commands inside its sandbox and asks before leaving it | `acceptEdits` | `workspace-write` sandbox, approval `on-request` |
// spec(MODE): | `auto` | runs without routine prompts. The harness's own reviewer checks actions beyond reads and workspace edits (Claude Code) or beyond the sandbox (Codex) | `auto` | `workspace-write` sandbox, approval `on-request`, reviewer `auto_review` |
// spec(MODE): | `full` | skips permission prompts and the harness's checks, except the few actions the harness never auto-approves. The VM is the boundary | `bypassPermissions` | `--dangerously-bypass-approvals-and-sandbox` |
// spec(MODE): The adapter sets the mode on the harness's command line, with flags or Codex's `-c` overrides, every time it starts the harness, including on `--resume`, so a project's settings or an earlier session can't pick a different one.
// spec(MODE): With `auto`, the adapter turns it on (`sandbox_workspace_write.network_access`), because `NETWORK` already decides what the agent can reach.
// spec(MODE): With `edit`, it stays off, so Codex asks before commands that need the network.
// spec(MODE): In a headless Codex run, `MODE auto` uses `codex exec --approve-for-me`, which selects the same sandbox, approval policy, and reviewer.
#[test]
fn each_codex_mode_is_set_on_the_command_line() {
    let mut lines = String::new();
    for version in CODEX_VERSIONS {
        for interactive in [false, true] {
            for mode in MODES {
                let arguments = codex::arguments(
                    version,
                    Some("gpt-5.5-codex"),
                    Some(Effort::High),
                    mode,
                    interactive,
                );
                let run = if interactive {
                    "interactive"
                } else {
                    "headless"
                };
                lines.push_str(&format!(
                    "{version} {run} {}: {arguments:?}\n",
                    mode_name(mode)
                ));
            }
        }
    }
    golden_in("codex", "mode.txt", lines.as_bytes());
}

// spec(MODE): With `edit` and `auto`, the Codex adapter also empties `sandbox_workspace_write.writable_roots`, so a project's settings can't add directories the sandbox lets the agent write.
#[test]
fn a_codex_project_cant_add_writable_roots() {
    let empty = "sandbox_workspace_write.writable_roots=[]";
    for version in CODEX_VERSIONS {
        for interactive in [false, true] {
            for mode in MODES {
                let arguments = codex::arguments(version, None, None, mode, interactive);
                let emptied = (arguments.windows(2)).any(|pair| pair == ["-c", empty]);
                let writes = matches!(mode, Mode::Edit | Mode::Auto);
                assert_eq!(emptied, writes, "{version} {mode:?} {interactive}");
            }
        }
    }
}

// spec(MODE): With `auto`, the Codex adapter also keeps `features.guardian_approval` on, and blanks the instructions a project can give the reviewer in `[auto_review]`, such as `policy`, so a project can't turn the reviewer off or give it instructions with those keys.
#[test]
fn a_codex_project_cant_change_the_reviewer() {
    let on = "features.guardian_approval=true";
    for version in CODEX_VERSIONS {
        for interactive in [false, true] {
            for mode in MODES {
                let arguments = codex::arguments(version, None, None, mode, interactive);
                let overrides: Vec<&str> = (arguments.windows(2))
                    .filter(|pair| pair[0] == "-c")
                    .map(|pair| pair[1].as_str())
                    .filter(|value| {
                        value.starts_with("auto_review.") || value.starts_with("features.")
                    })
                    .collect();
                let run = format!("{version} {mode:?} {interactive}");
                if mode != Mode::Auto {
                    assert!(overrides.is_empty(), "{run} {overrides:?}");
                    continue;
                }
                assert!(overrides.contains(&on), "{run}");
                assert!(overrides.contains(&"auto_review.policy=\"\""), "{run}");
                for value in overrides {
                    let blank = value.starts_with("auto_review.") && value.ends_with("=\"\"");
                    assert!(blank || value == on, "{run} {value}");
                }
            }
        }
    }
}

// spec(WORKDIR): A headless Codex run can start in a directory that isn't a git repository, such as `/home/agent`, because the adapter passes `codex exec --skip-git-repo-check`.
#[test]
fn a_headless_codex_run_skips_the_git_repository_check() {
    for version in CODEX_VERSIONS {
        for mode in MODES {
            let headless = codex::arguments(version, None, None, mode, false);
            assert_eq!(
                headless[..2],
                ["exec", "--skip-git-repo-check"],
                "{version}"
            );
            let interactive = codex::arguments(version, None, None, mode, true);
            assert!(!interactive.iter().any(|a| a == "--skip-git-repo-check"));
        }
    }
}

// spec(MODE): The default is `auto`.
#[test]
fn a_codex_agentfile_without_mode_starts_in_auto() {
    let instructions = agentfile::parse("AGENT codex\nOS minimal\n").unwrap();
    let settings = agentfile::Settings::read(&instructions).unwrap();
    let arguments = codex::arguments("0.169.0", None, None, settings.mode, false);
    assert_eq!(
        arguments,
        [
            "exec",
            "--skip-git-repo-check",
            "--approve-for-me",
            "-c",
            "sandbox_workspace_write.network_access=true",
            "-c",
            "sandbox_workspace_write.writable_roots=[]",
            "-c",
            "features.guardian_approval=true",
            "-c",
            "auto_review.policy=\"\"",
            "-c",
            "auto_review.experimental_conversation_history_prompt=\"\""
        ]
    );
}

/// The errors the adapter for `agent`'s harness finds in `lines`.
fn reserved_errors(agent: &str, lines: &str) -> Vec<agentfile::Error> {
    let instructions = agentfile::parse(&format!("AGENT {agent}\n{lines}")).unwrap();
    let harness = agentfile::Settings::read(&instructions)
        .unwrap()
        .agent
        .harness;
    agentfile::reserved(&instructions, &ap::adapter::reserved(harness))
}

/// The variables the adapter for `harness` keeps, one per line, each after
/// the reason it is kept: `set` when `ap` or the adapter sets it, `sign-in`
/// when the harness reads it for sign-in, and `key` when `AUTH key` reads it.
fn reserved_variables(harness: agentfile::Harness) -> String {
    let reserved = ap::adapter::reserved(harness);
    let mut lines = String::new();
    for (why, names) in [
        ("set", reserved.set.as_slice()),
        ("sign-in", reserved.sign_in),
        ("key", &[reserved.key]),
    ] {
        for name in names {
            lines.push_str(&format!("{why} {name}\n"));
        }
    }
    lines
}

#[test]
fn each_adapter_keeps_its_variables() {
    golden(
        "reserved.txt",
        reserved_variables(agentfile::Harness::Claude).as_bytes(),
    );
    golden_in(
        "codex",
        "reserved.txt",
        reserved_variables(agentfile::Harness::Codex).as_bytes(),
    );
}

#[test]
fn one_harness_may_use_what_the_other_keeps() {
    let claude = "ENV CODEX_API_KEY x\nSECRET OPENAI_API_KEY\nSECRET MINE @chatgpt.com\n";
    assert!(reserved_errors("claude", claude).is_empty());
    let codex = "ENV CLAUDE_CONFIG_DIR x\nSECRET ANTHROPIC_API_KEY\nSECRET MINE @claude.ai\n";
    assert!(reserved_errors("codex", codex).is_empty());
    assert_eq!(reserved_errors("claude", codex).len(), 3);
    assert_eq!(reserved_errors("codex", claude).len(), 3);
}

// spec(ENV): With [`AUTH key`](#auth), a value that reads the host variable the key comes from is an error too, as in `ENV LEAK $ANTHROPIC_API_KEY` with `AGENT claude`, because it would hand the agent the key that `AUTH key` hides.
#[test]
fn an_env_value_cannot_read_the_key_auth_key_hides() {
    for (agent, key) in [("claude", claude::KEY), ("codex", codex::KEY)] {
        for value in [
            format!("${key}"),
            format!("${{{key}}}"),
            format!("${{{key}:-none}}"),
            format!("\"x-api-key: ${key}\""),
        ] {
            let env = format!("ENV LEAK {value}");
            for (lines, env_line, auth_line) in [
                (format!("AUTH key\n{env}\n"), 4, 3),
                (format!("{env}\nAUTH key\n"), 3, 4),
            ] {
                let source = format!("AGENT {agent}\nOS minimal\n{lines}");
                let errors = agentfile::parse_for(&source, ap::adapter::reserved).unwrap_err();
                let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
                assert_eq!(lines, [env_line], "{source:?}: {errors:?}");
                let message = &errors[0].message;
                for named in [
                    "`ENV LEAK`",
                    &format!("`${key}`"),
                    &format!("line {auth_line}"),
                    "`AUTH key`",
                    "Remove",
                ] {
                    assert!(message.contains(named), "{named}: {message}");
                }
            }
        }
        // Without `AUTH key`, or with `\$`, the value reads no hidden key.
        let mut allowed = vec![
            format!("ENV LEAK ${key}\n"),
            format!("AUTH key\nENV LEAK \\${key}\n"),
        ];
        if agent == "claude" {
            // `AUTH token` works only with Claude Code.
            allowed.push(format!("AUTH token\nENV LEAK ${key}\n"));
        } else {
            // Codex reads `OPENAI_API_KEY` to sign in, but `AUTH key` doesn't.
            allowed.push("AUTH key\nENV LEAK $OPENAI_API_KEY\n".to_string());
        }
        for lines in allowed {
            let source = format!("AGENT {agent}\nOS minimal\n{lines}");
            let parsed = agentfile::parse_for(&source, ap::adapter::reserved);
            assert!(parsed.is_ok(), "{source:?}: {parsed:?}");
        }
    }
}

/// The Agentfile the workspace golden files show, and whether each `SHARE`
/// is a directory on the host, in line order.
const SHARES: &str = "\
SHARE ./app
SHARE ./notes.md ~/notes.md ro
SHARE /var/log/output.log rw
SHARE /var/log/error.log rw
SHARE ./data /srv/my.data
SHARE \"./My Notes\"
SHARE ./app /srv/app
";
const SHARE_DIRECTORIES: [bool; 7] = [true, false, false, false, true, true, true];

/// The Agentfile's shares, for `agent`, as [`ap::adapter::workspace`] takes
/// them, and the directories it gives.
fn shares(agent: &str) -> (String, Vec<String>) {
    let text = format!("AGENT {agent}\n{SHARES}");
    let instructions = agentfile::parse(&text).unwrap();
    let placed = agentfile::GuestPath::read(&instructions, "project").unwrap();
    assert_eq!(placed.len(), SHARE_DIRECTORIES.len());
    let paths: Vec<(&str, bool)> = (placed.iter().zip(SHARE_DIRECTORIES))
        .map(|(placed, directory)| (placed.path.as_str(), directory))
        .collect();
    let directories = ap::adapter::workspace(paths.iter().copied());
    let mut shown = text.clone();
    for (path, directory) in &paths {
        let kind = if *directory { "directory" } else { "file" };
        shown.push_str(&format!("# {kind} {path}\n"));
    }
    (shown, directories.into_iter().map(str::to_owned).collect())
}

// spec(MODE): The harness's workspace is the working directory plus every `SHARE` and `REPO`.
// spec(MODE): The adapter adds those on the harness's command line, with `--add-dir` or a configuration override.
// spec(MODE): For a single file, it adds the parent directory, unless that is the home directory.
#[test]
fn every_share_joins_the_claude_workspace() {
    let (shown, directories) = shares("claude");
    let directories: Vec<&str> = directories.iter().map(String::as_str).collect();
    let arguments = claude::workspace(&directories);
    golden(
        "workspace.txt",
        format!("{shown}{arguments:?}\n").as_bytes(),
    );
    assert!(claude::workspace(&[]).is_empty());
}

// spec(MODE): The harness's workspace is the working directory plus every `SHARE` and `REPO`.
// spec(MODE): The adapter adds those on the harness's command line, with `--add-dir` or a configuration override.
// spec(MODE): For a single file, it adds the parent directory, unless that is the home directory.
// spec(Run): The Codex adapter marks every share as trusted (`trust_level = "trusted"`), so Codex loads a repository's own configuration, hooks, and rules.
#[test]
fn every_share_joins_the_codex_workspace_and_is_trusted() {
    let (shown, directories) = shares("codex");
    let directories: Vec<&str> = directories.iter().map(String::as_str).collect();
    let arguments = codex::workspace(&directories);
    golden_in(
        "codex",
        "workspace.txt",
        format!("{shown}{arguments:?}\n").as_bytes(),
    );
    assert!(codex::workspace(&[]).is_empty());
    let quoted = codex::workspace(&["/srv/a\"b\\c"]);
    assert_eq!(
        quoted[3],
        r#"projects={"/srv/a\"b\\c"={trust_level="trusted"}}"#
    );
}

// spec(MODE): With `read`, `ask`, `edit`, or `auto`, when the working directory is a share, a rule in its `.codex/rules` that allows a command lets Codex run that command outside its sandbox without asking.
#[test]
fn a_codex_share_that_is_the_working_directory_keeps_its_rules() {
    // Real Codex follows the rule in T2. This checks the adapter's part.
    // Codex loads a directory's `.codex/rules` only when it trusts the
    // directory. So in each of these modes, the adapter trusts every share
    // that can be the working directory, and doesn't bypass the sandbox.
    let (_, directories) = shares("codex");
    let directories: Vec<&str> = directories.iter().map(String::as_str).collect();
    for version in CODEX_VERSIONS {
        for interactive in [false, true] {
            for mode in [Mode::Read, Mode::Ask, Mode::Edit, Mode::Auto] {
                let mut arguments = codex::arguments(version, None, None, mode, interactive);
                arguments.extend(codex::workspace(&directories));
                let run = format!("{version} {mode:?} {interactive}");
                let bypass = "--dangerously-bypass-approvals-and-sandbox";
                assert!(!arguments.iter().any(|a| a == bypass), "{run}");
                let overrides: Vec<&str> = (arguments.windows(2))
                    .filter(|pair| pair[0] == "-c")
                    .map(|pair| pair[1].as_str())
                    .collect();
                let projects: Vec<&str> = (overrides.iter().copied())
                    .filter(|value| value.starts_with("projects"))
                    .collect();
                assert_eq!(projects.len(), 1, "{run} {projects:?}");
                for share in &directories {
                    let trusted = format!("\"{share}\"={{trust_level=\"trusted\"}}");
                    assert!(projects[0].contains(&trusted), "{run} {share}");
                }
            }
        }
    }
}

// spec(SECRET): Each adapter's allowlist is `Authorization` and `x-api-key`.
#[test]
fn each_adapter_lets_the_proxy_fill_authorization_and_x_api_key() {
    for harness in [agentfile::Harness::Claude, agentfile::Harness::Codex] {
        let headers: Vec<String> = (ap::adapter::headers(harness).iter())
            .map(|header| header.to_ascii_lowercase())
            .collect();
        assert_eq!(headers, ["authorization", "x-api-key"], "{harness:?}");
    }
}

/// How each `AUTH` method signs `harness` in, one line each.
fn sign_ins(harness: agentfile::Harness) -> String {
    let mut lines = String::new();
    for (method, auth) in [
        ("login", Auth::Login),
        ("key", Auth::Key),
        ("token", Auth::Token),
    ] {
        let shown = match ap::adapter::sign_in(harness, auth) {
            None => "none".to_owned(),
            Some(sign_in) if sign_in.hosts.is_empty() => format!("{} visible", sign_in.variable),
            Some(sign_in) => {
                let mut shown = format!("{} hidden", sign_in.variable);
                for host in sign_in.hosts {
                    shown.push_str(&format!(" @{host}"));
                }
                shown
            }
        };
        lines.push_str(&format!("AUTH {method}: {shown}\n"));
    }
    lines
}

// spec(AUTH): On your host, `ap` reads `ANTHROPIC_API_KEY` for Claude Code and `CODEX_API_KEY` for Codex, the way it reads any [secret](#secret).
#[test]
fn each_adapter_signs_in_with_a_key_and_claude_code_with_a_token() {
    golden("auth.txt", sign_ins(agentfile::Harness::Claude).as_bytes());
    golden_in(
        "codex",
        "auth.txt",
        sign_ins(agentfile::Harness::Codex).as_bytes(),
    );
    // A key's hosts are the harness's own, which a `SECRET` can't bind.
    for harness in [agentfile::Harness::Claude, agentfile::Harness::Codex] {
        let sign_in = ap::adapter::sign_in(harness, Auth::Key).unwrap();
        let own = ap::adapter::reserved(harness);
        assert!(sign_in.hosts.iter().all(|host| own.hosts.contains(host)));
        assert!(own.sign_in.contains(&sign_in.variable));
    }
    assert!(claude::SIGN_IN.contains(&claude::TOKEN));
}

// spec(AUTH): The adapter never turns off a sign-in method the harness offers.
#[test]
fn no_adapter_output_limits_how_the_harness_signs_in() {
    // The settings that limit sign-in: Claude Code's `forceLoginMethod` and
    // `forceLoginOrgUUID`, and Codex's `forced_login_method` and
    // `forced_chatgpt_workspace_id`.
    const LIMITS: [&str; 4] = [
        "forceLoginMethod",
        "forceLoginOrgUUID",
        "forced_login_method",
        "forced_chatgpt_workspace_id",
    ];
    let system = [b"Be careful.".to_vec()];
    let placeholder = b"0123456789abcdef0123456789abcdef";
    let mut outputs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |what: String, bytes: Vec<u8>| outputs.push((what, bytes));
    for mode in MODES {
        for (path, bytes) in claude::files(&system, mode) {
            add(format!("claude {mode:?} {path}"), bytes);
        }
        for version in VERSIONS {
            for effort in [None].into_iter().chain(EFFORTS.map(Some)) {
                let what = format!("claude {version} {mode:?} {effort:?}");
                let arguments = claude::arguments(version, Some("opus"), effort, mode);
                add(what.clone(), arguments.join(" ").into_bytes());
                let environment = claude::environment(version, effort);
                add(what, format!("{environment:?}").into_bytes());
            }
        }
        for version in CODEX_VERSIONS {
            for interactive in [false, true] {
                for effort in [None].into_iter().chain(EFFORTS.map(Some)) {
                    let arguments =
                        codex::arguments(version, Some("gpt"), effort, mode, interactive);
                    let what = format!("codex {version} {mode:?} {interactive} {effort:?}");
                    add(what, arguments.join(" ").into_bytes());
                }
            }
        }
    }
    for (path, bytes) in codex::files(&system) {
        add(format!("codex {path}"), bytes);
    }
    for harness in [agentfile::Harness::Claude, agentfile::Harness::Codex] {
        for given in [None, Some(&placeholder[..])] {
            for (path, bytes) in ap::adapter::run_files(harness, "/home/agent", given) {
                add(format!("{harness:?} run {path}"), bytes);
            }
        }
    }
    assert!(outputs.len() > 100, "{}", outputs.len());
    for (what, bytes) in &outputs {
        let text = String::from_utf8_lossy(bytes);
        for limit in LIMITS {
            assert!(!text.contains(limit), "{what} sets {limit}: {text}");
        }
    }
}

// spec(ENV): A name the harness reads only to reach a model through another provider, such as Claude Code's `ANTHROPIC_FOUNDRY_API_KEY`, `AWS_BEARER_TOKEN_BEDROCK`, or `CLAUDE_CODE_USE_VERTEX`, isn't one it reads for sign-in, because `AUTH key` and `AUTH token` hold a credential only for the harness's own provider.
// spec(ENV): Claude Code's `ANTHROPIC_CUSTOM_HEADERS` is one, because an `Authorization` or `x-api-key` header in it signs Claude Code in.
// spec(SECRET): A name the harness reads only to reach a model through another provider can be a `SECRET`, as it can be an [`ENV`](#env) key, and Claude Code's `ANTHROPIC_CUSTOM_HEADERS` can't.
#[test]
fn another_providers_names_are_not_claude_codes_sign_in() {
    // Claude Code 2.1.330 reads each of these to reach Foundry, Bedrock, or
    // Vertex.
    for name in [
        "ANTHROPIC_FOUNDRY_API_KEY",
        "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
        "ANTHROPIC_FOUNDRY_RESOURCE",
        "AWS_BEARER_TOKEN_BEDROCK",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_VERTEX",
        "ANTHROPIC_VERTEX_PROJECT_ID",
    ] {
        for line in [
            format!("ENV {name} x"),
            format!("SECRET {name} @example.com"),
            format!("SECRET {name} visible"),
        ] {
            let source = format!("AGENT claude\nOS minimal\n{line}\n");
            let parsed = agentfile::parse_for(&source, ap::adapter::reserved);
            assert!(parsed.is_ok(), "{line}: {parsed:?}");
        }
    }
    for line in [
        "ENV ANTHROPIC_CUSTOM_HEADERS x",
        "SECRET ANTHROPIC_CUSTOM_HEADERS @example.com",
        "SECRET ANTHROPIC_CUSTOM_HEADERS visible",
    ] {
        let source = format!("AGENT claude\nOS minimal\n{line}\n");
        let errors = agentfile::parse_for(&source, ap::adapter::reserved).unwrap_err();
        assert_eq!(errors.len(), 1, "{line}: {errors:?}");
        assert_eq!(errors[0].line, 3, "{line}");
        let message = &errors[0].message;
        assert!(message.contains("`ANTHROPIC_CUSTOM_HEADERS`"), "{message}");
        assert!(message.contains("`AUTH`"), "{message}");
        // Codex doesn't read it, so it's an ordinary name there.
        let codex = source.replace("AGENT claude", "AGENT codex");
        let parsed = agentfile::parse_for(&codex, ap::adapter::reserved);
        assert!(parsed.is_ok(), "codex {line}: {parsed:?}");
    }
}

// Claude Code approves a key from its environment by the key's last 20
// characters, in `customApiKeyResponses.approved`.
#[test]
fn a_keyed_runs_state_approves_its_placeholder_alone() {
    let placeholder = b"0123456789abcdef0123456789abcdef";
    let files = ap::adapter::run_files(agentfile::Harness::Claude, "/", Some(placeholder));
    let paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
    assert_eq!(paths, [claude::STATE]);
    golden("claude-key.json", &files[0].1);
    assert!(ap::adapter::run_files(agentfile::Harness::Codex, "/", Some(placeholder)).is_empty());
}

// spec(Run): Each run also marks its working directory and every directory above it as trusted, so an interactive run whose working directory is in a git repository doesn't ask whether to trust it.
#[test]
fn a_runs_state_trusts_its_working_directory_and_every_directory_above_it() {
    let files = ap::adapter::run_files(agentfile::Harness::Claude, "/home/agent/project", None);
    let paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
    assert_eq!(paths, [claude::STATE]);
    golden("claude-run.json", &files[0].1);
    // The path is made normal, and written as a JSON string.
    let state = claude::state("/home//agent/./x/../a \"b\" \\c\u{1}/", None);
    let state = String::from_utf8(state).unwrap();
    let parsed = ap::json::parse(&state).expect(&state);
    let Some(ap::json::Value::Object(projects)) = parsed.get("projects") else {
        panic!("{state}");
    };
    let trusted: Vec<&str> = projects.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        trusted,
        ["/", "/home", "/home/agent", "/home/agent/a \"b\" \\c\u{1}"],
        "{state}"
    );
    assert!(ap::adapter::run_files(agentfile::Harness::Codex, "/home/agent", None).is_empty());
}

/// How the proxy decides an HTTPS request for `url`, such as
/// `https://api.anthropic.com/v1/messages`, under the worked example's rules
/// in SPEC.md's NETWORK section, with `harness`'s hosts as its adapter lists
/// them.
fn worked_example(harness: agentfile::Harness, url: &str) -> String {
    let agent = match harness {
        agentfile::Harness::Claude => "claude",
        agentfile::Harness::Codex => "codex",
    };
    let instructions = agentfile::parse(&format!(
        "AGENT {agent}\nNETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\n\
         NETWORK +https://codeberg.org/forgejo/forgejo\n"
    ))
    .unwrap();
    let network = agentfile::Network::read(&instructions).unwrap();
    let rest = url.strip_prefix("https://").unwrap();
    let (name, target) = rest.split_at(rest.find('/').unwrap());
    let connection = net::Connection {
        name: Some(name),
        address: "203.0.113.9".parse().unwrap(),
        port: 443,
        request: Some(net::Request {
            scheme: agentfile::Scheme::Https,
            target,
        }),
    };
    let hosts = ap::adapter::reserved(harness).hosts;
    match net::proxied(&network, hosts, &connection) {
        net::Proxied::Harness => "allow by harness host".to_owned(),
        net::Proxied::Decided(decision) => match (decision.sign, decision.by) {
            (agentfile::Sign::Deny, net::By::Default) => "deny by default".to_owned(),
            other => format!("{other:?}"),
        },
        net::Proxied::Loopback => "loopback".to_owned(),
    }
}

// spec(NETWORK): | `https://api.anthropic.com/v1/messages` | allow | harness host |
// spec(NETWORK): | `https://api.openai.com/v1/responses` | deny | default `-` (a harness host only for `codex`) |
#[test]
fn the_worked_example_s_harness_rows_decide_as_its_table_says() {
    use agentfile::Harness::{Claude, Codex};
    let anthropic = "https://api.anthropic.com/v1/messages";
    let openai = "https://api.openai.com/v1/responses";
    assert_eq!(worked_example(Claude, anthropic), "allow by harness host");
    assert_eq!(worked_example(Claude, openai), "deny by default");
    // Under `AGENT codex`, the hosts trade places.
    assert_eq!(worked_example(Codex, openai), "allow by harness host");
    assert_eq!(worked_example(Codex, anthropic), "deny by default");
}
