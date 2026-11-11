//! The text values of `SYSTEM` and `PROMPT`, from SPEC.md's Text values
//! section.

use agentfile::{Error, Instruction, Text, parse};

/// Parses `rest` after an `AGENT` line, and returns its instructions.
fn instructions_of(rest: &str) -> Vec<Instruction> {
    let mut instructions = parse(&format!("AGENT claude\n{rest}")).unwrap();
    instructions.remove(0);
    instructions
}

/// Parses `rest` after an `AGENT` line, and returns the text of its one
/// instruction.
fn text_of(rest: &str) -> Text {
    let instructions = instructions_of(rest);
    assert_eq!(instructions.len(), 1, "{instructions:?}");
    let instruction = &instructions[0];
    assert!(instruction.arguments.is_empty(), "{instruction:?}");
    instruction.text.clone().expect("no text")
}

/// Parses `rest` after an `AGENT` line, and returns its errors.
fn errors_of(rest: &str) -> Vec<Error> {
    parse(&format!("AGENT claude\n{rest}")).unwrap_err()
}

/// Parses `rest` after an `AGENT` line, and returns the line of its one
/// error.
fn error_line(rest: &str) -> usize {
    let errors = errors_of(rest);
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors[0].line
}

fn literal(value: &str) -> Text {
    Text::Literal(value.to_string())
}

fn path(value: &str) -> Text {
    Text::Path(value.to_string())
}

// spec(Text values): `SYSTEM` and `PROMPT` take text in one of three forms.
#[test]
fn system_and_prompt_take_all_three_forms() {
    for name in ["SYSTEM", "PROMPT", "system", "Prompt"] {
        assert_eq!(
            text_of(&format!("{name} Be brief.\n")),
            literal("Be brief.")
        );
        assert_eq!(text_of(&format!("{name} ./a.md\n")), path("./a.md"));
        assert_eq!(
            text_of(&format!("{name}\n```\nBe brief.\n```\n")),
            literal("Be brief.\n")
        );
    }
}

