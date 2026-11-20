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

use net::Blocked;

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
    let text = match blocked {
        Blocked::Lookup(name) => format!("lookup {name}"),
        Blocked::Connection {
            name,
            address,
            port,
        } => format!(
            "connect {} {address} {port}",
            name.as_deref().unwrap_or("-")
        ),
    };
    [&[TAG][..], text.as_bytes()].concat()
}

fn read(message: &[u8]) -> Option<Read> {
    let text = std::str::from_utf8(message.strip_prefix(&[TAG])?).ok()?;
    let words: Vec<&str> = text.split(' ').collect();
    match words[..] {
        ["more"] => Some(Read::More),
        ["lookup", name] => Some(Read::Blocked(Blocked::Lookup(host_name(name)?))),
        ["connect", name, address, port] => Some(Read::Blocked(Blocked::Connection {
            name: match name {
                "-" => None,
                name => Some(host_name(name)?),
            },
            address: address.parse::<IpAddr>().ok()?,
            port: port.parse().ok()?,
        })),
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

/// The network process's side: it reports each host once.
pub(super) struct Reporter {
    fd: i32,
    hosts: RefCell<HashSet<String>>,
}

impl Reporter {
    /// Reports on `fd`, the socket the process asks for lookups on, which
    /// it may write to once confined.
    pub(super) fn new(fd: i32) -> Reporter {
        Reporter {
            fd,
            hosts: RefCell::new(HashSet::new()),
        }
    }

    /// Reports `blocked`, unless its host was reported already. A reverse
    /// lookup, of the name for an address, is no host, so it isn't
    /// reported.
    pub(super) fn report(&self, blocked: Blocked) {
        if let Blocked::Lookup(name) = &blocked
            && (name.ends_with(".in-addr.arpa")
                || name.ends_with(".ip6.arpa")
                || name.parse::<IpAddr>().is_ok())
        {
            return;
        }
        let mut hosts = self.hosts.borrow_mut();
        let host = blocked.host();
        if hosts.contains(&host) || hosts.len() > MOST_BLOCKED {
            return;
        }
        let message = match hosts.len() {
            MOST_BLOCKED => [&[TAG][..], b"more"].concat(),
            _ => write(&blocked),
        };
        hosts.insert(host);
        // The socket doesn't wait, so a report that doesn't fit is lost, as
        // a frame on a full link is.
        // SAFETY: `message` holds the bytes the call reads.
        let _ = unsafe { libc::write(self.fd, message.as_ptr().cast(), message.len()) };
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
            (None, Blocked::Connection { address, .. }) if loopback(*address) => {
                "your host's loopback, which only host.ap.internal reaches".to_owned()
            }
            // A rule for the host alone always beats the default.
            (None, _) => "a `-` rule denies it".to_owned(),
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
        let reporter = Reporter::new(std::os::fd::AsRawFd::as_raw_fd(&theirs));
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
            }
        }
        drop(theirs);
        let mut reports = Reports::default();
        let mut buffer = [0; 512];
        let mine = std::os::unix::net::UnixDatagram::from(mine);
        mine.set_nonblocking(true).unwrap();
        while let Ok(len @ 1..) = mine.recv(&mut buffer) {
            reports.take(&buffer[..len]);
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
        assert!(text.contains("example.com  a `-` rule denies it"), "{text}");
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
