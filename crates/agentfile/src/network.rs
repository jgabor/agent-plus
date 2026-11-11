//! Checks the arguments of `NETWORK` and `POLICY`, and reads them into a
//! typed [`Network`] and [`Policy`].
//!
//! A rule parses into its sign and its destination. Matching a connection
//! against the rules belongs to the network proxy.

use std::net::IpAddr;

use crate::{Error, Instruction};

/// Whether a rule allows or denies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    Allow,
    Deny,
}

/// The scheme of a URL rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// What a `NETWORK` rule names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// A host and its subdomains, such as `github.com:22`. The name is in
    /// lowercase.
    Host { name: String, port: Option<u16> },
    /// An address, or a CIDR block, such as `10.1.0.0/16`. A single address
    /// has the full prefix: 32 for IPv4, and 128 for IPv6.
    Address {
        network: IpAddr,
        prefix: u8,
        port: Option<u16>,
    },
    /// A URL prefix, such as `https://github.com/jgabor`. The host is in
    /// lowercase, and the path starts with `/`.
    Url {
        scheme: Scheme,
        host: String,
        port: Option<u16>,
        path: String,
    },
}

/// One `NETWORK` line that isn't bare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkRule {
    pub sign: Sign,
    pub destination: Destination,
}

/// The `NETWORK` rules of an Agentfile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// What a bare rule set, or `Allow` without one.
    pub default: Sign,
    /// The rules in file order. Order never changes what they decide.
    pub rules: Vec<NetworkRule>,
}

impl Network {
    /// Reads the `NETWORK` lines of the instructions that [`crate::parse`]
    /// returned, and returns every error it finds, in line order.
    pub fn read(instructions: &[Instruction]) -> Result<Network, Vec<Error>> {
        let (bare, rules) = read_lines(instructions, "NETWORK", rule)?;
        Ok(Network {
            default: bare.unwrap_or(Sign::Allow),
            rules,
        })
    }

