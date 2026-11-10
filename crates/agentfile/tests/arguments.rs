//! The argument rules of an Agentfile line.

use agentfile::{Error, Instruction, Text, parse};

/// Parses `line` after an `AGENT` line, and returns its arguments.
fn arguments_of(line: &str) -> Vec<String> {
    let instructions = parse(&format!("AGENT claude\n{line}")).unwrap();
    assert_eq!(instructions.len(), 2, "{instructions:?}");
    instructions[1].arguments.clone()
}

/// Parses `line` after an `AGENT` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = parse(&format!("AGENT claude\n{line}")).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors.remove(0)
}

// spec(Arguments): An unquoted argument that starts with `--` is an error, which keeps flags available to later versions.
#[test]
fn an_unquoted_double_dash_argument_is_an_error() {
    let error = error_of("SHARE --force\n");
    assert_eq!(error.line, 2);
    assert!(error.message.contains("--force"), "{}", error.message);
}

#[test]
fn any_argument_can_be_the_flag() {
    let error = error_of("# comment\n\nSHARE ./out --rw\n");
    assert_eq!(error.line, 4);
    assert!(error.message.contains("--rw"), "{}", error.message);
}

#[test]
fn every_flag_on_a_line_is_an_error() {
    let errors = parse("AGENT claude\nSHARE --a --b --c\n").unwrap_err();
    let found: Vec<_> = errors.iter().map(|e| (e.line, &e.message)).collect();
    assert_eq!(errors.len(), 3, "{errors:?}");
    for (error, flag) in errors.iter().zip(["--a", "--b", "--c"]) {
        assert_eq!(error.line, 2, "{found:?}");
        assert!(error.message.contains(flag), "{found:?}");
    }
}

#[test]
fn an_unclosed_quote_ends_the_line_errors() {
    let errors = parse("AGENT claude\nSHARE --a \"x --b\n").unwrap_err();
    let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].contains("--a"), "{messages:?}");
    assert_eq!(messages[1], "a quote is never closed");
}

#[test]
fn an_argument_that_starts_unquoted_and_continues_quoted_is_a_flag() {
    error_of("SHARE --\"force\"\n");
}

#[test]
fn a_quoted_double_dash_is_a_value() {
    assert_eq!(arguments_of("SHARE \"--force\"\n"), ["--force"]);
}

#[test]
fn dashes_later_in_an_argument_are_a_value() {
    assert_eq!(arguments_of("SHARE ./a--b\n"), ["./a--b"]);
}

#[test]
fn one_dash_is_a_value() {
    assert_eq!(arguments_of("NETWORK -github.com\n"), ["-github.com"]);
}

#[test]
fn text_instructions_keep_double_dashes() {
    assert_eq!(
        arguments_of("ENV Run tool --verbose\n"),
        ["Run tool --verbose"]
    );
    for name in ["SYSTEM", "PROMPT", "system"] {
        let source = format!("AGENT claude\n{name} Run tool --verbose\n");
        let text = parse(&source).unwrap().remove(1).text;
        let expected = Text::Literal("Run tool --verbose".to_string());
        assert_eq!(text, Some(expected), "{name}");
    }
}

#[test]
fn comments_and_blank_lines_are_not_instructions() {
    assert_eq!(
        parse("AGENT claude\n  # SHARE --force\n\n   \n")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn the_instruction_name_is_uppercased() {
    assert_eq!(
        parse("AGENT claude\nshare ./out\n").unwrap()[1..],
        [Instruction {
            line: 2,
            name: "SHARE".to_string(),
            arguments: vec!["./out".to_string()],
            text: None,
        }]
    );
}

#[test]
fn quotes_group_characters_into_one_argument() {
    assert_eq!(
        arguments_of("SHARE \"./My Notes\" rw\n"),
        ["./My Notes", "rw"]
    );
}

#[test]
fn inside_quotes_a_backslash_escapes_a_quote_or_a_backslash() {
    assert_eq!(arguments_of(r#"SHARE "a\"b\\c\d""#), [r#"a"b\c\d"#]);
}

#[test]
fn outside_quotes_a_backslash_is_a_backslash() {
    assert_eq!(arguments_of(r"SHARE a\\b"), [r"a\\b"]);
}

#[test]
fn an_unterminated_quote_is_an_error() {
    let error = error_of("SHARE \"./out\n");
    assert_eq!(error.line, 2);
}

#[test]
fn an_error_prints_its_line_and_message() {
    let error = error_of("\nSHARE \"./out\n");
    assert_eq!(error.to_string(), "line 3: a quote is never closed");
}
