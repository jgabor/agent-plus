//! The arguments of `NETWORK` and `POLICY`, and the typed rules they parse
//! into.

use std::net::IpAddr;

use agentfile::{Destination, Error, Network, NetworkRule, Policy, Scheme, Sign, parse};

/// Parses `lines` after an `AGENT` line, and returns its network rules.
fn network_of(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

/// Parses `lines` after an `AGENT` line, and returns its policy.
fn policy_of(lines: &str) -> Policy {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Policy::read(&instructions).unwrap()
}

/// Parses `lines` after an `AGENT` line, and returns its errors.
fn errors_of(lines: &str) -> Vec<Error> {
    parse(&format!("AGENT claude\n{lines}")).unwrap_err()
}

/// Parses `line` after an `AGENT` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = errors_of(line);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.line, 2, "{error:?}");
    error
}

/// The one rule of a `NETWORK` line.
fn rule_of(line: &str) -> NetworkRule {
    let mut rules = network_of(line).rules;
    assert_eq!(rules.len(), 1, "{rules:?}");
    rules.remove(0)
}

fn destination_of(line: &str) -> Destination {
    rule_of(line).destination
}

fn host(name: &str, port: Option<u16>) -> Destination {
    Destination::Host {
        name: name.to_string(),
        port,
    }
}

fn address(network: &str, prefix: u8, port: Option<u16>) -> Destination {
    Destination::Address {
        network: network.parse::<IpAddr>().unwrap(),
        prefix,
        port,
    }
}

fn url(scheme: Scheme, name: &str, port: Option<u16>, path: &str) -> Destination {
    Destination::Url {
        scheme,
        host: name.to_string(),
        port,
        path: path.to_string(),
    }
}

// spec(NETWORK): `+` allows and `-` denies.
#[test]
fn network_reads_the_sign_of_a_rule() {
    assert_eq!(rule_of("NETWORK +kagi.com\n").sign, Sign::Allow);
    assert_eq!(rule_of("NETWORK -kagi.com\n").sign, Sign::Deny);
}

// spec(NETWORK): Without a bare rule, the default is `+`.
#[test]
fn network_defaults_to_allow() {
    assert_eq!(network_of("").default, Sign::Allow);
    assert_eq!(network_of("NETWORK -kagi.com\n").default, Sign::Allow);
    assert_eq!(network_of("NETWORK +kagi.com\n").default, Sign::Allow);
}

#[test]
fn a_bare_network_rule_sets_the_default() {
    let network = network_of("NETWORK -\nNETWORK +kagi.com\n");
    assert_eq!(network.default, Sign::Deny);
    assert_eq!(
        network.rules,
        [NetworkRule {
            sign: Sign::Allow,
            destination: host("kagi.com", None),
        }]
    );
    assert_eq!(network_of("NETWORK +\n").default, Sign::Allow);
}

// spec(NETWORK): `true`, `yes`, and `1` mean `+`; `false`, `no`, and `0` mean `-`.
#[test]
fn network_spells_a_bare_rule_as_a_word() {
    for word in ["true", "yes", "1"] {
        let network = network_of(&format!("NETWORK -kagi.com\nNETWORK {word}\n"));
        assert_eq!(network.default, Sign::Allow, "{word}");
    }
    for word in ["false", "no", "0"] {
        let network = network_of(&format!("NETWORK {word}\n"));
        assert_eq!(network.default, Sign::Deny, "{word}");
    }
}

#[test]
fn the_words_are_matched_exactly() {
    for word in ["True", "YES", "on", "off", "01"] {
        let error = error_of(&format!("NETWORK {word}\n"));
        assert!(error.message.contains(word), "{error:?}");
    }
}

// spec(NETWORK): Two bare rules that disagree are an error.
#[test]
fn network_bare_rules_that_disagree_are_an_error() {
    let errors = errors_of("NETWORK -\nNETWORK +kagi.com\nNETWORK yes\n");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].line, 4, "{errors:?}");
    assert!(errors[0].message.contains("line 2"), "{errors:?}");
}

#[test]
fn network_bare_rules_that_agree_are_fine() {
    assert_eq!(
        network_of("NETWORK -\nNETWORK false\nNETWORK 0\n").default,
        Sign::Deny
    );
}

