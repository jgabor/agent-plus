//! Lists what an Agentfile asks of the host, by the table under SPEC.md's
//! Grants section, and prints and reads that list as text.
//!
//! The text form is what `ap inspect --grants` prints and what
//! `--approve=<file>` reads. It uses the Agentfile's line rules and argument
//! reader, one grant per line:
//!
//! ```text
//! PATH /home/me/notes
//! PATH /home/me/project/out rw
//! VARIABLE USER
//! SECRET GITHUB_TOKEN @api.github.com @github.com
//! SEAL NPM_TOKEN @registry.npmjs.org visible
//! AUTH key
//! NETWORK -
//! NETWORK +github.com
//! POLICY -
//! POLICY +reviewer
//! ```
//!
//! In each argument, `\\` is a backslash, and `\u{<hex>}` is the character
//! with that code. Printing uses `\u{<hex>}` for control characters,
//! bidirectional controls, and whitespace other than a space, so a grant
//! shows as it is. Git credentials aren't listed yet, because nothing is
//! fetched.

use std::fmt;
use std::str::FromStr;

use crate::network::{self, Line};
use crate::secrets::{self, name_length};
use crate::{
    Auth, Destination, Error, Files, Instruction, Mcp, Network, NetworkRule, Policy, Scheme,
    Secret, Secrets, ShareMode, Sign, Source, Sources, Text, read_argument,
};

/// Where a host path from an Agentfile really is, as the caller resolved it
/// by SPEC.md's Paths section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The canonical path.
    pub path: String,
    /// Whether the canonical path is outside the build context.
    pub outside: bool,
}

/// A host path that is outside the build context, or that the agent may
/// write to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathGrant {
    /// The canonical path.
    pub path: String,
    /// Whether a `SHARE` with `rw` lets the agent change it.
    pub write: bool,
}

/// What an Agentfile asks of the host. Each list is sorted and holds no
/// duplicates, so two lists with the same grants are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grants {
    pub paths: Vec<PathGrant>,
    /// The host environment variables that `ENV` values read.
    pub variables: Vec<String>,
    pub secrets: Vec<Secret>,
    pub seals: Vec<Secret>,
    /// `AUTH key` or `AUTH token`. `AUTH login` isn't a grant.
    pub auth: Option<Auth>,
    pub network: Network,
    pub policy: Policy,
}

impl Grants {
    /// Lists the grants of the instructions that [`crate::parse`] returned.
    /// `resolve` takes a host path as written and says where it really is.
    /// Its error becomes an error on the line that names the path.
    pub fn read(
        instructions: &[Instruction],
        mut resolve: impl FnMut(&str) -> Result<Location, String>,
    ) -> Result<Grants, Vec<Error>> {
        let sources = Sources::read(instructions)?;
        let files = Files::read(instructions)?;
        let secrets = Secrets::read(instructions)?;
        let network = Network::read(instructions)?;
        let policy = Policy::read(instructions)?;
        // Each host path, as written, its line, and whether it is written to.
        let mut written: Vec<(&str, usize, bool)> = Vec::new();
        for instruction in instructions {
            if let Some(Text::Path(path)) = &instruction.text {
                written.push((path, instruction.line, false));
            }
        }
        written.extend(
            sources
                .add
                .iter()
                .map(|add| (add.source.as_str(), add.line, false)),
        );
        for (source, line) in sources
            .repo
            .iter()
            .map(|repo| (&repo.source, repo.line))
            .chain(
                sources
                    .plugin
                    .iter()
                    .map(|plugin| (&plugin.source, plugin.line)),
            )
            .chain(
                sources
                    .skill
                    .iter()
                    .map(|skill| (&skill.source, skill.line)),
            )
            .chain(sources.mcp.iter().filter_map(|mcp| match mcp {
                Mcp::Declared { line, source } => Some((source, *line)),
                Mcp::Remote { .. } => None,
            }))
        {
            if let Source::Local(path) = source {
                written.push((path, line, false));
            }
        }
        written.extend(files.share.iter().map(|share| {
            let write = share.mode == ShareMode::ReadWrite;
            (share.source.as_str(), share.line, write)
        }));
        let mut errors = Vec::new();
        let mut paths = Vec::new();
        for (text, line, write) in written {
            match resolve(text) {
                Err(message) => errors.push(Error { line, message }),
                Ok(location) if location.outside || write => paths.push(PathGrant {
                    path: location.path,
                    write,
                }),
                Ok(_) => {}
            }
        }
        if !errors.is_empty() {
            errors.sort_by_key(|error| error.line);
            return Err(errors);
        }
        let variables = secrets
            .env
            .iter()
            .flat_map(|env| &env.value)
            .filter_map(|part| match part {
                secrets::Part::Variable { name, .. } => Some(name.clone()),
                secrets::Part::Literal(_) => None,
            })
            .collect();
        let auth = secrets.auth.filter(|auth| *auth != Auth::Login);
        Ok(Grants {
            paths,
            variables,
            secrets: secrets.secrets,
            seals: secrets.seals,
            auth,
            network,
            policy,
        }
        .sorted())
    }

