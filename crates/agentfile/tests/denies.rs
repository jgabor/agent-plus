//! Whether `NETWORK` rules deny every request to a host, or allow every one,
//! by the evaluation and the enforcement under NETWORK in SPEC.md.

use agentfile::{Network, parse};

/// Checks whether the rules in `lines`, after an `AGENT` line, deny every
/// request to `host`.
#[track_caller]
fn denies(lines: &str, host: &str, expected: bool) {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    let network = Network::read(&instructions).unwrap();
    assert_eq!(network.denies(host), expected, "{host} under {lines:?}");
}

// spec(NETWORK): It answers a name only if some rule, or the default, could allow a connection to it.
#[test]
fn a_port_that_no_rule_names_can_still_be_allowed() {
    denies(
        "NETWORK -kagi.com:80\nNETWORK -kagi.com:443\n",
        "kagi.com",
        false,
    );
    denies("NETWORK -kagi.com\n", "kagi.com", true);
}

// spec(NETWORK): If both decisions exist and disagree, deny.
#[test]
fn a_name_and_an_address_decision_that_agree_allow() {
    let lines = "NETWORK -\nNETWORK +kagi.com\nNETWORK +0.0.0.0/0\nNETWORK +[::]/0\n";
    denies(lines, "kagi.com", false);
    let lines = "NETWORK -\nNETWORK +kagi.com\nNETWORK -0.0.0.0/0\nNETWORK -[::]/0\n";
    denies(lines, "kagi.com", true);
}

// spec(NETWORK): A rule with a port beats the same rule without one.
#[test]
fn an_ip_rule_with_a_port_allows_a_name_on_that_port() {
    denies(
        "NETWORK -\nNETWORK +kagi.com\nNETWORK -0.0.0.0/0\nNETWORK -[::]/0\n\
         NETWORK +0.0.0.0/0:22\n",
        "kagi.com",
        false,
    );
}

// spec(NETWORK): A port suffix limits a host or IP rule to that port.
#[test]
fn a_port_suffix_limits_a_rule_to_that_port() {
    denies(
        "NETWORK -\nNETWORK +kagi.com:22\nNETWORK -0.0.0.0/0:22\nNETWORK -[::]/0:22\n",
        "kagi.com",
        true,
    );
    denies(
        "NETWORK -\nNETWORK +1.2.3.4:22\nNETWORK -1.2.3.4:23\nNETWORK -1.2.3.4:24\n",
        "1.2.3.4",
        false,
    );
}

// spec(NETWORK): | URL prefix | `+https://github.com/jgabor/agent-plus` | That scheme and exact host, and paths under that prefix |
#[test]
fn a_url_rule_allows_its_own_path() {
    denies(
        "NETWORK -\nNETWORK +https://kagi.com/a\nNETWORK -https://kagi.com/bb\n\
         NETWORK -https://kagi.com/cc\n",
        "kagi.com",
        false,
    );
    denies("NETWORK -\nNETWORK +http://kagi.com/a\n", "kagi.com", false);
}

// spec(NETWORK): | URL prefix | `+https://github.com/jgabor/agent-plus` | That scheme and exact host, and paths under that prefix |
#[test]
fn a_url_rule_applies_on_its_own_port() {
    denies(
        "NETWORK -\nNETWORK +https://kagi.com/a\nNETWORK -kagi.com\n\
         NETWORK -0.0.0.0/0\nNETWORK -[::]/0\nNETWORK +0.0.0.0/0:443\n",
        "kagi.com",
        false,
    );
}

// spec(NETWORK): A URL rule beats a host rule, a longer path beats a shorter one, and a host with more labels beats one with fewer.
#[test]
fn a_url_rule_for_the_root_covers_every_path() {
    denies(
        "NETWORK -\nNETWORK +kagi.com:443\nNETWORK -https://kagi.com/\n\
         NETWORK -http://kagi.com:443/\n",
        "kagi.com",
        true,
    );
}

// spec(NETWORK): A URL rule beats a host rule, a longer path beats a shorter one, and a host with more labels beats one with fewer.
#[test]
fn a_host_with_more_labels_beats_one_with_fewer_in_any_order() {
    denies(
        "NETWORK -api.kagi.com\nNETWORK +kagi.com\n",
        "api.kagi.com",
        true,
    );
    denies(
        "NETWORK +kagi.com\nNETWORK -api.kagi.com\n",
        "api.kagi.com",
        true,
    );
}

// spec(NETWORK): Two equally specific rules with opposite signs resolve to deny.
#[test]
fn equally_specific_rules_deny_in_any_order() {
    denies("NETWORK -kagi.com\nNETWORK +kagi.com\n", "kagi.com", true);
    denies("NETWORK +kagi.com\nNETWORK -kagi.com\n", "kagi.com", true);
}

// spec(NETWORK): The proxy refuses any other connection whose real destination is a loopback or unspecified address, including IPv4-mapped forms, and an IP rule can't allow one.
#[test]
fn loopback_and_unspecified_addresses_are_denied() {
    for address in ["127.0.0.1", "0.0.0.0", "::1", "::", "::ffff:127.0.0.1"] {
        denies("NETWORK +\n", address, true);
    }
    denies("NETWORK +\n", "1.1.1.1", false);
}

#[test]
fn an_ipv6_rule_never_matches_an_ipv4_address() {
    denies(
        "NETWORK -\nNETWORK +1.1.1.1\nNETWORK -[::]/0\n",
        "1.1.1.1",
        false,
    );
    denies("NETWORK +\nNETWORK -[2606:4700::]/64\n", "1.1.1.1", false);
}

#[test]
fn allows_never_counts_your_host_or_an_address() {
    let instructions =
        parse("AGENT claude\nNETWORK +\nNETWORK +host.ap.internal\nNETWORK +1.2.3.4\n").unwrap();
    let network = Network::read(&instructions).unwrap();
    assert!(network.allows("kagi.com"));
    assert!(!network.allows("host.ap.internal"));
    assert!(!network.allows("1.2.3.4"));
}
