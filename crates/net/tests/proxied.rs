//! How the proxy decides a connection it would open to the real
//! destination: the rules and the default, the harness hosts, and your
//! host's loopback, by the enforcement under NETWORK in SPEC.md.

use std::net::IpAddr;

use agentfile::{Network, Sign, parse};
use net::{By, Connection, Proxied, proxied};

/// Claude Code's hosts, as its adapter lists them.
const HARNESS: &[&str] = &[
    "api.anthropic.com",
    "claude.ai",
    "console.anthropic.com",
    "platform.claude.com",
];

/// The `NETWORK` rules of `lines`, after an `AGENT` line.
fn network(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

/// Whether the proxy connects to `address` on port 443 under `lines`, for
/// a connection attributed to `name`.
#[track_caller]
fn allows(lines: &str, name: Option<&str>, address: &str) -> bool {
    let network = network(lines);
    let connection = Connection {
        name,
        address: address.parse::<IpAddr>().unwrap(),
        port: 443,
        request: None,
    };
    proxied(&network, HARNESS, &connection).allows()
}

/// How the proxy decides a connection to `name` at `address` on port 443.
fn decided(network: &Network, name: Option<&str>, address: &str) -> String {
    let connection = Connection {
        name,
        address: address.parse::<IpAddr>().unwrap(),
        port: 443,
        request: None,
    };
    match proxied(network, HARNESS, &connection) {
        Proxied::Decided(decision) => match decision.by {
            By::Rule(rule) => format!("{:?} by {:?}", decision.sign, rule.destination),
            By::Default => format!("{:?} by default", decision.sign),
            By::Path => format!("{:?} by path", decision.sign),
        },
        Proxied::Harness => "harness".to_owned(),
        Proxied::Loopback => "loopback".to_owned(),
    }
}

// spec(NETWORK): `NETWORK` decides which destinations the agent can reach.
// spec(NETWORK): **Default.** A bare `NETWORK +` allows every destination that no rule denies.
#[test]
fn the_rules_and_the_default_decide() {
    assert!(allows("NETWORK +\n", Some("example.com"), "93.184.215.14"));
    assert!(allows("NETWORK +\n", None, "93.184.215.14"));
    assert!(!allows(
        "NETWORK +\nNETWORK -example.com\n",
        Some("example.com"),
        "93.184.215.14"
    ));
    assert!(!allows("NETWORK -\n", Some("example.com"), "93.184.215.14"));
    assert!(allows(
        "NETWORK -\nNETWORK +example.com\n",
        Some("www.example.com"),
        "93.184.215.14"
    ));
    let open = network("NETWORK +\n");
    assert_eq!(
        decided(&open, Some("example.com"), "93.184.215.14"),
        "Allow by default"
    );
}

// spec(NETWORK): **Harness hosts.** The hosts the harness needs for its model API and sign-in stay allowed under `NETWORK -`, so an agent never loses its own model.
// spec(NETWORK): An explicit `-` rule for such a host still wins.
#[test]
fn harness_hosts_stay_allowed_unless_a_rule_denies_them() {
    let closed = network("NETWORK -\n");
    assert_eq!(
        decided(&closed, Some("api.anthropic.com"), "160.79.104.10"),
        "harness"
    );
    assert_eq!(
        decided(&closed, Some("API.Anthropic.com."), "160.79.104.10"),
        "harness"
    );
    // Only the hosts themselves, not their neighbors.
    assert!(!allows("NETWORK -\n", Some("anthropic.com"), "160.79.104.10"));
    assert!(!allows(
        "NETWORK -\nNETWORK -api.anthropic.com\n",
        Some("api.anthropic.com"),
        "160.79.104.10"
    ));
    assert!(!allows(
        "NETWORK +\nNETWORK -anthropic.com\n",
        Some("api.anthropic.com"),
        "160.79.104.10"
    ));
    // An IP rule on the real address still decides.
    assert!(!allows(
        "NETWORK -\nNETWORK -160.79.104.0/24\n",
        Some("api.anthropic.com"),
        "160.79.104.10"
    ));
}

// spec(NETWORK): **Your host.** Services on your host's loopback interface are reachable only as `host.ap.internal`, and only when a `+` rule names it.
#[test]
fn your_host_is_reachable_only_by_its_name_and_a_rule_that_names_it() {
    for address in ["127.0.0.1", "::1"] {
        assert!(allows(
            "NETWORK -\nNETWORK +host.ap.internal\n",
            Some("host.ap.internal"),
            address
        ));
        assert!(allows(
            "NETWORK -\nNETWORK +HOST.ap.internal:443\n",
            Some("host.ap.internal."),
            address
        ));
        // The default never names it.
        assert!(!allows("NETWORK +\n", Some("host.ap.internal"), address));
        assert!(!allows(
            "NETWORK +\nNETWORK -host.ap.internal\n",
            Some("host.ap.internal"),
            address
        ));
        // Nor does a rule for another port.
        assert!(!allows(
            "NETWORK -\nNETWORK +host.ap.internal:22\n",
            Some("host.ap.internal"),
            address
        ));
    }
    // An IP rule still checks the real address.
    assert!(!allows(
        "NETWORK -\nNETWORK +host.ap.internal\nNETWORK -127.0.0.1\n",
        Some("host.ap.internal"),
        "127.0.0.1"
    ));
    assert_eq!(
        decided(&network("NETWORK +\n"), Some("host.ap.internal"), "127.0.0.1"),
        "loopback"
    );
}

// spec(NETWORK): The proxy refuses any other connection whose real destination is a loopback or unspecified address, including IPv4-mapped forms, and an IP rule can't allow one.
#[test]
fn any_other_connection_to_a_loopback_or_unspecified_address_is_refused() {
    let lines = "NETWORK +\nNETWORK +127.0.0.0/8\nNETWORK +0.0.0.0/0\nNETWORK +[::]/0\nNETWORK +loop.test\n";
    let open = network(lines);
    for address in [
        "127.0.0.1",
        "127.1.2.3",
        "0.0.0.0",
        "::1",
        "::",
        "::ffff:127.0.0.1",
        "::ffff:0.0.0.0",
    ] {
        assert_eq!(decided(&open, None, address), "loopback", "{address}");
        // Nor does a name that resolves there.
        assert_eq!(
            decided(&open, Some("loop.test"), address),
            "loopback",
            "{address}"
        );
    }
    // Other addresses are the rules' to decide.
    assert!(allows(lines, None, "10.0.0.1"));
    assert!(allows(lines, None, "::ffff:10.0.0.1"));
}

// spec(NETWORK): **IP rules** are checked against the real destination address.
#[test]
fn ip_rules_meet_the_real_destination_address() {
    let lines = "NETWORK -\nNETWORK +example.com\nNETWORK -93.184.0.0/16\n";
    // The name allows, and the real address denies, so the connection is
    // denied.
    assert!(!allows(lines, Some("example.com"), "93.184.215.14"));
    assert!(allows(lines, Some("example.com"), "23.192.228.80"));
    let denied = network(lines);
    assert_eq!(
        decided(&denied, Some("example.com"), "93.184.215.14"),
        format!(
            "{:?} by {:?}",
            Sign::Deny,
            denied.rules[1].destination
        )
    );
    // A literal address meets only the IP rules.
    assert!(allows(
        "NETWORK -\nNETWORK +192.0.2.0/24\n",
        None,
        "192.0.2.7"
    ));
    assert!(!allows("NETWORK -\nNETWORK +192.0.2.0/24\n", None, "192.0.3.7"));
}
