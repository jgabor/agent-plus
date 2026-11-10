//! The line rules of an Agentfile, from SPEC.md's Format section.

use agentfile::{Error, Instruction, Phase, parse};

fn names(source: &str) -> Vec<(usize, String)> {
    parse(source)
        .unwrap()
        .into_iter()
        .map(|instruction| (instruction.line, instruction.name))
        .collect()
}

fn errors(source: &str) -> Vec<Error> {
    parse(source).unwrap_err()
}

fn lines_of(errors: &[Error]) -> Vec<usize> {
    errors.iter().map(|error| error.line).collect()
}

fn arguments_of(source: &str, line: usize) -> Vec<String> {
    let instructions = parse(source).unwrap();
    let found = instructions.iter().find(|i| i.line == line);
    found
        .expect("no instruction on that line")
        .arguments
        .clone()
}

// spec(Format): Instructions are case-insensitive.
#[test]
fn instruction_names_are_read_in_any_case() {
    let source = "agent claude\nShare ./out\nnEtWoRk github.com\n";
    assert_eq!(
        names(source),
        [
            (1, "AGENT".to_string()),
            (2, "SHARE".to_string()),
            (3, "NETWORK".to_string()),
        ]
    );
}

#[test]
fn a_repeat_in_another_case_is_still_a_repeat() {
    assert_eq!(lines_of(&errors("AGENT claude\nMODEL a\nmodel b\n")), [3]);
}

// spec(Format): A line whose first non-whitespace character is `#` is a comment.
#[test]
fn a_line_that_starts_with_a_hash_is_a_comment() {
    let source = "# top\nAGENT claude\n  # indented\n\t# tabbed\n#SHARE --force\n";
    assert_eq!(names(source), [(2, "AGENT".to_string())]);
}

// spec(Format): There are no trailing comments: a `#` anywhere else is part of the argument, because prompts, URLs, and paths contain `#`.
#[test]
fn a_hash_after_the_start_is_part_of_the_argument() {
    let source = "AGENT claude\nSHARE ./a#b # not a comment\nPROMPT fix #12 # now\n";
    assert_eq!(
        arguments_of(source, 2),
        ["./a#b", "#", "not", "a", "comment"]
    );
    assert_eq!(arguments_of(source, 3), ["fix #12 # now"]);
}

// spec(Format): Blank lines are ignored outside fenced blocks.
#[test]
fn blank_lines_are_ignored() {
    let source = "\n\nAGENT claude\n   \n\t\nOS minimal\n\n";
    assert_eq!(
        names(source),
        [(3, "AGENT".to_string()), (6, "OS".to_string())]
    );
}

// spec(Format): Each instruction occupies one line.
// spec(Format): There is no line continuation.
#[test]
fn a_trailing_backslash_does_not_continue_the_line() {
    let source = "AGENT claude\nSHARE ./out \\\nrw\n";
    let errors = errors(source);
    assert_eq!(lines_of(&errors), [3], "{errors:?}");
    assert!(errors[0].message.contains("`rw`"), "{}", errors[0].message);
    assert_eq!(
        arguments_of("AGENT claude\nSHARE ./out \\\n", 2),
        ["./out", "\\"]
    );
}

// spec(Format): `AGENT` must be the first instruction, and it appears exactly once.
#[test]
fn agent_comes_first_and_appears_once() {
    assert_eq!(names("# setup\n\nAGENT claude\n")[0].1, "AGENT");

    let missing = errors("OS minimal\nMODEL a\n");
    assert_eq!(lines_of(&missing), [1], "{missing:?}");
    assert!(
        missing[0].message.contains("AGENT"),
        "{}",
        missing[0].message
    );

    let late = errors("OS minimal\n\nAGENT claude\n");
    assert_eq!(lines_of(&late), [3], "{late:?}");
    assert!(late[0].message.contains("AGENT"), "{}", late[0].message);

    let after_unknown = errors("BOGUS\nAGENT claude\n");
    assert_eq!(lines_of(&after_unknown), [1, 2], "{after_unknown:?}");

    let twice = errors("AGENT claude\nOS minimal\nAGENT codex\n");
    assert_eq!(lines_of(&twice), [3], "{twice:?}");
    assert!(twice[0].message.contains("AGENT"), "{}", twice[0].message);
}

#[test]
fn a_file_with_no_instructions_lacks_agent_on_line_1() {
    for source in ["", "# only a comment\n\n"] {
        let errors = errors(source);
        assert_eq!(lines_of(&errors), [1], "{source:?}: {errors:?}");
        assert!(errors[0].message.contains("AGENT"), "{}", errors[0].message);
    }
}

