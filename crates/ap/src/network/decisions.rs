//! A run's network decisions, which `ap logs` prints after the run's output,
//! as SPEC.md's `ap logs` section says.
//!
//! The network process reports each connection it decides, and each name
//! that the resolver refuses, as one message on the socket it asks for
//! lookups on: `!`, `allow` or `deny`, the rule that decided, then what it
//! decided, as [`super::blocked`] writes a host it blocked. The rule is the
//! number of a `NETWORK` rule, from 0 in the order of the run's rules, or
//! `default`, `harness`, `loopback`, `path`, `http`, or `decryption`.
//! `ap run` reads each report as data, and keeps it in the run's log, with
//! the time it came and the rule as the Agentfile wrote it.
//!
//! `ap inspect --network` names the rule that decides a destination the
//! same way, as SPEC.md's `ap inspect` section says, from the same
//! functions of [`net`] that the network process decides with.

use std::ffi::OsString;
use std::fs::File;
use std::net::{IpAddr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use agentfile::{Destination, Instruction, Network, Scheme, Sign};
use net::{Blocked, By, Connection, Proxied, Request};

use super::blocked::{self, TAG};

/// What decided.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Rule {
    /// The rule with this number.
    Numbered(usize),
    Default,
    /// A harness host that no rule decides.
    Harness,
    /// Your host's loopback, which only a `+` rule for
    /// [`net::YOUR_HOST`] reaches.
    Loopback,
    /// The request path.
    Path,
    /// The host's URL rules, under which only HTTP passes.
    NotHttp,
    /// The proxy's decryption of a host with no URL rules, which only TCP
    /// passes.
    Decryption,
}

impl Rule {
    /// The rule of `network` that `by` names.
    pub(super) fn by(network: &Network, by: By) -> Rule {
        match by {
            By::Rule(rule) => (network.rules.iter())
                .position(|known| std::ptr::eq(known, rule))
                .map_or(Rule::Default, Rule::Numbered),
            By::Default => Rule::Default,
            By::Path => Rule::Path,
        }
    }

    /// The rule of `network` that decided `proxied`.
    pub(super) fn proxied(network: &Network, proxied: Proxied) -> Rule {
        match proxied {
            Proxied::Decided(decision) => Rule::by(network, decision.by),
            Proxied::Harness => Rule::Harness,
            Proxied::Loopback => Rule::Loopback,
        }
    }

    /// The rule of `network` that decided a connection to `name` at
    /// `address` on `port`, with `request` on it, which [`net::proxied`]
    /// decided as `proxied`, or [`net::by_name`] did, without an address.
    pub(super) fn decided(
        network: &Network,
        proxied: Proxied,
        (name, address): (Option<&str>, Option<IpAddr>),
        port: u16,
        request: Option<Request>,
    ) -> Rule {
        if !(proxied == Proxied::Loopback && (request.is_some() || address.is_none())) {
            return Rule::proxied(network, proxied);
        }
        // A request on a connection to your host, which got there, or a
        // connection to your host by name alone, is denied by a `-` rule,
        // its path, or else the default, not by its loopback, and never by
        // a `+` IP rule.
        let decision = match (name, address) {
            (_, Some(address)) => {
                let connection = Connection {
                    name,
                    address,
                    port,
                    request,
                };
                net::decide(&network.rules, Sign::Deny, &connection)
            }
            (Some(name), None) => {
                net::decide_by_name(&network.rules, Sign::Deny, name, port, request)
            }
            (None, None) => return Rule::Default,
        };
        match decision {
            net::Decision {
                sign: Sign::Deny,
                by,
            } => Rule::by(network, by),
            _ => Rule::Default,
        }
    }

    /// What `ap logs` calls this rule, of `rules`, which [`written`] gave.
    fn named(&self, rules: &[String]) -> Option<String> {
        Some(match self {
            Rule::Numbered(number) => format!("NETWORK {}", rules.get(*number)?),
            Rule::Default => "the default".to_owned(),
            Rule::Harness => "the harness's hosts".to_owned(),
            Rule::Loopback => {
                "your host's loopback, which only host.ap.internal reaches".to_owned()
            }
            Rule::Path => "the request path".to_owned(),
            Rule::NotHttp => "the host's URL rules, which only HTTP meets".to_owned(),
            Rule::Decryption => "the host's decryption, which only TCP passes".to_owned(),
        })
    }
}

