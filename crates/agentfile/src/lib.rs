//! Parses an Agentfile, as SPEC.md defines it.
//!
//! So far this covers comments, instruction names, and arguments. It doesn't
//! yet check which instructions exist or read fenced blocks.

use std::fmt;

/// These take text, which doesn't split into arguments.
const TEXT_INSTRUCTIONS: [&str; 3] = ["SYSTEM", "PROMPT", "ENV"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    pub line: usize,
    /// In uppercase, whatever case the file used.
    pub name: String,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

pub fn parse(source: &str) -> Result<Vec<Instruction>, Error> {
    let mut instructions = Vec::new();
    for (index, text) in source.lines().enumerate() {
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let line = index + 1;
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let name = word.to_ascii_uppercase();
        let rest = rest.trim_start();
        let arguments = if TEXT_INSTRUCTIONS.contains(&name.as_str()) {
            vec![rest.to_string()]
        } else {
            split_arguments(rest).map_err(|message| Error { line, message })?
        };
        instructions.push(Instruction {
            line,
            name,
            arguments,
        });
    }
    Ok(instructions)
}

fn split_arguments(text: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (value, after) = read_argument(rest)?;
        if rest.starts_with("--") {
            return Err(format!(
                "an Agentfile has no flags, but found `{value}`. Quote it to use it as a value"
            ));
        }
        arguments.push(value);
        rest = after.trim_start();
    }
    Ok(arguments)
}

/// Reads one argument from the start of `text`, and returns what follows it.
fn read_argument(text: &str) -> Result<(String, &str), String> {
    let mut value = String::new();
    let mut quoted = false;
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => match chars.clone().next() {
                Some((_, escaped @ ('"' | '\\'))) => {
                    value.push(escaped);
                    chars.next();
                }
                _ => value.push(c),
            },
            c if c.is_whitespace() && !quoted => return Ok((value, &text[at..])),
            c => value.push(c),
        }
    }
    if quoted {
        return Err("a quote is never closed".to_string());
    }
    Ok((value, ""))
}