// spec(Format): Unknown instructions, and a non-repeatable instruction that appears twice, are errors.
#[test]
fn unknown_and_repeated_instructions_are_errors() {
    let unknown = errors("AGENT claude\nFROM ubuntu:24.04\n");
    assert_eq!(lines_of(&unknown), [2], "{unknown:?}");
    assert!(
        unknown[0].message.contains("FROM"),
        "{}",
        unknown[0].message
    );

    let repeated = errors("AGENT claude\nMODEL a\nOS minimal\nMODEL b\n");
    assert_eq!(lines_of(&repeated), [4], "{repeated:?}");
    let message = &repeated[0].message;
    assert!(message.contains("MODEL"), "{message}");
    assert!(message.contains("line 2"), "{message}");
}

#[test]
fn repeatable_instructions_may_appear_many_times() {
    let source = "AGENT claude\nSHARE ./a\nSHARE ./b\nSYSTEM one\nSYSTEM two\n";
    assert_eq!(parse(source).unwrap().len(), 5);
}

#[test]
fn every_instruction_in_the_overview_is_known() {
    for definition in agentfile::INSTRUCTIONS {
        let source = if definition.name == "AGENT" {
            "AGENT claude\n".to_string()
        } else {
            format!("AGENT claude\n{} x\n", definition.name)
        };
        assert!(parse(&source).is_ok(), "{}", definition.name);
    }
}

// spec(Format): Line endings are normalized to LF when the file is read.
#[test]
fn line_endings_are_normalized_to_lf() {
    let lf = "AGENT claude\nSYSTEM be brief\nSHARE \"./My Notes\" rw\n";
    let crlf = lf.replace('\n', "\r\n");
    let cr = lf.replace('\n', "\r");
    assert_eq!(parse(&crlf), parse(lf));
    assert_eq!(parse(&cr), parse(lf));
    let errors = errors("AGENT claude\r\nFROM x\rFROM y\r\n");
    assert_eq!(lines_of(&errors), [2, 3], "{errors:?}");
}

#[test]
fn every_error_is_reported_with_its_line() {
    let source = "OS minimal\nAGENT claude\nBOGUS\nSHARE \"./out\nOS standard\n";
    let errors = errors(source);
    assert_eq!(lines_of(&errors), [2, 3, 4, 5], "{errors:?}");
}

#[test]
fn errors_come_in_line_order() {
    let errors = errors("OS minimal\nFROM x\n");
    let lines = lines_of(&errors);
    assert!(lines.is_sorted(), "{errors:?}");
    assert_eq!(lines, [1, 2], "{errors:?}");
}

#[test]
fn a_valid_file_returns_each_instruction_with_its_line() {
    let source = "AGENT claude 2.1\n\n# a note\nSHARE ./out rw\nPROMPT go\n";
    assert_eq!(
        parse(source).unwrap(),
        [
            Instruction {
                line: 1,
                name: "AGENT".to_string(),
                arguments: vec!["claude".to_string(), "2.1".to_string()],
            },
            Instruction {
                line: 4,
                name: "SHARE".to_string(),
                arguments: vec!["./out".to_string(), "rw".to_string()],
            },
            Instruction {
                line: 5,
                name: "PROMPT".to_string(),
                arguments: vec!["go".to_string()],
            },
        ]
    );
}

/// The Overview table in SPEC.md, as (name, phase, repeatable) rows.
fn overview_table() -> Vec<(String, String, String)> {
    let spec = include_str!("../../../SPEC.md");
    let overview = spec.split("\n## Overview\n").nth(1).unwrap();
    let overview = overview.split("\n## ").next().unwrap();
    overview
        .lines()
        .filter(|line| line.starts_with("| [`"))
        .map(|row| {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            let name = cells[1].split('`').nth(1).unwrap().to_string();
            (name, cells[2].to_string(), cells[3].to_string())
        })
        .collect()
}

#[test]
fn the_instruction_table_matches_the_spec_overview() {
    let ours: Vec<(String, String, String)> = agentfile::INSTRUCTIONS
        .iter()
        .map(|definition| {
            let phase = match definition.phase {
                Phase::Build => "build",
                Phase::Run => "run",
            };
            let repeatable = if definition.repeatable { "yes" } else { "no" };
            (
                definition.name.to_string(),
                phase.to_string(),
                repeatable.to_string(),
            )
        })
        .collect();
    let spec = overview_table();
    assert_eq!(spec.len(), 24);
    assert_eq!(ours, spec);
}
