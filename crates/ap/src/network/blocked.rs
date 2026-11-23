//! The hosts that `NETWORK` blocked in a run, which `ap run` prints when the
//! run ends, as SPEC.md's Run section says under **Blocked hosts**.
//!
//! The network process reports each host the first time it blocks it, as
//! one message on the socket it asks for lookups on: `!`, then `lookup
//! <name>`, or `connect <name or -> <address> <port>`. Past
//! [`MOST_BLOCKED`] hosts, it reports `more` once, and nothing after.
//! `ap run` reads each report as data, and works out each host's rule
//! itself, from the run's rules.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write;
use std::net::IpAddr;

use agentfile::{Network, Sign};
use net::Blocked;

use super::decisions::{self, Rule};

/// The first byte of a report, which no question starts with.
pub(super) const TAG: u8 = b'!';
/// The most hosts a run reports.
pub const MOST_BLOCKED: usize = 100;

/// What `ap run` got from the network process.
#[derive(Debug, Default)]
pub struct Reports {
    pub blocked: Vec<Blocked>,
    /// Whether the network process blocked more hosts than it reported.
    pub more: bool,
}

impl Reports {
    /// Takes one report. A host it has already is taken once.
    pub(super) fn take(&mut self, message: &[u8]) {
        match read(message) {
            Some(Read::More) => self.more = true,
            Some(Read::Blocked(blocked)) => {
                let new = !self
                    .blocked
                    .iter()
                    .any(|known| known.host() == blocked.host());
                if new && self.blocked.len() < MOST_BLOCKED {
                    self.blocked.push(blocked);
                }
            }
            None => {}
        }
    }
}

enum Read {
    Blocked(Blocked),
    More,
}

fn write(blocked: &Blocked) -> Vec<u8> {
    [&[TAG][..], text(blocked).as_bytes()].concat()
}

/// `blocked` as a report writes it, after the tag.
pub(super) fn text(blocked: &Blocked) -> String {
    match blocked {
        Blocked::Lookup(name) => format!("lookup {name}"),
        Blocked::Connection {
            name,
            address,
            port,
        } => format!(
            "connect {} {address} {port}",
            name.as_deref().unwrap_or("-")
        ),
    }
}

fn read(message: &[u8]) -> Option<Read> {
    let text = std::str::from_utf8(message.strip_prefix(&[TAG])?).ok()?;
    match text {
        "more" => Some(Read::More),
        text => parse(text).map(Read::Blocked),
    }
}

/// What [`text`] wrote, if it reads as a host the network process could
/// have blocked.
pub(super) fn parse(text: &str) -> Option<Blocked> {
    let words: Vec<&str> = text.split(' ').collect();
    match words[..] {
        ["lookup", name] => Some(Blocked::Lookup(host_name(name)?)),
        ["connect", name, address, port] => Some(Blocked::Connection {
            name: match name {
                "-" => None,
                name => Some(host_name(name)?),
            },
            address: address.parse::<IpAddr>().ok()?,
            port: port.parse().ok()?,
        }),
        _ => None,
    }
}

/// `name`, if it is a name the resolver could have read: letters, digits,
/// hyphens, underscores, and dots. Nothing else reaches your terminal.
fn host_name(name: &str) -> Option<String> {
    let fits = !name.is_empty()
        && name.len() <= 253
        && (name.bytes()).all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
    fits.then(|| name.to_owned())
}

/// The network process's side: it reports each decision, as
/// [`super::decisions`] says, and each host it blocks once.
pub(super) struct Reporter {
    fd: i32,
    hosts: RefCell<HashSet<String>>,
    /// The rules and the harness's hosts, which name what decided.
    network: Network,
    harness: Vec<String>,
    /// The name the resolver refused in the last decision reported, which a
    /// lookup of its other records doesn't report again.
    refused: RefCell<Option<String>>,
}

impl Reporter {
    /// Reports on `fd`, the socket the process asks for lookups on, which
    /// it may write to once confined, the decisions of `network`, with
    /// `harness` as the harness's hosts.
    pub(super) fn new(fd: i32, network: Network, harness: Vec<String>) -> Reporter {
        Reporter {
            fd,
            hosts: RefCell::new(HashSet::new()),
            network,
            harness,
            refused: RefCell::new(None),
        }
    }

    /// Reports a connection to `address` on `port`, attributed to `name`,
    /// that the proxy allows or not, and the rule that decided.
    pub(super) fn decided(&self, allow: bool, name: Option<&str>, address: IpAddr, port: u16) {
        self.requested(allow, name, address, port, None);
    }