/// The rules in a network process's `args`, which [`super::arguments`]
/// made, as the Agentfile wrote them, in their order. A bare rule sets the
/// default, and has no number.
fn written(args: &[OsString]) -> Vec<String> {
    let rules = args.iter().position(|arg| arg == super::RULES);
    let rules = rules.map_or(&[][..], |at| &args[at + 1..]);
    (rules.iter())
        .filter_map(|arg| {
            let text = arg.to_str()?;
            let read = Network::read(&[line(text)]).ok()?;
            (!read.rules.is_empty()).then(|| text.to_owned())
        })
        .collect()
}

/// `text` as the argument of a `NETWORK` line.
fn line(text: &str) -> Instruction {
    Instruction {
        line: 1,
        name: "NETWORK".to_owned(),
        arguments: vec![text.to_owned()],
        text: None,
    }
}

/// A destination that `ap inspect --network` asks about.
#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    /// A name, on its port, if it has one. A URL also has its request: its
    /// scheme, and its target, from the path on.
    Name {
        name: String,
        port: Option<u16>,
        request: Option<(Scheme, String)>,
    },
    Address {
        address: IpAddr,
        port: Option<u16>,
    },
}

impl Asked {
    /// Reads a destination as a `NETWORK` rule writes one, without its
    /// sign: a URL, a host, a host with a port, or an address with or
    /// without a port. A URL's path may be one that a rule can't have,
    /// since the proxy decides such a path too. The error says what is
    /// wrong.
    pub fn read(text: &str) -> Result<Asked, String> {
        let shown = agentfile::print_argument(text);
        if text.starts_with(['+', '-']) {
            return Err(format!(
                "`{shown}` is a rule. Ask about its destination without the sign, or add the rule to a run with `ap run --network`"
            ));
        }
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(format!(
                "`{shown}` isn't a destination: a URL, a host, or an address"
            ));
        }
        let unread = |errors: Vec<agentfile::Error>| {
            let messages: Vec<String> = errors.into_iter().map(|error| error.message).collect();
            messages.join("; ")
        };
        // A URL's authority is read as a rule's, and its target as the proxy
        // reads a request's.
        let (rule, target) = match text.split_once("://") {
            Some((scheme, rest)) => {
                let at = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                let (authority, target) = rest.split_at(at);
                let target = match target.starts_with('/') {
                    true => target.to_owned(),
                    false => format!("/{target}"),
                };
                (format!("+{scheme}://{authority}"), Some(target))
            }
            None => (format!("+{text}"), None),
        };
        let read = Network::read(&[line(&rule)]).map_err(unread)?;
        let Some(rule) = read.rules.into_iter().next() else {
            return Err(format!("`{shown}` isn't a destination"));
        };
        match (rule.destination, target) {
            (
                Destination::Url {
                    scheme, host, port, ..
                },
                Some(target),
            ) => Ok(Asked::Name {
                name: host,
                port,
                request: Some((scheme, target)),
            }),
            (Destination::Host { name, port }, None) => Ok(Asked::Name {
                name,
                port,
                request: None,
            }),
            (
                Destination::Address {
                    network,
                    prefix,
                    port,
                },
                None,
            ) if prefix == if network.is_ipv4() { 32 } else { 128 } => Ok(Asked::Address {
                address: network,
                port,
            }),
            _ => Err(format!(
                "`{shown}` is a block of addresses. Ask about one address"
            )),
        }
    }
}

/// What `ap inspect --network` answers.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    /// Whether the rules allow the destination.
    pub allows: bool,
    /// The rule that decides, as `ap logs` names it.
    pub rule: String,
}

