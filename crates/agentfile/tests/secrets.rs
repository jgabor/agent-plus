//! The arguments of `ENV`, `SECRET`, `SEAL`, and `AUTH`, the variables in
//! `ENV` values, and the typed values they parse into.

use agentfile::{Auth, Env, Error, Part, Secret, Secrets, Text, parse};

/// Parses `lines` after an `AGENT claude` line, and returns their values.
fn secrets_of(lines: &str) -> Secrets {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Secrets::read(&instructions).unwrap()
}

/// Parses a whole Agentfile, and returns its errors.
fn errors(source: &str) -> Vec<Error> {
    parse(source).unwrap_err()
}

/// Parses `lines` after an `AGENT claude` line, and returns their errors.
fn errors_after_agent(lines: &str) -> Vec<Error> {
    errors(&format!("AGENT claude\n{lines}"))
}

/// Parses `line` after an `AGENT claude` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = errors_after_agent(line);
    assert_eq!(errors.len(), 1, "{line:?}: {errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.line, 2, "{error:?}");
    error
}

fn lines_of(errors: &[Error]) -> Vec<usize> {
    errors.iter().map(|error| error.line).collect()
}

/// The parsed value of the one `ENV` in `line`.
fn env_value(line: &str) -> Vec<Part> {
    let mut env = secrets_of(line).env;
    assert_eq!(env.len(), 1, "{env:?}");
    env.remove(0).value
}

fn literal(text: &str) -> Part {
    Part::Literal(text.to_string())
}

fn variable(name: &str) -> Part {
    Part::Variable {
        name: name.to_string(),
        default: None,
    }
}

fn variable_or(name: &str, default: &str) -> Part {
    Part::Variable {
        name: name.to_string(),
        default: Some(default.to_string()),
    }
}

fn secret(name: &str, hosts: &[&str], visible: bool) -> Secret {
    Secret {
        name: name.to_string(),
        hosts: hosts.iter().map(|host| host.to_string()).collect(),
        visible,
    }
}

#[test]
fn env_reads_a_key_and_a_value() {
    assert_eq!(
        secrets_of("ENV LOG_LEVEL debug\n").env,
        [Env {
            key: "LOG_LEVEL".to_string(),
            value: vec![literal("debug")],
        }]
    );
}

