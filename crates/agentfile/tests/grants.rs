//! The grants of an Agentfile, and the text form that `ap inspect --grants`
//! prints and `--approve=<file>` reads.

use agentfile::{
    Auth, Destination, Error, Grants, Location, Network, NetworkRule, PathGrant, Policy, Secret,
    Sign, parse,
};

/// Resolves a host path the way a build in `/ctx` with home `/home/u` would,
/// without symlinks: relative paths land inside the build context.
fn resolve(text: &str) -> Result<Location, String> {
    let (path, outside) = if text == "." {
        ("/ctx".to_string(), false)
    } else if let Some(rest) = text.strip_prefix("./") {
        (format!("/ctx/{rest}"), false)
    } else if let Some(rest) = text.strip_prefix("~/") {
        (format!("/home/u/{rest}"), true)
    } else if text.starts_with('/') {
        (text.to_string(), true)
    } else {
        return Err(format!("{text} doesn't exist"));
    };
    Ok(Location { path, outside })
}

/// Parses `lines` after an `AGENT claude` line, and returns their grants.
fn grants_of(lines: &str) -> Grants {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Grants::read(&instructions, resolve).unwrap()
}

fn path(path: &str, write: bool) -> PathGrant {
    PathGrant {
        path: path.to_string(),
        write,
    }
}

fn secret(name: &str, hosts: &[&str], visible: bool) -> Secret {
    Secret {
        name: name.to_string(),
        hosts: hosts.iter().map(|host| host.to_string()).collect(),
        visible,
    }
}

/// Prints `grants`, reads the text back, and checks that nothing changed.
fn round_trip(grants: &Grants) -> String {
    let text = grants.to_string();
    let read: Grants = text
        .parse()
        .unwrap_or_else(|errors| panic!("{text}\n{errors:?}"));
    assert_eq!(&read, grants, "{text}");
    text
}

fn errors_of(text: &str) -> Vec<Error> {
    text.parse::<Grants>().unwrap_err()
}

// spec(Grants): | Host paths outside the build context | Text paths, `ADD`, local sources, `SHARE` |
#[test]
fn host_paths_outside_the_context_are_grants() {
    let grants = grants_of(
        "SYSTEM ~/prompts/system.md\n\
         SYSTEM \"~/a literal, not a path\"\n\
         PROMPT /etc/prompt.md\n\
         ADD ~/notes\n\
         ADD ./fixtures\n\
         REPO /src/linux\n\
         REPO jgabor/agent-plus\n\
         PLUGIN ~/plugins/review\n\
         SKILL /skills\n\
         MCP ~/servers\n\
         MCP docs https://docs.example.com/mcp\n\
         SHARE ~/data ~/data\n\
         SHARE ./out",
    );
    assert_eq!(
        grants.paths,
        vec![
            path("/etc/prompt.md", false),
            path("/home/u/data", false),
            path("/home/u/notes", false),
            path("/home/u/plugins/review", false),
            path("/home/u/prompts/system.md", false),
            path("/home/u/servers", false),
            path("/skills", false),
            path("/src/linux", false),
        ],
    );
}

// spec(Grants): | Host paths outside the build context | Text paths, `ADD`, local sources, `SHARE` |
#[test]
fn a_symlink_that_leaves_the_context_is_a_grant() {
    let instructions = parse("AGENT claude\nADD ./link\nADD ./data").unwrap();
    let grants = Grants::read(&instructions, |text| {
        if text == "./link" {
            Ok(Location {
                path: "/home/u/.ssh".to_string(),
                outside: true,
            })
        } else {
            resolve(text)
        }
    })
    .unwrap();
    assert_eq!(grants.paths, vec![path("/home/u/.ssh", false)]);
}

#[test]
fn the_caller_reports_paths_that_fail_to_resolve() {
    let instructions = parse("AGENT claude\nADD ./data\nSHARE missing\nADD gone").unwrap();
    let errors = Grants::read(&instructions, resolve).unwrap_err();
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, vec![3, 4], "{errors:?}");
    assert!(
        errors[0].message.contains("missing doesn't exist"),
        "{errors:?}"
    );
}

// spec(Grants): | Write access to host files | `SHARE` with `rw` |
#[test]
fn a_read_write_share_is_a_write_grant() {
    let grants = grants_of(
        "SHARE ./out ~/out rw\n\
         SHARE ~/logs ~/logs rw\n\
         SHARE ./cow ~/cow\n\
         SHARE ./ro ~/ro ro\n\
         SHARE ~/notes ~/notes ro",
    );
    assert_eq!(
        grants.paths,
        vec![
            path("/ctx/out", true),
            path("/home/u/logs", true),
            path("/home/u/notes", false),
        ],
    );
}

