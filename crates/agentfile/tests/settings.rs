//! The arguments of `AGENT`, `OS`, `MODEL`, `EFFORT`, `MODE`, and
//! `COMPRESSION`, and the typed settings they parse into.

use agentfile::{Agent, Compression, Effort, Error, Harness, Mode, Os, Settings, parse};

/// Parses a whole Agentfile, and returns its settings.
fn settings(source: &str) -> Settings {
    let instructions = parse(source).unwrap();
    Settings::read(&instructions).unwrap()
}

/// Parses `lines` after an `AGENT` line, and returns its settings.
fn settings_after_agent(lines: &str) -> Settings {
    settings(&format!("AGENT claude\n{lines}"))
}

/// Parses a whole Agentfile, and returns its errors.
fn errors(source: &str) -> Vec<Error> {
    parse(source).unwrap_err()
}

/// Parses `line` after an `AGENT` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = errors(&format!("AGENT claude\n{line}"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.line, 2, "{error:?}");
    error
}

fn lines_of(errors: &[Error]) -> Vec<usize> {
    errors.iter().map(|error| error.line).collect()
}

fn agent(harness: Harness, version: &str) -> Agent {
    Agent {
        harness,
        version: version.to_string(),
    }
}

#[test]
fn agent_claude_is_the_claude_code_harness() {
    assert_eq!(settings("AGENT claude\n").agent.harness, Harness::Claude);
}

#[test]
fn agent_codex_is_the_codex_harness() {
    assert_eq!(settings("AGENT codex\n").agent.harness, Harness::Codex);
}

#[test]
fn any_other_harness_is_an_error() {
    for source in [
        "AGENT gemini\n",
        "AGENT Claude\n",
        "AGENT claude-desktop\n",
        "AGENT \"\"\n",
        "AGENT :latest\n",
    ] {
        let errors = errors(source);
        assert_eq!(lines_of(&errors), [1], "{source:?}: {errors:?}");
        let message = &errors[0].message;
        assert!(message.contains("`claude`"), "{message}");
        assert!(message.contains("`codex`"), "{message}");
    }
}

#[test]
fn an_unknown_harness_is_named_in_its_error() {
    let errors = errors("AGENT gemini:1.0\n");
    assert!(errors[0].message.contains("`gemini`"), "{errors:?}");
}

#[test]
fn the_version_follows_a_colon_and_is_kept_as_written() {
    for (source, expected) in [
        ("AGENT claude:2.1.319\n", agent(Harness::Claude, "2.1.319")),
        ("AGENT claude:stable\n", agent(Harness::Claude, "stable")),
        ("AGENT codex:latest\n", agent(Harness::Codex, "latest")),
        (
            "AGENT codex:0.48.0-alpha.2\n",
            agent(Harness::Codex, "0.48.0-alpha.2"),
        ),
    ] {
        assert_eq!(settings(source).agent, expected, "{source:?}");
    }
}

// spec(AGENT): Without a version, `ap` uses `latest`.
#[test]
fn without_a_version_the_agent_uses_latest() {
    assert_eq!(
        settings("AGENT codex\n").agent,
        agent(Harness::Codex, "latest")
    );
}

#[test]
fn an_empty_version_is_an_error() {
    let errors = errors("AGENT claude:\n");
    assert_eq!(lines_of(&errors), [1], "{errors:?}");
    assert!(errors[0].message.contains("version"), "{errors:?}");
}

#[test]
fn agent_takes_exactly_one_argument() {
    for source in ["AGENT\n", "AGENT claude 2.1\n", "AGENT claude latest\n"] {
        let errors = errors(source);
        assert_eq!(lines_of(&errors), [1], "{source:?}: {errors:?}");
        assert!(errors[0].message.contains("AGENT"), "{errors:?}");
    }
}

// spec(AGENT): It must be the first instruction, and it appears exactly once.
#[test]
fn agent_is_first_and_appears_once() {
    assert_eq!(
        settings("# note\n\nAGENT codex\nOS minimal\n")
            .agent
            .harness,
        Harness::Codex
    );
    assert_eq!(lines_of(&errors("OS minimal\nAGENT claude\n")), [2]);
    assert_eq!(lines_of(&errors("AGENT claude\nAGENT codex\n")), [2]);
    assert_eq!(lines_of(&errors("OS minimal\n")), [1]);
}

// spec(OS): `OS` chooses one of three curated operating system images.
#[test]
fn os_chooses_one_of_three_tiers() {
    for (word, os) in [
        ("minimal", Os::Minimal),
        ("standard", Os::Standard),
        ("desktop", Os::Desktop),
    ] {
        assert_eq!(
            settings_after_agent(&format!("OS {word}\n")).os,
            os,
            "{word}"
        );
    }
    for word in ["server", "Minimal", "debian", "\"\""] {
        let message = error_of(&format!("OS {word}\n")).message;
        assert!(message.contains("`minimal`"), "{message}");
        assert!(message.contains("`standard`"), "{message}");
        assert!(message.contains("`desktop`"), "{message}");
    }
}

