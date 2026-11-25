//! Decides each connection from the `NETWORK` rules that `agentfile` reads,
//! by the evaluation under NETWORK in SPEC.md. It is pure functions: the
//! resolver and the proxy hand it what they know of a connection.

mod check;
mod inject;

pub use check::{NameCheck, Refused};
pub use inject::{LONGEST, Swap, inject};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use agentfile::{
    Destination, Instruction, Network, NetworkRule, Scheme, Sign, unescape_unreserved,
};

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
    /// encoded slash, a dot segment, an empty segment, or a broken escape,
    /// or it doesn't start with `/`.
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

/// The name that the guest reaches your host's loopback by.
pub const YOUR_HOST: &str = "host.ap.internal";

/// How the proxy decides a connection it would open to the real
/// destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proxied<'a> {
    /// The rules, or the default.
    Decided(Decision<'a>),
    /// Allowed, because the name is a harness host that no rule decides.
    Harness,
    /// Refused, because the real destination is a loopback or unspecified
    /// address that no `+` rule for [`YOUR_HOST`] reaches.
    Loopback,
}

impl Proxied<'_> {
    /// Whether a name decided: a host or URL rule, or the name being a
    /// harness host. Such a connection gets the name check.
    pub fn by_name(&self) -> bool {
        match self {
            Proxied::Decided(Decision {
                by: By::Rule(rule), ..
            }) => !matches!(rule.destination, Destination::Address { .. }),
            Proxied::Harness => true,
            Proxied::Decided(_) | Proxied::Loopback => false,
        }
    }

    pub fn allows(&self) -> bool {
        match self {
            Proxied::Decided(decision) => decision.sign == Sign::Allow,
            Proxied::Harness => true,
            Proxied::Loopback => false,
        }
    }
}

/// Decides `connection` as the proxy does, where its address is the real
/// destination: by `network`'s rules and default, with `harness` as the
/// harness's hosts. A harness host is allowed wherever no rule decides. A
/// loopback or unspecified address is refused, unless the connection is to
/// [`YOUR_HOST`] and a `+` rule for that name decides it.
pub fn proxied<'a>(network: &'a Network, harness: &[&str], connection: &Connection) -> Proxied<'a> {
    let name = connection.name.map(|name| {
        let name = name.to_ascii_lowercase();
        name.strip_suffix('.').map(str::to_owned).unwrap_or(name)
    });
    let loopback = refused(connection.address);
    if name.as_deref() == Some(YOUR_HOST) {
        let decision = decide(&network.rules, Sign::Deny, connection);
        return match decision.by {
            By::Rule(_) if decision.sign == Sign::Deny => Proxied::Decided(decision),
            By::Rule(rule) if !matches!(rule.destination, Destination::Address { .. }) => {
                Proxied::Decided(decision)
            }
            _ if loopback => Proxied::Loopback,
            _ => Proxied::Decided(Decision {
                sign: Sign::Deny,
                by: By::Default,
            }),
        };
    }
    if loopback {
        return Proxied::Loopback;
    }
    let decision = decide(&network.rules, network.default, connection);
    let harness_host = name
        .as_deref()
        .is_some_and(|name| harness.iter().any(|host| host.eq_ignore_ascii_case(name)));
    match decision.by {
        By::Default if harness_host => Proxied::Harness,
        _ => Proxied::Decided(decision),
    }
}

/// Whether `network` has URL rules for `name`, so that the proxy reads
/// each HTTP request on a connection to it.
pub fn url_rules(network: &Network, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    (network.rules.iter()).any(|rule| {
        matches!(&rule.destination, Destination::Url { host, .. } if host.eq_ignore_ascii_case(name))
    })
}