    /// Sorts each list, and merges what names one grant twice, keeping the
    /// widest access.
    fn sorted(mut self) -> Grants {
        self.paths.sort_by(|a, b| a.path.cmp(&b.path));
        self.paths.dedup_by(|later, kept| {
            let same = later.path == kept.path;
            kept.write |= same && later.write;
            same
        });
        self.variables.sort();
        self.variables.dedup();
        for list in [&mut self.secrets, &mut self.seals] {
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list.dedup_by(|later, kept| {
                let same = later.name == kept.name;
                if same {
                    kept.hosts.append(&mut later.hosts);
                    kept.visible |= later.visible;
                }
                same
            });
            for secret in list.iter_mut() {
                secret.hosts.sort();
                secret.hosts.dedup();
            }
        }
        self.network.rules.sort_by_cached_key(rule_text);
        self.network.rules.dedup();
        self.policy.rules.sort_by_cached_key(agent_text);
        self.policy.rules.dedup();
        self
    }
}

/// One way that an Agentfile's grants are wider than the grants you
/// approved, by the list under SPEC.md's Grants section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Widening {
    /// A host path you haven't approved, including an approved path whose
    /// canonical location changed.
    Path(PathGrant),
    /// An approved path that the agent may now write to.
    Write(String),
    /// A new host environment variable.
    Variable(String),
    /// A new secret, from `SEAL` when `sealed` is true and `SECRET`
    /// otherwise.
    Secret { secret: Secret, sealed: bool },
    /// A new bound host of an approved secret.
    Host { secret: String, host: String },
    /// An approved secret that becomes `visible`.
    Visible(String),
    /// An approved secret that moves from `SECRET` to `SEAL`.
    Sealed(String),
    /// `AUTH key` or `AUTH token`, added or changed from the other one.
    Auth(Auth),
    /// A new `NETWORK +` rule.
    Allow(NetworkRule),
    /// An approved `NETWORK -` rule that is gone.
    Unblock(NetworkRule),
    /// The network default moves from `-` to `+`.
    Open,
    /// The `POLICY` default moves from `-` to `+`, so the agent may call
    /// every agent that no rule names.
    CallAny,
    /// An agent that the agent may now call.
    Call(String),
}

