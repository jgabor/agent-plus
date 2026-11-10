//! Which changes to an Agentfile widen its grants, by the list under SPEC.md's
//! Grants section.

use agentfile::{
    Auth, Destination, Grants, Location, NetworkRule, PathGrant, Secret, Sign, Widening, parse,
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

/// What `now` widens, compared with the grants approved from `approved`.
fn widened(approved: &str, now: &str) -> Vec<Widening> {
    grants_of(now).widening(&grants_of(approved))
}

fn host(sign: Sign, name: &str) -> NetworkRule {
    NetworkRule {
        sign,
        destination: Destination::Host {
            name: name.to_string(),
            port: None,
        },
    }
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn the_same_grants_widen_nothing() {
    let lines = "SHARE ~/notes /notes rw\nENV USER $USER\nSECRET T @a.com\n\
                 AUTH key\nNETWORK -\nNETWORK +github.com\nPOLICY +reviewer\n";
    assert_eq!(widened(lines, lines), []);
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn a_change_that_touches_no_grant_widens_nothing() {
    let approved = "NETWORK -\nPOLICY -\n";
    let now = "NETWORK -\nPOLICY -\nPLUGIN https://github.com/a/b\nMODEL opus\n\
               ADD ./src /src\nSHARE ./data /data\n";
    assert_eq!(widened(approved, now), []);
}

// spec(Grants): a host path outside the build context that you haven't approved, including an approved path whose canonical location changed
#[test]
fn a_new_host_path_outside_the_build_context_widens() {
    let approved = "SHARE ~/notes /notes\n";
    let now = "SHARE ~/notes /notes\nADD ~/.gitconfig /etc/gitconfig\n";
    assert_eq!(
        widened(approved, now),
        [Widening::Path(PathGrant {
            path: "/home/u/.gitconfig".to_string(),
            write: false,
        })]
    );
}

// spec(Grants): a host path outside the build context that you haven't approved, including an approved path whose canonical location changed
#[test]
fn an_approved_path_whose_canonical_location_changed_widens() {
    let instructions = parse("AGENT claude\nSHARE ~/data /data\n").unwrap();
    let approved = Grants::read(&instructions, resolve).unwrap();
    let now = Grants::read(&instructions, |_: &str| {
        Ok(Location {
            path: "/home/u/.ssh".to_string(),
            outside: true,
        })
    })
    .unwrap();
    assert_eq!(
        now.widening(&approved),
        [Widening::Path(PathGrant {
            path: "/home/u/.ssh".to_string(),
            write: false,
        })]
    );
}

// spec(Grants): write access (`rw`) to a path that didn't have it
#[test]
fn write_access_to_a_path_that_did_not_have_it_widens() {
    assert_eq!(
        widened("SHARE ~/notes /notes\n", "SHARE ~/notes /notes rw\n"),
        [Widening::Write("/home/u/notes".to_string())]
    );
    assert_eq!(
        widened("SHARE ./out /out\n", "SHARE ./out /out rw\n"),
        [Widening::Path(PathGrant {
            path: "/ctx/out".to_string(),
            write: true,
        })]
    );
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn dropping_a_path_or_its_write_access_narrows() {
    let approved = "SHARE ~/notes /notes rw\nADD ~/.gitconfig /etc/gitconfig\n";
    assert_eq!(widened(approved, "SHARE ~/notes /notes\n"), []);
}

// spec(Grants): a new host environment variable
#[test]
fn a_new_host_environment_variable_widens() {
    assert_eq!(
        widened("ENV A $USER\n", "ENV A $USER\nENV B ${HOME}/x\n"),
        [Widening::Variable("HOME".to_string())]
    );
    assert_eq!(widened("ENV A $USER\nENV B $HOME\n", "ENV A $USER\n"), []);
}

// spec(Grants): a new secret, a new bound host, or a secret that becomes `visible` or moves from `SECRET` to `SEAL`
#[test]
fn a_new_secret_widens() {
    assert_eq!(
        widened("", "SECRET GITHUB_TOKEN\n"),
        [Widening::Secret {
            secret: Secret {
                name: "GITHUB_TOKEN".to_string(),
                hosts: vec!["api.github.com".to_string(), "github.com".to_string()],
                visible: false,
            },
            sealed: false,
        }]
    );
    assert_eq!(
        widened("", "SEAL KEY visible\n"),
        [Widening::Secret {
            secret: Secret {
                name: "KEY".to_string(),
                hosts: Vec::new(),
                visible: true,
            },
            sealed: true,
        }]
    );
}

// spec(Grants): a new secret, a new bound host, or a secret that becomes `visible` or moves from `SECRET` to `SEAL`
#[test]
fn a_new_bound_host_widens() {
    assert_eq!(
        widened("SECRET T @a.com\n", "SECRET T @a.com @b.com\n"),
        [Widening::Host {
            secret: "T".to_string(),
            host: "b.com".to_string(),
        }]
    );
    // An `@<host>` list replaces the adapter's defaults, so dropping it
    // brings them back.
    assert_eq!(
        widened("SECRET GITHUB_TOKEN @github.com\n", "SECRET GITHUB_TOKEN\n"),
        [Widening::Host {
            secret: "GITHUB_TOKEN".to_string(),
            host: "api.github.com".to_string(),
        }]
    );
    // Host names ignore case.
    assert_eq!(widened("SECRET T @a.com\n", "SECRET T @A.COM\n"), []);
}

// spec(Grants): a new secret, a new bound host, or a secret that becomes `visible` or moves from `SECRET` to `SEAL`
#[test]
fn a_secret_that_becomes_visible_widens() {
    assert_eq!(
        widened("SECRET T @a.com\n", "SECRET T @a.com visible\n"),
        [Widening::Visible("T".to_string())]
    );
}

// spec(Grants): a new secret, a new bound host, or a secret that becomes `visible` or moves from `SECRET` to `SEAL`
#[test]
fn a_secret_that_moves_from_secret_to_seal_widens() {
    assert_eq!(
        widened("SECRET T @a.com\n", "SEAL T @a.com\n"),
        [Widening::Sealed("T".to_string())]
    );
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn a_secret_that_narrows_widens_nothing() {
    let approved = "SECRET T @a.com @b.com visible\nSEAL S @c.com\nSECRET GONE @d.com\n";
    let now = "SECRET T @a.com\nSECRET S @c.com\n";
    assert_eq!(widened(approved, now), []);
}

// spec(Grants): adding `AUTH key` or `AUTH token`, or changing one to the other
#[test]
fn adding_auth_key_or_token_widens() {
    assert_eq!(widened("", "AUTH key\n"), [Widening::Auth(Auth::Key)]);
    assert_eq!(
        widened("AUTH login\n", "AUTH token\n"),
        [Widening::Auth(Auth::Token)]
    );
}

// spec(Grants): adding `AUTH key` or `AUTH token`, or changing one to the other
#[test]
fn changing_auth_key_to_token_or_back_widens() {
    assert_eq!(
        widened("AUTH key\n", "AUTH token\n"),
        [Widening::Auth(Auth::Token)]
    );
    assert_eq!(
        widened("AUTH token\n", "AUTH key\n"),
        [Widening::Auth(Auth::Key)]
    );
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn dropping_auth_key_or_token_narrows() {
    assert_eq!(widened("AUTH key\n", "AUTH login\n"), []);
    assert_eq!(widened("AUTH token\n", ""), []);
}

#[test]
fn a_new_allow_rule_widens() {
    assert_eq!(
        widened("NETWORK -\n", "NETWORK -\nNETWORK +github.com\n"),
        [Widening::Allow(host(Sign::Allow, "github.com"))]
    );
}

#[test]
fn removing_a_deny_rule_widens() {
    assert_eq!(
        widened("NETWORK +\nNETWORK -evil.com\n", "NETWORK +\n"),
        [Widening::Unblock(host(Sign::Deny, "evil.com"))]
    );
}

#[test]
fn changing_a_rule_widens_unless_it_only_drops_an_allow_or_adds_a_deny() {
    let approved = "NETWORK -\nNETWORK +github.com\n";
    assert_eq!(
        widened(approved, "NETWORK -\nNETWORK +github.com:22\n"),
        [Widening::Allow(NetworkRule {
            sign: Sign::Allow,
            destination: Destination::Host {
                name: "github.com".to_string(),
                port: Some(22),
            },
        })]
    );
    assert_eq!(widened(approved, "NETWORK -\nNETWORK -github.com\n"), []);
}

#[test]
fn opening_the_network_default_widens() {
    assert_eq!(widened("NETWORK -\n", "NETWORK +\n"), [Widening::Open]);
    // Without a bare rule, the default is `+`.
    assert_eq!(widened("NETWORK -\n", ""), [Widening::Open]);
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn removing_allow_rules_and_adding_deny_rules_narrows() {
    let approved = "NETWORK +\nNETWORK +a.com\nNETWORK -b.com\n";
    let now = "NETWORK -\nNETWORK -b.com\nNETWORK -c.com\n";
    assert_eq!(widened(approved, now), []);
}

// spec(Grants): a `POLICY` change that lets the agent call an agent it couldn't before
#[test]
fn a_policy_that_lets_the_agent_call_a_new_agent_widens() {
    assert_eq!(
        widened("POLICY +reviewer\n", "POLICY +reviewer\nPOLICY +tester\n"),
        [Widening::Call("tester".to_string())]
    );
    assert_eq!(
        widened("POLICY -stranger\n", "POLICY +\n"),
        [Widening::Call("stranger".to_string())]
    );
    // Opposite signs for one name deny it, so dropping the deny allows it.
    assert_eq!(
        widened("POLICY -\nPOLICY +a\nPOLICY -a\n", "POLICY -\nPOLICY +a\n"),
        [Widening::Call("a".to_string())]
    );
}

// spec(Grants): a `POLICY` change that lets the agent call an agent it couldn't before
#[test]
fn a_policy_that_opens_its_default_widens() {
    assert_eq!(widened("POLICY -\n", "POLICY +\n"), [Widening::CallAny]);
    assert_eq!(
        widened("POLICY +reviewer\n", "POLICY -stranger\n"),
        [Widening::CallAny]
    );
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn a_policy_that_only_narrows_widens_nothing() {
    assert_eq!(widened("", "POLICY -stranger\n"), []);
    assert_eq!(widened("POLICY +\n", "POLICY +reviewer\n"), []);
    assert_eq!(widened("POLICY +a\nPOLICY +b\n", "POLICY +a\n"), []);
    assert_eq!(widened("POLICY +a\n", "POLICY +a\nPOLICY -a\n"), []);
}

#[test]
fn each_widening_is_listed() {
    let approved = "NETWORK -\nPOLICY -\n";
    let now = "SHARE ~/n /n\nENV A $USER\nAUTH key\nNETWORK +\nPOLICY +r\n";
    assert_eq!(
        widened(approved, now),
        [
            Widening::Path(PathGrant {
                path: "/home/u/n".to_string(),
                write: false,
            }),
            Widening::Variable("USER".to_string()),
            Widening::Auth(Auth::Key),
            Widening::Open,
            Widening::Call("r".to_string()),
        ]
    );
}

// spec(Grants): A later build that **widens** a grant asks again and shows what changed.
#[test]
fn each_widening_shows_as_one_line() {
    let approved = "SHARE ~/a /a\nSECRET T @a.com\nSECRET S @s.com\n\
                    NETWORK -\nNETWORK -evil.com\nPOLICY -\n";
    let now = "SHARE ~/a /a rw\nSHARE ~/b /b\nENV USER $USER\nSECRET T @a.com @b.com visible\n\
               SEAL S @s.com\nSEAL N @n.com\nAUTH token\nNETWORK +\nNETWORK +github.com\n\
               POLICY +\nPOLICY -writer\n";
    let lines: Vec<String> = widened(approved, now).iter().map(ToString::to_string).collect();
    assert_eq!(
        lines,
        [
            "PATH /home/u/a rw, which was read-only",
            "PATH /home/u/b",
            "VARIABLE USER",
            "T becomes visible",
            "T may go to @b.com",
            "SEAL N @n.com",
            "S moves from SECRET to SEAL",
            "AUTH token",
            "NETWORK + replaces NETWORK -",
            "NETWORK +github.com",
            "NETWORK -evil.com is gone",
            "POLICY + replaces POLICY -",
        ]
    );
}

// spec(Grants): Approval prompts escape control characters and bidirectional overrides in paths, names, and hosts, so a grant shows as it is.
#[test]
fn a_widening_escapes_what_would_hide() {
    let path = Widening::Path(PathGrant {
        path: "/home/u/\u{202e}txt.sh\n".to_string(),
        write: false,
    });
    assert_eq!(path.to_string(), "PATH /home/u/\\u{202e}txt.sh\\u{a}");
    let host = Widening::Host {
        secret: "T".to_string(),
        host: "a.com\u{1b}[2K".to_string(),
    };
    assert_eq!(host.to_string(), "T may go to @a.com\\u{1b}[2K");
    assert_eq!(
        Widening::Call("re viewer".to_string()).to_string(),
        "may call \"re viewer\""
    );
}
