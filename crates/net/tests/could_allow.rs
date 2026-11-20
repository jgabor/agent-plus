//! Which names the resolver answers: those that some rule, the default, or a
//! harness host could allow a connection to, by the enforcement under
//! NETWORK in SPEC.md.

use agentfile::{Network, parse};
use net::could_allow;

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

/// Checks which of `names` resolve under `lines`.
#[track_caller]
fn resolves(lines: &str, names: &[(&str, bool)]) {
    let network = network(lines);
    for &(name, expected) in names {
        assert_eq!(
            could_allow(&network, HARNESS, name),
            expected,
            "{name} under {lines:?}"
        );
    }
}

// spec(NETWORK): It answers a name only if some rule, or the default, could allow a connection to it.
#[test]
fn a_name_resolves_only_when_a_rule_or_the_default_could_allow_it() {
    resolves(
        "NETWORK +\nNETWORK -evil.net\n",
        &[("example.com", true), ("evil.net", false)],
    );
    resolves(
        "NETWORK -\nNETWORK +kagi.com\n",
        &[("kagi.com", true), ("example.com", false)],
    );
    // A rule for one port is still a way to connect.
    resolves(
        "NETWORK -\nNETWORK +github.com:22\n",
        &[("github.com", true)],
    );
    // So is a URL rule, for its own paths.
    resolves(
        "NETWORK -\nNETWORK +https://codeberg.org/forgejo/forgejo\n",
        &[("codeberg.org", true), ("forgejo.codeberg.org", false)],
    );
    // A denied address can't make a name unreachable while others pass.
    resolves(
        "NETWORK +\nNETWORK -1.1.1.1\n",
        &[("one.one.one.one", true)],
    );
}

// spec(NETWORK): | Host | `+kagi.com`, `+github.com:22` | `kagi.com` and its subdomains, on any port or the given port |
#[test]
fn a_host_rule_covers_its_domain_and_its_subdomains() {
    resolves(
        "NETWORK -\nNETWORK +kagi.com\n",
        &[
            ("kagi.com", true),
            ("api.kagi.com", true),
            ("a.b.kagi.com", true),
            ("notkagi.com", false),
            ("kagi.com.evil.net", false),
        ],
    );
    // A more specific `-` rule takes a subdomain back, on every port.
    resolves(
        "NETWORK -\nNETWORK +kagi.com\nNETWORK -api.kagi.com\n",
        &[("kagi.com", true), ("api.kagi.com", false)],
    );
    // But not when the broader rule still allows another port.
    resolves(
        "NETWORK -\nNETWORK +kagi.com\nNETWORK -api.kagi.com:443\n",
        &[("api.kagi.com", true)],
    );
}

// spec(NETWORK): Names match in any case, with or without a final dot.
#[test]
fn names_resolve_in_any_case_and_with_a_final_dot() {
    resolves(
        "NETWORK -\nNETWORK +kagi.com\n",
        &[
            ("KaGi.CoM.", true),
            ("API.kagi.com", true),
            ("Example.COM.", false),
        ],
    );
    resolves("NETWORK -\n", &[("API.Anthropic.com.", true)]);
}

// spec(NETWORK): Under `NETWORK -` with only IP rules, no name resolves except the harness hosts, so only connections to literal addresses work.
#[test]
fn under_a_closed_network_with_ip_rules_only_the_harness_hosts_resolve() {
    let lines = "NETWORK -\nNETWORK +10.1.0.0/16\nNETWORK +[2606:4700::]/32:443\n";
    let mut names: Vec<(&str, bool)> = HARNESS.iter().map(|host| (*host, true)).collect();
    names.extend([
        ("example.com", false),
        ("anthropic.com", false),
        // A harness host is the name, not its subdomains.
        ("x.api.anthropic.com", false),
    ]);
    resolves(lines, &names);
}

// spec(NETWORK): **Harness hosts.** The hosts the harness needs for its model API and sign-in stay allowed under `NETWORK -`, so an agent never loses its own model.
// spec(NETWORK): An explicit `-` rule for such a host still wins.
#[test]
fn an_explicit_deny_rule_for_a_harness_host_still_wins() {
    resolves(
        "NETWORK -\nNETWORK -api.anthropic.com\n",
        &[("api.anthropic.com", false), ("claude.ai", true)],
    );
    // A broader rule names the host too.
    resolves(
        "NETWORK -\nNETWORK -anthropic.com\n",
        &[
            ("api.anthropic.com", false),
            ("console.anthropic.com", false),
        ],
    );
    resolves(
        "NETWORK +\nNETWORK -claude.ai\n",
        &[("claude.ai", false), ("api.anthropic.com", true)],
    );
}

#[test]
fn your_host_resolves_only_when_a_rule_names_it() {
    resolves("NETWORK +\n", &[("host.ap.internal", false)]);
    resolves(
        "NETWORK -\nNETWORK +host.ap.internal\n",
        &[("host.ap.internal", true)],
    );
}

#[test]
fn an_address_or_an_empty_name_never_resolves() {
    resolves(
        "NETWORK +\n",
        &[
            ("", false),
            (".", false),
            ("192.0.2.1", false),
            ("::1", false),
        ],
    );
}
