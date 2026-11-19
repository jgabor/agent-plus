//! Each decision the rules make, and the rule that made it, by the
//! evaluation under NETWORK in SPEC.md.

use std::net::IpAddr;

use agentfile::{Network, NetworkRule, Scheme, Sign, parse};
use net::{By, Connection, Request, decide};

/// An address that no rule in these tests names, as the resolver's pool
/// would hand out.
const POOL: &str = "100.64.0.9";

/// The `NETWORK` rules of `lines`, after an `AGENT` line.
fn network(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

/// The one rule that `NETWORK <text>` reads.
fn rule(text: &str) -> NetworkRule {
    network(&format!("NETWORK {text}\n")).rules.remove(0)
}

fn connection<'a>(name: Option<&'a str>, address: &str, port: u16) -> Connection<'a> {
    Connection {
        name,
        address: address.parse::<IpAddr>().unwrap(),
        port,
        request: None,
    }
}

/// A request for `url`, such as `https://github.com/x`, to a pool address.
fn request(url: &str) -> Connection<'_> {
    let (scheme, rest) = url.split_once("://").unwrap();
    let (scheme, port) = match scheme {
        "https" => (Scheme::Https, 443),
        _ => (Scheme::Http, 80),
    };
    let at = rest.find('/').unwrap_or(rest.len());
    let (authority, target) = rest.split_at(at);
    let (name, port) = match authority.split_once(':') {
        Some((name, port)) => (name, port.parse().unwrap()),
        None => (authority, port),
    };
    Connection {
        request: Some(Request { scheme, target }),
        ..connection(Some(name), POOL, port)
    }
}

/// Checks that `lines` decide `connection` with `sign`, by `by`: `default`,
/// `path`, or the text of the rule that decides.
#[track_caller]
fn decides(lines: &str, connection: Connection, sign: Sign, by: &str) {
    let network = network(lines);
    let decision = decide(&network.rules, network.default, &connection);
    assert_eq!(decision.sign, sign, "{connection:?} under {lines:?}");
    match (by, decision.by) {
        ("default", By::Default) | ("path", By::Path) => {}
        (text, By::Rule(found)) if *found == rule(text) => {}
        (_, found) => panic!("{connection:?} under {lines:?} was decided by {found:?}, not {by}"),
    }
}

const ALLOW: Sign = Sign::Allow;
const DENY: Sign = Sign::Deny;

// spec(NETWORK): | Host | `+kagi.com`, `+github.com:22` | `kagi.com` and its subdomains, on any port or the given port |
// spec(NETWORK): A port suffix limits a host or IP rule to that port.
#[test]
fn a_host_rule_matches_its_subdomains_on_its_port() {
    let rules = "NETWORK -\nNETWORK +kagi.com\nNETWORK +github.com:22\n";
    for port in [443, 80, 8080] {
        decides(
            rules,
            connection(Some("kagi.com"), POOL, port),
            ALLOW,
            "+kagi.com",
        );
        decides(
            rules,
            connection(Some("api.kagi.com"), POOL, port),
            ALLOW,
            "+kagi.com",
        );
    }
    decides(
        rules,
        connection(Some("a.b.kagi.com"), POOL, 22),
        ALLOW,
        "+kagi.com",
    );
    decides(
        rules,
        connection(Some("github.com"), POOL, 22),
        ALLOW,
        "+github.com:22",
    );
    decides(
        rules,
        connection(Some("ssh.github.com"), POOL, 22),
        ALLOW,
        "+github.com:22",
    );
    decides(
        rules,
        connection(Some("github.com"), POOL, 443),
        DENY,
        "default",
    );
    decides(rules, request("https://github.com/x"), DENY, "default");
}

// spec(NETWORK): Names match in any case, with or without a final dot.
#[test]
fn names_match_without_case_or_a_trailing_dot() {
    let rules = "NETWORK +\nNETWORK -kagi.com\n";
    decides(
        rules,
        connection(Some("KAGI.com."), POOL, 443),
        DENY,
        "-kagi.com",
    );
    decides(
        rules,
        connection(Some("Api.Kagi.Com"), POOL, 443),
        DENY,
        "-kagi.com",
    );
}