/// Whether a `+` host rule of `network` covers a connection to `name` on
/// `port` with a narrower `-` rule beneath it, so that the proxy decrypts
/// TLS to `name` and checks each request's host, because a client can reuse
/// one connection for several names. A name rule must allow the connection.
/// The narrower rule is a `-` rule on the connection's port: a host or URL
/// rule for a name under the `+` rule's, a URL rule for its own name, or a
/// host rule for its own name on one port when the `+` rule names no port.
/// The default is never the `+` rule, and a harness host, one of `harness`,
/// is never decrypted for a rule. A name is in any case, with or without a
/// final dot.
pub fn narrowed(network: &Network, harness: &[&str], name: &str, port: u16) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    if harness.iter().any(|host| host.eq_ignore_ascii_case(name)) {
        return false;
    }
    let allowed = name_decision(&network.rules, name, port, None);
    if !allowed.is_some_and(|rule| rule.sign == Sign::Allow) {
        return false;
    }
    // Rules hold their names in lowercase.
    let under = |host: &str, broad: &str| {
        (host.strip_suffix(broad)).is_some_and(|start| start.ends_with('.'))
    };
    // A narrower rule matters only on the connection's own port, since a
    // reused connection reaches no other.
    let narrower = |broad: &str, every_port: bool, narrow: &NetworkRule| match &narrow.destination {
        _ if narrow.sign == Sign::Allow => false,
        Destination::Url {
            scheme,
            host,
            port: only,
            ..
        } => {
            let default = match scheme {
                Scheme::Https => 443,
                Scheme::Http => 80,
            };
            only.unwrap_or(default) == port && (host == broad || under(host, broad))
        }
        Destination::Host { name, port: only } => {
            only.is_none_or(|only| only == port)
                && (under(name, broad) || name == broad && only.is_some() && every_port)
        }
        Destination::Address { .. } => false,
    };
    (network.rules.iter()).any(|broad| match &broad.destination {
        Destination::Host {
            name: broad_name,
            port: only,
        } if broad.sign == Sign::Allow => {
            (name == broad_name || under(name, broad_name))
                && only.is_none_or(|only| only == port)
                && (network.rules.iter()).any(|narrow| narrower(broad_name, only.is_none(), narrow))
        }
        _ => false,
    })
}

/// Whether the proxy may open `connection`, to a host with URL rules, as
/// [`proxied`] decides it: because it allows the connection before any
/// request, or would allow a request for the path of one of its `+` URL
/// rules. Each request on it is decided again.
pub fn could_request(network: &Network, harness: &[&str], connection: &Connection) -> bool {
    if proxied(network, harness, connection).allows() {
        return true;
    }
    let name = connection.name.unwrap_or_default();
    (network.rules.iter()).any(|rule| match &rule.destination {
        Destination::Url {
            scheme, host, path, ..
        } if rule.sign == Sign::Allow && host.eq_ignore_ascii_case(name.trim_end_matches('.')) => {
            let request = Request {
                scheme: *scheme,
                target: path,
            };
            let connection = Connection {
                request: Some(request),
                ..*connection
            };
            proxied(network, harness, &connection).allows()
        }
        _ => false,
    })
}

/// Whether `address` is a loopback or unspecified address, in either family
/// or IPv4-mapped.
fn refused(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Whether `network`, with `harness` as the harness's hosts, could allow a
/// connection to `name`, so that the resolver answers it. A harness host is
/// allowed wherever no rule decides, so only an explicit `-` rule takes it
/// away. A name is in any case, with or without a final dot. An address, or
/// an empty name, is never one to answer.
pub fn could_allow(network: &Network, harness: &[&str], name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    if name.is_empty() || name.parse::<IpAddr>().is_ok() {
        return false;
    }
    let harness_host = harness.iter().any(|host| host.eq_ignore_ascii_case(name));
    if harness_host && network.default == Sign::Deny {
        let opened = Network {
            default: Sign::Allow,
            rules: network.rules.clone(),
        };
        return !opened.denies(name);
    }
    !network.denies(name)
}

/// What decided that the resolver refuses `name`, which [`could_allow`]
/// refuses: the most specific `-` host rule that names it on every port,
/// the `-` IP rule that refuses your host's loopback for [`YOUR_HOST`], or
/// else the default.
pub fn refusing<'a>(network: &'a Network, name: &str) -> By<'a> {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    // A port that no rule names, so that only a rule for every port matches.
    let port = (1..=u16::MAX).find(|&port| {
        !network.rules.iter().any(|rule| match &rule.destination {
            Destination::Host { port: only, .. }
            | Destination::Address { port: only, .. }
            | Destination::Url { port: only, .. } => *only == Some(port),
        })
    });
    let port = port.unwrap_or(0);
    let rule = name_decision(&network.rules, name, port, None);
    if let Some(rule) = rule.filter(|rule| rule.sign == Sign::Deny) {
        return By::Rule(rule);
    }
    if name != YOUR_HOST {
        return By::Default;
    }
    // Where a `+` rule names your host, on any port, the IP rules refused
    // its loopback.
    let ports = (network.rules.iter()).filter_map(|rule| match &rule.destination {
        Destination::Host {
            port: Some(only), ..
        } => Some(*only),
        _ => None,
    });
    let loopback = [Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()];
    std::iter::once(port)
        .chain(ports)
        .filter(|&port| {
            name_decision(&network.rules, name, port, None)
                .is_some_and(|rule| rule.sign == Sign::Allow)
        })
        .flat_map(|port| loopback.map(|address| (address, port)))
        .find_map(|(address, port)| {
            address_decision(&network.rules, address, port).filter(|rule| rule.sign == Sign::Deny)
        })
        .map_or(By::Default, By::Rule)
}

