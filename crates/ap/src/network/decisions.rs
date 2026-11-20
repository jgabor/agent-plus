//! A run's network decisions, which `ap logs` prints after the run's output,
//! as SPEC.md's `ap logs` section says.
//!
//! The network process reports each connection it decides, and each name
//! that the resolver refuses, as one message on the socket it asks for
//! lookups on: `!`, `allow` or `deny`, the rule that decided, then what it
//! decided, as [`super::blocked`] writes a host it blocked. The rule is the
//! number of a `NETWORK` rule, from 0 in the order of the run's rules, or
//! `default`, `harness`, `loopback`, or `path`. `ap run` reads each report
//! as data, and keeps it in the run's log, with the time it came and the
//! rule as the Agentfile wrote it.

use std::ffi::OsString;
use std::fs::File;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfile::{Instruction, Network};
use net::{Blocked, By, Proxied};

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
    };
    let text = format!("{verdict} {rule} {}", blocked::text(about));
    [&[TAG][..], text.as_bytes()].concat()
}

fn read(message: &[u8]) -> Option<(bool, Rule, Blocked)> {
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
        let rules = args.iter().position(|arg| arg == super::RULES);
        let rules = rules.map_or(&[][..], |at| &args[at + 1..]);
        let rules = (rules.iter())
            .filter_map(|arg| {
                let text = arg.to_str()?;
                let line = Instruction {
                    line: 1,
                    name: "NETWORK".to_owned(),
                    arguments: vec![text.to_owned()],
                    text: None,
                };
                // A bare rule sets the default, and has no number.
                let read = Network::read(&[line]).ok()?;
                (!read.rules.is_empty()).then(|| text.to_owned())
            })
            .collect();
        Keeper { rules, log }
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
        let rule = match rule {
            Rule::Numbered(number) => format!("NETWORK {}", self.rules.get(number)?),
            Rule::Default => "the default".to_owned(),
            Rule::Harness => "the harness's hosts".to_owned(),
            Rule::Loopback => {
                "your host's loopback, which only host.ap.internal reaches".to_owned()
            }
            Rule::Path => "the request path".to_owned(),
        };
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
mod tests {
    use super::*;

    fn keeper(rules: &[&str]) -> (Keeper, File) {
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