impl Grants {
    /// Lists how these grants are wider than `approved`, in the order the
    /// text form prints them. It is empty when they only narrow, or are the
    /// same. Git credentials and the harness hosts aren't compared yet.
    pub fn widening(&self, approved: &Grants) -> Vec<Widening> {
        let mut wider = Vec::new();
        for path in &self.paths {
            match approved.paths.iter().find(|old| old.path == path.path) {
                None => wider.push(Widening::Path(path.clone())),
                Some(old) if path.write && !old.write => {
                    wider.push(Widening::Write(path.path.clone()));
                }
                Some(_) => {}
            }
        }
        wider.extend(
            self.variables
                .iter()
                .filter(|name| !approved.variables.contains(name))
                .map(|name| Widening::Variable(name.clone())),
        );
        for (sealed, list) in [(false, &self.secrets), (true, &self.seals)] {
            for secret in list {
                let old_secret = approved.secrets.iter().find(|old| old.name == secret.name);
                let old_seal = approved.seals.iter().find(|old| old.name == secret.name);
                let Some(old) = old_secret.or(old_seal) else {
                    wider.push(Widening::Secret {
                        secret: secret.clone(),
                        sealed,
                    });
                    continue;
                };
                if sealed && old_secret.is_some() {
                    wider.push(Widening::Sealed(secret.name.clone()));
                }
                if secret.visible && !old.visible {
                    wider.push(Widening::Visible(secret.name.clone()));
                }
                wider.extend(
                    secret
                        .hosts
                        .iter()
                        .filter(|host| !old.hosts.iter().any(|old| old.eq_ignore_ascii_case(host)))
                        .map(|host| Widening::Host {
                            secret: secret.name.clone(),
                            host: host.clone(),
                        }),
                );
            }
        }
        if let Some(auth) = self.auth
            && approved.auth != Some(auth)
        {
            wider.push(Widening::Auth(auth));
        }
        if self.network.default == Sign::Allow && approved.network.default == Sign::Deny {
            wider.push(Widening::Open);
        }
        let (now, old) = (&self.network.rules, &approved.network.rules);
        wider.extend(
            now.iter()
                .filter(|rule| rule.sign == Sign::Allow && !old.contains(rule))
                .map(|rule| Widening::Allow(rule.clone())),
        );
        wider.extend(
            old.iter()
                .filter(|rule| rule.sign == Sign::Deny && !now.contains(rule))
                .map(|rule| Widening::Unblock(rule.clone())),
        );
        // Agents that no rule names follow the default, so only the default
        // and the named agents can change.
        if self.policy.default == Sign::Allow && approved.policy.default == Sign::Deny {
            wider.push(Widening::CallAny);
        }
        let mut names: Vec<&String> = (self.policy.rules.iter())
            .chain(&approved.policy.rules)
            .map(|(_, name)| name)
            .collect();
        names.sort();
        names.dedup();
        wider.extend(
            names
                .into_iter()
                .filter(|name| self.policy.allows(name) && !approved.policy.allows(name))
                .map(|name| Widening::Call(name.clone())),
        );
        wider
    }
}

/// One line that shows what widened, with its paths, names, and hosts
/// printed as the text form prints them, so each shows as it is.
impl fmt::Display for Widening {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Widening::Path(path) if path.write => write!(f, "PATH {} rw", printed(&path.path)),
            Widening::Path(path) => write!(f, "PATH {}", printed(&path.path)),
            Widening::Write(path) => write!(f, "PATH {} rw, which was read-only", printed(path)),
            Widening::Variable(name) => write!(f, "VARIABLE {}", printed(name)),
            Widening::Secret { secret, sealed } => {
                write!(f, "{} {}", if *sealed { "SEAL" } else { "SECRET" }, printed(&secret.name))?;
                for host in &secret.hosts {
                    write!(f, " {}", printed(&format!("@{host}")))?;
                }
                if secret.visible {
                    write!(f, " visible")?;
                }
                Ok(())
            }
            Widening::Host { secret, host } => write!(
                f,
                "{} may go to {}",
                printed(secret),
                printed(&format!("@{host}"))
            ),
            Widening::Visible(name) => write!(f, "{} becomes visible", printed(name)),
            Widening::Sealed(name) => write!(f, "{} moves from SECRET to SEAL", printed(name)),
            Widening::Auth(auth) => write!(
                f,
                "AUTH {}",
                match auth {
                    Auth::Key => "key",
                    Auth::Token => "token",
                    Auth::Login => "login",
                }
            ),
            Widening::Allow(rule) => write!(f, "NETWORK {}", printed(&rule_text(rule))),
            Widening::Unblock(rule) => {
                write!(f, "NETWORK {} is gone", printed(&rule_text(rule)))
            }
            Widening::Open => write!(f, "NETWORK + replaces NETWORK -"),
            Widening::CallAny => write!(f, "POLICY + replaces POLICY -"),
            Widening::Call(name) => write!(f, "may call {}", printed(name)),
        }
    }
}

