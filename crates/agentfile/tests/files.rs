//! The arguments of `SHARE`, `WORKDIR`, `PERSIST`, `PERMISSIONS`, and `PORT`,
//! and the typed values they parse into.

use agentfile::{
    Access, Error, Files, INSTRUCTIONS, Phase, Port, Rule, Share, ShareMode, State, Workdir, parse,
};

/// Parses `lines` after an `AGENT` line, and returns its files.
fn files(lines: &str) -> Files {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Files::read(&instructions).unwrap()
}

/// Parses `line` after an `AGENT` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = parse(&format!("AGENT claude\n{line}")).unwrap_err();
    assert_eq!(errors.len(), 1, "{line:?}: {errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.line, 2, "{error:?}");
    error
}

/// Parses `line` after an `AGENT` line, and returns its error message.
fn message_of(line: &str) -> String {
    error_of(line).message
}

fn share(line: usize, source: &str, destination: Option<&str>, mode: ShareMode) -> Share {
    Share {
        line,
        source: source.to_string(),
        destination: destination.map(str::to_string),
        mode,
    }
}

// spec(SHARE): `SHARE` is a run instruction.
#[test]
fn share_is_a_run_instruction() {
    let definition = INSTRUCTIONS.iter().find(|d| d.name == "SHARE").unwrap();
    assert_eq!(definition.phase, Phase::Run);
}

#[test]
fn share_takes_a_source_an_optional_destination_and_an_optional_mode() {
    let files = files(
        "SHARE ./references\n\
         SHARE ./data /data\n\
         SHARE /var/log/output.log rw\n\
         SHARE ~/notes ro\n\
         SHARE ./out ~/out ro\n",
    );
    assert_eq!(
        files.share,
        vec![
            share(2, "./references", None, ShareMode::CopyOnWrite),
            share(3, "./data", Some("/data"), ShareMode::CopyOnWrite),
            share(4, "/var/log/output.log", None, ShareMode::ReadWrite),
            share(5, "~/notes", None, ShareMode::ReadOnly),
            share(6, "./out", Some("~/out"), ShareMode::ReadOnly),
        ]
    );
}

#[test]
fn a_share_without_a_mode_is_copy_on_write() {
    assert_eq!(ShareMode::default(), ShareMode::CopyOnWrite);
    assert_eq!(files("SHARE .\n").share[0].mode, ShareMode::CopyOnWrite);
}

#[test]
fn a_quoted_share_path_keeps_its_spaces() {
    assert_eq!(
        files("SHARE \"./My Notes\"\n").share,
        vec![share(2, "./My Notes", None, ShareMode::CopyOnWrite)]
    );
}

// spec(Arguments): A few instructions end in an optional word that sets a mode.
#[test]
fn share_repo_and_secret_end_in_an_optional_mode_word() {
    let instructions = parse(
        "AGENT claude\n\
         SHARE ./out rw\n\
         REPO jgabor/agent-plus history\n\
         SECRET AWS_SECRET_ACCESS_KEY visible\n",
    )
    .unwrap();
    let files = Files::read(&instructions).unwrap();
    assert_eq!(files.share[0].mode, ShareMode::ReadWrite);
    let sources = agentfile::Sources::read(&instructions).unwrap();
    assert!(sources.repo[0].history);
    let secrets = agentfile::Secrets::read(&instructions).unwrap();
    assert!(secrets.secrets[0].visible);
}

// spec(Arguments): The word is matched exactly and must be the last argument.
#[test]
fn a_share_mode_word_is_matched_exactly() {
    for word in ["RW", "Ro", "rw.", "readonly"] {
        let files = files(&format!("SHARE ./out {word}\n"));
        assert_eq!(
            files.share,
            vec![share(2, "./out", Some(word), ShareMode::CopyOnWrite)],
            "{word}"
        );
    }
}

