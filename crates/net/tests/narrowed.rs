//! Which names the proxy decrypts because a `+` rule that allows them has a
//! narrower `-` rule beneath it, by **Decryption** under NETWORK in SPEC.md.

use agentfile::{Network, parse};
use net::narrowed;

/// The `NETWORK` rules of `lines`, after an `AGENT` line.
fn network(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

// spec(NETWORK): under a broader rule of the opposite sign: with `+github.com` and `-gist.github.com`, on every TLS connection that `+github.com` allows, because a client can reuse one connection for several names.
#[test]
fn a_plus_rule_with_a_narrower_minus_rule_beneath_it_is_decrypted() {
    let rules = network("NETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\n");
    // Every name that `+github.com` allows, in any case, with or without a
    // final dot.
    for name in [
        "github.com",
        "api.github.com",
        "GitHub.com.",
        "x.y.github.com",
    ] {
        assert!(narrowed(&rules, name), "{name}");
    }
    // A name the `+` rule doesn't reach, or one that only ends the same.
    for name in ["example.com", "notgithub.com", "com", ""] {
        assert!(!narrowed(&rules, name), "{name}");
    }
    // A narrower `-` rule anywhere beneath the `+` rule, for a host, a URL,
    // or one port, counts. So does a `-` rule for the same name on one port.
    for narrower in [
        "-deep.api.github.com",
        "-https://github.com/x",
        "-https://api.github.com/",
        "-gist.github.com:22",
        "-github.com:80",
    ] {
        let rules = network(&format!(
            "NETWORK -\nNETWORK +github.com\nNETWORK {narrower}\n"
        ));
        assert!(narrowed(&rules, "github.com"), "{narrower}");
    }
    // A `+` rule with a port still allows a reused connection.
    let rules = network("NETWORK -\nNETWORK +github.com:443\nNETWORK -gist.github.com\n");
    assert!(narrowed(&rules, "github.com"));
    // A name under a narrower `+` rule is still under the broader one.
    let rules = network(
        "NETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\nNETWORK +api.github.com\n",
    );
    assert!(narrowed(&rules, "api.github.com"));
}

// spec(NETWORK): The default is never such a rule, so under `NETWORK +`, a `-` rule on its own never causes decryption.
#[test]
fn the_default_is_never_a_broader_rule() {
    let open = network("NETWORK +\nNETWORK -gist.github.com\n");
    assert!(!narrowed(&open, "github.com"));
    assert!(!narrowed(&open, "api.github.com"));
    // Under `NETWORK -`, a `+` rule is narrower than the default, which
    // decrypts nothing either.
    let closed = network("NETWORK -\nNETWORK +api.github.com\n");
    assert!(!narrowed(&closed, "api.github.com"));
    // An explicit `+` rule under `NETWORK +` is such a rule.
    let explicit = network("NETWORK +\nNETWORK +github.com\nNETWORK -gist.github.com\n");
    assert!(narrowed(&explicit, "github.com"));
}

#[test]
fn only_a_narrower_minus_rule_beneath_a_plus_rule_counts() {
    for lines in [
        // A broader `-` rule allows nothing to decrypt, and a narrower `+`
        // rule beneath it decides what it allows on its own.
        "NETWORK -\nNETWORK -github.com\nNETWORK +api.github.com\n",
        // A `-` rule beside the `+` rule, or above it, isn't beneath it.
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -gist.github.com\n",
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -github.com\n",
        // An IP rule isn't a narrower name.
        "NETWORK -\nNETWORK +api.github.com\nNETWORK -10.0.0.0/8\n",
        // Narrower `+` rules beneath a `+` rule.
        "NETWORK -\nNETWORK +github.com\nNETWORK +gist.github.com\n",
    ] {
        assert!(!narrowed(&network(lines), "api.github.com"), "{lines}");
    }
}
