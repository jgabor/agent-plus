//! Reads and checks the text values of `SYSTEM` and `PROMPT`.

use crate::{Error, Instruction, Text, read_argument};

/// How an unquoted bare path starts.
const PATH_STARTS: [&str; 4] = ["./", "../", "/", "~/"];

/// Accepts every value, because `read` already applied the rules for text.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}

/// Reads the value of the instruction on `line`, whose argument is `rest`.
/// `lines` is the whole file, and `next` is the index of the line after the
/// instruction. A fenced block moves `next` past its closing fence.
pub(crate) fn read(
    line: usize,
    rest: &str,
    lines: &[&str],
    next: &mut usize,
) -> Result<Text, Error> {
    if let Some(fence) = opening_fence(rest) {
        return read_block(line, fence, lines, next);
    }
    if !rest.is_empty() {
        return Ok(single_line(rest));
    }
    if let Some(fence) = lines.get(*next).and_then(|text| opening_fence(text)) {
        *next += 1;
        return read_block(line + 1, fence, lines, next);
    }
    // Skip a block that opens after blank lines or indented, so its lines
    // aren't read as instructions.
    let mut at = *next;
    while lines.get(at).is_some_and(|text| text.trim().is_empty()) {
        at += 1;
    }
    if let Some(fence) = lines
        .get(at)
        .and_then(|text| opening_fence(text.trim_start()))
    {
        *next = at + 1;
        let _ = read_block(at + 1, fence, lines, next);
    }
    Err(Error {
        line,
        message: "a fenced block must open on the next line, in the first column".to_string(),
    })
}

/// Reads single-line text, which may be a bare path.
fn single_line(rest: &str) -> Text {
    if let Some(value) = one_quoted_string(rest) {
        return Text::Literal(value);
    }
    if PATH_STARTS.iter().any(|start| rest.starts_with(start))
        && let Ok((path, "")) = read_argument(rest)
    {
        return Text::Path(path);
    }
    Text::Literal(rest.to_string())
}

/// The content of `text`, if it's exactly one quoted string.
fn one_quoted_string(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = inner.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return (at + 1 == inner.len()).then_some(value),
            '\\' => match chars.clone().next() {
                Some((_, escaped @ ('"' | '\\'))) => {
                    value.push(escaped);
                    chars.next();
                }
                _ => value.push(c),
            },
            c => value.push(c),
        }
    }
    None
}

/// The number of backticks that open a fence at the start of `text`, if
/// there are enough. What follows them is the info string.
fn opening_fence(text: &str) -> Option<usize> {
    let ticks = backticks(text);
    (ticks >= 3).then_some(ticks)
}

fn backticks(text: &str) -> usize {
    text.len() - text.trim_start_matches('`').len()
}

/// Reads a block whose opening fence of `fence` backticks is on
/// `fence_line`. `next` is the index of its first content line.
fn read_block(
    fence_line: usize,
    fence: usize,
    lines: &[&str],
    next: &mut usize,
) -> Result<Text, Error> {
    let start = *next;
    while let Some(text) = lines.get(*next) {
        *next += 1;
        let ticks = backticks(text);
        if ticks >= fence && text[ticks..].trim().is_empty() {
            let content = &lines[start..*next - 1];
            if content.is_empty() {
                return Err(Error {
                    line: fence_line,
                    message: "a fenced block is empty".to_string(),
                });
            }
            return Ok(Text::Literal(content.join("\n") + "\n"));
        }
    }
    Err(Error {
        line: fence_line,
        message: "a fenced block is never closed".to_string(),
    })
}