// spec(NETWORK): | IP or CIDR | `-1.1.1.1`, `+10.1.0.0/16`, `-[2606:4700::]/32:443` | Connections to those addresses, on any port or the given port |
// spec(NETWORK): A port suffix limits a host or IP rule to that port.
#[test]
fn an_ip_rule_matches_its_addresses_on_its_port() {
    let rules =
        "NETWORK +\nNETWORK -1.1.1.1\nNETWORK +10.1.0.0/16\nNETWORK -[2606:4700::]/32:443\n";
    for port in [53, 443] {
        decides(rules, connection(None, "1.1.1.1", port), DENY, "-1.1.1.1");
    }
    decides(rules, connection(None, "1.1.1.2", 53), ALLOW, "default");
    decides(
        rules,
        connection(None, "10.1.255.3", 22),
        ALLOW,
        "+10.1.0.0/16",
    );
    decides(rules, connection(None, "10.2.0.1", 22), ALLOW, "default");
    let v6 = "2606:4700:1::1";
    decides(
        rules,
        connection(None, v6, 443),
        DENY,
        "-[2606:4700::]/32:443",
    );
    decides(rules, connection(None, v6, 80), ALLOW, "default");
    // A name doesn't hide the real address.
    decides(
        rules,
        connection(Some("one.one.one.one"), "1.1.1.1", 443),
        DENY,
        "-1.1.1.1",
    );
}

// spec(NETWORK): An IPv4-mapped IPv6 address, such as `::ffff:1.1.1.1`, meets the rules for its IPv4 address as well as its own.
#[test]
fn an_ipv4_mapped_address_meets_the_ipv4_rules_and_its_own() {
    let rules = "NETWORK +\nNETWORK -1.1.1.0/24\n";
    decides(
        rules,
        connection(None, "::ffff:1.1.1.1", 443),
        DENY,
        "-1.1.1.0/24",
    );
    let rules = "NETWORK -\nNETWORK +[::ffff:10.0.0.0]/104\n";
    decides(
        rules,
        connection(None, "10.9.0.1", 443),
        ALLOW,
        "+[::ffff:10.0.0.0]/104",
    );
    for v6 in ["[::]/0", "[::fffe:0:0]/95", "[::ffff:0:0]/96"] {
        let rules = format!("NETWORK +\nNETWORK -{v6}\n");
        let by = format!("-{v6}");
        decides(&rules, connection(None, "::ffff:1.1.1.1", 443), DENY, &by);
    }
    // An IPv6 rule that holds no IPv4-mapped address isn't an IPv4 rule.
    let rules = "NETWORK +\nNETWORK -[::]/0\n";
    decides(rules, connection(None, "1.1.1.1", 443), ALLOW, "default");
    // The longest prefix still wins, counted in IPv6 bits.
    let rules = "NETWORK -\nNETWORK -[::]/0\nNETWORK +10.0.0.0/8\n";
    decides(
        rules,
        connection(None, "::ffff:10.0.0.1", 443),
        ALLOW,
        "+10.0.0.0/8",
    );
    let rules = "NETWORK +\nNETWORK +10.0.0.0/8\nNETWORK -[::ffff:10.1.0.0]/112\n";
    let by = "-[::ffff:10.1.0.0]/112";
    decides(rules, connection(None, "10.1.0.1", 443), DENY, by);
    decides(rules, connection(None, "::ffff:10.1.0.1", 443), DENY, by);
}

// spec(NETWORK): | URL prefix | `+https://github.com/jgabor/agent-plus` | That scheme and exact host, and paths under that prefix |
#[test]
fn a_url_rule_matches_its_scheme_host_and_paths() {
    let rules = "NETWORK -\nNETWORK +https://github.com/jgabor/agent-plus\n";
    let by = "+https://github.com/jgabor/agent-plus";
    for url in [
        "https://github.com/jgabor/agent-plus",
        "https://github.com/jgabor/agent-plus/",
        "https://github.com/jgabor/agent-plus/pulls?q=is%3Aopen",
        "https://github.com:443/jgabor/agent-plus/issues",
    ] {
        decides(rules, request(url), ALLOW, by);
    }
    for url in [
        "http://github.com/jgabor/agent-plus",
        "https://github.com:8443/jgabor/agent-plus",
        "https://github.com/jgabor",
        "https://github.com/",
    ] {
        decides(rules, request(url), DENY, "default");
    }
    // Without a request to read, no URL rule can match.
    decides(
        rules,
        connection(Some("github.com"), POOL, 443),
        DENY,
        "default",
    );
}

