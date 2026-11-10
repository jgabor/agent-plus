//! Parses an Agentfile, as SPEC.md defines it.
//!
//! So far this covers the line rules, instruction names, and arguments. It
//! doesn't yet read fenced blocks or apply the rules for text values, so
//! `SYSTEM`, `PROMPT`, and `ENV` keep their raw text. Each group of
//! instructions checks its own arguments in its own module.

mod files;
mod network;
mod secrets;
mod settings;
mod sources;
mod text;

use std::collections::HashMap;
use std::fmt;

/// These take text, which doesn't split into arguments.
const TEXT_INSTRUCTIONS: [&str; 3] = ["SYSTEM", "PROMPT", "ENV"];

/// When an instruction is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Resolved by `ap build`, and captured into the image.
    Build,
    /// Stored as a declaration, and bound each time the agent starts.
    Run,
}

/// One row of the Overview table in SPEC.md.
#[derive(Clone, Copy, Debug)]
pub struct Definition {
    pub name: &'static str,
    pub phase: Phase,
    pub repeatable: bool,
    check: fn(&Instruction) -> Result<(), String>,
}

const fn define(
    name: &'static str,
    phase: Phase,
    repeatable: bool,
    check: fn(&Instruction) -> Result<(), String>,
) -> Definition {
    Definition {
        name,
        phase,
        repeatable,
        check,
    }
}

/// Every instruction, in the order of SPEC.md's Overview table.
pub const INSTRUCTIONS: [Definition; 24] = {
    use Phase::{Build, Run};
    [
        define("AGENT", Build, false, settings::check),
        define("OS", Build, false, settings::check),
        define("SYSTEM", Build, true, text::check),
        define("PROMPT", Build, false, text::check),
        define("MODEL", Build, false, settings::check),
        define("EFFORT", Build, false, settings::check),
        define("MODE", Build, false, settings::check),
        define("ADD", Build, true, sources::check),
        define("REPO", Build, true, sources::check),
        define("PLUGIN", Build, true, sources::check),
        define("SKILL", Build, true, sources::check),
        define("MCP", Build, true, sources::check),
        define("COMPRESSION", Build, false, settings::check),
        define("ENV", Run, true, secrets::check),
        define("SECRET", Run, true, secrets::check),
        define("SEAL", Build, true, secrets::check),
        define("AUTH", Run, false, secrets::check),
        define("SHARE", Run, true, files::check),
        define("WORKDIR", Run, false, files::check),
        define("PERSIST", Run, true, files::check),
        define("PERMISSIONS", Run, true, files::check),
        define("PORT", Run, true, files::check),
        define("NETWORK", Run, true, network::check),
        define("POLICY", Run, true, network::check),
    ]
};

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

/// Reads a whole Agentfile. It returns every instruction, or every error it
/// found, in line order.
pub fn parse(source: &str) -> Result<Vec<Instruction>, Vec<Error>> {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut instructions = Vec::new();
    let mut errors = Vec::new();
    // The line where each non-repeatable instruction first appears.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut first_line = None;
    for (index, text) in source.split('\n').enumerate() {
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let line = index + 1;
        first_line.get_or_insert(line);
        let mut fail = |message: String| errors.push(Error { line, message });
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let name = word.to_ascii_uppercase();
        let Some(definition) = INSTRUCTIONS.iter().find(|d| d.name == name) else {
            fail(format!("unknown instruction `{word}`"));
            continue;
        };
        if name == "AGENT" && first_line != Some(line) && !seen.contains_key("AGENT") {
            fail("`AGENT` must be the first instruction".to_string());
        }
        if !definition.repeatable {
            if let Some(first) = seen.get(definition.name) {
                fail(format!(
                    "`{}` can appear only once, and line {first} already has it",
                    definition.name
                ));
            } else {
                seen.insert(definition.name, line);
            }
        }
        let rest = rest.trim_start();
        let arguments = if TEXT_INSTRUCTIONS.contains(&name.as_str()) {
            vec![rest.to_string()]
        } else {
            match split_arguments(rest) {
                Ok(arguments) => arguments,
                Err(message) => {
                    fail(message);
                    continue;
                }
            }
        };
        let instruction = Instruction {
            line,
            name,
            arguments,
        };
        if let Err(message) = (definition.check)(&instruction) {
            fail(message);
        }
        instructions.push(instruction);
    }
    if !seen.contains_key("AGENT") {
        errors.push(Error {
            line: first_line.unwrap_or(1),
            message: "an Agentfile must start with `AGENT`".to_string(),
        });
    }
    if errors.is_empty() {
        Ok(instructions)
    } else {
        errors.sort_by_key(|error| error.line);
        Err(errors)
    }
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