// spec(Text values): **Single-line text.** If the rest of the line is exactly one quoted string, the value is that string's content.
#[test]
fn one_quoted_string_is_its_content() {
    assert_eq!(text_of("SYSTEM \"Be concise.\"\n"), literal("Be concise."));
    assert_eq!(text_of("SYSTEM \"\"\n"), literal(""));
    assert_eq!(text_of("PROMPT \"  spaced  \"  \n"), literal("  spaced  "));
    assert_eq!(text_of(r#"SYSTEM "a\"b\\c\d""#), literal(r#"a"b\c\d"#));
}

// spec(Text values): Otherwise, the value is the rest of the line, verbatim, quotes included.
#[test]
fn anything_else_is_the_rest_of_the_line() {
    for rest in [
        r#"Say "hello" first."#,
        r#""a" "b""#,
        r#""a"b"#,
        r#""a""b""#,
        r#""never closed"#,
        r#""ends with \""#,
        "Run tool --verbose",
        "fix #12 # now",
        "$HOME is ${HOME}",
    ] {
        assert_eq!(
            text_of(&format!("SYSTEM {rest}\n")),
            literal(rest),
            "{rest}"
        );
    }
}

// spec(Text values): Whitespace at either end of the rest of the line isn't part of the value.
#[test]
fn whitespace_at_either_end_of_the_line_is_not_part_of_the_value() {
    for (rest, value) in [
        ("Be brief.  ", literal("Be brief.")),
        ("  Be brief.", literal("Be brief.")),
        ("Be brief.\t", literal("Be brief.")),
        ("Be brief. \r", literal("Be brief.")),
        (" Be  brief. ", literal("Be  brief.")),
        ("./a.md ", path("./a.md")),
        ("\"x\" ", literal("x")),
        ("\" x \"\t", literal(" x ")),
    ] {
        assert_eq!(text_of(&format!("SYSTEM {rest}\n")), value, "{rest:?}");
        assert_eq!(text_of(&format!("PROMPT {rest}\n")), value, "{rest:?}");
    }
}

// spec(Text values): The values are `You are a careful reviewer.`, `Be concise.`, and `Say "hello" first.` In the third line, the rest of the line is not one quoted string, so the quotes are kept.
#[test]
fn the_single_line_examples_have_their_values() {
    let source = "SYSTEM You are a careful reviewer.\n\
                  SYSTEM \"Be concise.\"\n\
                  SYSTEM Say \"hello\" first.\n";
    let texts: Vec<_> = instructions_of(source)
        .into_iter()
        .map(|instruction| instruction.text.unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            literal("You are a careful reviewer."),
            literal("Be concise."),
            literal("Say \"hello\" first."),
        ]
    );
}

// spec(Text values): **Bare path.** A value that is one argument and starts with an unquoted `./`, `../`, `/`, or `~/` is a path.
#[test]
fn one_argument_that_starts_like_a_path_is_a_path() {
    for value in [
        "./prompts/system.md",
        "../up.md",
        "/abs/x.md",
        "~/.claude/CLAUDE.md",
    ] {
        assert_eq!(
            text_of(&format!("SYSTEM {value}\n")),
            path(value),
            "{value}"
        );
        assert_eq!(
            text_of(&format!("PROMPT {value}\n")),
            path(value),
            "{value}"
        );
    }
    for value in [
        "./a b",
        "/review this",
        "x./a",
        "~user/a",
        ".hidden",
        "~",
        "./\"a",
    ] {
        assert_eq!(
            text_of(&format!("SYSTEM {value}\n")),
            literal(value),
            "{value}"
        );
    }
}

#[test]
fn a_path_is_kept_as_a_path_and_not_read() {
    let missing = "./this/file/does/not/exist.md";
    assert_eq!(text_of(&format!("SYSTEM {missing}\n")), path(missing));
}

// spec(Text values): Quote the whole value to use it as text.
#[test]
fn a_quoted_path_is_text() {
    assert_eq!(text_of("SYSTEM \"./a.md\"\n"), literal("./a.md"));
    assert_eq!(
        text_of("SYSTEM \"~/.claude/CLAUDE.md\"\n"),
        literal("~/.claude/CLAUDE.md")
    );
}

// spec(PROMPT): A prompt that starts with `/` must be quoted, or it is read as a path.
#[test]
fn a_slash_prompt_is_a_path_unless_quoted() {
    assert_eq!(text_of("PROMPT \"/review\"\n"), literal("/review"));
    assert_eq!(text_of("PROMPT /review\n"), path("/review"));
}

// spec(Text values): A path with spaces stays one argument if you quote only the part that has them.
// spec(Text values): `ap` removes those quotes.
#[test]
fn a_partly_quoted_path_is_one_path_without_its_quotes() {
    assert_eq!(
        text_of("SYSTEM ./\"My Prompts\"/system.md\n"),
        path("./My Prompts/system.md")
    );
    assert_eq!(text_of("PROMPT ~/\"a b\"\n"), path("~/a b"));
}

// spec(Text values): **Fenced block.** For multi-line text, use a CommonMark-style code fence.
// spec(Format): Text values may continue in a fenced block (see [Text values](#text-values)).
// spec(SYSTEM): followed by a fenced block.
#[test]
fn system_takes_a_fenced_block() {
    let source = "SYSTEM\n```\nYou are a careful reviewer.\n\n# Rules\nPrefer small diffs.\n```\n";
    assert_eq!(
        text_of(source),
        literal("You are a careful reviewer.\n\n# Rules\nPrefer small diffs.\n")
    );
}

// spec(PROMPT): followed by a fenced block.
#[test]
fn prompt_takes_a_fenced_block() {
    assert_eq!(
        text_of("PROMPT\n```\nReview the diff.\nList risks.\n```\n"),
        literal("Review the diff.\nList risks.\n")
    );
}

// spec(Text values): The opening fence is either the line directly after the instruction, or the instruction's entire argument (`SYSTEM ``` `).
#[test]
fn the_fence_opens_on_the_next_line_or_as_the_whole_argument() {
    let next = text_of("SYSTEM\n```\nhi\n```\n");
    let same = text_of("SYSTEM ```\nhi\n```\n");
    assert_eq!(next, literal("hi\n"));
    assert_eq!(same, next);
    assert_eq!(text_of("PROMPT ```markdown\nhi\n```\n"), next);
}

// spec(Text values): After a bare `SYSTEM` or `PROMPT`, the very next line must be the opening fence, starting in the first column.
#[test]
fn a_bare_instruction_needs_a_fence_on_the_next_line() {
    let lines: Vec<_> = errors_of("SYSTEM\nBe brief.\n")
        .iter()
        .map(|e| e.line)
        .collect();
    assert_eq!(lines, [2, 3], "the next line is still read");
    assert_eq!(error_line("PROMPT\nSHARE ./out\n"), 2);
    assert_eq!(error_line("PROMPT\n"), 2);
    assert_eq!(error_line("PROMPT"), 2);
    let error = &errors_of("SYSTEM\nBe brief.\n")[0];
    assert!(error.message.contains("fence"), "{}", error.message);
}

// spec(Text values): A blank line or an indented fence in between is an error.
#[test]
fn a_blank_line_or_an_indented_fence_is_an_error() {
    // The block is still read, so its lines aren't reported as instructions.
    assert_eq!(error_line("SYSTEM\n\n```\nBe brief.\n```\n"), 2);
    assert_eq!(error_line("SYSTEM\n  ```\nBe brief.\n```\n"), 2);
    assert_eq!(error_line("PROMPT\n\t```\nBe brief.\n```\n"), 2);
    let errors = errors_of("PROMPT\n\n \n  ```\nBe brief.\n```\nBOGUS\n");
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [2, 8], "{errors:?}");
}