/// Something that `NETWORK` blocked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// A name the resolver wouldn't answer.
    Lookup(String),
    /// A connection the proxy refused: the name it is attributed to, if
    /// any, its real destination address, and its port.
    Connection {
        name: Option<String>,
        address: IpAddr,
        port: u16,
    },
}

impl Blocked {
    /// The host it was for: the name, or else the address.
    pub fn host(&self) -> String {
        match self {
            Blocked::Lookup(name)
            | Blocked::Connection {
                name: Some(name), ..
            } => name.clone(),
            Blocked::Connection { address, .. } => address.to_string(),
        }
    }
}

/// The rule that, added to `network`'s rules, allows `blocked`, with
/// `harness` as the harness's hosts, as it would be written after
/// `NETWORK`. It is a rule for the host alone, or with the port where a
/// rule for the host would tie with a `-` rule or lose to one. `None` when
/// neither would allow it.
pub fn allowing(network: &Network, harness: &[&str], blocked: &Blocked) -> Option<String> {
    match blocked {
        Blocked::Lookup(name) => {
            suggest(network, blocked, |added| could_allow(added, harness, name))
        }
        Blocked::Connection {
            name,
            address,
            port,
        } => {
            let connection = Connection {
                name: name.as_deref(),
                address: *address,
                port: *port,
                request: None,
            };
            allowing_request(network, harness, &connection)
        }
    }
}

/// The rule that [`allowing`] suggests for `connection`, which allows the
/// connection's request too, if it has one. `None` where the request's
/// URL rules or its path decide it, because no rule for the host alone
/// beats them.
pub fn allowing_request(
    network: &Network,
    harness: &[&str],
    connection: &Connection,
) -> Option<String> {
    let blocked = Blocked::Connection {
        name: connection.name.map(str::to_owned),
        address: connection.address,
        port: connection.port,
    };
    suggest(network, &blocked, |added| {
        proxied(added, harness, connection).allows()
    })
}

/// The first rule for the host of `blocked`, alone or with its port, that
/// `allows` once it is added to `network`'s rules.
fn suggest(
    network: &Network,
    blocked: &Blocked,
    allows: impl Fn(&Network) -> bool,
) -> Option<String> {
    let host = match blocked {
        Blocked::Connection {
            name: None,
            address: IpAddr::V6(v6),
            ..
        } => format!("[{v6}]"),
        _ => blocked.host(),
    };
    let mut candidates = vec![format!("+{host}")];
    if let Blocked::Connection { port, .. } = blocked {
        candidates.push(format!("+{host}:{port}"));
    }
    candidates.into_iter().find(|text| {
        let line = Instruction {
            line: 1,
            name: "NETWORK".to_owned(),
            arguments: vec![text.clone()],
            text: None,
        };
        let Ok(mut added) = Network::read(&[line]) else {
            return false;
        };
        added.default = network.default;
        added.rules.splice(..0, network.rules.iter().cloned());
        allows(&added)
    })
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

/// The normalized path of a request target, which [`decide`] matches URL
/// rules against, or `None` when a host with URL rules refuses it.
pub fn request_path(target: &str) -> Option<String> {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let path = unescape_unreserved(path.strip_prefix('/')?)?;
    // An empty segment is refused, but not the empty last one of `/a/`.
    if path.contains("%2F")
        || path.starts_with('/')
        || path.contains("//")
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