/// Whether the network process with `args`, which [`super::arguments`]
/// made, allows a connection to `asked`, and the rule that decides. A name
/// is decided by name alone, as before it resolves, and a destination
/// without a port on a port that no rule names. `None` when `args` can't
/// be read.
pub fn answer(args: &[OsString], asked: &Asked) -> Option<Answer> {
    let (network, harness) = super::rules(args).ok()?;
    let harness: Vec<&str> = harness.iter().map(String::as_str).collect();
    let any_port = net::any_port(&network);
    let (allows, rule) = match asked {
        Asked::Address { address, port } => {
            let port = port.unwrap_or(any_port);
            let connection = Connection {
                name: None,
                address: *address,
                port,
                request: None,
            };
            let proxied = net::proxied(&network, &harness, &connection);
            (proxied.allows(), Rule::proxied(&network, proxied))
        }
        Asked::Name {
            name,
            port,
            request,
        } => {
            let port = match request {
                Some((Scheme::Https, _)) => port.unwrap_or(443),
                Some((Scheme::Http, _)) => port.unwrap_or(80),
                None => port.unwrap_or(any_port),
            };
            let request = (request.as_ref()).map(|(scheme, target)| Request {
                scheme: *scheme,
                target,
            });
            let proxied = net::by_name(&network, &harness, name, port, request);
            let rule = Rule::decided(&network, proxied, (Some(name), None), port, request);
            (proxied.allows(), rule)
        }
    };
    Some(Answer {
        allows,
        rule: rule.named(&written(args))?,
    })
}

/// The report of a decision: whether it allows, the rule that decided, and
/// what it decided.
pub(super) fn write(allow: bool, rule: &Rule, about: &Blocked) -> Vec<u8> {
    let verdict = if allow { "allow" } else { "deny" };
    let rule = match rule {
        Rule::Numbered(number) => number.to_string(),
        Rule::Default => "default".to_owned(),
        Rule::Harness => "harness".to_owned(),
        Rule::Loopback => "loopback".to_owned(),
        Rule::Path => "path".to_owned(),
        Rule::NotHttp => "http".to_owned(),
        Rule::Decryption => "decryption".to_owned(),
    };
    let text = format!("{verdict} {rule} {}", blocked::text(about));
    [&[TAG][..], text.as_bytes()].concat()
}

pub(super) fn read(message: &[u8]) -> Option<(bool, Rule, Blocked)> {
    let text = std::str::from_utf8(message.strip_prefix(&[TAG])?).ok()?;
    let (verdict, text) = text.split_once(' ')?;
    let (rule, text) = text.split_once(' ')?;
    let allow = match verdict {
        "allow" => true,
        "deny" => false,
        _ => return None,
    };
    let rule = match rule {
        "default" => Rule::Default,
        "harness" => Rule::Harness,
        "loopback" => Rule::Loopback,
        "path" => Rule::Path,
        "http" => Rule::NotHttp,
        "decryption" => Rule::Decryption,
        number if number.bytes().all(|byte| byte.is_ascii_digit()) => {
            Rule::Numbered(number.parse().ok()?)
        }
        _ => return None,
    };
    Some((allow, rule, blocked::parse(text)?))
}

/// Where `ap run` keeps a run's decisions: the run's log.
pub struct Keeper {
    /// The run's rules, as the Agentfile wrote them, in their order.
    rules: Vec<String>,
    log: File,
}

impl Keeper {
    /// Keeps the decisions of a network process with the `args` that
    /// [`super::arguments`] made in `log`, which `ap run` writes its output
    /// to too.
    pub fn new(args: &[OsString], log: File) -> Keeper {
        Keeper {
            rules: written(args),
            log,
        }
    }

    /// Takes one report, and returns whether it was a decision. A report
    /// it can't read is dropped.
    pub(super) fn take(&mut self, message: &[u8]) -> bool {
        let Some(line) = self.line(message) else {
            return false;
        };
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        // As with the run's output, a failed write doesn't change the run.
        let _ = crate::runs::write_decision(&mut self.log, at, &line);
        true
    }