#[test]
fn a_host_rule_reads_its_name_and_port() {
    assert_eq!(
        destination_of("NETWORK +kagi.com\n"),
        host("kagi.com", None)
    );
    assert_eq!(
        destination_of("NETWORK +github.com:22\n"),
        host("github.com", Some(22))
    );
    assert_eq!(
        destination_of("NETWORK +host.ap.internal:8080\n"),
        host("host.ap.internal", Some(8080))
    );
}

#[test]
fn a_host_name_is_read_in_lowercase() {
    assert_eq!(
        destination_of("NETWORK +GitHub.COM\n"),
        host("github.com", None)
    );
}

#[test]
fn an_ip_rule_reads_its_address_prefix_and_port() {
    assert_eq!(
        destination_of("NETWORK -1.1.1.1\n"),
        address("1.1.1.1", 32, None)
    );
    assert_eq!(
        destination_of("NETWORK +10.1.0.0/16\n"),
        address("10.1.0.0", 16, None)
    );
    assert_eq!(
        destination_of("NETWORK +10.1.0.0/16:443\n"),
        address("10.1.0.0", 16, Some(443))
    );
    assert_eq!(
        destination_of("NETWORK -[2606:4700::]/32:443\n"),
        address("2606:4700::", 32, Some(443))
    );
    assert_eq!(
        destination_of("NETWORK +[2606:4700::1111]\n"),
        address("2606:4700::1111", 128, None)
    );
    assert_eq!(
        destination_of("NETWORK +[2606:4700::1111]:53\n"),
        address("2606:4700::1111", 128, Some(53))
    );
}

// spec(NETWORK): IPv6 addresses go in brackets.
#[test]
fn an_ipv6_address_without_brackets_is_an_error() {
    for line in [
        "NETWORK -2606:4700::\n",
        "NETWORK -2606:4700::/32\n",
        "NETWORK +::1\n",
    ] {
        let error = error_of(line);
        assert!(error.message.contains("brackets"), "{line}: {error:?}");
    }
}

#[test]
fn brackets_hold_only_an_ipv6_address() {
    for line in [
        "NETWORK +[1.1.1.1]\n",
        "NETWORK +[kagi.com]\n",
        "NETWORK +[2606:4700::/32]\n",
        "NETWORK +[2606:4700::\n",
    ] {
        error_of(line);
    }
}

#[test]
fn a_prefix_must_fit_its_address() {
    for line in [
        "NETWORK +10.0.0.0/33\n",
        "NETWORK +[2606:4700::]/129\n",
        "NETWORK +10.0.0.0/\n",
        "NETWORK +10.0.0.0/x\n",
        "NETWORK +10.0.0.0/08\n",
    ] {
        error_of(line);
    }
}

#[test]
fn a_prefix_with_host_bits_set_is_an_error() {
    let error = error_of("NETWORK +10.1.2.3/16\n");
    assert!(error.message.contains("10.1.0.0/16"), "{error:?}");
}

#[test]
fn a_port_must_be_from_1_to_65535() {
    for line in [
        "NETWORK +github.com:0\n",
        "NETWORK +github.com:65536\n",
        "NETWORK +github.com:\n",
        "NETWORK +github.com:ssh\n",
        "NETWORK +1.1.1.1:+53\n",
    ] {
        error_of(line);
    }
    assert_eq!(
        destination_of("NETWORK +github.com:65535\n"),
        host("github.com", Some(65535))
    );
}

#[test]
fn a_host_rule_needs_a_host_name() {
    for line in [
        "NETWORK +*.kagi.com\n",
        "NETWORK +kagi..com\n",
        "NETWORK +.kagi.com\n",
        "NETWORK +kagi.com.\n",
        "NETWORK +-kagi.com\n",
        "NETWORK +kagi-.com\n",
        "NETWORK +kagi_search.com\n",
        "NETWORK +1.1.1\n",
        "NETWORK +256.1.1.1\n",
        "NETWORK +user@kagi.com\n",
        "NETWORK +kagi.com/search\n",
        "NETWORK \"+kagi .com\"\n",
    ] {
        error_of(line);
    }
}

#[test]
fn a_label_is_at_most_63_characters() {
    let label = "a".repeat(63);
    assert_eq!(
        destination_of(&format!("NETWORK +{label}.com\n")),
        host(&format!("{label}.com"), None)
    );
    error_of(&format!("NETWORK +a{label}.com\n"));
}