// spec(Arguments): The word is matched exactly and must be the last argument.
#[test]
fn a_share_mode_word_must_be_the_last_argument() {
    for line in [
        "SHARE ./out rw ro",
        "SHARE ./out ro ~/out",
        "SHARE ./out rw rw",
    ] {
        assert_eq!(
            message_of(line),
            format!(
                "`{word}` must be the last argument of `SHARE`. To use it as a destination, write `./{word}`",
                word = line.split(' ').nth(2).unwrap()
            ),
            "{line}"
        );
    }
}

// spec(Arguments): To use one of these words as a destination, write it as a path, such as `./rw`.
#[test]
fn a_mode_word_written_as_a_path_is_a_destination() {
    let instructions = parse(
        "AGENT claude\n\
         SHARE ./out ./rw\n\
         SHARE ./in ./ro ro\n\
         REPO jgabor/agent-plus ./history\n",
    )
    .unwrap();
    assert_eq!(
        Files::read(&instructions).unwrap().share,
        vec![
            share(2, "./out", Some("./rw"), ShareMode::CopyOnWrite),
            share(3, "./in", Some("./ro"), ShareMode::ReadOnly),
        ]
    );
    let sources = agentfile::Sources::read(&instructions).unwrap();
    assert_eq!(sources.repo[0].destination.as_deref(), Some("./history"));
    assert!(!sources.repo[0].history);
}

#[test]
fn share_rejects_a_missing_or_empty_path() {
    assert_eq!(message_of("SHARE"), "`SHARE` needs a host path");
    assert_eq!(message_of("SHARE \"\""), "`SHARE` needs a host path");
    assert_eq!(message_of("SHARE \"\" rw"), "`SHARE` needs a host path");
    assert_eq!(
        message_of("SHARE ./data \"\""),
        "`SHARE` can't take an empty destination"
    );
}

#[test]
fn share_rejects_extra_arguments() {
    assert_eq!(
        message_of("SHARE ./a ./b ./c"),
        "`SHARE` takes a host path, an optional destination, and an optional `rw` or `ro`, but found 3 arguments"
    );
    assert_eq!(
        message_of("SHARE ./a ./b ./c rw"),
        "`SHARE` takes a host path, an optional destination, and an optional `rw` or `ro`, but found 4 arguments"
    );
}

#[test]
fn workdir_takes_one_guest_path() {
    assert_eq!(files("").workdir, None);
    assert_eq!(
        files("WORKDIR ~/app/packages/web\n").workdir,
        Some(Workdir {
            line: 2,
            path: "~/app/packages/web".to_string(),
        })
    );
    assert_eq!(message_of("WORKDIR"), "`WORKDIR` needs a guest path");
    assert_eq!(message_of("WORKDIR \"\""), "`WORKDIR` needs a guest path");
    assert_eq!(
        message_of("WORKDIR ~/a ~/b"),
        "`WORKDIR` takes one guest path, but found 2 arguments"
    );
}

// spec(PERSIST): `PERSIST` names the state that survives between runs.
#[test]
fn persist_names_sessions_or_home() {
    let files = files("PERSIST sessions\nPERSIST home\n");
    let states: Vec<(usize, State)> = files.persist.iter().map(|p| (p.line, p.state)).collect();
    assert_eq!(states, vec![(2, State::Sessions), (3, State::Home)]);
}

#[test]
fn persist_rejects_other_words() {
    for word in ["Sessions", "HOME", "history", "\"\""] {
        let shown = if word == "\"\"" { "" } else { word };
        assert_eq!(
            message_of(&format!("PERSIST {word}")),
            format!("`PERSIST` takes `sessions` or `home`, but found `{shown}`"),
            "{word}"
        );
    }
    assert_eq!(message_of("PERSIST"), "`PERSIST` needs an argument");
    assert_eq!(
        message_of("PERSIST sessions home"),
        "`PERSIST` takes one argument, but found 2"
    );
}