impl fmt::Display for Grants {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut line = |name: &str, arguments: &[&str]| {
            write!(f, "{name}")?;
            for argument in arguments {
                write!(f, " {}", printed(argument))?;
            }
            writeln!(f)
        };
        for path in &self.paths {
            match path.write {
                true => line("PATH", &[&path.path, "rw"])?,
                false => line("PATH", &[&path.path])?,
            }
        }
        for name in &self.variables {
            line("VARIABLE", &[name])?;
        }
        for (name, list) in [("SECRET", &self.secrets), ("SEAL", &self.seals)] {
            for secret in list {
                let hosts: Vec<String> =
                    secret.hosts.iter().map(|host| format!("@{host}")).collect();
                let mut arguments = vec![secret.name.as_str()];
                arguments.extend(hosts.iter().map(String::as_str));
                if secret.visible {
                    arguments.push("visible");
                }
                line(name, &arguments)?;
            }
        }
        match self.auth {
            Some(Auth::Key) => line("AUTH", &["key"])?,
            Some(Auth::Token) => line("AUTH", &["token"])?,
            Some(Auth::Login) | None => {}
        }
        line("NETWORK", &[sign(self.network.default)])?;
        for rule in &self.network.rules {
            line("NETWORK", &[&rule_text(rule)])?;
        }
        line("POLICY", &[sign(self.policy.default)])?;
        for rule in &self.policy.rules {
            line("POLICY", &[&agent_text(rule)])?;
        }
        Ok(())
    }
}

impl FromStr for Grants {
    type Err = Vec<Error>;

    /// Reads the text form. It returns every error it finds, in line order.
    fn from_str(text: &str) -> Result<Grants, Vec<Error>> {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut errors = Vec::new();
        let mut grants = Grants {
            paths: Vec::new(),
            variables: Vec::new(),
            secrets: Vec::new(),
            seals: Vec::new(),
            auth: None,
            network: Network {
                default: Sign::Allow,
                rules: Vec::new(),
            },
            policy: Policy {
                default: Sign::Allow,
                rules: Vec::new(),
            },
        };
        let mut network_default = None;
        let mut policy_default = None;
        let mut last = 1;
        for (index, content) in text.split('\n').enumerate() {
            let line = index + 1;
            last = line;
            let content = content.trim();
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            let (word, rest) = content
                .split_once(char::is_whitespace)
                .unwrap_or((content, ""));
            let name = word.to_ascii_uppercase();
            let read = arguments(rest).and_then(|arguments| {
                let instruction = Instruction {
                    line,
                    name,
                    arguments,
                    text: None,
                };
                grants.read_line(&instruction, &mut network_default, &mut policy_default)
            });
            if let Err(message) = read {
                errors.push(Error { line, message });
            }
        }
        for (name, default) in [("NETWORK", network_default), ("POLICY", policy_default)] {
            if default.is_none() {
                errors.push(Error {
                    line: last,
                    message: format!("the grants need a bare `{name} +` or `{name} -`"),
                });
            }
        }
        if !errors.is_empty() {
            errors.sort_by_key(|error| error.line);
            return Err(errors);
        }
        grants.network.default = network_default.unwrap_or(Sign::Allow);
        grants.policy.default = policy_default.unwrap_or(Sign::Allow);
        Ok(grants.sorted())
    }
}