    /// The line that `ap logs` prints for a report, after the time.
    fn line(&self, message: &[u8]) -> Option<String> {
        let (allow, rule, about) = read(message)?;
        let rule = rule.named(&self.rules)?;
        let destination = match about {
            Blocked::Lookup(name) => name,
            Blocked::Connection {
                name: Some(name),
                port,
                ..
            } => format!("{name}:{port}"),
            Blocked::Connection {
                name: None,
                address,
                port,
            } => SocketAddr::new(address, port).to_string(),
        };
        let verdict = if allow { "allow" } else { "deny" };
        Some(format!("{verdict:5}  {destination}  {rule}"))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A keeper for a run with the `NETWORK` rules of `rules`, and its log,
    /// to read back.
    pub(in super::super) fn keeper(rules: &[&str]) -> (Keeper, File) {
        let args = super::super::arguments(&[], &[]);
        let mut args: Vec<OsString> = args;
        args.extend(rules.iter().map(OsString::from));
        let path = std::env::temp_dir().join(format!(
            "ap-decisions-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let log = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let reader = log.try_clone().unwrap();
        (Keeper::new(&args, log), reader)
    }

    // spec(ap logs): A connection's destination is its name, or else its address, with its port.
    // spec(ap logs): A name that the resolver refuses is a decision too, and its destination is the name alone.
    // spec(ap logs): The rule is the `NETWORK` line that decided, or else the default, the harness's hosts, or your host's loopback.
    #[test]
    fn a_decision_reads_back_as_its_line() {
        let (keeper, _) = keeper(&["-", "-evil.net", "+", "+host.ap.internal"]);
        let to_host = Blocked::Connection {
            name: Some("host.ap.internal".to_owned()),
            address: "127.0.0.1".parse().unwrap(),
            port: 8080,
        };
        let literal = Blocked::Connection {
            name: None,
            address: "2001:db8::7".parse().unwrap(),
            port: 443,
        };
        let lookup = Blocked::Lookup("api.evil.net".to_owned());
        let line = |allow, rule, about| keeper.line(&write(allow, &rule, about));
        assert_eq!(
            line(true, Rule::Numbered(1), &to_host).as_deref(),
            Some("allow  host.ap.internal:8080  NETWORK +host.ap.internal")
        );
        assert_eq!(
            line(false, Rule::Numbered(0), &lookup).as_deref(),
            Some("deny   api.evil.net  NETWORK -evil.net")
        );
        assert_eq!(
            line(false, Rule::Default, &literal).as_deref(),
            Some("deny   [2001:db8::7]:443  the default")
        );
        assert_eq!(
            line(true, Rule::Harness, &to_host).as_deref(),
            Some("allow  host.ap.internal:8080  the harness's hosts")
        );
        assert!(
            line(false, Rule::Loopback, &literal)
                .unwrap()
                .contains("loopback")
        );
        assert_eq!(
            line(false, Rule::NotHttp, &to_host).as_deref(),
            Some("deny   host.ap.internal:8080  the host's URL rules, which only HTTP meets")
        );
        assert_eq!(
            line(false, Rule::Decryption, &to_host).as_deref(),
            Some("deny   host.ap.internal:8080  the host's decryption, which only TCP passes")
        );
    }

    #[test]
    fn a_report_ap_run_can_t_read_is_dropped() {
        let (mut keeper, mut log) = keeper(&["+example.com"]);
        for bad in [
            &b"!allow 1 lookup example.com"[..],
            b"!allow -1 lookup example.com",
            b"!maybe default lookup example.com",
            b"!allow nobody lookup example.com",
            b"!allow default lookup \x1b[2Jexample.com",
            b"!allow default connect - 192.0.2.1 99999",
            b"allow default lookup example.com",
            b"!lookup example.com",
            b"!allow default",
            b"!\xffallow",
        ] {
            assert!(!keeper.take(bad), "{bad:?}");
        }
        assert!(keeper.take(b"!allow 0 lookup example.com"));
        use std::io::Seek;
        log.rewind().unwrap();
        let read = crate::runs::read_log(&mut log).unwrap();
        assert!(
            matches!(&read, Some(crate::runs::Logged::Decision(_, line))
                if line == "allow  example.com  NETWORK +example.com"),
            "{read:?}"
        );
        assert_eq!(crate::runs::read_log(&mut log).unwrap(), None);
    }
}