// spec(NETWORK): `kagi.com` matches `api.kagi.com`, but never `kagi.com.evil.net` or `notkagi.com`.
#[test]
fn a_host_rule_matches_on_label_boundaries() {
    let rules = "NETWORK -\nNETWORK +kagi.com\n";
    decides(
        rules,
        connection(Some("api.kagi.com"), POOL, 443),
        ALLOW,
        "+kagi.com",
    );
    for name in ["kagi.com.evil.net", "notkagi.com", "com", "kagi.co"] {
        decides(rules, connection(Some(name), POOL, 443), DENY, "default");
    }
}

// spec(NETWORK): `https://github.com/jgabor/agent-plus` matches `https://github.com/jgabor/agent-plus/issues`, but never `https://github.com/jgabor/agent-plus-evil`.
// spec(NETWORK): URL rules match the exact host, not subdomains.
#[test]
fn a_url_rule_matches_on_segment_boundaries_and_its_exact_host() {
    let rules = "NETWORK -\nNETWORK +https://github.com/jgabor/agent-plus\n";
    let by = "+https://github.com/jgabor/agent-plus";
    decides(
        rules,
        request("https://github.com/jgabor/agent-plus/issues"),
        ALLOW,
        by,
    );
    for url in [
        "https://github.com/jgabor/agent-plus-evil",
        "https://www.github.com/jgabor/agent-plus",
        "https://api.github.com/jgabor/agent-plus",
    ] {
        decides(rules, request(url), DENY, "default");
    }
}

// spec(NETWORK): A URL rule covers only its own paths on its own host.
// spec(NETWORK): The rule above doesn't allow `git clone https://github.com/jgabor/agent-plus.git`, because `agent-plus.git` is a different path segment.
#[test]
fn a_url_rule_covers_only_its_own_paths() {
    let rules = "NETWORK -\nNETWORK +https://github.com/jgabor/agent-plus\n";
    for url in [
        "https://github.com/jgabor/agent-plus.git/info/refs?service=git-upload-pack",
        "https://github.com/jgabor/agent-plus.git",
        "https://github.com/torvalds/linux",
        "https://objects.githubusercontent.com/jgabor/agent-plus",
    ] {
        decides(rules, request(url), DENY, "default");
    }
}

// spec(NETWORK): Request paths are normalized (RFC 3986) before matching.
#[test]
fn request_paths_are_normalized_before_matching() {
    let rules = "NETWORK -\nNETWORK +https://github.com/jgabor/agent-plus\n";
    let by = "+https://github.com/jgabor/agent-plus";
    for url in [
        "https://github.com/jgabor/agent%2Dplus/issues",
        "https://github.com/%6agabor/agent-plus",
        "https://github.com/%6Agabor/agent-plus",
    ] {
        decides(rules, request(url), ALLOW, by);
    }
    // An escape of a reserved character stays one, so it is another path.
    decides(
        rules,
        request("https://github.com/jgabor/agent-plus%3B"),
        DENY,
        "default",
    );
    // A rule that escapes an unreserved character matches it unescaped.
    let rules = "NETWORK -\nNETWORK +https://example.com/%7Euser\n";
    decides(
        rules,
        request("https://example.com/~user/a"),
        ALLOW,
        "+https://example.com/%7Euser",
    );
    // And an escape can't slip past a deny rule.
    let rules = "NETWORK +\nNETWORK -https://github.com/jgabor/secret\n";
    let by = "-https://github.com/jgabor/secret";
    decides(
        rules,
        request("https://github.com/jgabor/%73ecret/x"),
        DENY,
        by,
    );
    decides(
        rules,
        request("https://github.com/JGABOR/secret"),
        ALLOW,
        "default",
    );
    // A rule's own escaped dots never move it to another path.
    let rules = "NETWORK -\nNETWORK +https://a.com/public/%2e%2e/secret\n";
    decides(rules, request("https://a.com/secret"), DENY, "default");
}