// spec(ENV): The value follows the [single-line text](#text-values) rule: one quoted string is unquoted, and anything else is taken verbatim.
#[test]
fn an_env_value_follows_the_single_line_text_rule() {
    for (rest, expected) in [
        (r#""Be concise.""#, "Be concise."),
        (r#""say \"hi\"""#, r#"say "hi""#),
        (r#"Say "hello" first."#, r#"Say "hello" first."#),
        (r#""a" "b""#, r#""a" "b""#),
        ("fix #12 # now", "fix #12 # now"),
        ("Run tool --verbose", "Run tool --verbose"),
        ("./not/a/path", "./not/a/path"),
        (r#""""#, ""),
    ] {
        let value = env_value(&format!("ENV KEY {rest}\n"));
        let expected = if expected.is_empty() {
            vec![]
        } else {
            vec![literal(expected)]
        };
        assert_eq!(value, expected, "{rest}");
    }
}

#[test]
fn whitespace_around_an_env_value_is_not_part_of_it() {
    assert_eq!(
        env_value("ENV KEY   two  words   \n"),
        [literal("two  words")]
    );
}

#[test]
fn env_needs_a_key_and_a_value() {
    for line in ["ENV\n", "ENV KEY\n", "ENV KEY   \n"] {
        let error = error_of(line);
        assert!(error.message.contains("`ENV`"), "{line:?}: {error:?}");
    }
}

#[test]
fn an_env_key_that_starts_with_two_dashes_is_a_flag() {
    let error = error_of("ENV --force value\n");
    assert!(error.message.contains("`--force`"), "{error:?}");
    assert_eq!(secrets_of("ENV \"--force\" value\n").env[0].key, "--force");
}

#[test]
fn an_env_key_cannot_be_empty_or_hold_an_equals_sign() {
    for line in ["ENV \"\" value\n", "ENV KEY=value other\n"] {
        error_of(line);
    }
}

// spec(Variables): `$NAME` and `${NAME}` expand only in `ENV` values.
// spec(ENV): The value is either literal or uses [variables](#variables) read from the host at run time.
#[test]
fn an_env_value_parses_into_literal_parts_and_variables() {
    assert_eq!(
        env_value("ENV AWS_REGION $AWS_REGION\n"),
        [variable("AWS_REGION")]
    );
    assert_eq!(env_value("ENV HOME_DIR ${HOME}\n"), [variable("HOME")]);
    assert_eq!(
        env_value("ENV PATHS $HOME/bin:${XDG_BIN}/x\n"),
        [
            variable("HOME"),
            literal("/bin:"),
            variable("XDG_BIN"),
            literal("/x"),
        ]
    );
    assert_eq!(
        env_value("ENV TAG ${A}${B}_$C_D.$e9\n"),
        [
            variable("A"),
            variable("B"),
            literal("_"),
            variable("C_D"),
            literal("."),
            variable("e9"),
        ]
    );
}

#[test]
fn a_variable_can_have_a_default() {
    assert_eq!(
        env_value("ENV EDITOR ${EDITOR:-vi}\n"),
        [variable_or("EDITOR", "vi")]
    );
    assert_eq!(
        env_value("ENV EDITOR ${EDITOR:-}\n"),
        [variable_or("EDITOR", "")]
    );
    assert_eq!(
        env_value("ENV URL ${URL:-http://localhost:8080/a b}\n"),
        [variable_or("URL", "http://localhost:8080/a b")]
    );
    assert_eq!(
        env_value("ENV PRICE ${PRICE:-\\$5}!\n"),
        [variable_or("PRICE", "$5"), literal("!")]
    );
    assert_eq!(
        env_value("ENV PRICE ${PRICE:-$5 or 5$}\n"),
        [variable_or("PRICE", "$5 or 5$")]
    );
}

// spec(Variables): `\$` is a literal dollar sign.
#[test]
fn an_escaped_dollar_sign_is_literal() {
    assert_eq!(
        env_value("ENV PRICE \\$HOME costs \\$5\n"),
        [literal("$HOME costs $5")]
    );
    assert_eq!(env_value("ENV PATH_SEP a\\b\\$\n"), [literal(r"a\b$")]);
}

#[test]
fn a_dollar_sign_that_starts_no_name_is_literal() {
    for rest in ["5$", "$", "$ 5", "$5", "$-", "a$.b"] {
        assert_eq!(
            env_value(&format!("ENV PRICE {rest}\n")),
            [literal(rest)],
            "{rest}"
        );
    }
}

// spec(Variables): Quotes don't stop expansion.
// spec(Variables): In `ENV GREETING "Hello, $USER"`, `$USER` expands.
#[test]
fn quotes_do_not_stop_variables() {
    assert_eq!(
        env_value("ENV GREETING \"Hello, $USER\"\n"),
        [literal("Hello, "), variable("USER")]
    );
    assert_eq!(
        env_value("ENV GREETING say \"$USER\"\n"),
        [literal("say \""), variable("USER"), literal("\"")]
    );
}

#[test]
fn a_bad_variable_is_an_error() {
    for rest in [
        "${HOME",
        "${}",
        "${1A}",
        "${A B}",
        "${A-b}",
        "${A:+b}",
        "${A:=b}",
        "${A:-$B}",
        "${A:-${B}}",
    ] {
        let error = error_of(&format!("ENV KEY {rest}\n"));
        assert!(error.message.contains("${"), "{rest}: {error:?}");
    }
}

// spec(Variables): Everywhere else, `$` is literal.
#[test]
fn a_dollar_sign_outside_env_is_literal() {
    let instructions =
        parse("AGENT claude\nSYSTEM Hello, $USER\nPROMPT \"${HOME:-x}\"\nMODEL $MODEL\n").unwrap();
    assert_eq!(
        instructions[1].text,
        Some(Text::Literal("Hello, $USER".to_string()))
    );
    assert_eq!(
        instructions[2].text,
        Some(Text::Literal("${HOME:-x}".to_string()))
    );
    assert_eq!(instructions[3].arguments, ["$MODEL"]);
    assert_eq!(
        secrets_of("SECRET $TOKEN @example.com\n").secrets,
        [secret("$TOKEN", &["example.com"], false)]
    );
}

// spec(ENV): Setting the same key twice, or a key that a `SECRET` or `SEAL` also names, is an error.
#[test]
fn an_env_key_can_be_set_only_once() {
    let errors = errors_after_agent("ENV A 1\nENV B 2\nENV A 3\nENV A 4\n");
    assert_eq!(lines_of(&errors), [4, 5], "{errors:?}");
    assert!(errors[0].message.contains("line 2"), "{errors:?}");
}

// spec(ENV): Setting the same key twice, or a key that a `SECRET` or `SEAL` also names, is an error.
#[test]
fn an_env_key_cannot_be_a_secret_or_a_seal() {
    for lines in [
        "ENV TOKEN x\nSECRET TOKEN @example.com\n",
        "SECRET TOKEN @example.com\nENV TOKEN x\n",
        "ENV TOKEN x\nSEAL TOKEN visible\n",
        "SEAL TOKEN visible\nENV TOKEN x\n",
    ] {
        let errors = errors_after_agent(lines);
        assert_eq!(lines_of(&errors), [3], "{lines:?}: {errors:?}");
        assert!(errors[0].message.contains("`TOKEN`"), "{errors:?}");
    }
}

#[test]
fn secret_reads_a_name_hosts_and_visible() {
    assert_eq!(
        secrets_of(
            "SECRET STRIPE_API_KEY @api.stripe.com\n\
             SECRET AWS_SECRET_ACCESS_KEY visible\n\
             SECRET MULTI @a.example.com @b.example.com visible\n"
        )
        .secrets,
        [
            secret("STRIPE_API_KEY", &["api.stripe.com"], false),
            secret("AWS_SECRET_ACCESS_KEY", &[], true),
            secret("MULTI", &["a.example.com", "b.example.com"], true),
        ]
    );
}

// spec(SECRET): Bound hosts come from the `@<host>` list.
#[test]
fn bound_hosts_come_from_the_host_list() {
    assert_eq!(
        secrets_of("SECRET DEPLOY @deploy.example.com @10.0.0.1\n").secrets,
        [secret("DEPLOY", &["deploy.example.com", "10.0.0.1"], false)]
    );
}

// spec(SECRET): An `@<host>` list replaces the adapter's defaults.
#[test]
fn a_host_list_replaces_the_defaults() {
    assert_eq!(
        secrets_of("SECRET GITHUB_TOKEN @ghe.example.com\n").secrets,
        [secret("GITHUB_TOKEN", &["ghe.example.com"], false)]
    );
}

// spec(SECRET): Well-known names have adapter defaults, so `@<host>` is optional for them.
// spec(SECRET): | `ANTHROPIC_API_KEY` | `api.anthropic.com` |
// spec(SECRET): | `OPENAI_API_KEY`, `CODEX_API_KEY` | `api.openai.com` |
// spec(SECRET): | `GITHUB_TOKEN`, `GH_TOKEN` | `github.com`, `api.github.com` |
#[test]
fn well_known_names_have_default_hosts() {
    let github = ["github.com", "api.github.com"];
    assert_eq!(
        secrets_of(
            "SECRET ANTHROPIC_API_KEY\n\
             SECRET OPENAI_API_KEY\n\
             SECRET CODEX_API_KEY\n\
             SECRET GITHUB_TOKEN\n\
             SECRET GH_TOKEN\n"
        )
        .secrets,
        [
            secret("ANTHROPIC_API_KEY", &["api.anthropic.com"], false),
            secret("OPENAI_API_KEY", &["api.openai.com"], false),
            secret("CODEX_API_KEY", &["api.openai.com"], false),
            secret("GITHUB_TOKEN", &github, false),
            secret("GH_TOKEN", &github, false),
        ]
    );
}

#[test]
fn a_visible_well_known_secret_keeps_its_default_hosts() {
    assert_eq!(
        secrets_of("SECRET GH_TOKEN visible\n").secrets,
        [secret("GH_TOKEN", &["github.com", "api.github.com"], true)]
    );
}

// spec(SECRET): A hidden secret with neither `@<host>` nor an adapter default is an error.
#[test]
fn a_hidden_secret_needs_bound_hosts() {
    let error = error_of("SECRET STRIPE_API_KEY\n");
    assert!(error.message.contains("`STRIPE_API_KEY`"), "{error:?}");
    assert!(error.message.contains("@"), "{error:?}");
    assert_eq!(
        secrets_of("SECRET STRIPE_API_KEY visible\n").secrets.len(),
        1
    );
}

#[test]
fn secret_needs_a_name() {
    for line in [
        "SECRET\n",
        "SECRET @api.stripe.com\n",
        "SECRET visible\n",
        "SECRET \"\" visible\n",
        "SECRET A=B visible\n",
    ] {
        let error = error_of(line);
        assert!(error.message.contains("`SECRET`"), "{line:?}: {error:?}");
    }
}

#[test]
fn a_host_must_be_a_host_name() {
    for host in [
        "@",
        "@https://api.stripe.com",
        "@api.stripe.com/v1",
        "@*.example.com",
        "@host:443",
    ] {
        let error = error_of(&format!("SECRET KEY {host}\n"));
        assert!(error.message.contains(host), "{host}: {error:?}");
    }
}

// spec(Arguments): The word is matched exactly and must be the last argument.
#[test]
fn visible_is_matched_exactly_and_comes_last() {
    for line in [
        "SECRET KEY visible @api.example.com\n",
        "SECRET KEY Visible\n",
        "SECRET KEY @api.example.com hidden\n",
        "SECRET KEY visible visible\n",
        "SEAL KEY visible @api.example.com\n",
    ] {
        error_of(line);
    }
}

#[test]
fn seal_takes_the_same_arguments_as_secret() {
    let lines = "DEPLOY_TOKEN @deploy.example.com\nAWS_SECRET_ACCESS_KEY visible\nGH_TOKEN\n";
    let as_secrets = secrets_of(
        &lines
            .replace("DEPLOY", "SECRET DEPLOY")
            .replace("AWS", "SECRET AWS")
            .replace("GH_", "SECRET GH_"),
    );
    let as_seals = secrets_of(
        &lines
            .replace("DEPLOY", "SEAL DEPLOY")
            .replace("AWS", "SEAL AWS")
            .replace("GH_", "SEAL GH_"),
    );
    assert!(as_seals.secrets.is_empty());
    assert_eq!(as_seals.seals, as_secrets.secrets);
    assert_eq!(as_seals.seals.len(), 3);
    let secret = error_of("SECRET STRIPE_API_KEY\n");
    let seal = error_of("SEAL STRIPE_API_KEY\n");
    assert_eq!(seal.message, secret.message.replace("`SECRET`", "`SEAL`"));
}

// spec(SEAL): A name can't appear in both `SECRET` and `SEAL`.
#[test]
fn a_name_cannot_be_both_a_secret_and_a_seal() {
    for lines in [
        "SECRET TOKEN visible\nSEAL TOKEN visible\n",
        "SEAL TOKEN visible\nSECRET TOKEN visible\n",
    ] {
        let errors = errors_after_agent(lines);
        assert_eq!(lines_of(&errors), [3], "{lines:?}: {errors:?}");
        assert!(errors[0].message.contains("`TOKEN`"), "{errors:?}");
    }
}

// spec(AUTH): `AUTH` chooses how the harness signs in.
#[test]
fn auth_reads_its_method() {
    for (word, expected) in [
        ("login", Auth::Login),
        ("key", Auth::Key),
        ("token", Auth::Token),
    ] {
        assert_eq!(
            secrets_of(&format!("AUTH {word}\n")).auth,
            Some(expected),
            "{word}"
        );
    }
    assert_eq!(secrets_of("").auth, None);
}

#[test]
fn any_other_auth_method_is_an_error() {
    for line in ["AUTH\n", "AUTH Login\n", "AUTH oauth\n", "AUTH key token\n"] {
        let error = error_of(line);
        assert!(error.message.contains("`AUTH`"), "{line:?}: {error:?}");
    }
    let message = error_of("AUTH oauth\n").message;
    for word in ["`login`", "`key`", "`token`", "`oauth`"] {
        assert!(message.contains(word), "{message}");
    }
}

// spec(AUTH): With any harness but Claude Code, it is a build error.
#[test]
fn auth_token_needs_claude_code() {
    let errors = errors("AGENT codex\nAUTH token\n");
    assert_eq!(lines_of(&errors), [2], "{errors:?}");
    assert!(errors[0].message.contains("`token`"), "{errors:?}");
    for source in [
        "AGENT codex\nAUTH key\n",
        "AGENT codex\nAUTH login\n",
        "AGENT claude:2.1\nAUTH token\n",
    ] {
        assert!(parse(source).is_ok(), "{source:?}");
    }
}

#[test]
fn one_bad_instruction_does_not_hide_the_next() {
    let errors = errors_after_agent(
        "ENV KEY\nSECRET STRIPE_API_KEY\nENV OK ${BAD\nSEAL\nAUTH oauth\nENV FINE yes\n",
    );
    assert_eq!(lines_of(&errors), [2, 3, 4, 5, 6], "{errors:?}");
}

/// Asserts that `lines` fail on line 2 alone, once for each of `hosts`, and
/// that each error names its host and its secret.
fn assert_denied(lines: &str, name: &str, hosts: &[&str]) {
    let errors = errors_after_agent(lines);
    assert_eq!(
        lines_of(&errors),
        vec![2; hosts.len()],
        "{lines:?}: {errors:?}"
    );
    for (error, host) in errors.iter().zip(hosts) {
        assert!(error.message.contains(&format!("`{host}`")), "{error:?}");
        assert!(error.message.contains(&format!("`{name}`")), "{error:?}");
        assert!(error.message.contains("NETWORK"), "{error:?}");
    }
}

/// Asserts that `lines` parse, and that `Secrets::read` accepts them.
fn assert_allowed(lines: &str) {
    let instructions = parse(&format!("AGENT claude\n{lines}"))
        .unwrap_or_else(|errors| panic!("{lines:?}: {errors:?}"));
    Secrets::read(&instructions).unwrap_or_else(|errors| panic!("{lines:?}: {errors:?}"));
}

// spec(SECRET): A bound host that `NETWORK` denies is an error.
#[test]
fn a_bound_host_that_a_deny_rule_denies_is_an_error() {
    for (lines, name, hosts) in [
        (
            "SECRET STRIPE_API_KEY @api.stripe.com\nNETWORK -api.stripe.com\n",
            "STRIPE_API_KEY",
            &["api.stripe.com"][..],
        ),
        (
            "SECRET STRIPE_API_KEY @api.stripe.com\nNETWORK -stripe.com\n",
            "STRIPE_API_KEY",
            &["api.stripe.com"],
        ),
        (
            "SECRET T @a.example.com @b.example.com\nNETWORK -example.com\n",
            "T",
            &["a.example.com", "b.example.com"],
        ),
        (
            "SECRET T @API.Example.com\nNETWORK -api.example.com\n",
            "T",
            &["API.Example.com"],
        ),
        (
            "SEAL DEPLOY @deploy.example.com\nNETWORK -deploy.example.com\n",
            "DEPLOY",
            &["deploy.example.com"],
        ),
        (
            "SECRET GITHUB_TOKEN\nNETWORK -api.github.com\n",
            "GITHUB_TOKEN",
            &["api.github.com"],
        ),
        (
            "SECRET T @a.com\nNETWORK +a.com\nNETWORK -a.com\n",
            "T",
            &["a.com"],
        ),
        ("SECRET T @a.com visible\nNETWORK -a.com\n", "T", &["a.com"]),
        (
            "SECRET T @10.0.0.1\nNETWORK -10.0.0.0/8\n",
            "T",
            &["10.0.0.1"],
        ),
        (
            "SECRET T @a.com\nNETWORK +a.com\nNETWORK -0.0.0.0/0\nNETWORK -[::]/0\n",
            "T",
            &["a.com"],
        ),
        (
            "SECRET T @a.com\nNETWORK -0.0.0.0/1\nNETWORK -128.0.0.0/1\nNETWORK -[::]/0\n",
            "T",
            &["a.com"],
        ),
        (
            "SECRET T @a.com\nNETWORK -\nNETWORK +10.0.0.0/8\nNETWORK -a.com\n",
            "T",
            &["a.com"],
        ),
    ] {
        assert_denied(lines, name, hosts);
    }
}

// spec(SECRET): A bound host that `NETWORK` denies is an error.
#[test]
fn a_bound_host_that_the_closed_default_denies_is_an_error() {
    for (lines, name, hosts) in [
        (
            "SECRET STRIPE_API_KEY @api.stripe.com\nNETWORK -\n",
            "STRIPE_API_KEY",
            &["api.stripe.com"][..],
        ),
        (
            "SECRET GITHUB_TOKEN\nNETWORK -\n",
            "GITHUB_TOKEN",
            &["github.com", "api.github.com"],
        ),
        (
            "SEAL T @a.com\nNETWORK no\nNETWORK +b.com\n",
            "T",
            &["a.com"],
        ),
        (
            "SECRET T @api.a.com\nNETWORK -\nNETWORK +b.api.a.com\n",
            "T",
            &["api.a.com"],
        ),
        (
            "SECRET T @a.com\nNETWORK -\nNETWORK +https://a.com.evil.net/\n",
            "T",
            &["a.com"],
        ),
        ("SECRET T @10.0.0.1\nNETWORK -\n", "T", &["10.0.0.1"]),
    ] {
        assert_denied(lines, name, hosts);
    }
}

#[test]
fn a_bound_host_that_some_request_reaches_is_allowed() {
    for lines in [
        "SECRET T @api.a.com\n",
        "SECRET T @api.a.com\nNETWORK -\nNETWORK +a.com\n",
        "SECRET T @api.a.com\nNETWORK -\nNETWORK +api.a.com:443\n",
        "SECRET T @api.a.com\nNETWORK -\nNETWORK +https://api.a.com/v1\n",
        "SECRET T @api.a.com\nNETWORK -a.com\nNETWORK +api.a.com\n",
        "SECRET T @api.a.com\nNETWORK -api.a.com\nNETWORK +https://api.a.com/v1\n",
        "SECRET T @api.a.com\nNETWORK -api.a.com:443\n",
        "SECRET T @api.a.com\nNETWORK -https://api.a.com/\n",
        "SECRET T @api.a.com\nNETWORK -https://api.a.com/v1\n",
        "SECRET T @api.a.com\nNETWORK -other.com\nNETWORK -10.0.0.0/8\n",
        "SECRET T @api.a.com\nNETWORK -\nNETWORK +10.0.0.0/8\n",
        "SECRET T @a.com\nNETWORK +a.com\nNETWORK -0.0.0.0/0\n",
        "SECRET T @a.com\nNETWORK -0.0.0.0/0\nNETWORK -[::]/0\nNETWORK +10.1.2.3\n",
        "SECRET T @a.com\nNETWORK -0.0.0.0/0:443\nNETWORK -[::]/0:443\n",
        "SECRET T @a.com\nNETWORK -0.0.0.0/1\nNETWORK -[::]/0\n",
        "SECRET T @10.0.0.1\nNETWORK -\nNETWORK +10.0.0.0/8\n",
        "SECRET T @10.0.0.1\nNETWORK -10.0.0.0/8\nNETWORK +10.0.0.1\n",
        "SECRET GITHUB_TOKEN\nNETWORK -\nNETWORK +github.com\n",
        "SECRET T visible\nNETWORK -\n",
    ] {
        assert_allowed(lines);
    }
}

#[test]
fn a_denied_bound_host_fails_secrets_read_too() {
    let instructions = vec![
        agentfile::Instruction {
            line: 1,
            name: "SECRET".to_string(),
            arguments: vec!["T".to_string(), "@a.com".to_string()],
            text: None,
        },
        agentfile::Instruction {
            line: 2,
            name: "NETWORK".to_string(),
            arguments: vec!["-".to_string()],
            text: None,
        },
    ];
    let errors = Secrets::read(&instructions).unwrap_err();
    assert_eq!(lines_of(&errors), [1], "{errors:?}");
}