#[test]
fn a_url_rule_reads_its_scheme_host_and_path() {
    assert_eq!(
        destination_of("NETWORK +https://github.com/jgabor/agent-plus\n"),
        url(Scheme::Https, "github.com", None, "/jgabor/agent-plus")
    );
    assert_eq!(
        destination_of("NETWORK -http://Example.com:8080/a\n"),
        url(Scheme::Http, "example.com", Some(8080), "/a")
    );
    assert_eq!(
        destination_of("NETWORK +https://codeberg.org\n"),
        url(Scheme::Https, "codeberg.org", None, "/")
    );
}

#[test]
fn a_url_rule_takes_only_http_and_https() {
    for line in [
        "NETWORK +ftp://example.com/a\n",
        "NETWORK +ssh://github.com/jgabor\n",
    ] {
        error_of(line);
    }
}

#[test]
fn a_url_scheme_is_read_in_any_case() {
    assert_eq!(
        destination_of("NETWORK +HTTPS://github.com/jgabor\n"),
        url(Scheme::Https, "github.com", None, "/jgabor")
    );
}

#[test]
fn a_url_path_is_written_as_rfc_3986_allows() {
    for line in [
        "NETWORK \"+https://github.com/my repo\"\n",
        "NETWORK +https://github.com/%zz\n",
        "NETWORK +https://github.com/a%2\n",
        "NETWORK +https://github.com/a\\b\n",
        "NETWORK +https://github.com/caf\u{e9}\n",
    ] {
        error_of(line);
    }
    assert_eq!(
        destination_of("NETWORK +https://example.com/a%20b/~c:d@e!$&'()*+,;=\n"),
        url(
            Scheme::Https,
            "example.com",
            None,
            "/a%20b/~c:d@e!$&'()*+,;="
        )
    );
}

#[test]
fn a_url_rule_is_a_host_and_a_plain_path() {
    for line in [
        "NETWORK +https:///jgabor\n",
        "NETWORK +https://user@github.com/jgabor\n",
        "NETWORK +https://github.com/jgabor?tab=repositories\n",
        "NETWORK +https://github.com/jgabor#readme\n",
        "NETWORK +https://github.com/jgabor/../torvalds\n",
        "NETWORK +https://github.com/./jgabor\n",
        "NETWORK +https://github.com/jgabor%2Fagent-plus\n",
        "NETWORK +https://github.com/jgabor%2fagent-plus\n",
        "NETWORK +https://1.1.1.1/dns-query\n",
        "NETWORK +https://github.com:0/jgabor\n",
    ] {
        error_of(line);
    }
}

#[test]
fn a_network_rule_needs_a_sign() {
    let error = error_of("NETWORK kagi.com\n");
    assert!(error.message.contains("`+`"), "{error:?}");
}

#[test]
fn network_takes_one_rule_per_line() {
    error_of("NETWORK\n");
    error_of("NETWORK +kagi.com +github.com\n");
    error_of("NETWORK \"\"\n");
}

#[test]
fn every_bad_rule_is_reported_on_its_own_line() {
    let errors = errors_of("NETWORK kagi.com\nNETWORK +ok.com\nNETWORK +::1\nPOLICY +Bad\n");
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [2, 4, 5], "{errors:?}");
}

#[test]
fn network_keeps_its_rules_in_file_order() {
    let network = network_of("NETWORK -gist.github.com\nNETWORK +github.com\n");
    assert_eq!(
        network.rules,
        [
            NetworkRule {
                sign: Sign::Deny,
                destination: host("gist.github.com", None),
            },
            NetworkRule {
                sign: Sign::Allow,
                destination: host("github.com", None),
            },
        ]
    );
}

// spec(POLICY): `+` allows and `-` denies.
#[test]
fn policy_reads_the_sign_of_a_rule() {
    let policy = policy_of("POLICY +reviewer\nPOLICY -stranger\n");
    assert!(policy.allows("reviewer"));
    assert!(!policy.allows("stranger"));
}

// spec(POLICY): | no `POLICY` line | every agent in your store |
#[test]
fn no_policy_line_may_call_every_agent() {
    let policy = policy_of("");
    assert_eq!(policy.default, Sign::Allow);
    assert!(policy.allows("reviewer"));
}

// spec(POLICY): | `POLICY +` | every agent in your store |
#[test]
fn policy_plus_may_call_every_agent() {
    let policy = policy_of("POLICY +\n");
    assert_eq!(policy.default, Sign::Allow);
    assert!(policy.allows("reviewer"));
}

