//! The arguments of `ADD`, `REPO`, `PLUGIN`, `SKILL`, and `MCP`, the source
//! forms they take, and the typed values they parse into.

use agentfile::{Add, Error, Form, Mcp, Plugin, Remote, Repo, Skill, Source, Sources, parse};

/// Parses `lines` after an `AGENT` line, and returns its sources.
fn sources(lines: &str) -> Sources {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Sources::read(&instructions).unwrap()
}

/// Parses `line` after an `AGENT` line, and returns its one error.
fn error_of(line: &str) -> Error {
    let mut errors = parse(&format!("AGENT claude\n{line}")).unwrap_err();
    assert_eq!(errors.len(), 1, "{line:?}: {errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.line, 2, "{error:?}");
    error
}

/// Parses `line` after an `AGENT` line, and returns its error message.
fn message_of(line: &str) -> String {
    error_of(line).message
}

/// The source of a one-line `REPO`.
fn source_of(text: &str) -> Source {
    let mut sources = sources(&format!("REPO \"{text}\"\n"));
    sources.repo.remove(0).source
}

fn remote(
    form: Form,
    repository: &str,
    subdirectory: Option<&str>,
    reference: Option<&str>,
) -> Source {
    Source::Remote(Remote {
        form,
        repository: repository.to_string(),
        subdirectory: subdirectory.map(str::to_string),
        reference: reference.map(str::to_string),
    })
}

fn local(path: &str) -> Source {
    Source::Local(path.to_string())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

// spec(Sources): | Shorthand | `anthropics/skills/skills/algorithmic-art@main` | A GitHub repository (`owner/repo`), with an optional subdirectory and an optional `@ref` |
#[test]
fn a_shorthand_is_a_github_repository_with_an_optional_subdirectory_and_ref() {
    let github = "https://github.com/anthropics/skills";
    for (text, expected) in [
        (
            "anthropics/skills",
            remote(Form::Shorthand, github, None, None),
        ),
        (
            "anthropics/skills@main",
            remote(Form::Shorthand, github, None, Some("main")),
        ),
        (
            "anthropics/skills/skills/algorithmic-art",
            remote(
                Form::Shorthand,
                github,
                Some("skills/algorithmic-art"),
                None,
            ),
        ),
        (
            "anthropics/skills/skills/algorithmic-art@main",
            remote(
                Form::Shorthand,
                github,
                Some("skills/algorithmic-art"),
                Some("main"),
            ),
        ),
    ] {
        assert_eq!(source_of(text), expected, "{text}");
    }
}

// spec(Sources): | Git URL | `https://codeberg.org/forgejo/forgejo` | A repository root at its default branch, with an optional `@ref` |
#[test]
fn a_git_url_is_a_repository_root_with_an_optional_ref() {
    let forgejo = "https://codeberg.org/forgejo/forgejo";
    assert_eq!(source_of(forgejo), remote(Form::Git, forgejo, None, None));
    assert_eq!(
        source_of("https://codeberg.org/forgejo/forgejo@v9.0.0"),
        remote(Form::Git, forgejo, None, Some("v9.0.0"))
    );
    for url in [
        "https://gitlab.com/group/subgroup/project",
        "https://example.com/repo.git",
        "ssh://git@example.com/team/repo.git",
        "https://github.com/torvalds/linux",
    ] {
        assert_eq!(source_of(url), remote(Form::Git, url, None, None), "{url}");
    }
}

#[test]
fn an_at_sign_before_the_path_of_a_git_url_is_not_a_ref() {
    let url = "ssh://git@example.com/team/repo";
    assert_eq!(source_of(url), remote(Form::Git, url, None, None));
    assert_eq!(
        source_of("ssh://git@example.com/team/repo@main"),
        remote(Form::Git, url, None, Some("main"))
    );
}

#[test]
fn a_trailing_slash_is_not_part_of_a_source() {
    assert_eq!(
        source_of("https://codeberg.org/forgejo/forgejo/"),
        remote(
            Form::Git,
            "https://codeberg.org/forgejo/forgejo",
            None,
            None
        )
    );
    assert_eq!(
        source_of("anthropics/skills/skills/"),
        remote(
            Form::Shorthand,
            "https://github.com/anthropics/skills",
            Some("skills"),
            None
        )
    );
}

#[test]
fn a_github_tree_url_names_its_ref_and_subdirectory() {
    let skills = "https://github.com/anthropics/skills";
    assert_eq!(
        source_of("https://github.com/anthropics/skills/tree/main/skills/algorithmic-art"),
        remote(
            Form::Forge,
            skills,
            Some("skills/algorithmic-art"),
            Some("main")
        )
    );
    assert_eq!(
        source_of("https://github.com/anthropics/skills/tree/v1.2"),
        remote(Form::Forge, skills, None, Some("v1.2"))
    );
    assert_eq!(
        source_of("https://git@github.com/anthropics/skills/tree/main/skills/pdf"),
        remote(
            Form::Forge,
            "https://git@github.com/anthropics/skills",
            Some("skills/pdf"),
            Some("main")
        )
    );
}

#[test]
fn a_github_url_that_names_no_repository_or_tree_is_an_error() {
    for url in [
        "https://github.com/anthropics",
        "https://github.com/anthropics/skills/pulls",
        "https://github.com/anthropics/skills/tree",
        "https://github.com/anthropics/skills/tree/",
    ] {
        let message = message_of(&format!("REPO {url}\n"));
        assert!(
            message.contains(url.trim_end_matches('/')),
            "{url}: {message}"
        );
        assert!(message.contains("/tree/"), "{url}: {message}");
    }
}

#[test]
fn a_local_path_is_a_host_directory() {
    for path in [
        "./my-plugin",
        "../shared/plugin",
        "/opt/plugin",
        "~/plugins/mine",
        ".",
        "..",
    ] {
        assert_eq!(source_of(path), local(path), "{path}");
    }
}

// spec(Sources): A ref in a shorthand or Git URL runs from the first `@` in the path to the end, so it may contain `/`, as in `torvalds/linux@v6.12` or `https://codeberg.org/forgejo/forgejo@release/v1`.
#[test]
fn a_ref_runs_from_the_first_at_sign_to_the_end() {
    for (text, expected) in [
        (
            "torvalds/linux@v6.12",
            remote(
                Form::Shorthand,
                "https://github.com/torvalds/linux",
                None,
                Some("v6.12"),
            ),
        ),
        (
            "https://codeberg.org/forgejo/forgejo@release/v1",
            remote(
                Form::Git,
                "https://codeberg.org/forgejo/forgejo",
                None,
                Some("release/v1"),
            ),
        ),
        (
            "owner/repo/sub@feature/a@b",
            remote(
                Form::Shorthand,
                "https://github.com/owner/repo",
                Some("sub"),
                Some("feature/a@b"),
            ),
        ),
    ] {
        assert_eq!(source_of(text), expected, "{text}");
    }
}

#[test]
fn a_source_that_is_no_form_is_an_error_that_names_it() {
    for text in [
        "linux",
        "owner//repo",
        "@main",
        "owner/repo@",
        "https://codeberg.org/forgejo/forgejo@",
        "https://",
        "https://codeberg.org",
        "owner/repo/../escape",
        "owner/repo/./sub",
        "git@github.com:owner/repo",
    ] {
        let message = message_of(&format!("REPO \"{text}\"\n"));
        assert!(message.contains(&format!("`{text}`")), "{text}: {message}");
    }
}

#[test]
fn an_empty_source_is_an_error() {
    for name in ["REPO", "PLUGIN", "SKILL", "MCP"] {
        let message = message_of(&format!("{name} \"\"\n"));
        assert!(message.contains(name), "{message}");
    }
}

#[test]
fn add_takes_a_host_path_and_an_optional_destination() {
    let sources = sources("ADD ./fixtures/database.sqlite\nADD ./prompts /opt/prompts\n");
    assert_eq!(
        sources.add,
        [
            Add {
                line: 2,
                source: "./fixtures/database.sqlite".to_string(),
                destination: None,
            },
            Add {
                line: 3,
                source: "./prompts".to_string(),
                destination: Some("/opt/prompts".to_string()),
            },
        ]
    );
}

#[test]
fn add_needs_one_or_two_arguments() {
    for (line, count) in [("ADD\n", "0"), ("ADD ./a ./b ./c\n", "3")] {
        let message = message_of(line);
        assert!(message.contains("`ADD`"), "{message}");
        assert!(message.contains(count), "{message}");
    }
    assert!(message_of("ADD \"\"\n").contains("`ADD`"));
    assert!(message_of("ADD ./a \"\"\n").contains("destination"));
}

// spec(REPO): `<source>` is a [source](#sources).
#[test]
fn repo_takes_every_source_form() {
    let sources = sources(
        "REPO torvalds/linux\n\
         REPO https://github.com/anthropics/skills/tree/main/skills\n\
         REPO https://codeberg.org/forgejo/forgejo ~/src/forgejo\n\
         REPO ./vendor/lib\n",
    );
    let forms: Vec<_> = sources
        .repo
        .iter()
        .map(|repo| repo.source.clone())
        .collect();
    assert_eq!(
        forms,
        [
            remote(
                Form::Shorthand,
                "https://github.com/torvalds/linux",
                None,
                None
            ),
            remote(
                Form::Forge,
                "https://github.com/anthropics/skills",
                Some("skills"),
                Some("main")
            ),
            remote(
                Form::Git,
                "https://codeberg.org/forgejo/forgejo",
                None,
                None
            ),
            local("./vendor/lib"),
        ]
    );
}

#[test]
fn repo_takes_a_destination_and_history() {
    let sources = sources(
        "REPO torvalds/linux\n\
         REPO jgabor/agent-plus history\n\
         REPO https://codeberg.org/forgejo/forgejo ~/src/forgejo\n\
         REPO owner/a ~/a history\n",
    );
    let read: Vec<_> = sources
        .repo
        .iter()
        .map(|repo| (repo.line, repo.destination.as_deref(), repo.history))
        .collect();
    assert_eq!(
        read,
        [
            (2, None, false),
            (3, None, true),
            (4, Some("~/src/forgejo"), false),
            (5, Some("~/a"), true),
        ]
    );
}

#[test]
fn history_is_matched_exactly_and_only_last() {
    let sources = sources("REPO owner/a History\nREPO owner/b ./history\n");
    let read: Vec<_> = sources
        .repo
        .iter()
        .map(|repo| (repo.destination.as_deref(), repo.history))
        .collect();
    assert_eq!(read, [(Some("History"), false), (Some("./history"), false)]);
}

#[test]
fn repo_needs_one_to_three_arguments() {
    let message = message_of("REPO\n");
    assert!(message.contains("`REPO`"), "{message}");
    let message = message_of("REPO owner/a ~/a history extra\n");
    assert!(message.contains("4"), "{message}");
    assert!(message_of("REPO owner/a \"\"\n").contains("destination"));
}

// spec(REPO): A trailing `.git` is not part of the name.
#[test]
fn a_trailing_dot_git_is_not_part_of_the_name() {
    let sources = sources(
        "REPO https://example.com/team/tool.git\n\
         REPO owner/widget.git ~/w\n\
         REPO torvalds/linux@v6.12\n\
         REPO https://github.com/anthropics/skills/tree/main/skills\n\
         REPO ./vendor/lib.git\n\
         REPO .\n",
    );
    let names: Vec<_> = sources.repo.iter().map(Repo::name).collect();
    assert_eq!(
        names,
        [
            Some("tool"),
            Some("widget"),
            Some("linux"),
            Some("skills"),
            None,
            None,
        ]
    );
}

// A local repository's name is left to the host, because canonicalizing
// its path can change the name, so this proves only the text-decided part.
#[test]
fn two_repositories_with_the_same_name_are_an_error() {
    let instructions = parse(
        "AGENT claude\n\
         REPO jgabor/tool\n\
         REPO https://example.com/other/tool.git\n\
         REPO someone/tool ~/elsewhere\n\
         REPO third/tool@main\n",
    )
    .unwrap();
    let errors = Sources::read(&instructions).unwrap_err();
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [3, 5], "{errors:?}");
    for error in &errors {
        assert!(error.message.contains("`tool`"), "{}", error.message);
        assert!(error.message.contains("line 2"), "{}", error.message);
        assert!(error.message.contains("destination"), "{}", error.message);
    }
}

#[test]
fn plugin_takes_a_source_and_names() {
    let sources = sources(
        "PLUGIN https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/context7\n\
         PLUGIN anthropics/claude-plugins-official context7 github\n",
    );
    assert_eq!(
        sources.plugin,
        [
            Plugin {
                line: 2,
                source: remote(
                    Form::Forge,
                    "https://github.com/anthropics/claude-plugins-official",
                    Some("external_plugins/context7"),
                    Some("main"),
                ),
                names: Vec::new(),
            },
            Plugin {
                line: 3,
                source: remote(
                    Form::Shorthand,
                    "https://github.com/anthropics/claude-plugins-official",
                    None,
                    None,
                ),
                names: strings(&["context7", "github"]),
            },
        ]
    );
}

#[test]
fn skill_takes_a_source_and_names() {
    let sources = sources(
        "SKILL https://github.com/anthropics/skills/tree/main/skills/algorithmic-art\n\
         SKILL anthropics/skills pdf docx\n\
         SKILL ./skills/my-skill\n",
    );
    assert_eq!(
        sources.skill,
        [
            Skill {
                line: 2,
                source: remote(
                    Form::Forge,
                    "https://github.com/anthropics/skills",
                    Some("skills/algorithmic-art"),
                    Some("main"),
                ),
                names: Vec::new(),
            },
            Skill {
                line: 3,
                source: remote(
                    Form::Shorthand,
                    "https://github.com/anthropics/skills",
                    None,
                    None
                ),
                names: strings(&["pdf", "docx"]),
            },
            Skill {
                line: 4,
                source: local("./skills/my-skill"),
                names: Vec::new(),
            },
        ]
    );
}

#[test]
fn plugin_and_skill_need_a_source_and_no_empty_name() {
    for name in ["PLUGIN", "SKILL"] {
        assert!(message_of(&format!("{name}\n")).contains(name));
        let message = message_of(&format!("{name} owner/repo a \"\"\n"));
        assert!(message.contains("name"), "{message}");
    }
}

#[test]
fn mcp_takes_a_source_or_a_name_and_a_url() {
    let sources = sources(
        "MCP https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/github\n\
         MCP example https://mcp.example.com/mcp\n\
         MCP local http://host.ap.internal:8080/mcp\n",
    );
    assert_eq!(
        sources.mcp,
        [
            Mcp::Declared {
                line: 2,
                source: remote(
                    Form::Forge,
                    "https://github.com/anthropics/claude-plugins-official",
                    Some("external_plugins/github"),
                    Some("main"),
                ),
            },
            Mcp::Remote {
                line: 3,
                name: "example".to_string(),
                url: "https://mcp.example.com/mcp".to_string(),
            },
            Mcp::Remote {
                line: 4,
                name: "local".to_string(),
                url: "http://host.ap.internal:8080/mcp".to_string(),
            },
        ]
    );
}

#[test]
fn an_mcp_server_url_must_be_http() {
    for url in [
        "mcp.example.com/mcp",
        "ftp://example.com/mcp",
        "https://",
        "./server",
    ] {
        let message = message_of(&format!("MCP example {url}\n"));
        assert!(message.contains(&format!("`{url}`")), "{message}");
        assert!(message.contains("`https`"), "{message}");
    }
    assert!(message_of("MCP \"\" https://mcp.example.com\n").contains("name"));
}

#[test]
fn mcp_needs_one_or_two_arguments() {
    for (line, count) in [("MCP\n", "0"), ("MCP a https://b c\n", "3")] {
        let message = message_of(line);
        assert!(message.contains("`MCP`"), "{message}");
        assert!(message.contains(count), "{message}");
    }
}

#[test]
fn every_bad_source_line_is_reported_with_its_line() {
    let errors = parse(
        "AGENT claude\n\
         REPO linux\n\
         SKILL anthropics/skills\n\
         PLUGIN owner//repo\n\
         MCP example nowhere\n\
         ADD\n",
    )
    .unwrap_err();
    let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
    assert_eq!(lines, [2, 4, 5, 6], "{errors:?}");
}

#[test]
fn read_keeps_each_instruction_in_file_order_and_skips_the_rest() {
    let sources = sources(
        "OS minimal\n\
         REPO owner/a\n\
         ADD ./x\n\
         REPO owner/b\n\
         MODEL opus\n",
    );
    let lines: Vec<_> = sources.repo.iter().map(|repo| repo.line).collect();
    assert_eq!(lines, [3, 5]);
    assert_eq!(sources.add.len(), 1);
    assert!(sources.plugin.is_empty() && sources.skill.is_empty() && sources.mcp.is_empty());
}

#[test]
fn a_github_url_ref_may_contain_a_slash() {
    assert_eq!(
        source_of("https://github.com/torvalds/linux@release/v1"),
        remote(
            Form::Git,
            "https://github.com/torvalds/linux",
            None,
            Some("release/v1")
        )
    );
}

#[test]
fn a_github_tree_url_is_percent_decoded() {
    assert_eq!(
        source_of("https://github.com/owner/repo/tree/feature%2Bx/My%20Skills/a"),
        remote(
            Form::Forge,
            "https://github.com/owner/repo",
            Some("My Skills/a"),
            Some("feature+x")
        )
    );
    for url in [
        "https://github.com/owner/repo/tree/main/%2e%2e/escape",
        "https://github.com/owner/repo/tree/main/a%2F..%2F..",
        "https://github.com/owner/repo/tree/main/bad%zz",
        "https://github.com/owner/repo/tree/main/bad%ff",
    ] {
        let message = message_of(&format!("REPO {url}\n"));
        assert!(message.contains(&format!("`{url}`")), "{url}: {message}");
    }
}

#[test]
fn a_url_needs_a_host() {
    for line in [
        "REPO https://:443/team/repo\n",
        "REPO ssh://git@/team/repo\n",
        "MCP example https://:443/mcp\n",
        "MCP example https://user@/mcp\n",
    ] {
        let message = message_of(line);
        assert!(message.contains("host"), "{line:?}: {message}");
    }
    let url = "https://[::1]:8080/team/repo";
    assert_eq!(source_of(url), remote(Form::Git, url, None, None));
    sources("MCP example http://[::1]:8080/mcp\n");
}

#[test]
fn history_written_anywhere_but_last_is_an_error() {
    for line in [
        "REPO owner/a history history\n",
        "REPO owner/a history ~/a\n",
    ] {
        let message = message_of(line);
        assert!(message.contains("`./history`"), "{line:?}: {message}");
    }
}

#[test]
fn two_mcp_servers_with_the_same_name_are_an_error() {
    let instructions = parse(
        "AGENT claude\n\
         MCP example https://one.example/mcp\n\
         MCP other https://two.example/mcp\n\
         MCP example https://three.example/mcp\n",
    )
    .unwrap();
    let errors = Sources::read(&instructions).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].line, 4);
    assert!(
        errors[0].message.contains("`example`"),
        "{}",
        errors[0].message
    );
    assert!(
        errors[0].message.contains("line 2"),
        "{}",
        errors[0].message
    );
}