// spec(Grants): | Write access to host files | `SHARE` with `rw` |
#[test]
fn a_path_named_twice_is_one_grant_with_the_widest_access() {
    let grants = grants_of("ADD ~/data ~/copy\nSHARE ~/data ~/data rw\nSHARE ~/data ~/again");
    assert_eq!(grants.paths, vec![path("/home/u/data", true)]);
}

// spec(Grants): | Host environment variables | `$NAME` in `ENV` values |
#[test]
fn variables_in_env_values_are_grants() {
    let grants = grants_of(
        "ENV GREETING \"Hello, $USER\"\n\
         ENV PATHS ${HOME}/bin:${EXTRA:-/opt}\n\
         ENV AGAIN $USER\n\
         ENV PRICE \\$5\n\
         ENV PLAIN text",
    );
    assert_eq!(grants.variables, vec!["EXTRA", "HOME", "USER"]);
}

// spec(Grants): | Secrets, their bound hosts, and their visibility | `SECRET`, `SEAL`, `AUTH key`, `AUTH token` |
#[test]
fn secrets_seals_and_their_hosts_are_grants() {
    let grants = grants_of(
        "SECRET STRIPE_KEY @api.stripe.com @stripe.com\n\
         SECRET GITHUB_TOKEN\n\
         SECRET NOTE visible\n\
         SEAL NPM_TOKEN @registry.npmjs.org visible",
    );
    assert_eq!(
        grants.secrets,
        vec![
            secret("GITHUB_TOKEN", &["api.github.com", "github.com"], false),
            secret("NOTE", &[], true),
            secret("STRIPE_KEY", &["api.stripe.com", "stripe.com"], false),
        ],
    );
    assert_eq!(
        grants.seals,
        vec![secret("NPM_TOKEN", &["registry.npmjs.org"], true)],
    );
    assert_eq!(grants.auth, None);
}

// spec(Grants): | Secrets, their bound hosts, and their visibility | `SECRET`, `SEAL`, `AUTH key`, `AUTH token` |
#[test]
fn auth_key_and_token_are_grants_and_login_is_not() {
    assert_eq!(grants_of("AUTH key").auth, Some(Auth::Key));
    assert_eq!(grants_of("AUTH token").auth, Some(Auth::Token));
    assert_eq!(grants_of("AUTH login").auth, None);
    assert_eq!(grants_of("").auth, None);
}

// spec(Grants): | Network policy | `NETWORK`, including the default |
#[test]
fn the_network_policy_is_a_grant_with_its_default() {
    assert_eq!(
        grants_of("").network,
        Network {
            default: Sign::Allow,
            rules: Vec::new(),
        },
    );
    let grants = grants_of("NETWORK -\nNETWORK +github.com:22");
    assert_eq!(grants.network.default, Sign::Deny);
    assert_eq!(
        grants.network.rules,
        vec![NetworkRule {
            sign: Sign::Allow,
            destination: Destination::Host {
                name: "github.com".to_string(),
                port: Some(22),
            },
        }],
    );
    assert!(round_trip(&grants_of("")).contains("NETWORK +\n"));
}

// spec(Grants): | Other agents it may call | `POLICY`, including the default |
#[test]
fn the_agents_it_may_call_are_a_grant_with_the_default() {
    assert_eq!(
        grants_of("").policy,
        Policy {
            default: Sign::Allow,
            rules: Vec::new(),
        },
    );
    assert_eq!(
        grants_of("POLICY +reviewer").policy,
        Policy {
            default: Sign::Deny,
            rules: vec![(Sign::Allow, "reviewer".to_string())],
        },
    );
    let text = round_trip(&grants_of("POLICY +reviewer"));
    assert!(text.contains("POLICY -\n"), "{text}");
    assert!(text.contains("POLICY +reviewer\n"), "{text}");
}

#[test]
fn git_credentials_are_left_out() {
    let grants = grants_of("REPO jgabor/private\nPLUGIN https://github.com/jgabor/plugins.git");
    assert_eq!(grants, grants_of(""));
}