#[test]
fn a_misplaced_block_still_reports_its_own_errors() {
    let errors = errors_of("SYSTEM\n\n```\n```\nBOGUS\n");
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [2, 4, 6], "{errors:?}");
    let errors = errors_of("SYSTEM\n  ```\nBe brief.\n");
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [2, 3], "{errors:?}");
}

// spec(Text values): Text followed by backticks on the instruction line is single-line text, not a fence.
#[test]
fn text_then_backticks_is_single_line_text() {
    assert_eq!(text_of("SYSTEM Use ```\n"), literal("Use ```"));
    let instructions = instructions_of("SYSTEM Use ```\nSHARE ./out\n");
    assert_eq!(instructions[0].text, Some(literal("Use ```")));
    assert_eq!(instructions[1].name, "SHARE");
}

// spec(Text values): An opening fence is three or more backticks, optionally followed by an info string.
#[test]
fn an_opening_fence_is_three_or_more_backticks() {
    for fence in ["```", "````", "``````", "```markdown", "``` json extra"] {
        let source = format!("SYSTEM\n{fence}\nhi\n{}\n", "`".repeat(6));
        assert_eq!(text_of(&source), literal("hi\n"), "{fence}");
    }
    assert_eq!(errors_of("SYSTEM\n``\nhi\n``\n")[0].line, 2);
    assert_eq!(text_of("SYSTEM ``\n"), literal("``"));
}

// spec(Text values): The info string is ignored, so ` ```markdown ` works.
#[test]
fn the_info_string_is_ignored() {
    assert_eq!(
        text_of("SYSTEM\n```markdown\n# Rules\n```\n"),
        text_of("SYSTEM\n```\n# Rules\n```\n")
    );
}

// spec(Text values): The block ends at the first line that starts in the first column and holds only backticks, at least as many as the opening fence, optionally followed by whitespace.
#[test]
fn the_block_ends_at_a_long_enough_fence_in_the_first_column() {
    assert_eq!(text_of("SYSTEM\n```\na\n```  \t\n"), literal("a\n"));
    assert_eq!(text_of("SYSTEM\n```\na\n`````\n"), literal("a\n"));
    assert_eq!(
        text_of("SYSTEM\n```\n ```\n```x\nx```\n```\n"),
        literal(" ```\n```x\nx```\n")
    );
    assert_eq!(text_of("SYSTEM\n````\n```\n````\n"), literal("```\n"));
    let instructions = instructions_of("SYSTEM\n```\na\n```\nSHARE ./out\n");
    assert_eq!(instructions[0].text, Some(literal("a\n")));
    assert_eq!(instructions[1].name, "SHARE");
}

