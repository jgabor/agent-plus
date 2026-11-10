//! Rewrites an Agentfile by the rules in SPEC.md's `ap fmt` section.

use crate::{Error, parse, text};

/// Formats a whole Agentfile. It returns the formatted text, or every error
/// that [`parse`] found, because a file with errors isn't changed.
///
/// Only the layout changes: names become uppercase, one space follows each
/// name, trailing whitespace and extra blank lines go, and an end-of-line
/// fence moves to the next line. Arguments, comments, fence lines apart from
/// their trailing whitespace, and block content are kept as written.
pub fn format(source: &str) -> Result<String, Vec<Error>> {
    parse(source)?;
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    // Whether a blank line comes before the next line that isn't blank.
    let mut blank = false;
    let mut lines = source.split('\n');
    while let Some(line) = lines.next() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push('\n');
            blank = false;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            push_line(&mut out, line);
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        let (word, rest) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let rest = rest.trim_start();
        let name = word.to_ascii_uppercase();
        out += indent;
        out += &name;
        let takes_text = name == "SYSTEM" || name == "PROMPT";
        let fence = if takes_text && rest.is_empty() {
            // `parse` checked that the opening fence is the next line.
            let opening = lines.next().unwrap_or_default().trim_end();
            out.push('\n');
            out += opening;
            text::opening_fence(opening)
        } else if let Some(fence) = text::opening_fence(rest).filter(|_| takes_text) {
            out.push('\n');
            out += rest;
            Some(fence)
        } else {
            if !rest.is_empty() {
                out.push(' ');
                out += rest;
            }
            None
        };
        out.push('\n');
        if let Some(fence) = fence {
            for content in lines.by_ref() {
                if text::closes(content, fence) {
                    push_line(&mut out, content.trim_end());
                    break;
                }
                push_line(&mut out, content);
            }
        }
    }
    Ok(out)
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}