// spec(POLICY): | `POLICY -` | no agent |
#[test]
fn policy_minus_may_call_no_agent() {
    let policy = policy_of("POLICY -\n");
    assert_eq!(policy.default, Sign::Deny);
    assert!(!policy.allows("reviewer"));
}

// spec(POLICY): | `POLICY +reviewer` | `reviewer` only |
#[test]
fn policy_plus_an_agent_may_call_only_it() {
    let policy = policy_of("POLICY +reviewer\n");
    assert!(policy.allows("reviewer"));
    assert!(!policy.allows("tester"));
}

// spec(POLICY): | `POLICY -stranger` | every agent except `stranger` |
#[test]
fn policy_minus_an_agent_may_call_every_other() {
    let policy = policy_of("POLICY -stranger\n");
    assert!(!policy.allows("stranger"));
    assert!(policy.allows("reviewer"));
}

// spec(POLICY): A bare `+` or `-` sets the default, with the same spellings as `NETWORK`.
#[test]
fn policy_spells_a_bare_rule_like_network() {
    for word in ["+", "true", "yes", "1"] {
        assert_eq!(policy_of(&format!("POLICY {word}\n")).default, Sign::Allow);
    }
    for word in ["-", "false", "no", "0"] {
        assert_eq!(policy_of(&format!("POLICY {word}\n")).default, Sign::Deny);
    }
    let policy = policy_of("POLICY +reviewer\nPOLICY yes\n");
    assert!(policy.allows("tester"));
}

// spec(POLICY): Two bare rules that disagree are an error.
#[test]
fn policy_bare_rules_that_disagree_are_an_error() {
    let errors = errors_of("POLICY no\nPOLICY +\n");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].line, 3, "{errors:?}");
    assert!(errors[0].message.contains("line 2"), "{errors:?}");
}

#[test]
fn network_and_policy_have_their_own_defaults() {
    let source = "NETWORK -\nPOLICY +\n";
    assert_eq!(network_of(source).default, Sign::Deny);
    assert_eq!(policy_of(source).default, Sign::Allow);
}

// spec(POLICY): Without a bare rule, any `+<agent>` rule makes the default `-`.
#[test]
fn an_allow_rule_makes_the_policy_default_deny() {
    assert_eq!(policy_of("POLICY +reviewer\n").default, Sign::Deny);
    assert_eq!(policy_of("POLICY -stranger\n").default, Sign::Allow);
    assert_eq!(
        policy_of("POLICY -stranger\nPOLICY +reviewer\n").default,
        Sign::Deny
    );
}

// spec(POLICY): Naming the agents it may call means only those.
#[test]
fn naming_agents_means_only_those() {
    let policy = policy_of("POLICY +reviewer\nPOLICY +tester\n");
    assert!(policy.allows("reviewer"));
    assert!(policy.allows("tester"));
    assert!(!policy.allows("writer"));
}

// spec(POLICY): `<agent>` is an exact agent name, and it can name an agent you haven't built yet.
#[test]
fn policy_names_an_exact_agent() {
    let policy = policy_of("POLICY +not-built-yet\n");
    assert!(policy.allows("not-built-yet"));
    assert!(!policy.allows("not-built"));
    assert!(!policy.allows("not-built-yet-2"));
    for line in [
        "POLICY +Reviewer\n",
        "POLICY +review*\n",
        "POLICY +-reviewer\n",
        "POLICY +reviewer-\n",
        "POLICY +re_viewer\n",
        "POLICY reviewer\n",
        "POLICY +reviewer tester\n",
        "POLICY\n",
    ] {
        error_of(line);
    }
    let longest = "a".repeat(63);
    assert!(policy_of(&format!("POLICY +{longest}\n")).allows(&longest));
    error_of(&format!("POLICY +a{longest}\n"));
}

// spec(POLICY): Two rules with opposite signs for one name resolve to deny.
#[test]
fn opposite_rules_for_one_name_resolve_to_deny() {
    for source in [
        "POLICY +reviewer\nPOLICY -reviewer\n",
        "POLICY -reviewer\nPOLICY +reviewer\n",
        "POLICY +\nPOLICY +reviewer\nPOLICY -reviewer\n",
    ] {
        assert!(!policy_of(source).allows("reviewer"), "{source}");
    }
}
