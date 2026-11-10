//! Checks the arguments of `AGENT`, `OS`, `MODEL`, `EFFORT`, `MODE`, and
//! `COMPRESSION`, and reads them into typed [`Settings`].

use crate::{Error, Instruction};

/// What `AGENT` names: the harness and its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub harness: Harness,
    /// An exact version or a channel, as written. `latest` when the line
    /// names none.
    pub version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

/// The OS tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Os {
    Minimal,
    Standard,
    #[default]
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    Read,
    Ask,
    Edit,
    #[default]
    Auto,
    Full,
}

/// A zstd level: `1` to `22`, or a fast mode from `-1` to `-7`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compression(i8);

impl Compression {
    pub fn level(self) -> i8 {
        self.0
    }
}

impl Default for Compression {
    fn default() -> Self {
        Compression(3)
    }
}

/// The settings of an Agentfile, with the defaults filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub agent: Agent,
    pub os: Os,
    /// `None` leaves the harness's default model.
    pub model: Option<String>,
    /// `None` leaves the harness's default effort.
    pub effort: Option<Effort>,
    pub mode: Mode,
    pub compression: Compression,
}

impl Settings {
    /// Reads the settings from the instructions that [`crate::parse`]
    /// returned. It skips the other instructions, and returns every error it
    /// finds, in line order.
    pub fn read(instructions: &[Instruction]) -> Result<Settings, Vec<Error>> {
        let mut errors = Vec::new();
        let mut agent = None;
        let mut os = Os::default();
        let mut model = None;
        let mut effort = None;
        let mut mode = Mode::default();
        let mut compression = Compression::default();
        for instruction in instructions {
            match setting(instruction) {
                None => {}
                Some(Err(message)) => errors.push(Error {
                    line: instruction.line,
                    message,
                }),
                Some(Ok(Setting::Agent(value))) => agent = Some(value),
                Some(Ok(Setting::Os(value))) => os = value,
                Some(Ok(Setting::Model(value))) => model = Some(value),
                Some(Ok(Setting::Effort(value))) => effort = Some(value),
                Some(Ok(Setting::Mode(value))) => mode = value,
                Some(Ok(Setting::Compression(value))) => compression = value,
            }
        }
        if !instructions
            .iter()
            .any(|instruction| instruction.name == "AGENT")
        {
            errors.push(Error {
                line: instructions.first().map_or(1, |first| first.line),
                message: "an Agentfile must start with `AGENT`".to_string(),
            });
        }
        match agent {
            Some(agent) if errors.is_empty() => Ok(Settings {
                agent,
                os,
                model,
                effort,
                mode,
                compression,
            }),
            _ => {
                errors.sort_by_key(|error| error.line);
                Err(errors)
            }
        }
    }
}

enum Setting {
    Agent(Agent),
    Os(Os),
    Model(String),
    Effort(Effort),
    Mode(Mode),
    Compression(Compression),
}

pub(crate) fn check(instruction: &Instruction) -> Result<(), String> {
    match setting(instruction) {
        Some(Err(message)) => Err(message),
        _ => Ok(()),
    }
}

/// Reads one setting, or returns `None` for an instruction that isn't one.
fn setting(instruction: &Instruction) -> Option<Result<Setting, String>> {
    let name = instruction.name.as_str();
    let read: fn(&str) -> Result<Setting, String> = match name {
        "AGENT" => |value| agent(value).map(Setting::Agent),
        "OS" => |value| {
            word(
                "OS",
                value,
                &[
                    ("minimal", Os::Minimal),
                    ("standard", Os::Standard),
                    ("desktop", Os::Desktop),
                ],
            )
            .map(Setting::Os)
        },
        "MODEL" => |value| match value {
            "" => Err("`MODEL` needs a model name".to_string()),
            _ => Ok(Setting::Model(value.to_string())),
        },
        "EFFORT" => |value| {
            word(
                "EFFORT",
                value,
                &[
                    ("low", Effort::Low),
                    ("medium", Effort::Medium),
                    ("high", Effort::High),
                    ("xhigh", Effort::Xhigh),
                    ("max", Effort::Max),
                ],
            )
            .map(Setting::Effort)
        },
        "MODE" => |value| {
            word(
                "MODE",
                value,
                &[
                    ("read", Mode::Read),
                    ("ask", Mode::Ask),
                    ("edit", Mode::Edit),
                    ("auto", Mode::Auto),
                    ("full", Mode::Full),
                ],
            )
            .map(Setting::Mode)
        },
        "COMPRESSION" => |value| compression(value).map(Setting::Compression),
        _ => return None,
    };
    Some(match instruction.arguments.as_slice() {
        [value] => read(value),
        [] => Err(format!("`{name}` needs an argument")),
        more => Err(format!(
            "`{name}` takes one argument, but found {}",
            more.len()
        )),
    })
}

/// Reads `<harness>[:<version>]`.
fn agent(value: &str) -> Result<Agent, String> {
    let (harness, version) = value.split_once(':').unwrap_or((value, "latest"));
    let harness = word(
        "AGENT",
        harness,
        &[("claude", Harness::Claude), ("codex", Harness::Codex)],
    )?;
    if version.is_empty() {
        return Err(format!(
            "`AGENT` needs a version after `:`, such as `{value}latest`"
        ));
    }
    Ok(Agent {
        harness,
        version: version.to_string(),
    })
}

/// Matches `value` exactly against the words that `name` takes.
fn word<T: Copy>(name: &str, value: &str, words: &[(&str, T)]) -> Result<T, String> {
    if let Some((_, found)) = words.iter().find(|(word, _)| *word == value) {
        return Ok(*found);
    }
    let quoted: Vec<String> = words.iter().map(|(word, _)| format!("`{word}`")).collect();
    let list = match quoted.as_slice() {
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
        [] => String::new(),
    };
    Err(format!("`{name}` takes {list}, but found `{value}`"))
}

/// Reads a level written as a plain integer, such as `19` or `-3`.
fn compression(value: &str) -> Result<Compression, String> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let plain = (1..=2).contains(&digits.len())
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && !digits.starts_with('0');
    plain
        .then(|| value.parse::<i8>().ok())
        .flatten()
        .filter(|level| (1..=22).contains(level) || (-7..=-1).contains(level))
        .map(Compression)
        .ok_or_else(|| {
            format!(
                "`COMPRESSION` takes a level from `1` to `22`, or from `-1` to `-7`, but found `{value}`"
            )
        })
}
