//! Checks the arguments of `ENV`, `SECRET`, `SEAL`, and `AUTH`, and reads
//! them into typed [`Secrets`].

use std::collections::HashMap;

use crate::settings::{agent, word};
use crate::text::one_quoted_string;
use crate::{Error, Harness, Instruction, read_argument};

/// One `ENV` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    pub key: String,
    /// The value in order, with its variables not yet expanded. An empty
    /// value has no parts.
    pub value: Vec<Part>,
}

/// A piece of an `ENV` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// Text, with each `\$` already turned into `$`.
    Literal(String),
    /// `$NAME`, `${NAME}`, or `${NAME:-default}`, read from the host at run
    /// time.
    Variable {
        name: String,
        default: Option<String>,
    },
}

/// One `SECRET` or `SEAL` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    /// The bound hosts: the `@<host>` list, or the defaults for a
    /// well-known name when the line has none.
    pub hosts: Vec<String>,
    pub visible: bool,
}

/// How the harness signs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    Login,
    Key,
    Token,
}

/// The environment, secrets, seals, and sign-in of an Agentfile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Secrets {
    pub env: Vec<Env>,
    pub secrets: Vec<Secret>,
    pub seals: Vec<Secret>,
    /// `None` when there is no `AUTH` line.
    pub auth: Option<Auth>,
}

impl Secrets {
    /// Reads these values from the instructions that [`crate::parse`]
    /// returned. It skips the other instructions, and returns every error it
    /// finds, in line order.
    pub fn read(instructions: &[Instruction]) -> Result<Secrets, Vec<Error>> {
        let mut errors = Vec::new();
        let mut items = Vec::new();
        for instruction in instructions {
            match item(instruction) {
                None => {}
                Some(Err(message)) => errors.push(Error {
                    line: instruction.line,
                    message,
                }),
                Some(Ok(item)) => items.push((instruction.line, item)),
            }
        }
        errors.extend(together(instructions, &items));
        if !errors.is_empty() {
            errors.sort_by_key(|error| error.line);
            return Err(errors);
        }
        let mut secrets = Secrets::default();
        for (_, item) in items {
            match item {
                Item::Env(env) => secrets.env.push(env),
                Item::Secret(secret) => secrets.secrets.push(secret),
                Item::Seal(seal) => secrets.seals.push(seal),
                Item::Auth(auth) => secrets.auth = Some(auth),
            }
        }
        Ok(secrets)
    }
}

enum Item {
    Env(Env),
    Secret(Secret),
    Seal(Secret),
    Auth(Auth),
}

pub(crate) fn check(instruction: &Instruction) -> Result<(), String> {
    match item(instruction) {
        Some(Err(message)) => Err(message),
        _ => Ok(()),
    }
}

/// The errors between instructions, such as a key set twice. It skips the
/// instructions that have errors of their own.
pub(crate) fn check_together(instructions: &[Instruction]) -> Vec<Error> {
    let items: Vec<(usize, Item)> = instructions
        .iter()
        .filter_map(|instruction| match item(instruction) {
            Some(Ok(item)) => Some((instruction.line, item)),
            _ => None,
        })
        .collect();
    together(instructions, &items)
}

fn together(instructions: &[Instruction], items: &[(usize, Item)]) -> Vec<Error> {
    let mut errors = Vec::new();
    // The line of the first `ENV`, and of the first `SECRET` or `SEAL`, that
    // names each key.
    let mut env: HashMap<&str, usize> = HashMap::new();
    let mut held: HashMap<&str, (usize, &str)> = HashMap::new();
    let harness = instructions
        .iter()
        .find(|instruction| instruction.name == "AGENT")
        .and_then(|instruction| instruction.arguments.first())
        .and_then(|value| agent(value).ok())
        .map(|agent| agent.harness);
    for (line, item) in items {
        let mut fail = |message: String| {
            errors.push(Error {
                line: *line,
                message,
            })
        };
        match item {
            Item::Env(Env { key, .. }) => {
                if let Some(first) = env.get(key.as_str()) {
                    fail(format!(
                        "`ENV` sets `{key}` twice, and line {first} already sets it"
                    ));
                } else if let Some((first, name)) = held.get(key.as_str()) {
                    fail(format!(
                        "`{key}` is a `{name}` on line {first}, so `ENV` can't set it"
                    ));
                }
                env.entry(key).or_insert(*line);
            }
            Item::Secret(Secret { name: key, .. }) | Item::Seal(Secret { name: key, .. }) => {
                let name = if matches!(item, Item::Secret(_)) {
                    "SECRET"
                } else {
                    "SEAL"
                };
                if let Some(first) = env.get(key.as_str()) {
                    fail(format!(
                        "`ENV` sets `{key}` on line {first}, so it can't be a `{name}`"
                    ));
                } else if let Some((first, other)) = held.get(key.as_str())
                    && *other != name
                {
                    fail(format!(
                        "`{key}` is a `{other}` on line {first}, so it can't be a `{name}` too"
                    ));
                }
                held.entry(key).or_insert((*line, name));
            }
            Item::Auth(Auth::Token)
                if harness.is_some_and(|harness| harness != Harness::Claude) =>
            {
                fail(
                    "`AUTH` `token` works only with Claude Code, so it needs `AGENT claude`"
                        .to_string(),
                );
            }
            Item::Auth(_) => {}
        }
    }
    errors
}

