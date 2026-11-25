//! Which connections the proxy decrypts because a `+` rule that allows them
//! has a narrower `-` rule beneath it, by **Decryption** under NETWORK in
//! SPEC.md.

use agentfile::{Network, parse};

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

/// Whether the proxy decrypts a connection to `name` on port 443 under the
/// rules of `lines`.
fn narrowed(lines: &str, name: &str) -> bool {
    net::narrowed(&network(lines), HARNESS, name, 443)
}

// spec(NETWORK): under a broader rule of the opposite sign: with `+github.com` and `-gist.github.com`, on every TLS connection that `+github.com` allows, because a client can reuse one connection for several names.
#[test]
fn a_plus_rule_with_a_narrower_minus_rule_beneath_it_is_decrypted() {
    let rules = "NETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\n";
    // Every name that `+github.com` allows, in any case, with or without a
    // final dot.
    for name in [
        "github.com",
        "api.github.com",
        "GitHub.com.",
        "x.y.github.com",
    ] {
        assert!(narrowed(rules, name), "{name}");
    }
    // A name the `+` rule doesn't reach, one that only ends the same, and
    // the name the `-` rule denies.
    for name in ["example.com", "notgithub.com", "com", "", "gist.github.com"] {
        assert!(!narrowed(rules, name), "{name}");
    }
    // A name under a narrower `+` rule is still under the broader one.
    let beneath = format!("{rules}NETWORK +api.github.com\n");
    assert!(narrowed(&beneath, "api.github.com"));
}

// spec(NETWORK): The default is never such a rule, so under `NETWORK +`, a `-` rule on its own never causes decryption.
#[test]
fn the_default_is_never_a_broader_rule() {
    let open = "NETWORK +\nNETWORK -gist.github.com\n";
    assert!(!narrowed(open, "github.com"));
    assert!(!narrowed(open, "api.github.com"));
    // Under `NETWORK -`, a `+` rule is narrower than the default, which
    // decrypts nothing either.
    assert!(!narrowed(
        "NETWORK -\nNETWORK +api.github.com\n",
        "api.github.com"
    ));
    // An explicit `+` rule under `NETWORK +` is such a rule.
    let explicit = "NETWORK +\nNETWORK +github.com\nNETWORK -gist.github.com\n";
    assert!(narrowed(explicit, "github.com"));
}

// spec(NETWORK): The narrower rule is a `-` rule for a name under the `+` rule's, a URL rule for the `+` rule's own name, or a rule for that name on one port when the `+` rule names no port.
// spec(NETWORK): Only a connection on a port that the `+` rule covers is decrypted for it.
#[test]
fn a_narrower_rule_is_under_the_plus_rule_by_name_path_or_port() {
    for narrower in [
        "-deep.api.github.com",
        "-https://api.github.com/",
        "-https://github.com/x",
        // A `-` rule for a narrower name counts on any port.
        "-gist.github.com:22",
        "-github.com:80",
    ] {
        let lines = format!("NETWORK -\nNETWORK +github.com\nNETWORK {narrower}\n");
        assert!(narrowed(&lines, "api.github.com"), "{narrower}");
    }
    for lines in [
        // A `-` rule for the same name on every port is broader by port.
        "NETWORK -\nNETWORK +github.com:443\nNETWORK -github.com\n",
        // Rules for two ports of one name are disjoint.
        "NETWORK -\nNETWORK +github.com:443\nNETWORK -github.com:22\n",
        // A broader `-` rule allows nothing to decrypt, and a narrower `+`
        // rule beneath it decides what it allows on its own.
        "NETWORK -\nNETWORK -github.com\nNETWORK +api.github.com\n",
        // A `-` rule beside the `+` rule, or above it, isn't beneath it.
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -gist.github.com\n",
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -github.com\n",
        // An IP rule isn't a narrower name, and neither is a `+` rule.
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -10.0.0.0/8\n",
        "NETWORK -\nNETWORK +github.com\nNETWORK +gist.github.com\n",
    ] {
        assert!(!narrowed(lines, "api.github.com"), "{lines}");
    }
    // A `+` rule on another port allows no connection on this one, though a
    // rule for every port does.
    let other_port = "NETWORK +\nNETWORK +github.com:22\nNETWORK -gist.github.com\n";
    assert!(!narrowed(other_port, "github.com"));
    let allowed_beside = format!("{other_port}NETWORK +api.github.com\n");
    assert!(!narrowed(&allowed_beside, "api.github.com"));
    let lines = "NETWORK -\nNETWORK +github.com:443\nNETWORK -gist.github.com\n";
    assert!(narrowed(lines, "github.com"));
    let rules = network(lines);
    assert!(!net::narrowed(&rules, HARNESS, "github.com", 22));
}

// spec(NETWORK): **Harness hosts are never decrypted for a rule.**
#[test]
fn a_harness_host_is_never_decrypted_for_a_narrower_rule() {
    let lines = "NETWORK -\nNETWORK +anthropic.com\nNETWORK -console.anthropic.com\n";
    assert!(!narrowed(lines, "api.anthropic.com"));
    assert!(!narrowed(lines, "API.Anthropic.com."));
    // Another name under the same rule is decrypted.
    assert!(narrowed(lines, "docs.anthropic.com"));
}