#[test]
fn the_first_closing_fence_ends_the_block() {
    let errors = errors_of("SYSTEM\n```\na\n```\n```\n");
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [6], "{errors:?}");
}

// spec(Text values): To put a code block inside the text, open with four backticks.
#[test]
fn four_backticks_hold_a_code_block() {
    let source =
        "SYSTEM\n````\nAnswer in this format:\n\n```json\n{\"verdict\": \"approve\"}\n```\n````\n";
    assert_eq!(
        text_of(source),
        literal("Answer in this format:\n\n```json\n{\"verdict\": \"approve\"}\n```\n")
    );
}

// spec(Text values): Content lines are kept verbatim, including indentation.
#[test]
fn content_lines_are_kept_verbatim() {
    assert_eq!(
        text_of("SYSTEM\n```\n  two\n\tTab\ntrailing  \n\"quoted\"\n```\n"),
        literal("  two\n\tTab\ntrailing  \n\"quoted\"\n")
    );
    assert_eq!(text_of("SYSTEM\r\n```\r\na\r\n```\r\n"), literal("a\n"));
}

// spec(Text values): Lines starting with `#` are content, not comments, and `$` is never expanded.
// spec(Format): Blank lines are ignored outside fenced blocks.
#[test]
fn hash_lines_blank_lines_and_dollars_are_content() {
    assert_eq!(
        text_of("PROMPT\n```\n# Title\n\n  # indented\nSHARE --force\n$HOME ${X}\n```\n"),
        literal("# Title\n\n  # indented\nSHARE --force\n$HOME ${X}\n")
    );
    let source = "SYSTEM\n```\n\n```\n\n\nSHARE ./out\n";
    assert_eq!(instructions_of(source)[1].line, 8);
}

// spec(Text values): The value is the lines between the fences, joined with newlines and ending with a newline.
#[test]
fn the_value_joins_the_lines_and_ends_with_a_newline() {
    assert_eq!(text_of("SYSTEM\n```\na\n\nb\n```"), literal("a\n\nb\n"));
    assert_eq!(text_of("SYSTEM\n```\n\n```\n"), literal("\n"));
}

// spec(Text values): An empty block and an unclosed fence are errors.
#[test]
fn an_empty_block_or_an_unclosed_fence_is_an_error() {
    assert_eq!(error_line("SYSTEM\n```\n```\n"), 3);
    assert_eq!(error_line("SYSTEM ```\n```\n"), 2);
    assert_eq!(error_line("SYSTEM\n```\na\nSHARE ./out\n"), 3);
    assert_eq!(error_line("PROMPT ````\na\n```\n"), 2);
    let error = &errors_of("SYSTEM\n```\na\n")[0];
    assert!(error.message.contains("never closed"), "{}", error.message);
}

#[test]
fn a_block_keeps_the_lines_of_what_follows() {
    let source = "SYSTEM\n```\na\nb\n```\n\nSHARE ./out\nPROMPT ```\nc\n```\nMODEL x\n";
    let lines: Vec<_> = instructions_of(source)
        .iter()
        .map(|instruction| (instruction.line, instruction.name.clone()))
        .collect();
    assert_eq!(
        lines,
        [
            (2, "SYSTEM".to_string()),
            (8, "SHARE".to_string()),
            (9, "PROMPT".to_string()),
            (12, "MODEL".to_string()),
        ]
    );
    assert_eq!(error_line("SYSTEM\n```\na\n```\nBOGUS\n"), 6);
}

// spec(SYSTEM): `SYSTEM` can repeat.
#[test]
fn system_can_repeat_in_any_form() {
    let source =
        "SYSTEM ~/.claude/CLAUDE.md\nSYSTEM Never push to main.\nSYSTEM\n```\nBe kind.\n```\n";
    let texts: Vec<_> = instructions_of(source)
        .into_iter()
        .map(|instruction| instruction.text.unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            path("~/.claude/CLAUDE.md"),
            literal("Never push to main."),
            literal("Be kind.\n"),
        ]
    );
}

#[test]
fn prompt_cannot_repeat() {
    assert_eq!(error_line("PROMPT a\nPROMPT\n```\nb\n```\n"), 3);
}