// spec(OS): The default is `desktop`.
#[test]
fn the_default_os_is_desktop() {
    assert_eq!(settings("AGENT claude\n").os, Os::Desktop);
}

#[test]
fn os_takes_exactly_one_argument() {
    assert_eq!(error_of("OS\n").message, "`OS` needs an argument");
    assert_eq!(
        error_of("OS minimal standard desktop\n").message,
        "`OS` takes one argument, but found 3"
    );
}

#[test]
fn model_is_kept_unchanged() {
    for (line, model) in [
        ("MODEL opus\n", "opus"),
        ("MODEL claude-opus-5-5[1m]\n", "claude-opus-5-5[1m]"),
        ("MODEL gpt-6.1-sol\n", "gpt-6.1-sol"),
        ("MODEL \"My Model\"\n", "My Model"),
        ("MODEL OPUS\n", "OPUS"),
    ] {
        let settings = settings_after_agent(line);
        assert_eq!(settings.model.as_deref(), Some(model), "{line:?}");
    }
}

#[test]
fn without_model_no_model_is_set() {
    assert_eq!(settings("AGENT claude\n").model, None);
}

#[test]
fn model_takes_exactly_one_nonempty_argument() {
    for line in ["MODEL\n", "MODEL opus sonnet\n", "MODEL \"\"\n"] {
        assert!(error_of(line).message.contains("MODEL"), "{line:?}");
    }
}

#[test]
fn effort_takes_one_of_five_levels() {
    for (word, effort) in [
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("xhigh", Effort::Xhigh),
        ("max", Effort::Max),
    ] {
        let settings = settings_after_agent(&format!("EFFORT {word}\n"));
        assert_eq!(settings.effort, Some(effort), "{word}");
    }
    for word in ["none", "minimal", "High", "x-high", "3"] {
        let message = error_of(&format!("EFFORT {word}\n")).message;
        for level in ["`low`", "`medium`", "`high`", "`xhigh`", "`max`"] {
            assert!(message.contains(level), "{message}");
        }
    }
}

#[test]
fn without_effort_no_effort_is_set() {
    assert_eq!(settings("AGENT claude\n").effort, None);
}

#[test]
fn effort_takes_exactly_one_argument() {
    for line in ["EFFORT\n", "EFFORT high max\n"] {
        assert!(error_of(line).message.contains("EFFORT"), "{line:?}");
    }
}

// spec(EFFORT): `EFFORT` works with or without `MODEL`.
#[test]
fn effort_works_with_or_without_model() {
    let alone = settings_after_agent("EFFORT high\n");
    assert_eq!((alone.model, alone.effort), (None, Some(Effort::High)));
    let both = settings_after_agent("EFFORT low\nMODEL opus\n");
    assert_eq!(
        (both.model.as_deref(), both.effort),
        (Some("opus"), Some(Effort::Low))
    );
}

#[test]
fn every_level_parses_with_any_model() {
    for model in ["haiku", "gpt-6.1-sol", "no-such-model"] {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            let source = format!("AGENT codex\nMODEL {model}\nEFFORT {level}\n");
            assert!(parse(&source).is_ok(), "{source:?}");
        }
    }
}

#[test]
fn mode_takes_one_of_five_words() {
    for (word, mode) in [
        ("read", Mode::Read),
        ("ask", Mode::Ask),
        ("edit", Mode::Edit),
        ("auto", Mode::Auto),
        ("full", Mode::Full),
    ] {
        assert_eq!(
            settings_after_agent(&format!("MODE {word}\n")).mode,
            mode,
            "{word}"
        );
    }
    for word in ["plan", "bypassPermissions", "Full", "yolo"] {
        let message = error_of(&format!("MODE {word}\n")).message;
        for mode in ["`read`", "`ask`", "`edit`", "`auto`", "`full`"] {
            assert!(message.contains(mode), "{message}");
        }
    }
}

// spec(MODE): The default is `auto`.
#[test]
fn the_default_mode_is_auto() {
    assert_eq!(settings("AGENT claude\n").mode, Mode::Auto);
}

#[test]
fn mode_takes_exactly_one_argument() {
    for line in ["MODE\n", "MODE edit full\n"] {
        assert!(error_of(line).message.contains("MODE"), "{line:?}");
    }
}