#[test]
fn the_text_form_uses_agentfile_lines() {
    let grants = grants_of(
        "SHARE ./out ~/out rw\n\
         ADD ~/notes\n\
         ENV GREETING \"Hello, $USER\"\n\
         SECRET STRIPE_KEY @stripe.com @api.stripe.com\n\
         SEAL NPM_TOKEN @registry.npmjs.org visible\n\
         AUTH key\n\
         NETWORK -\n\
         NETWORK +github.com:22\n\
         NETWORK -10.1.0.0/16\n\
         NETWORK +[2606:4700::]/32:443\n\
         NETWORK +https://codeberg.org/forgejo\n\
         POLICY +reviewer",
    );
    assert_eq!(
        round_trip(&grants),
        "PATH /ctx/out rw\n\
         PATH /home/u/notes\n\
         VARIABLE USER\n\
         SECRET STRIPE_KEY @api.stripe.com @stripe.com\n\
         SEAL NPM_TOKEN @registry.npmjs.org visible\n\
         AUTH key\n\
         NETWORK -\n\
         NETWORK +[2606:4700::]/32:443\n\
         NETWORK +github.com:22\n\
         NETWORK +https://codeberg.org/forgejo\n\
         NETWORK -10.1.0.0/16\n\
         POLICY -\n\
         POLICY +reviewer\n",
    );
}

#[test]
fn the_order_of_lines_does_not_change_the_list() {
    let one = grants_of("ADD ~/b\nADD ~/a\nNETWORK +b.com\nNETWORK +a.com\nPOLICY -x\nPOLICY -w");
    let other = grants_of("ADD ~/a\nADD ~/b\nNETWORK +a.com\nNETWORK +b.com\nPOLICY -w\nPOLICY -x");
    assert_eq!(one, other);
    assert_eq!(one.to_string(), other.to_string());
}

#[test]
fn every_kind_of_rule_reads_back() {
    round_trip(&grants_of(
        "NETWORK +\n\
         NETWORK -1.1.1.1\n\
         NETWORK -1.1.1.1:53\n\
         NETWORK +[::1]\n\
         NETWORK +[2606:4700::]/32\n\
         NETWORK +http://example.com\n\
         NETWORK -https://example.com:8443/a/b\n\
         POLICY +\n\
         POLICY -stranger",
    ));
    round_trip(&grants_of("AUTH token\nSECRET NOTE visible"));
}

#[test]
fn reading_takes_comments_blank_lines_and_any_case() {
    let grants: Grants = "# approved\n\n  path /a rw\nnetwork -\r\nPolicy +\n"
        .parse()
        .unwrap();
    assert_eq!(grants.paths, vec![path("/a", true)]);
    assert_eq!(grants.network.default, Sign::Deny);
    assert_eq!(grants.policy.default, Sign::Allow);
}

#[test]
fn paths_with_spaces_quotes_and_backslashes_read_back() {
    for name in [
        "my notes",
        "say \"hi\"",
        "back\\slash",
        "\\u{41}",
        "#hash",
        "--flag",
    ] {
        let instructions = parse("AGENT claude\nADD ./x").unwrap();
        let grants = Grants::read(&instructions, |_| {
            Ok(Location {
                path: format!("/home/u/{name}"),
                outside: true,
            })
        })
        .unwrap();
        round_trip(&grants);
    }
}

#[test]
fn control_characters_and_bidirectional_overrides_are_escaped() {
    let name = "a\u{1b}[31m\u{202e}b\u{2066}c\nd\te\u{7f}f\u{2028}g\u{200f}h";
    let instructions = parse("AGENT claude\nADD ./x\nSECRET X\u{202e}Y visible").unwrap();
    let grants = Grants::read(&instructions, |_| {
        Ok(Location {
            path: format!("/home/u/{name}"),
            outside: true,
        })
    })
    .unwrap();
    let text = round_trip(&grants);
    assert!(
        text.contains(
            "PATH /home/u/a\\u{1b}[31m\\u{202e}b\\u{2066}c\\u{a}d\\u{9}e\\u{7f}f\\u{2028}g\\u{200f}h\n"
        ),
        "{text}",
    );
    assert!(text.contains("SECRET X\\u{202e}Y visible\n"), "{text}");
    let printable = |c: char| !c.is_control() && !('\u{2000}'..='\u{206f}').contains(&c);
    assert!(
        text.lines().all(|line| line.chars().all(printable)),
        "{text}"
    );
}

#[test]
fn reading_rejects_what_it_does_not_know() {
    let errors = errors_of(
        "PATH relative\n\
         PATH /a wr\n\
         VARIABLE 1BAD\n\
         SECRET HIDDEN\n\
         AUTH login\n\
         NETWORK +bad_host\n\
         POLICY +Upper\n\
         SHARE /a\n\
         PATH /bad\\escape\n\
         PATH /bad\\u{110000}\n\
         NETWORK -\n\
         POLICY -",
    );
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10], "{errors:?}");
}

#[test]
fn reading_needs_both_defaults_once() {
    let errors = errors_of("PATH /a");
    assert_eq!(errors.len(), 2, "{errors:?}");
    let errors = errors_of("NETWORK -\nNETWORK +\nPOLICY +\nPOLICY +");
    let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, vec![2, 4], "{errors:?}");
}