/// Reads one instruction, or returns `None` for one this module doesn't
/// read.
fn item(instruction: &Instruction) -> Option<Result<Item, String>> {
    let arguments = instruction.arguments.as_slice();
    Some(match instruction.name.as_str() {
        "ENV" => env(arguments.first().map_or("", String::as_str)).map(Item::Env),
        "SECRET" => secret("SECRET", arguments).map(Item::Secret),
        "SEAL" => secret("SEAL", arguments).map(Item::Seal),
        "AUTH" => auth(arguments).map(Item::Auth),
        _ => return None,
    })
}

/// Reads `<key> <value>`, where `rest` is the rest of the line.
fn env(rest: &str) -> Result<Env, String> {
    if rest.is_empty() {
        return Err("`ENV` needs a key and a value".to_string());
    }
    let (key, after) = read_argument(rest)?;
    if key.is_empty() {
        return Err("`ENV` needs a key before its value".to_string());
    }
    if key.contains('=') {
        return Err(format!(
            "an `ENV` key can't hold `=`, but found `{key}`. Put a space between the key and the value"
        ));
    }
    let text = after.trim();
    if text.is_empty() {
        return Err(format!(
            "`ENV` needs a value after `{key}`. Write `\"\"` for an empty one"
        ));
    }
    let text = one_quoted_string(text).unwrap_or_else(|| text.to_string());
    Ok(Env {
        key,
        value: parts(&text)?,
    })
}

/// Splits an `ENV` value into its literal text and its variables.
fn parts(value: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = value;
    let push = |parts: &mut Vec<Part>, text: &mut String, part| {
        if !text.is_empty() {
            parts.push(Part::Literal(std::mem::take(text)));
        }
        parts.push(part);
    };
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("\\$") {
            text.push('$');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err(format!("`${{` is never closed in `{value}`"));
            };
            let inner = &after[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((name, default)) => (name, Some(default_of(inner, default)?)),
                None => (inner, None),
            };
            if name_length(name) != name.len() || name.is_empty() {
                return Err(format!(
                    "`${{{inner}}}` needs a variable name, optionally followed by `:-` and a default"
                ));
            }
            let name = name.to_string();
            push(&mut parts, &mut text, Part::Variable { name, default });
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('$')
            && let length @ 1.. = name_length(after)
        {
            let name = after[..length].to_string();
            push(
                &mut parts,
                &mut text,
                Part::Variable {
                    name,
                    default: None,
                },
            );
            rest = &after[length..];
        } else {
            text.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !text.is_empty() {
        parts.push(Part::Literal(text));
    }
    Ok(parts)
}

/// The default in `${<inner>}`, which is literal apart from `\$`.
fn default_of(inner: &str, default: &str) -> Result<String, String> {
    let escaped = default.replace("\\$", "");
    if escaped.contains('$') {
        return Err(format!(
            "the default in `${{{inner}}}` can't use a variable. Write `\\$` for a dollar sign"
        ));
    }
    Ok(default.replace("\\$", "$"))
}

/// The length of the variable name at the start of `text`: a letter or `_`,
/// then letters, digits, and `_`. Zero when `text` starts with no name.
fn name_length(text: &str) -> usize {
    let mut chars = text.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return 0;
    }
    1 + chars
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .count()
}

/// Reads `<name> [@<host>...] [visible]` for `SECRET` or `SEAL`.
fn secret(instruction: &str, arguments: &[String]) -> Result<Secret, String> {
    let Some((name, rest)) = arguments.split_first() else {
        return Err(format!("`{instruction}` needs a name"));
    };
    if name.is_empty() || name.starts_with('@') || name == "visible" {
        return Err(format!(
            "`{instruction}` needs a name first, but found `{name}`"
        ));
    }
    if name.contains('=') {
        return Err(format!(
            "a `{instruction}` name can't hold `=`, but found `{name}`"
        ));
    }
    let (visible, listed) = match rest.split_last() {
        Some((last, before)) if last == "visible" => (true, before),
        _ => (false, rest),
    };
    let mut hosts = Vec::new();
    for argument in listed {
        if argument == "visible" {
            return Err("`visible` must be the last argument".to_string());
        }
        let Some(host) = argument.strip_prefix('@') else {
            return Err(format!(
                "`{instruction}` takes `@<host>` after its name, and then an optional `visible`, but found `{argument}`"
            ));
        };
        let is_host = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !is_host {
            return Err(format!("`{argument}` isn't `@` and a host name"));
        }
        hosts.push(host.to_string());
    }
    if hosts.is_empty() {
        hosts = default_hosts(name)
            .iter()
            .map(|host| host.to_string())
            .collect();
    }
    if hosts.is_empty() && !visible {
        return Err(format!(
            "`{name}` is hidden, so `{instruction}` needs `@<host>` for the hosts it is sent to, or `visible`"
        ));
    }
    Ok(Secret {
        name: name.clone(),
        hosts,
        visible,
    })
}

/// The adapter defaults for well-known names, from the table under SECRET in
/// SPEC.md.
fn default_hosts(name: &str) -> &'static [&'static str] {
    match name {
        "ANTHROPIC_API_KEY" => &["api.anthropic.com"],
        "OPENAI_API_KEY" | "CODEX_API_KEY" => &["api.openai.com"],
        "GITHUB_TOKEN" | "GH_TOKEN" => &["github.com", "api.github.com"],
        _ => &[],
    }
}

fn auth(arguments: &[String]) -> Result<Auth, String> {
    match arguments {
        [value] => word(
            "AUTH",
            value,
            &[
                ("login", Auth::Login),
                ("key", Auth::Key),
                ("token", Auth::Token),
            ],
        ),
        [] => Err("`AUTH` needs `login`, `key`, or `token`".to_string()),
        more => Err(format!(
            "`AUTH` takes one argument, but found {}",
            more.len()
        )),
    }
}