// spec(NETWORK): Requests whose paths contain encoded slashes (`%2F`) or dot segments under a host with URL rules are denied.
// spec(NETWORK): So are paths there that don't start with `/`, and paths with a `%` that two hex digits don't follow.
#[test]
fn bad_paths_under_a_host_with_url_rules_are_denied() {
    let rules = "NETWORK +\nNETWORK -https://github.com/jgabor/secret\n";
    for url in [
        "https://github.com/jgabor%2Fsecret",
        "https://github.com/jgabor%2fsecret",
        "https://github.com/x/../jgabor/secret",
        "https://github.com/jgabor/./secret",
        "https://github.com/jgabor/secret/..",
        "https://github.com/x/%2E%2E/jgabor/secret",
        "https://github.com/x/.%2e/jgabor/secret",
        "https://github.com/jgabor/%",
        "https://github.com/jgabor/%4",
        "https://github.com/jgabor/%zzsecret",
    ] {
        decides(rules, request(url), DENY, "path");
    }
    for target in ["*", "jgabor/secret", "https://github.com/jgabor/secret", ""] {
        let mut connection = request("https://github.com/");
        connection.request = Some(Request {
            scheme: Scheme::Https,
            target,
        });
        decides(rules, connection, DENY, "path");
    }
    // A query isn't part of the path.
    decides(
        rules,
        request("https://github.com/x?to=a%2Fb/../c"),
        ALLOW,
        "default",
    );
    // Without URL rules for the host, the path doesn't matter.
    let rules = "NETWORK -\nNETWORK +github.com\nNETWORK +https://codeberg.org/x\n";
    decides(
        rules,
        request("https://github.com/a/../b%2Fc%"),
        ALLOW,
        "+github.com",
    );
}

// spec(NETWORK): **Evaluation.** The most specific matching rule wins, so line order never matters.
#[test]
fn line_order_never_matters() {
    let lines = [
        "NETWORK +github.com\n",
        "NETWORK -gist.github.com\n",
        "NETWORK -https://github.com/jgabor\n",
        "NETWORK +https://github.com/jgabor/agent-plus\n",
        "NETWORK -140.82.0.0/16\n",
        "NETWORK +140.82.112.0/20\n",
    ];
    let cases = [
        (request("https://github.com/x"), ALLOW, "+github.com"),
        (
            request("https://gist.github.com/x"),
            DENY,
            "-gist.github.com",
        ),
        (
            request("https://github.com/jgabor/x"),
            DENY,
            "-https://github.com/jgabor",
        ),
        (
            request("https://github.com/jgabor/agent-plus/x"),
            ALLOW,
            "+https://github.com/jgabor/agent-plus",
        ),
        (connection(None, "140.82.1.1", 443), DENY, "-140.82.0.0/16"),
        (
            connection(None, "140.82.113.4", 443),
            ALLOW,
            "+140.82.112.0/20",
        ),
    ];
    let mut order: Vec<usize> = (0..lines.len()).collect();
    for turn in 0..lines.len() {
        order.rotate_left(1);
        if turn % 2 == 1 {
            order.reverse();
        }
        let rules: String = order.iter().map(|&at| lines[at]).collect();
        for &(connection, sign, by) in &cases {
            decides(&format!("NETWORK -\n{rules}"), connection, sign, by);
        }
    }
}