#[test]
fn permissions_reads_a_sign_an_access_and_a_path() {
    let files = files(
        "PERMISSIONS -write:.husky\n\
         PERMISSIONS -write:.github/workflows\n\
         PERMISSIONS -read:.env\n\
         PERMISSIONS +read:~/notes\n\
         PERMISSIONS +write:/tmp/out\n\
         PERMISSIONS \"-read:./My Notes\"\n",
    );
    let rule = |line, allow, access, path: &str| Rule {
        line,
        allow,
        access,
        path: path.to_string(),
    };
    assert_eq!(
        files.permissions,
        vec![
            rule(2, false, Access::Write, ".husky"),
            rule(3, false, Access::Write, ".github/workflows"),
            rule(4, false, Access::Read, ".env"),
            rule(5, true, Access::Read, "~/notes"),
            rule(6, true, Access::Write, "/tmp/out"),
            rule(7, false, Access::Read, "./My Notes"),
        ]
    );
}

#[test]
fn permissions_rejects_a_rule_without_a_sign_access_or_path() {
    let shape = |found: &str| {
        format!(
            "`PERMISSIONS` takes `+` or `-`, then `read` or `write`, a colon, and a guest path, such as `-read:.env`, but found `{found}`"
        )
    };
    for rule in [
        "read:.env",
        "*read:.env",
        "-exec:.env",
        "-READ:.env",
        "-read",
        "+write.env",
        "-:.env",
    ] {
        assert_eq!(
            message_of(&format!("PERMISSIONS {rule}")),
            shape(rule),
            "{rule}"
        );
    }
    assert_eq!(
        message_of("PERMISSIONS -read:"),
        "`PERMISSIONS` needs a guest path after `-read:`"
    );
    assert_eq!(message_of("PERMISSIONS"), "`PERMISSIONS` needs an argument");
    assert_eq!(
        message_of("PERMISSIONS -read:.env -write:.env"),
        "`PERMISSIONS` takes one argument, but found 2"
    );
}

#[test]
fn port_takes_a_guest_port_and_an_optional_host_port() {
    let files = files("PORT 5173\nPORT 8080 9000\nPORT 1 65535\n");
    assert_eq!(
        files.port,
        vec![
            Port {
                line: 2,
                guest: 5173,
                host: None,
            },
            Port {
                line: 3,
                guest: 8080,
                host: Some(9000),
            },
            Port {
                line: 4,
                guest: 1,
                host: Some(65535),
            },
        ]
    );
}

#[test]
fn port_rejects_a_number_outside_1_to_65535() {
    for value in [
        "0", "65536", "-1", "+80", "80.0", "http", "0x50", " 80", "\"\"",
    ] {
        let shown = if value == "\"\"" { "" } else { value };
        let line = format!("PORT \"{shown}\"");
        assert_eq!(
            message_of(&line),
            format!("`PORT` takes a port from `1` to `65535`, but found `{shown}`"),
            "{line}"
        );
        let line = format!("PORT 8080 \"{shown}\"");
        assert_eq!(
            message_of(&line),
            format!("`PORT` takes a port from `1` to `65535`, but found `{shown}`"),
            "{line}"
        );
    }
    assert_eq!(message_of("PORT"), "`PORT` needs a guest port");
    assert_eq!(
        message_of("PORT 80 81 82"),
        "`PORT` takes a guest port and an optional host port, but found 3 arguments"
    );
}

#[test]
fn every_bad_line_is_reported_with_its_line() {
    let errors = parse(
        "AGENT claude\n\
         SHARE\n\
         WORKDIR\n\
         PERSIST all\n\
         PERMISSIONS read:.env\n\
         PORT 0\n",
    )
    .unwrap_err();
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, vec![2, 3, 4, 5, 6], "{errors:?}");
}

#[test]
fn read_reports_every_error_in_line_order() {
    let instructions = vec![
        agentfile::Instruction {
            line: 3,
            name: "PORT".to_string(),
            arguments: vec!["0".to_string()],
            text: None,
        },
        agentfile::Instruction {
            line: 1,
            name: "PERSIST".to_string(),
            arguments: vec!["all".to_string()],
            text: None,
        },
    ];
    let errors = Files::read(&instructions).unwrap_err();
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, vec![1, 3]);
}

#[test]
fn read_skips_instructions_it_does_not_own() {
    let files = files("MODEL sonnet\nADD ./a\nENV A b\n");
    assert_eq!(files, Files::default());
}