// spec(COMPRESSION): The default is `3`.
#[test]
fn the_default_compression_is_3() {
    assert_eq!(settings("AGENT claude\n").compression.level(), 3);
    assert_eq!(Compression::default().level(), 3);
}

#[test]
fn compression_levels_1_to_22_are_accepted() {
    for level in 1..=22 {
        let settings = settings_after_agent(&format!("COMPRESSION {level}\n"));
        assert_eq!(settings.compression.level(), level);
    }
    for word in ["0", "23", "100", "256"] {
        error_of(&format!("COMPRESSION {word}\n"));
    }
}

#[test]
fn compression_levels_minus_1_to_minus_7_are_accepted() {
    for level in -7..=-1 {
        let settings = settings_after_agent(&format!("COMPRESSION {level}\n"));
        assert_eq!(settings.compression.level(), level);
    }
    for word in ["-0", "-8", "-22", "-129"] {
        error_of(&format!("COMPRESSION {word}\n"));
    }
}

#[test]
fn compression_is_a_decimal_integer() {
    for (word, level) in [("+3", 3), ("03", 3), ("-03", -3)] {
        let settings = settings_after_agent(&format!("COMPRESSION {word}\n"));
        assert_eq!(settings.compression.level(), level, "{word}");
    }
    let huge = "99999999999999999999";
    for word in ["fast", "3.0", "1e1", "three", "\" 3\"", "\"\"", "-", huge] {
        let message = error_of(&format!("COMPRESSION {word}\n")).message;
        assert!(message.contains("`1` to `22`"), "{word}: {message}");
        assert!(message.contains("`-1` to `-7`"), "{word}: {message}");
    }
}

#[test]
fn compression_takes_exactly_one_argument() {
    for line in ["COMPRESSION\n", "COMPRESSION 3 19\n"] {
        assert!(error_of(line).message.contains("COMPRESSION"), "{line:?}");
    }
}

#[test]
fn values_may_be_quoted() {
    let settings =
        settings("AGENT \"codex:latest\"\nOS \"minimal\"\nMODE \"read\"\nCOMPRESSION \"-3\"\n");
    assert_eq!(settings.agent, agent(Harness::Codex, "latest"));
    assert_eq!(settings.os, Os::Minimal);
    assert_eq!(settings.mode, Mode::Read);
    assert_eq!(settings.compression.level(), -3);
}

#[test]
fn instruction_names_may_be_any_case() {
    let settings = settings("agent codex\nos standard\nEffort max\nmode full\n");
    assert_eq!(settings.agent.harness, Harness::Codex);
    assert_eq!(settings.os, Os::Standard);
    assert_eq!(settings.effort, Some(Effort::Max));
    assert_eq!(settings.mode, Mode::Full);
}

#[test]
fn every_setting_is_read_together() {
    let source = "AGENT claude:2.1.319\nOS minimal\nMODEL opus\nEFFORT xhigh\nMODE edit\nCOMPRESSION 19\nSHARE ./out rw\n";
    let settings = settings(source);
    assert_eq!(settings.agent, agent(Harness::Claude, "2.1.319"));
    assert_eq!(settings.os, Os::Minimal);
    assert_eq!(settings.model.as_deref(), Some("opus"));
    assert_eq!(settings.effort, Some(Effort::Xhigh));
    assert_eq!(settings.mode, Mode::Edit);
    assert_eq!(settings.compression.level(), 19);
}

#[test]
fn one_bad_setting_does_not_hide_the_next() {
    let source = "AGENT claude:\nOS huge\nMODEL\nEFFORT none\nMODE yolo\nCOMPRESSION 0\nBOGUS\n";
    let errors = errors(source);
    assert_eq!(lines_of(&errors), [1, 2, 3, 4, 5, 6, 7], "{errors:?}");
}

#[test]
fn an_error_names_its_line_and_the_value() {
    let errors = errors("AGENT claude\n\n# tier\nOS huge\n");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].to_string().starts_with("line 4: "),
        "{}",
        errors[0]
    );
    assert!(errors[0].message.contains("`huge`"), "{}", errors[0]);
}

#[test]
fn reading_settings_reports_each_bad_instruction_with_its_line() {
    let instructions = parse("AGENT claude\nOS minimal\nMODE ask\n").unwrap();
    let mut changed = instructions.clone();
    changed[1].arguments = vec!["huge".to_string()];
    changed[2].arguments = vec!["yolo".to_string()];
    let errors = Settings::read(&changed).unwrap_err();
    assert_eq!(lines_of(&errors), [2, 3], "{errors:?}");
}

#[test]
fn reading_settings_without_agent_is_an_error() {
    let instructions = parse("AGENT claude\nOS minimal\n").unwrap();
    let errors = Settings::read(&instructions[1..]).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("AGENT"), "{errors:?}");
}