// spec(NETWORK): **Name decision.** Among URL and host rules that match the connection's name, take the most specific.
// spec(NETWORK): A URL rule beats a host rule, a longer path beats a shorter one, and a host with more labels beats one with fewer.
// spec(NETWORK): A rule with a port beats the same rule without one.
#[test]
fn the_most_specific_name_rule_decides() {
    let rules = concat!(
        "NETWORK +\n",
        "NETWORK -github.com\n",
        "NETWORK +api.github.com\n",
        "NETWORK -x.api.github.com\n",
        "NETWORK +https://github.com/\n",
        "NETWORK -https://github.com/jgabor\n",
        "NETWORK +https://github.com/jgabor/agent-plus\n",
        "NETWORK -gitlab.com:22\n",
        "NETWORK +gitlab.com\n",
        "NETWORK -https://codeberg.org:8443/x\n",
        "NETWORK +https://codeberg.org/x\n",
    );
    decides(
        rules,
        connection(Some("github.com"), POOL, 22),
        DENY,
        "-github.com",
    );
    decides(
        rules,
        request("https://github.com/torvalds"),
        ALLOW,
        "+https://github.com/",
    );
    decides(
        rules,
        request("https://github.com/jgabor/x"),
        DENY,
        "-https://github.com/jgabor",
    );
    let by = "+https://github.com/jgabor/agent-plus";
    decides(
        rules,
        request("https://github.com/jgabor/agent-plus/x"),
        ALLOW,
        by,
    );
    decides(
        rules,
        request("https://api.github.com/x"),
        ALLOW,
        "+api.github.com",
    );
    decides(
        rules,
        request("https://x.api.github.com/x"),
        DENY,
        "-x.api.github.com",
    );
    decides(
        rules,
        connection(Some("gitlab.com"), POOL, 22),
        DENY,
        "-gitlab.com:22",
    );
    decides(
        rules,
        connection(Some("gitlab.com"), POOL, 443),
        ALLOW,
        "+gitlab.com",
    );
    let by = "-https://codeberg.org:8443/x";
    decides(rules, request("https://codeberg.org:8443/x/y"), DENY, by);
    decides(
        rules,
        request("https://codeberg.org/x/y"),
        ALLOW,
        "+https://codeberg.org/x",
    );
    // A URL rule written with the scheme's own port beats one without it.
    let rules = "NETWORK -\nNETWORK -https://codeberg.org/x\nNETWORK +https://codeberg.org:443/x\n";
    decides(
        rules,
        request("https://codeberg.org/x"),
        ALLOW,
        "+https://codeberg.org:443/x",
    );
}

// spec(NETWORK): **Address decision.** Among IP rules that match the destination address, take the one with the longest prefix.
// spec(NETWORK): A rule with a port beats the same rule without one.
#[test]
fn the_longest_ip_prefix_decides() {
    let rules = concat!(
        "NETWORK +\n",
        "NETWORK -10.0.0.0/8\n",
        "NETWORK +10.1.0.0/16\n",
        "NETWORK -10.1.2.3\n",
        "NETWORK +10.1.2.3:443\n",
        "NETWORK +[fd00::]/8\n",
        "NETWORK -[fd00::]/8:22\n",
        "NETWORK -0.0.0.0/0\n",
    );
    decides(rules, connection(None, "10.9.9.9", 80), DENY, "-10.0.0.0/8");
    decides(
        rules,
        connection(None, "10.1.9.9", 80),
        ALLOW,
        "+10.1.0.0/16",
    );
    decides(rules, connection(None, "10.1.2.3", 80), DENY, "-10.1.2.3");
    decides(
        rules,
        connection(None, "10.1.2.3", 443),
        ALLOW,
        "+10.1.2.3:443",
    );
    decides(
        rules,
        connection(None, "192.0.2.1", 443),
        DENY,
        "-0.0.0.0/0",
    );
    decides(
        rules,
        connection(None, "fd12::1", 443),
        ALLOW,
        "+[fd00::]/8",
    );
    decides(
        rules,
        connection(None, "fd12::1", 22),
        DENY,
        "-[fd00::]/8:22",
    );
    decides(rules, connection(None, "2001:db8::1", 22), ALLOW, "default");
}

// spec(NETWORK): If both decisions exist and disagree, deny.
#[test]
fn a_name_and_an_address_that_disagree_deny() {
    let rules = "NETWORK +\nNETWORK +github.com\nNETWORK -140.82.0.0/16\n";
    decides(
        rules,
        connection(Some("github.com"), "140.82.1.1", 443),
        DENY,
        "-140.82.0.0/16",
    );
    let rules = "NETWORK -\nNETWORK -github.com\nNETWORK +140.82.0.0/16\n";
    decides(
        rules,
        connection(Some("github.com"), "140.82.1.1", 443),
        DENY,
        "-github.com",
    );
    // When they agree, the name rule is the one named.
    let rules = "NETWORK -\nNETWORK +github.com\nNETWORK +140.82.0.0/16\n";
    decides(
        rules,
        connection(Some("github.com"), "140.82.1.1", 443),
        ALLOW,
        "+github.com",
    );
}

