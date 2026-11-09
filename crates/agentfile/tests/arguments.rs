//! The argument rules of an Agentfile line.

use agentfile::{Instruction, parse};

fn arguments_of(source: &str) -> Vec<String> {
    let instructions = parse(source).unwrap();
    assert_eq!(instructions.len(), 1, "{instructions:?}");
    instructions[0].arguments.clone()
}

// spec(Arguments): An unquoted argument that starts with `--` is an error, which keeps flags available to later versions.
#[test]
fn an_unquoted_double_dash_argument_is_an_error() {
    let error = parse("SHARE --force\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert!(error.message.contains("--force"), "{}", error.message);
}

#[test]
fn any_argument_can_be_the_flag() {
    let error = parse("# comment\n\nSHARE ./out --rw\n").unwrap_err();
    assert_eq!(error.line, 3);
    assert!(error.message.contains("--rw"), "{}", error.message);
}

#[test]
fn an_argument_that_starts_unquoted_and_continues_quoted_is_a_flag() {
    assert!(parse("SHARE --\"force\"\n").is_err());
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
    for name in ["SYSTEM", "PROMPT", "ENV", "system"] {
        let source = format!("{name} Run tool --verbose\n");
        assert_eq!(arguments_of(&source), ["Run tool --verbose"], "{name}");
    }
}

#[test]
fn comments_and_blank_lines_are_not_instructions() {
    assert_eq!(parse("  # SHARE --force\n\n   \n").unwrap(), []);
}

#[test]
fn the_instruction_name_is_uppercased() {
    assert_eq!(
        parse("share ./out\n").unwrap(),
        [Instruction {
            line: 1,
            name: "SHARE".to_string(),
            arguments: vec!["./out".to_string()],
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
    let error = parse("SHARE \"./out\n").unwrap_err();
    assert_eq!(error.line, 1);
}

#[test]
fn an_error_prints_its_line_and_message() {
    let error = parse("\nSHARE \"./out\n").unwrap_err();
    assert_eq!(error.to_string(), "line 2: a quote is never closed");
}
