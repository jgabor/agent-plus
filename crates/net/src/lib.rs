//! Decides each connection from the `NETWORK` rules that `agentfile` reads,
//! by the evaluation under NETWORK in SPEC.md. It is pure functions: the
//! resolver and the proxy hand it what they know of a connection.

use std::net::IpAddr;

use agentfile::{Destination, NetworkRule, Scheme, Sign};

/// A connection the agent makes.
#[derive(Clone, Copy, Debug)]
pub struct Connection<'a> {
    /// The name the connection is attributed to, if any.
    pub name: Option<&'a str>,
    /// The real destination address.
    pub address: IpAddr,
    pub port: u16,
    /// The HTTP request on the connection, where the proxy reads one.
    pub request: Option<Request<'a>>,
}

/// An HTTP request.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub scheme: Scheme,
    /// The request target, such as `/jgabor/agent-plus?tab=readme`.
    pub target: &'a str,
}

/// What decided a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By<'a> {
    /// This rule. When rules disagree and the connection is denied, it is
    /// a `-` rule. Of equally specific rules that agree, it is one of them.
    Rule(&'a NetworkRule),
    /// The default, because no rule matched.
    Default,
    /// The request path, which a host with URL rules refuses: it has an
    /// encoded slash, a dot segment, or a broken escape, or it doesn't
    /// start with `/`.
    Path,
}

/// Whether a connection is allowed, and what decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision<'a> {
    pub sign: Sign,
    pub by: By<'a>,
}

/// Decides `connection` from `rules`, and from `default` where no rule
/// matches.
pub fn decide<'a>(
    rules: &'a [NetworkRule],
    default: Sign,
    connection: &Connection,
) -> Decision<'a> {
    let name = connection.name.map(|name| {
        let name = name.to_ascii_lowercase();
        match name.strip_suffix('.') {
            Some(name) => name.to_string(),
            None => name,
        }
    });
    let mut path = None;
    if let (Some(name), Some(request)) = (&name, &connection.request) {
        let has_url_rules = rules
            .iter()
            .any(|rule| matches!(&rule.destination, Destination::Url { host, .. } if host == name));
        if has_url_rules {
            match request_path(request.target) {
                Some(found) => path = Some(found),
                None => {
                    return Decision {
                        sign: Sign::Deny,
                        by: By::Path,
                    };
                }
            }
        }
    }
    let by_name = name.and_then(|name| {
        let request = connection.request.zip(path.as_deref());
        name_decision(rules, &name, connection.port, request)
    });
    let by_address = address_decision(rules, connection.address, connection.port);
    let rule = match (by_name, by_address) {
        (Some(name), Some(address)) if name.sign != address.sign => {
            if name.sign == Sign::Deny {
                name
            } else {
                address
            }
        }
        (Some(rule), _) | (None, Some(rule)) => rule,
        (None, None) => {
            return Decision {
                sign: default,
                by: By::Default,
            };
        }
    };
    Decision {
        sign: rule.sign,
        by: By::Rule(rule),
    }
}

/// The most specific URL or host rule that matches a connection to `name`
/// on `port`. A URL rule needs the request, with its normalized path.
fn name_decision<'a>(
    rules: &'a [NetworkRule],
    name: &str,
    port: u16,
    request: Option<(Request, &str)>,
) -> Option<&'a NetworkRule> {
    most_specific(rules.iter().filter_map(|rule| {
        let key = match &rule.destination {
            Destination::Host {
                name: host,
                port: only,
            } => {
                let matches = name == host
                    || name
                        .strip_suffix(host.as_str())
                        .is_some_and(|start| start.ends_with('.'));
                (matches && only.is_none_or(|only| only == port))
                    .then(|| (false, host.split('.').count(), only.is_some()))
            }
            Destination::Url {
                scheme,
                host,
                port: only,
                path: prefix,
            } => {
                let (request, path) = request?;
                let default = match scheme {
                    Scheme::Https => 443,
                    Scheme::Http => 80,
                };
                // Only escapes, so a rule's dots never move it elsewhere.
                let prefix = unescape_unreserved(prefix).unwrap_or_else(|| prefix.clone());
                let under = path == prefix
                    || path
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| prefix.ends_with('/') || rest.starts_with('/'));
                (name == host
                    && request.scheme == *scheme
                    && only.unwrap_or(default) == port
                    && under)
                    .then(|| (true, prefix.len(), only.is_some()))
            }
            Destination::Address { .. } => None,
        };
        key.map(|key| (key, rule))
    }))
}