// spec(NETWORK): If only one exists, it decides.
#[test]
fn a_lone_decision_decides() {
    let rules = "NETWORK -\nNETWORK +github.com\nNETWORK +10.0.0.0/8\n";
    decides(
        rules,
        connection(Some("github.com"), POOL, 443),
        ALLOW,
        "+github.com",
    );
    decides(
        rules,
        connection(Some("example.com"), "10.0.0.1", 443),
        ALLOW,
        "+10.0.0.0/8",
    );
    decides(
        rules,
        connection(None, "10.0.0.1", 443),
        ALLOW,
        "+10.0.0.0/8",
    );
    let rules = "NETWORK +\nNETWORK -github.com\nNETWORK -10.0.0.0/8\n";
    decides(
        rules,
        connection(Some("github.com"), POOL, 443),
        DENY,
        "-github.com",
    );
    decides(
        rules,
        connection(Some("example.com"), "10.0.0.1", 443),
        DENY,
        "-10.0.0.0/8",
    );
}

// spec(NETWORK): If neither exists, the default decides.
#[test]
fn without_a_decision_the_default_decides() {
    let rules = "NETWORK +github.com\nNETWORK -10.0.0.0/8\n";
    for default in [ALLOW, DENY] {
        let network = network(rules);
        for connection in [
            connection(Some("example.com"), POOL, 443),
            connection(None, POOL, 443),
            request("https://example.com/"),
        ] {
            let decision = decide(&network.rules, default, &connection);
            assert_eq!((decision.sign, decision.by), (default, By::Default));
        }
    }
}

// spec(NETWORK): Two equally specific rules with opposite signs resolve to deny.
#[test]
fn equally_specific_rules_with_opposite_signs_deny() {
    let rules = "NETWORK +\nNETWORK +github.com\nNETWORK -github.com\n";
    decides(
        rules,
        connection(Some("github.com"), POOL, 443),
        DENY,
        "-github.com",
    );
    let rules = "NETWORK -\nNETWORK -1.1.1.0/24\nNETWORK +1.1.1.0/24\n";
    decides(rules, connection(None, "1.1.1.1", 443), DENY, "-1.1.1.0/24");
    let rules =
        "NETWORK +\nNETWORK +https://a.com/x\nNETWORK -http://a.com/x\nNETWORK -https://a.com/x\n";
    decides(
        rules,
        request("https://a.com/x/y"),
        DENY,
        "-https://a.com/x",
    );
    // Rules of one length and sign but other hosts are as specific.
    let rules = "NETWORK -\nNETWORK +a.kagi.com\nNETWORK -kagi.com\nNETWORK +b.kagi.com\n";
    decides(
        rules,
        connection(Some("a.kagi.com"), POOL, 443),
        ALLOW,
        "+a.kagi.com",
    );
}

// spec(NETWORK): | `https://github.com/torvalds/linux` | allow | `+github.com` |
// spec(NETWORK): | `https://gist.github.com/x` | deny | `-gist.github.com` |
// spec(NETWORK): | `https://codeberg.org/forgejo/forgejo/issues` | allow | URL rule |
// spec(NETWORK): | `https://codeberg.org/other/repo` | deny | default `-` |
#[test]
fn the_worked_example_decides_as_its_table_says() {
    let rules = concat!(
        "NETWORK -\n",
        "NETWORK +github.com\n",
        "NETWORK -gist.github.com\n",
        "NETWORK +https://codeberg.org/forgejo/forgejo\n",
    );
    decides(
        rules,
        request("https://github.com/torvalds/linux"),
        ALLOW,
        "+github.com",
    );
    decides(
        rules,
        request("https://gist.github.com/x"),
        DENY,
        "-gist.github.com",
    );
    let by = "+https://codeberg.org/forgejo/forgejo";
    decides(
        rules,
        request("https://codeberg.org/forgejo/forgejo/issues"),
        ALLOW,
        by,
    );
    decides(
        rules,
        request("https://codeberg.org/other/repo"),
        DENY,
        "default",
    );
}
