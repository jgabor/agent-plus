//! The rule that `ap run` suggests for each host that `NETWORK` blocked,
//! by the blocked hosts under Run in SPEC.md. Added to the same rules, it
//! allows what was blocked, and changes nothing for any other host but the
//! host's subdomains.

use std::net::IpAddr;

use agentfile::{Network, parse};
use net::{Blocked, Connection, allowing, could_allow, proxied};

/// Claude Code's hosts, as its adapter lists them.
const HARNESS: &[&str] = &["api.anthropic.com", "claude.ai"];

/// The `NETWORK` rules of `lines`, after an `AGENT` line.
fn network(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

/// Whether `network` allows `blocked`.
fn allows(network: &Network, blocked: &Blocked) -> bool {
    match blocked {
        Blocked::Lookup(name) => could_allow(network, HARNESS, name),
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
            proxied(network, HARNESS, &connection).allows()
        }
    }
}

fn connection(name: Option<&str>, address: &str, port: u16) -> Blocked {
    Blocked::Connection {
        name: name.map(str::to_owned),
        address: address.parse::<IpAddr>().unwrap(),
        port,
    }
}

/// Checks that `lines` block `blocked`, that the suggested rule is
/// `expected`, and that `lines` with that rule added allow `blocked` and
/// decide each of `others` as `lines` did.
#[track_caller]
fn suggests(lines: &str, blocked: Blocked, expected: &str, others: &[Blocked]) {
    let before = network(lines);
    assert!(!allows(&before, &blocked), "{lines} allows {blocked:?}");
    let rule = allowing(&before, HARNESS, &blocked).expect("a rule allows it");
    assert_eq!(rule, expected);
    let after = network(&format!("{lines}NETWORK {rule}\n"));
    assert!(allows(&after, &blocked), "{rule} doesn't allow {blocked:?}");
    for other in others {
        assert_eq!(
            allows(&after, other),
            allows(&before, other),
            "{rule} changed {other:?}"
        );
    }
}

// spec(Run): **Blocked hosts.** When a run ends, `ap` prints the hosts that `NETWORK` blocked and the rule that would allow each one, on standard error.
#[test]
fn the_rule_for_a_name_allows_that_name_and_no_other() {
    // Every host rule covers the host's subdomains too, as SPEC.md's
    // NETWORK says, and there is no rule for a host alone.
    let after = network("NETWORK -\nNETWORK +example.com\n");
    assert!(allows(
        &after,
        &Blocked::Lookup("api.example.com".to_owned())
    ));
    let others = [
        Blocked::Lookup("example.net".to_owned()),
        Blocked::Lookup("com".to_owned()),
        Blocked::Lookup("notexample.com".to_owned()),
        Blocked::Lookup("example.com.evil.net".to_owned()),
        connection(None, "192.0.2.7", 443),
    ];
    // A lookup the resolver wouldn't answer.
    suggests(
        "NETWORK -\n",
        Blocked::Lookup("example.com".to_owned()),
        "+example.com",
        &others,
    );
    // A connection to a name the resolver answered, on a port no rule
    // allows.
    suggests(
        "NETWORK -\nNETWORK +example.com:22\n",
        connection(Some("example.com"), "192.0.2.1", 443),
        "+example.com",
        &others,
    );
    // Your host, as the guest reaches it.
    suggests(
        "NETWORK -\nNETWORK +host.ap.internal:8080\n",
        connection(Some("host.ap.internal"), "127.0.0.1", 9090),
        "+host.ap.internal",
        &others,
    );
}

#[test]
fn the_rule_for_an_ipv4_address_allows_that_address_and_no_other() {
    let others = [
        connection(None, "192.0.2.8", 443),
        connection(None, "192.0.2.6", 443),
        connection(None, "2001:db8::7", 443),
        Blocked::Lookup("example.com".to_owned()),
    ];
    suggests(
        "NETWORK -\n",
        connection(None, "192.0.2.7", 443),
        "+192.0.2.7",
        &others,
    );
}

#[test]
fn the_rule_for_an_ipv6_address_allows_that_address_and_no_other() {
    let others = [
        connection(None, "2001:db8::8", 443),
        connection(None, "2001:db8::6", 443),
        connection(None, "192.0.2.7", 443),
        Blocked::Lookup("example.com".to_owned()),
    ];
    suggests(
        "NETWORK -\n",
        connection(None, "2001:db8::7", 443),
        "+[2001:db8::7]",
        &others,
    );
}

#[test]
fn a_port_rule_is_suggested_where_a_rule_for_the_host_denies() {
    // `+example.com` would tie with the `-` rule, and a tie denies.
    suggests(
        "NETWORK +\nNETWORK -example.com\n",
        connection(Some("example.com"), "192.0.2.1", 443),
        "+example.com:443",
        &[connection(Some("example.com"), "192.0.2.1", 80)],
    );
    suggests(
        "NETWORK +\nNETWORK -192.0.2.0/24\n",
        connection(None, "192.0.2.7", 443),
        "+192.0.2.7",
        &[connection(None, "192.0.2.8", 443)],
    );
    suggests(
        "NETWORK +\nNETWORK -[2001:db8::7]\n",
        connection(None, "2001:db8::7", 22),
        "+[2001:db8::7]:22",
        &[connection(None, "2001:db8::7", 443)],
    );
}

#[test]
fn no_rule_is_suggested_where_none_would_allow() {
    // An equally specific `-` rule ties with any `+` rule for the host.
    let denied = network("NETWORK +\nNETWORK -example.com:443\n");
    let blocked = connection(Some("example.com"), "192.0.2.1", 443);
    assert_eq!(allowing(&denied, HARNESS, &blocked), None);
    // No IP rule reaches your host's loopback.
    let closed = network("NETWORK -\n");
    let loopback = connection(None, "127.0.0.1", 8080);
    assert_eq!(allowing(&closed, HARNESS, &loopback), None);
}