/// The IP rule with the longest prefix that matches a connection to
/// `address` on `port`. An IPv4-mapped IPv6 address meets the IPv4 rules for
/// the address it maps, and the IPv6 rules for itself. Prefixes compare in
/// IPv6 bits, so an IPv4 `/8` is a `/104`.
fn address_decision(rules: &[NetworkRule], address: IpAddr, port: u16) -> Option<&NetworkRule> {
    let v4 = match address.to_canonical() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    };
    most_specific(rules.iter().filter_map(|rule| {
        let Destination::Address {
            network,
            prefix,
            port: only,
        } = rule.destination
        else {
            return None;
        };
        let matches = match (network, address) {
            (IpAddr::V4(network), _) => {
                v4.is_some_and(|v4| masked(v4.into(), prefix) == Some(network.into()))
            }
            (IpAddr::V6(_), IpAddr::V6(_)) => masked(address, prefix) == Some(network),
            // A plain IPv4 address meets an IPv6 rule only inside the
            // IPv4-mapped block.
            (IpAddr::V6(v6), IpAddr::V4(v4)) => {
                prefix >= 96
                    && v6.to_ipv4_mapped().is_some_and(|network| {
                        masked(v4.into(), prefix - 96) == Some(network.into())
                    })
            }
        };
        let bits = match network {
            IpAddr::V4(_) => prefix + 96,
            IpAddr::V6(_) => prefix,
        };
        (matches && only.is_none_or(|only| only == port)).then_some(((bits, only.is_some()), rule))
    }))
}

/// The match with the highest key. Among equally specific matches with
/// opposite signs, a `-` rule.
fn most_specific<'a, K: Ord>(
    matches: impl Iterator<Item = (K, &'a NetworkRule)>,
) -> Option<&'a NetworkRule> {
    let mut best: Option<(K, &NetworkRule)> = None;
    for (key, rule) in matches {
        match &best {
            Some((top, _)) if key < *top => {}
            Some((top, top_rule)) if key == *top && top_rule.sign == Sign::Deny => {}
            _ => best = Some((key, rule)),
        }
    }
    best.map(|(_, rule)| rule)
}

/// `address` with the bits past `prefix` cleared, or `None` when the prefix
/// is longer than the address, or the families differ.
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

/// The normalized path of a request target, or `None` when a host with URL
/// rules refuses it.
fn request_path(target: &str) -> Option<String> {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let path = unescape_unreserved(path.strip_prefix('/')?)?;
    if path.contains("%2F")
        || path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        return None;
    }
    Some(format!("/{path}"))
}

/// Normalizes a path by RFC 3986 section 6.2.2: escapes of unreserved
/// characters decode, other escapes are written in uppercase, and dot
/// segments go as section 5.2.4 says. `None` when a `%` doesn't start an
/// escape of two hex digits.
pub fn normalize_path(path: &str) -> Option<String> {
    Some(remove_dot_segments(&unescape_unreserved(path)?))
}

fn unescape_unreserved(path: &str) -> Option<String> {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let hex = rest.get(at + 1..at + 3)?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = u8::from_str_radix(hex, 16).ok()?;
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&hex.to_ascii_uppercase());
        }
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    Some(out)
}

/// The algorithm of RFC 3986 section 5.2.4.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input
            .strip_prefix("../")
            .or_else(|| input.strip_prefix("./"))
        {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            output.truncate(output.rfind('/').unwrap_or(0));
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..]
                .find('/')
                .map_or(input.len(), |at| at + start);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}