    /// Whether these rules deny every request to `host`, by the evaluation
    /// under NETWORK in SPEC.md. An address is judged by IP rules. A name
    /// is judged by host and URL rules alone, since the address it resolves
    /// to isn't known until a run.
    pub(crate) fn denies(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let address = host.parse::<IpAddr>().ok();
        // Each port and path a rule names, and one that none names, so that
        // every way a request can match the rules gets tried.
        let mut ports = vec![80, 443];
        let mut paths = Vec::new();
        for rule in &self.rules {
            match &rule.destination {
                Destination::Host { port, .. } | Destination::Address { port, .. } => {
                    ports.extend(*port)
                }
                Destination::Url { port, path, .. } => {
                    ports.extend(*port);
                    paths.push(path.clone());
                }
            }
        }
        let longest = paths.iter().map(String::len).max().unwrap_or(0);
        paths.push(format!("/{}", "x".repeat(longest)));
        ports.extend((1..=u16::MAX).find(|port| !ports.contains(port)));
        for scheme in [Scheme::Https, Scheme::Http] {
            for &port in &ports {
                for path in &paths {
                    let decision = match address {
                        Some(address) => self.address_decision(address, port),
                        None => self.name_decision(&host, scheme, port, path),
                    };
                    if decision.unwrap_or(self.default) == Sign::Allow {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The sign of the most specific URL or host rule that matches a
    /// request to `host`, if any matches.
    fn name_decision(&self, host: &str, scheme: Scheme, port: u16, path: &str) -> Option<Sign> {
        most_specific(self.rules.iter().filter_map(|rule| {
            let key = match &rule.destination {
                Destination::Host { name, port: only } => {
                    let matches = host == name
                        || host
                            .strip_suffix(name.as_str())
                            .is_some_and(|start| start.ends_with('.'));
                    (matches && only.is_none_or(|only| only == port))
                        .then(|| (false, name.split('.').count(), only.is_some()))
                }
                Destination::Url {
                    scheme: only,
                    host: name,
                    port: only_port,
                    path: prefix,
                } => {
                    let default = match only {
                        Scheme::Https => 443,
                        Scheme::Http => 80,
                    };
                    let under = path == prefix
                        || path
                            .strip_prefix(prefix.as_str())
                            .is_some_and(|rest| prefix.ends_with('/') || rest.starts_with('/'));
                    (host == name
                        && *only == scheme
                        && only_port.unwrap_or(default) == port
                        && under)
                        .then(|| (true, prefix.len(), only_port.is_some()))
                }
                Destination::Address { .. } => None,
            };
            key.map(|key| (key, rule.sign))
        }))
    }

    /// The sign of the IP rule with the longest prefix that matches a
    /// connection to `address`, if any matches.
    fn address_decision(&self, address: IpAddr, port: u16) -> Option<Sign> {
        most_specific(self.rules.iter().filter_map(|rule| match rule.destination {
            Destination::Address {
                network,
                prefix,
                port: only,
            } if masked(address, prefix) == Some(network)
                && only.is_none_or(|only| only == port) =>
            {
                Some(((prefix, only.is_some()), rule.sign))
            }
            _ => None,
        }))
    }
}

/// The sign of the matches with the highest key. Equally specific matches
/// with opposite signs deny.
fn most_specific<K: Ord>(matches: impl Iterator<Item = (K, Sign)>) -> Option<Sign> {
    let mut best: Option<(K, Sign)> = None;
    for (key, sign) in matches {
        match &mut best {
            Some((top, top_sign)) if key == *top => {
                if sign == Sign::Deny {
                    *top_sign = Sign::Deny;
                }
            }
            Some((top, _)) if key < *top => {}
            _ => best = Some((key, sign)),
        }
    }
    best.map(|(_, sign)| sign)
}

/// `address` with the bits past `prefix` cleared, or `None` when the prefix
/// is longer than the address.
fn masked(address: IpAddr, prefix: u8) -> Option<IpAddr> {
    match address {
        IpAddr::V4(address) if prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            Some(IpAddr::V4((u32::from(address) & mask).into()))
        }
        IpAddr::V6(address) if prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            Some(IpAddr::V6((u128::from(address) & mask).into()))
        }
        _ => None,
    }
}

/// The `POLICY` rules of an Agentfile: which agents this one may call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// What a bare rule set. Without one, `Deny` if any rule allows an
    /// agent, and `Allow` otherwise.
    pub default: Sign,
    /// Each named agent and its sign, in file order.
    pub rules: Vec<(Sign, String)>,
}

impl Policy {
    /// Reads the `POLICY` lines of the instructions that [`crate::parse`]
    /// returned, and returns every error it finds, in line order.
    pub fn read(instructions: &[Instruction]) -> Result<Policy, Vec<Error>> {
        let (bare, rules) = read_lines(instructions, "POLICY", agent_rule)?;
        let default = bare.unwrap_or(if rules.iter().any(|(sign, _)| *sign == Sign::Allow) {
            Sign::Deny
        } else {
            Sign::Allow
        });
        Ok(Policy { default, rules })
    }

    /// Whether this agent may call the agent named `agent`. Rules with
    /// opposite signs for one name deny it.
    pub fn allows(&self, agent: &str) -> bool {
        let mut signs = self
            .rules
            .iter()
            .filter(|(_, name)| name == agent)
            .map(|(sign, _)| *sign);
        match signs.next() {
            None => self.default == Sign::Allow,
            Some(first) => first == Sign::Allow && signs.all(|sign| sign == Sign::Allow),
        }
    }
}

/// One line of `NETWORK` or `POLICY`: a bare rule, or a rule.
pub(crate) enum Line<T> {
    Bare(Sign),
    Rule(T),
}

/// Reads every line named `name`. It returns the bare rule, if any, and the
/// other rules, or every error, including bare rules that disagree.
fn read_lines<T>(
    instructions: &[Instruction],
    name: &str,
    read: fn(Sign, &str) -> Result<T, String>,
) -> Result<(Option<Sign>, Vec<T>), Vec<Error>> {
    let mut errors = disagreements(instructions, name);
    let mut bare = None;
    let mut rules = Vec::new();
    for instruction in instructions.iter().filter(|i| i.name == name) {
        match line(instruction, read) {
            Err(message) => errors.push(Error {
                line: instruction.line,
                message,
            }),
            Ok(Line::Rule(rule)) => rules.push(rule),
            Ok(Line::Bare(sign)) => bare = Some(sign),
        }
    }
    if errors.is_empty() {
        Ok((bare, rules))
    } else {
        errors.sort_by_key(|error| error.line);
        Err(errors)
    }
}

pub(crate) fn check(instruction: &Instruction) -> Result<(), String> {
    match instruction.name.as_str() {
        "NETWORK" => line(instruction, rule).map(drop),
        _ => line(instruction, agent_rule).map(drop),
    }
}

/// The errors between instructions: bare rules that disagree.
pub(crate) fn check_together(instructions: &[Instruction]) -> Vec<Error> {
    let mut errors = disagreements(instructions, "NETWORK");
    errors.extend(disagreements(instructions, "POLICY"));
    errors
}

/// An error on each bare `name` rule that disagrees with the first one.
fn disagreements(instructions: &[Instruction], name: &str) -> Vec<Error> {
    let mut errors = Vec::new();
    let mut first: Option<(usize, Sign)> = None;
    for instruction in instructions.iter().filter(|i| i.name == name) {
        let Ok(Line::Bare(sign)) = line(instruction, |_, _| Ok(())) else {
            continue;
        };
        match first {
            Some((line, other)) if other != sign => errors.push(Error {
                line: instruction.line,
                message: format!("this bare `{name}` rule disagrees with the one on line {line}"),
            }),
            Some(_) => {}
            None => first = Some((instruction.line, sign)),
        }
    }
    errors
}

/// Reads one line, with `read` for the part after the sign of a rule.
pub(crate) fn line<T>(
    instruction: &Instruction,
    read: fn(Sign, &str) -> Result<T, String>,
) -> Result<Line<T>, String> {
    let name = instruction.name.as_str();
    let [value] = instruction.arguments.as_slice() else {
        return Err(format!(
            "`{name}` takes one rule: `+` or `-`, or a rule that starts with one"
        ));
    };
    let bare = match value.as_str() {
        "+" | "true" | "yes" | "1" => Some(Sign::Allow),
        "-" | "false" | "no" | "0" => Some(Sign::Deny),
        _ => None,
    };
    if let Some(sign) = bare {
        return Ok(Line::Bare(sign));
    }
    let (sign, rest) = if let Some(rest) = value.strip_prefix('+') {
        (Sign::Allow, rest)
    } else if let Some(rest) = value.strip_prefix('-') {
        (Sign::Deny, rest)
    } else {
        return Err(format!(
            "a `{name}` rule starts with `+` or `-`, but found `{value}`"
        ));
    };
    read(sign, rest).map(Line::Rule)
}

pub(crate) fn rule(sign: Sign, text: &str) -> Result<NetworkRule, String> {
    let destination = if let Some((scheme, rest)) = text.split_once("://") {
        url(scheme, rest).map_err(|reason| format!("`{text}` isn't a URL rule: {reason}"))?
    } else {
        destination(text)?
    };
    Ok(NetworkRule { sign, destination })
}

/// Reads a host or IP rule, with its optional `:<port>`.
fn destination(text: &str) -> Result<Destination, String> {
    if let Some(rest) = text.strip_prefix('[') {
        let Some((inside, after)) = rest.split_once(']') else {
            return Err(format!("`{text}` opens a bracket and never closes it"));
        };
        let Ok(IpAddr::V6(v6)) = inside.parse::<IpAddr>() else {
            return Err(format!(
                "brackets hold an IPv6 address, but found `[{inside}]`"
            ));
        };
        let (prefix, port) = match after.split_once(':') {
            Some((prefix, port)) => (prefix, Some(port_of(port)?)),
            None => (after, None),
        };
        let prefix = match prefix.strip_prefix('/') {
            Some(digits) => prefix_of(text, digits, 128)?,
            None if prefix.is_empty() => 128,
            None => return Err(format!("`{text}` has `{prefix}` after its address")),
        };
        return address(IpAddr::V6(v6), prefix, port);
    }
    if text.matches(':').count() > 1 {
        return Err(format!(
            "IPv6 addresses go in brackets, such as `[2606:4700::]/32`, but found `{text}`"
        ));
    }
    let (text, port) = match text.split_once(':') {
        Some((text, port)) => (text, Some(port_of(port)?)),
        None => (text, None),
    };
    let (text, prefix) = match text.split_once('/') {
        Some((text, digits)) => (text, Some(digits)),
        None => (text, None),
    };
    if let Ok(found @ IpAddr::V4(_)) = text.parse::<IpAddr>() {
        let prefix = match prefix {
            Some(digits) => prefix_of(text, digits, 32)?,
            None => 32,
        };
        return address(found, prefix, port);
    }
    if prefix.is_some() {
        return Err(format!(
            "a host rule has no path, but found `{text}/`. Use a URL rule, such as `+https://{text}/...`"
        ));
    }
    Ok(Destination::Host {
        name: host_name(text)?,
        port,
    })
}

/// Checks that `network` has no bits set past `prefix`.
fn address(network: IpAddr, prefix: u8, port: Option<u16>) -> Result<Destination, String> {
    let masked = masked(network, prefix).unwrap_or(network);
    if masked != network {
        let shown = match masked {
            IpAddr::V4(_) => format!("{masked}/{prefix}"),
            IpAddr::V6(_) => format!("[{masked}]/{prefix}"),
        };
        return Err(format!(
            "`{network}/{prefix}` has bits set past its prefix. Write `{shown}`"
        ));
    }
    Ok(Destination::Address {
        network,
        prefix,
        port,
    })
}

fn prefix_of(text: &str, digits: &str, most: u8) -> Result<u8, String> {
    digits
        .parse::<u8>()
        .ok()
        .filter(|prefix| *prefix <= most && digits.len() == prefix.to_string().len())
        .ok_or_else(|| format!("the prefix in `{text}` must be from 0 to {most}"))
}

fn port_of(digits: &str) -> Result<u16, String> {
    digits
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0 && digits.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| format!("a port is a number from 1 to 65535, but found `{digits}`"))
}

/// Reads a DNS name: labels of letters, digits, and `-`, each 1 to 63
/// characters long, neither starting nor ending with `-`. The last label
/// isn't all digits, so a mistyped address isn't read as a name.
fn host_name(text: &str) -> Result<String, String> {
    let labels: Vec<&str> = text.split('.').collect();
    let label_fits = |label: &&str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    let numeric = labels
        .last()
        .is_some_and(|label| label.bytes().all(|b| b.is_ascii_digit()));
    if text.len() > 253 || !labels.iter().all(label_fits) || numeric {
        return Err(format!(
            "a host rule takes a host name, such as `kagi.com`, an address, or a CIDR block, but found `{text}`"
        ));
    }
    Ok(text.to_ascii_lowercase())
}

/// Reads the part of a URL rule after `<scheme>://`.
fn url(scheme: &str, rest: &str) -> Result<Destination, String> {
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        _ => return Err("the scheme must be `https` or `http`".to_string()),
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    if path.contains(['?', '#']) {
        return Err("a URL rule has no query or fragment".to_string());
    }
    if !path_is_written_right(path) {
        return Err("its path has a character that RFC 3986 doesn't allow there".to_string());
    }
    if path.to_ascii_lowercase().contains("%2f") {
        return Err("its path has an encoded slash".to_string());
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("its path has a `.` or `..` segment".to_string());
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port_of(port)?)),
        None => (authority, None),
    };
    let host = host_name(host).map_err(|_| "it needs a host name".to_string())?;
    Ok(Destination::Url {
        scheme,
        host,
        port,
        path: path.to_string(),
    })
}

/// Whether every character of `path` is one RFC 3986 allows in a path, and
/// every `%` starts an escape of two hex digits.
fn path_is_written_right(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut at = 0;
    while let Some(&b) = bytes.get(at) {
        if b == b'%' {
            if !bytes
                .get(at + 1..at + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            at += 3;
            continue;
        }
        if !(b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b)) {
            return false;
        }
        at += 1;
    }
    true
}

/// Reads an agent name, by the rule under `ap build` in SPEC.md: 1 to 63
/// lowercase letters, digits, and `-`, starting and ending with a letter or
/// digit.
pub(crate) fn agent_rule(sign: Sign, name: &str) -> Result<(Sign, String), String> {
    let fits = (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !fits {
        return Err(format!(
            "a `POLICY` rule names an agent: 1 to 63 lowercase letters, digits, and `-`, but found `{name}`"
        ));
    }
    Ok((sign, name.to_string()))
}