    /// Reports a connection as [`Reporter::decided`] does, or the
    /// `request` on it.
    pub(super) fn requested(
        &self,
        allow: bool,
        name: Option<&str>,
        address: IpAddr,
        port: u16,
        request: Option<net::Request>,
    ) {
        let harness: Vec<&str> = self.harness.iter().map(String::as_str).collect();
        let connection = net::Connection {
            name,
            address,
            port,
            request,
        };
        let proxied = net::proxied(&self.network, &harness, &connection);
        let rule = match (proxied, request) {
            // A request on a connection to your host, which got there, is
            // denied by a `-` rule, its path, or else the default, not by
            // its loopback, and never by a `+` IP rule.
            (net::Proxied::Loopback, Some(_)) => {
                match net::decide(&self.network.rules, Sign::Deny, &connection) {
                    net::Decision {
                        sign: Sign::Deny,
                        by,
                    } => Rule::by(&self.network, by),
                    _ => Rule::Default,
                }
            }
            _ => Rule::proxied(&self.network, proxied),
        };
        self.send_decision(allow, &rule, name, address, port);
    }

    /// Reports a UDP flow to `name` at `address` on `port` that the proxy
    /// refused, because the name has URL rules, and UDP isn't HTTP.
    pub(super) fn not_http(&self, name: Option<&str>, address: IpAddr, port: u16) {
        self.send_decision(false, &Rule::NotHttp, name, address, port);
    }

    fn send_decision(
        &self,
        allow: bool,
        rule: &Rule,
        name: Option<&str>,
        address: IpAddr,
        port: u16,
    ) {
        let about = Blocked::Connection {
            name: name.map(str::to_owned),
            address,
            port,
        };
        if self.send(&decisions::write(allow, rule, &about)) {
            *self.refused.borrow_mut() = None;
        }
    }

    /// Reports `blocked`, as a decision, and as a host unless its host was
    /// reported already. A reverse lookup, of the name for an address, is
    /// no host, so it isn't reported.
    pub(super) fn report(&self, blocked: Blocked) {
        match &blocked {
            Blocked::Lookup(name)
                if name.ends_with(".in-addr.arpa")
                    || name.ends_with(".ip6.arpa")
                    || name.parse::<IpAddr>().is_ok() =>
            {
                return;
            }
            Blocked::Lookup(name) => {
                let mut refused = self.refused.borrow_mut();
                if refused.as_ref() != Some(name) {
                    let rule = Rule::by(&self.network, net::refusing(&self.network, name));
                    if self.send(&decisions::write(false, &rule, &blocked)) {
                        *refused = Some(name.clone());
                    }
                }
            }
            Blocked::Connection {
                name,
                address,
                port,
            } => self.decided(false, name.as_deref(), *address, *port),
        }
        self.note(&blocked);
    }

    /// Reports a request on a connection to `name` at `address` on
    /// `port` that the proxy allows or not, as a decision, and, if it
    /// doesn't, its host as [`Reporter::report`] does.
    pub(super) fn request(
        &self,
        allow: bool,
        name: &str,
        address: IpAddr,
        port: u16,
        request: net::Request,
    ) {
        self.requested(allow, Some(name), address, port, Some(request));
        if !allow {
            self.note(&Blocked::Connection {
                name: Some(name.to_owned()),
                address,
                port,
            });
        }
    }

    /// Notes the host of `blocked`, unless it was noted already.
    fn note(&self, blocked: &Blocked) {
        let mut hosts = self.hosts.borrow_mut();
        let host = blocked.host();
        if hosts.contains(&host) || hosts.len() > MOST_BLOCKED {
            return;
        }
        let message = match hosts.len() {
            MOST_BLOCKED => [&[TAG][..], b"more"].concat(),
            _ => write(blocked),
        };
        // The socket doesn't wait. A report that doesn't fit is sent again
        // the next time the host is blocked.
        if self.send(&message) {
            hosts.insert(host);
        }
    }

    /// Sends `message`, and returns whether it fit.
    fn send(&self, message: &[u8]) -> bool {
        // SAFETY: `message` holds the bytes the call reads.
        let sent = unsafe { libc::write(self.fd, message.as_ptr().cast(), message.len()) };
        sent >= 0
    }
}