impl Grants {
    /// Adds the grant on one line of the text form.
    fn read_line(
        &mut self,
        instruction: &Instruction,
        network_default: &mut Option<Sign>,
        policy_default: &mut Option<Sign>,
    ) -> Result<(), String> {
        let arguments = instruction.arguments.as_slice();
        match instruction.name.as_str() {
            "PATH" => {
                let (path, write) = match arguments {
                    [path] => (path, false),
                    [path, mode] if mode == "rw" => (path, true),
                    _ => return Err("`PATH` takes a path and an optional `rw`".to_string()),
                };
                if !path.starts_with('/') {
                    return Err(format!("`PATH` takes a canonical path, but found `{path}`"));
                }
                self.paths.push(PathGrant {
                    path: path.clone(),
                    write,
                });
            }
            "VARIABLE" => match arguments {
                [name] if !name.is_empty() && name_length(name) == name.len() => {
                    self.variables.push(name.clone());
                }
                _ => return Err("`VARIABLE` takes one variable name".to_string()),
            },
            "SECRET" => self.secrets.push(secrets::secret("SECRET", arguments)?),
            "SEAL" => self.seals.push(secrets::secret("SEAL", arguments)?),
            "AUTH" => {
                if self.auth.is_some() {
                    return Err("the grants already have an `AUTH` line".to_string());
                }
                match secrets::auth(arguments)? {
                    Auth::Login => return Err("`AUTH login` isn't a grant".to_string()),
                    auth => self.auth = Some(auth),
                }
            }
            "NETWORK" => match network::line(instruction, network::rule)? {
                Line::Bare(sign) => set_default("NETWORK", network_default, sign)?,
                Line::Rule(rule) => self.network.rules.push(rule),
            },
            "POLICY" => match network::line(instruction, network::agent_rule)? {
                Line::Bare(sign) => set_default("POLICY", policy_default, sign)?,
                Line::Rule(rule) => self.policy.rules.push(rule),
            },
            other => return Err(format!("unknown grant `{other}`")),
        }
        Ok(())
    }
}

fn set_default(name: &str, default: &mut Option<Sign>, sign: Sign) -> Result<(), String> {
    if default.is_some() {
        return Err(format!("the grants already have a bare `{name}` rule"));
    }
    *default = Some(sign);
    Ok(())
}

/// Reads the arguments on a line, and decodes the escapes in each.
pub fn arguments(text: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (value, after) = read_argument(rest)?;
        arguments.push(unescape(&value)?);
        rest = after.trim_start();
    }
    Ok(arguments)
}

fn sign(sign: Sign) -> &'static str {
    match sign {
        Sign::Allow => "+",
        Sign::Deny => "-",
    }
}

/// A `NETWORK` rule as the Agentfile writes it, such as `+github.com:22`.
fn rule_text(rule: &NetworkRule) -> String {
    let port = |port: &Option<u16>| port.map(|port| format!(":{port}")).unwrap_or_default();
    let destination = match &rule.destination {
        Destination::Host { name, port: p } => format!("{name}{}", port(p)),
        Destination::Address {
            network,
            prefix,
            port: p,
        } => {
            let (address, full) = match network {
                std::net::IpAddr::V4(v4) => (v4.to_string(), 32),
                std::net::IpAddr::V6(v6) => (format!("[{v6}]"), 128),
            };
            let prefix = if *prefix == full {
                String::new()
            } else {
                format!("/{prefix}")
            };
            format!("{address}{prefix}{}", port(p))
        }
        Destination::Url {
            scheme,
            host,
            port: p,
            path,
        } => {
            let scheme = match scheme {
                Scheme::Http => "http",
                Scheme::Https => "https",
            };
            format!("{scheme}://{host}{}{path}", port(p))
        }
    };
    format!("{}{destination}", sign(rule.sign))
}

/// A `POLICY` rule as the Agentfile writes it, such as `+reviewer`.
fn agent_text((rule, name): &(Sign, String)) -> String {
    format!("{}{name}", sign(*rule))
}

/// Whether printing shows `c` as an escape: control characters,
/// bidirectional controls, and whitespace other than a space.
fn hidden(c: char) -> bool {
    c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// One argument as printed: escaped, and quoted when the argument reader
/// needs it.
pub fn printed(value: &str) -> String {
    let mut escaped = String::new();
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            c if hidden(c) => escaped.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => escaped.push(c),
        }
    }
    if !escaped.is_empty() && !escaped.contains([' ', '"']) {
        return escaped;
    }
    let inner = escaped.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{inner}\"")
}

/// Decodes `\\` and `\u{<hex>}` in one argument.
fn unescape(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(after) = after.strip_prefix('\\') {
            out.push('\\');
            rest = after;
            continue;
        }
        let code = after
            .strip_prefix("u{")
            .and_then(|inner| inner.split_once('}'))
            .filter(|(hex, _)| {
                (1..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .and_then(|(hex, after)| {
                let c = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)?;
                Some((c, after))
            });
        let Some((c, after)) = code else {
            return Err(format!(
                "a backslash starts `\\\\` or `\\u{{<hex>}}`, but found `{value}`"
            ));
        };
        out.push(c);
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
}
