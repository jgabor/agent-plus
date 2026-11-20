//! The rules that `ap run --network` adds for one run, merged with the
//! Agentfile's, by the command line under NETWORK in SPEC.md.

use std::net::IpAddr;

use agentfile::{Error, Instruction, Network, Sign, parse, with_typed_network};
use net::{Connection, decide};

/// The Agentfile's `lines`, after an `AGENT` line, with `typed` from the
/// command line.
fn merged(lines: &str, typed: &[&str]) -> Result<Vec<Instruction>, Vec<Error>> {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    let typed: Vec<String> = typed.iter().map(|rule| rule.to_string()).collect();
    with_typed_network(&instructions, &typed)
}

/// The sign a connection to `name` on port 443 gets under the merged rules.
fn sign(lines: &str, typed: &[&str], name: &str) -> Sign {
    let merged = merged(lines, typed).unwrap_or_else(|errors| panic!("{errors:?}"));
    let network = Network::read(&merged).unwrap();
    let connection = Connection {
        name: Some(name),
        address: "192.0.2.1".parse::<IpAddr>().unwrap(),
        port: 443,
        request: None,
    };
    decide(&network.rules, network.default, &connection).sign
}

/// The messages of the errors the merge returns.
fn refused(lines: &str, typed: &[&str]) -> Vec<String> {
    let errors = merged(lines, typed).expect_err("the merged rules were accepted");
    errors.into_iter().map(|error| error.message).collect()
}

// spec(NETWORK): **Command line.** `ap run --network=<rule>` adds rules for a single run.
#[test]
fn a_typed_rule_adds_to_the_agentfile_s_rules() {
    let lines = "NETWORK -\nNETWORK +kagi.com\n";
    assert_eq!(sign(lines, &[], "example.com"), Sign::Deny);
    assert_eq!(sign(lines, &["+example.com"], "example.com"), Sign::Allow);
    // The Agentfile's own rules still apply.
    assert_eq!(sign(lines, &["+example.com"], "kagi.com"), Sign::Allow);
    assert_eq!(sign(lines, &["+example.com"], "other.net"), Sign::Deny);
}

// spec(NETWORK): A bare rule on the command line replaces the Agentfile's default, and command-line rules win ties against Agentfile rules.
#[test]
fn a_typed_bare_rule_replaces_the_default() {
    assert_eq!(sign("NETWORK -\n", &["+"], "example.com"), Sign::Allow);
    assert_eq!(sign("NETWORK +\n", &["-"], "example.com"), Sign::Deny);
    assert_eq!(sign("", &["no"], "example.com"), Sign::Deny);
    // The Agentfile's rules still beat the new default.
    assert_eq!(
        sign("NETWORK -\nNETWORK -evil.net\n", &["+"], "evil.net"),
        Sign::Deny
    );
}

// spec(NETWORK): A bare rule on the command line replaces the Agentfile's default, and command-line rules win ties against Agentfile rules.
#[test]
fn a_typed_rule_wins_a_tie_against_an_agentfile_rule() {
    assert_eq!(
        sign("NETWORK +\nNETWORK +example.com\n", &["-example.com"], "example.com"),
        Sign::Deny
    );
    // Two equally specific rules in the Agentfile would deny, so this one
    // shows the typed rule winning, not deny winning.
    assert_eq!(
        sign("NETWORK -\nNETWORK -example.com\n", &["+example.com"], "example.com"),
        Sign::Allow
    );
    assert_eq!(
        sign("NETWORK -\nNETWORK -192.0.2.0/24\n", &["+192.0.2.0/24"], "x.org"),
        Sign::Allow
    );
    // A more specific Agentfile rule still beats a typed one.
    assert_eq!(
        sign("NETWORK -\nNETWORK -api.example.com\n", &["+example.com"], "api.example.com"),
        Sign::Deny
    );
    // A rule for another port is no tie.
    assert_eq!(
        sign("NETWORK -\nNETWORK +example.com:443\n", &["-example.com"], "example.com"),
        Sign::Allow
    );
    // Typed rules that tie with each other deny, as Agentfile rules do.
    assert_eq!(
        sign("NETWORK -\n", &["+example.com", "-example.com"], "example.com"),
        Sign::Deny
    );
}

// spec(NETWORK): The checks that `ap build` runs on `NETWORK` and `SECRET` rules run again on the merged rules, and a failure fails the run.
#[test]
fn the_build_checks_run_on_the_merged_rules() {
    // The bound host was allowed when the agent was built.
    let lines = "SECRET STRIPE_API_KEY @api.stripe.com\nNETWORK +\n";
    merged(lines, &[]).unwrap();
    let denied = "`NETWORK` denies `api.stripe.com`, a bound host of `STRIPE_API_KEY`";
    for typed in [&["-"][..], &["-stripe.com"], &["-api.stripe.com"]] {
        let messages = refused(lines, typed);
        assert!(
            messages.iter().any(|message| message.starts_with(denied)),
            "{typed:?}: {messages:?}"
        );
    }
    assert_eq!(
        refused("NETWORK -\n", &["+", "-"]),
        ["`--network=-` disagrees with `--network=+`"]
    );
    assert_eq!(
        refused("NETWORK -\n", &["example.com"]),
        ["`--network=example.com`: a `NETWORK` rule starts with `+` or `-`, but found `example.com`"]
    );
    let [message] = &refused("NETWORK -\n", &["+[::1"])[..] else {
        panic!("not one error");
    };
    assert!(message.starts_with("`--network=+[::1`: "), "{message}");
}