/// What `ap run` prints when a run ends, for the network process's `args`,
/// which [`super::arguments`] made, or `None` when it blocked nothing.
pub fn report(args: &[OsString], reports: &Reports) -> Option<String> {
    if reports.blocked.is_empty() {
        return None;
    }
    let (network, harness) = super::rules(args).ok()?;
    let harness: Vec<&str> = harness.iter().map(String::as_str).collect();
    let hosts: Vec<String> = reports.blocked.iter().map(Blocked::host).collect();
    let width = hosts.iter().map(String::len).max().unwrap_or(0);
    let count = match hosts.len() {
        1 => "1 host".to_owned(),
        count => format!("{count} hosts"),
    };
    let mut text = format!("ap: NETWORK blocked {count}. The rule beside each would allow it:\n");
    for (blocked, host) in reports.blocked.iter().zip(&hosts) {
        let rule = match (net::allowing(&network, &harness, blocked), blocked) {
            (Some(rule), _) => format!("NETWORK {rule}"),
            // SPEC.md's NETWORK, under **Your host**.
            (
                None,
                Blocked::Connection {
                    name: None,
                    address,
                    ..
                },
            ) if loopback(*address) => {
                "your host's loopback, which only host.ap.internal reaches".to_owned()
            }
            (None, _) => "no rule for this host alone allows it".to_owned(),
        };
        let _ = writeln!(text, "  {host:width$}  {rule}");
    }
    if reports.more {
        let _ = writeln!(text, "  and others, left out past the first {MOST_BLOCKED}");
    }
    Some(text)
}

/// Whether `address` is a loopback or unspecified address, which no IP
/// rule reaches.
fn loopback(address: IpAddr) -> bool {
    let address = address.to_canonical();
    address.is_loopback() || address.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_reads_back() {
        let reports = [
            Blocked::Lookup("example.com".to_owned()),
            Blocked::Connection {
                name: None,
                address: "2001:db8::7".parse().unwrap(),
                port: 443,
            },
            Blocked::Connection {
                name: Some("host.ap.internal".to_owned()),
                address: "127.0.0.1".parse().unwrap(),
                port: 22,
            },
        ];
        for blocked in reports {
            assert!(matches!(read(&write(&blocked)), Some(Read::Blocked(read)) if read == blocked));
        }
        assert!(matches!(read(b"!more"), Some(Read::More)));
        // Nothing that could change your terminal gets through.
        assert!(read(b"!lookup \x1b[2Jexample.com").is_none());
        assert!(read(b"!connect - 192.0.2.1 99999").is_none());
        assert!(read(b"lookup example.com").is_none());
    }

    // spec(Run): It lists each host once, and at most 100 hosts, and then says that it left others out.
    #[test]
    fn a_run_lists_each_host_once_and_at_most_100() {
        let (mine, theirs) = super::super::link().unwrap();
        // The socket doesn't wait, as in the network process, and `ap run`
        // reads as the reports arrive. So what fits in the socket's buffer,
        // which differs between machines, decides nothing here.
        let theirs = std::os::unix::net::UnixDatagram::from(theirs);
        theirs.set_nonblocking(true).unwrap();
        let mine = std::os::unix::net::UnixDatagram::from(mine);
        mine.set_nonblocking(true).unwrap();
        let reporter = Reporter::new(
            std::os::fd::AsRawFd::as_raw_fd(&theirs),
            super::super::rules(&super::super::arguments(&[], &[]))
                .unwrap()
                .0,
            Vec::new(),
        );
        let mut reports = Reports::default();
        let mut buffer = [0; 512];
        for round in 0..2 {
            for host in 0..150 {
                reporter.report(Blocked::Lookup(format!("host{host}.example")));
                let port = 80 + round;
                let address = format!("192.0.2.{host}").parse().unwrap();
                reporter.report(Blocked::Connection {
                    name: None,
                    address,
                    port,
                });
                while let Ok(len @ 1..) = mine.recv(&mut buffer) {
                    reports.take(&buffer[..len]);
                }
            }
        }
        let args = super::super::arguments(&[], &[]);
        let text = report(&args, &reports).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "ap: NETWORK blocked 100 hosts. The rule beside each would allow it:"
        );
        assert_eq!(lines.len(), 1 + 100 + 1, "{text}");
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            ["host0.example", "NETWORK", "+host0.example"]
        );
        assert_eq!(
            lines[2].split_whitespace().collect::<Vec<_>>(),
            ["192.0.2.0", "NETWORK", "+192.0.2.0"]
        );
        assert_eq!(lines[101], "  and others, left out past the first 100");
        let hosts: HashSet<&str> = lines[1..101]
            .iter()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(hosts.len(), 100, "{text}");
    }

    #[test]
    fn a_report_that_didn_t_fit_is_sent_the_next_time() {
        let (mine, theirs) = super::super::link().unwrap();
        let theirs = std::os::unix::net::UnixDatagram::from(theirs);
        theirs.set_nonblocking(true).unwrap();
        while theirs.send(b"!filler").is_ok() {}
        let reporter = Reporter::new(
            std::os::fd::AsRawFd::as_raw_fd(&theirs),
            super::super::rules(&super::super::arguments(&[], &[]))
                .unwrap()
                .0,
            Vec::new(),
        );
        let blocked = Blocked::Lookup("example.com".to_owned());
        reporter.report(blocked.clone());
        let mine = std::os::unix::net::UnixDatagram::from(mine);
        mine.set_nonblocking(true).unwrap();
        let mut buffer = [0; 512];
        let mut drained = Vec::new();
        while let Ok(len @ 1..) = mine.recv(&mut buffer) {
            drained.push(buffer[..len].to_vec());
        }
        assert!(drained.iter().all(|message| message == b"!filler"));
        reporter.report(blocked);
        let len = mine.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"!deny default lookup example.com");
        let len = mine.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"!lookup example.com");
    }

    // spec(ap logs): On a host with URL rules, the rule can also be the request path, or, for traffic that isn't HTTP, the host's URL rules.
    #[test]
    fn a_request_denied_on_your_host_names_what_denied_it() {
        let (mine, theirs) = super::super::link().unwrap();
        let args: Vec<OsString> = [
            "--",
            "-",
            "+https://host.ap.internal/allowed",
            "-https://host.ap.internal/allowed/secret",
            "+127.0.0.1",
        ]
        .map(OsString::from)
        .to_vec();
        let (network, _) = super::super::rules(&args).unwrap();
        let reporter = Reporter::new(
            std::os::fd::AsRawFd::as_raw_fd(&theirs),
            network,
            Vec::new(),
        );
        let request = |target| net::Request {
            scheme: agentfile::Scheme::Https,
            target,
        };
        let address = "127.0.0.1".parse().unwrap();
        for target in ["/other", "/allowed/secret", "/allowed/../x"] {
            reporter.request(false, "host.ap.internal", address, 443, request(target));
        }
        reporter.not_http(Some("host.ap.internal"), address, 443);
        let mine = std::os::unix::net::UnixDatagram::from(mine);
        mine.set_nonblocking(true).unwrap();
        let mut buffer = [0; 512];
        let mut rules = Vec::new();
        while let Ok(len @ 1..) = mine.recv(&mut buffer) {
            if let Some((_, rule, _)) = decisions::read(&buffer[..len]) {
                rules.push(rule);
            }
        }
        // A `+` rule never names a denial, and the IP rule doesn't reach a
        // request that its path decides.
        assert_eq!(
            rules,
            [Rule::Default, Rule::Numbered(1), Rule::Path, Rule::NotHttp]
        );
    }

    // spec(Run): It leaves out UDP that a host's URL rules deny, which no rule can allow.
    #[test]
    fn udp_that_url_rules_deny_is_no_blocked_host() {
        let (mine, theirs) = super::super::link().unwrap();
        let args: Vec<OsString> = ["--", "+", "-https://example.com/secret"]
            .map(OsString::from)
            .to_vec();
        let (network, _) = super::super::rules(&args).unwrap();
        let reporter = Reporter::new(
            std::os::fd::AsRawFd::as_raw_fd(&theirs),
            network,
            Vec::new(),
        );
        let address = "192.0.2.7".parse().unwrap();
        reporter.not_http(Some("example.com"), address, 443);
        drop(theirs);
        let mine = std::os::unix::net::UnixDatagram::from(mine);
        let mut reports = Reports::default();
        let mut buffer = [0; 512];
        let mut decisions = 0;
        while let Ok(len @ 1..) = mine.recv(&mut buffer) {
            decisions += usize::from(decisions::read(&buffer[..len]).is_some());
            reports.take(&buffer[..len]);
        }
        assert_eq!(decisions, 1);
        assert_eq!(report(&args, &reports), None);
    }

    #[test]
    fn a_host_no_rule_of_its_own_allows_says_why() {
        let args: Vec<OsString> = ["--", "+", "-example.com"].map(OsString::from).to_vec();
        let reports = Reports {
            blocked: vec![
                Blocked::Lookup("example.com".to_owned()),
                Blocked::Connection {
                    name: None,
                    address: "127.0.0.1".parse().unwrap(),
                    port: 8080,
                },
            ],
            more: false,
        };
        let text = report(&args, &reports).unwrap();
        assert!(
            text.contains("example.com  no rule for this host alone"),
            "{text}"
        );
        assert!(text.contains("127.0.0.1    your host's loopback"), "{text}");
        assert_eq!(report(&args, &Reports::default()), None);
    }

    #[test]
    fn each_host_is_taken_once() {
        let mut reports = Reports::default();
        reports.take(b"!connect - 192.0.2.1 443");
        reports.take(b"!connect - 192.0.2.1 80");
        reports.take(b"!lookup example.com");
        assert_eq!(reports.blocked.len(), 2, "{reports:?}");
    }
}
