//! Finds and reads skills in trees held in memory, as SPEC.md's `SKILL` and
//! the Agent Skills specification say.

use std::collections::BTreeMap;
use std::io;

use extensions::skill::{self, Error, MAX_FRONT_MATTER, Problem, Skill};

/// A tree of files, by path.
fn tree(files: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(path, text)| (path.to_string(), text.as_bytes().to_vec()))
        .collect()
}

fn find(
    files: &BTreeMap<String, Vec<u8>>,
    root: &str,
    names: &[&str],
) -> Result<Vec<Skill>, Error> {
    skill::find(root, files.keys().map(String::as_str), names, |path| {
        Ok(files[path].clone())
    })
}

fn front(name: &str) -> String {
    format!("---\nname: {name}\ndescription: Does {name} things.\n---\n# {name}\n")
}

/// The problem with one SKILL.md, read alone as the root of a tree named
/// `root`.
fn problem(root: &str, text: &str) -> Problem {
    match find(&tree(&[("SKILL.md", text)]), root, &[]) {
        Err(Error::Invalid { path, problem }) => {
            assert_eq!(path, "SKILL.md");
            problem
        }
        other => panic!("expected a problem, got {other:?}"),
    }
}

fn read(root: &str, text: &str) -> Skill {
    let mut skills = find(&tree(&[("SKILL.md", text)]), root, &[]).unwrap();
    assert_eq!(skills.len(), 1);
    skills.remove(0)
}

fn names(skills: &[Skill]) -> Vec<(&str, &str)> {
    skills
        .iter()
        .map(|skill| (skill.directory.as_str(), skill.name.as_str()))
        .collect()
}

#[test]
fn root_with_skill_md_is_that_skill() {
    let files = tree(&[
        ("SKILL.md", &front("pdf")),
        ("scripts/extract.py", "print()\n"),
        // A skill's own files are its own, even one that looks like a skill.
        ("assets/template/SKILL.md", &front("template")),
    ]);
    let skills = find(&files, "pdf", &[]).unwrap();
    assert_eq!(names(&skills), [("", "pdf")]);
    assert_eq!(skills[0].description, "Does pdf things.");
}

#[test]
fn every_skill_under_the_root_without_skill_md() {
    let files = tree(&[
        ("README.md", "# skills\n"),
        ("skills/pdf/SKILL.md", &front("pdf")),
        ("skills/pdf/forms/SKILL.md", &front("forms")),
        // `-` sorts before `/`, so this comes between the two above.
        ("skills/pdf-x/SKILL.md", &front("pdf-x")),
        ("skills/docx/SKILL.md", &front("docx")),
        ("skills/docx/ooxml.md", "notes\n"),
        ("other/deep/er/xlsx/SKILL.md", &front("xlsx")),
        ("skills/not-a-skill/skill.txt", "no\n"),
    ]);
    let skills = find(&files, "skills", &[]).unwrap();
    assert_eq!(
        names(&skills),
        [
            ("other/deep/er/xlsx", "xlsx"),
            ("skills/docx", "docx"),
            ("skills/pdf", "pdf"),
            ("skills/pdf-x", "pdf-x"),
        ]
    );
}

#[test]
fn names_limit_the_set() {
    let files = tree(&[
        ("skills/pdf/SKILL.md", &front("pdf")),
        ("skills/docx/SKILL.md", &front("docx")),
        ("skills/xlsx/SKILL.md", &front("xlsx")),
    ]);
    let skills = find(&files, "skills", &["xlsx", "pdf"]).unwrap();
    assert_eq!(
        names(&skills),
        [("skills/pdf", "pdf"), ("skills/xlsx", "xlsx")]
    );
}

#[test]
fn names_skip_a_broken_skill_they_leave_out() {
    let files = tree(&[
        ("skills/pdf/SKILL.md", &front("pdf")),
        ("skills/broken/SKILL.md", "no front matter\n"),
    ]);
    let skills = find(&files, "skills", &["pdf"]).unwrap();
    assert_eq!(names(&skills), [("skills/pdf", "pdf")]);
    match find(&files, "skills", &[]) {
        Err(Error::Invalid { path, problem }) => {
            assert_eq!(path, "skills/broken/SKILL.md");
            assert_eq!(problem, Problem::NoFrontMatter);
        }
        other => panic!("expected the broken skill, got {other:?}"),
    }
}

#[test]
fn a_name_that_matches_no_skill_is_an_error_that_names_it() {
    let files = tree(&[
        ("skills/pdf/SKILL.md", &front("pdf")),
        ("skills/docx/SKILL.md", &front("docx")),
    ]);
    let error = find(&files, "skills", &["pdf", "pptx"]).unwrap_err();
    assert!(matches!(&error, Error::NoSkill(name) if name == "pptx"));
    assert_eq!(error.to_string(), "no skill is named `pptx`");

    let root = tree(&[("SKILL.md", &front("pdf"))]);
    assert_eq!(names(&find(&root, "pdf", &["pdf"]).unwrap()), [("", "pdf")]);
    let error = find(&root, "pdf", &["docx"]).unwrap_err();
    assert!(matches!(&error, Error::NoSkill(name) if name == "docx"));
}

#[test]
fn a_tree_with_no_skill_is_an_error() {
    let files = tree(&[("README.md", "# nothing\n"), ("skill.md", &front("lower"))]);
    let error = find(&files, "empty", &[]).unwrap_err();
    assert!(matches!(error, Error::NoSkills));
    assert_eq!(error.to_string(), "no directory holds a `SKILL.md`");
}

#[test]
fn a_read_error_names_the_file() {
    let error = skill::find("skills", ["skills/pdf/SKILL.md"], &[], |_| {
        Err(io::Error::other("gone"))
    })
    .unwrap_err();
    assert!(matches!(&error, Error::Read { path, .. } if path == "skills/pdf/SKILL.md"));
    assert_eq!(error.to_string(), "couldn't read skills/pdf/SKILL.md: gone");
}

#[test]
fn reads_the_keys_the_specification_defines() {
    let skill = read(
        "pdf-processing",
        concat!(
            "---\n",
            "name: pdf-processing\n",
            "description: Extract PDF text, fill forms, merge files. Use when handling PDFs.\n",
            "license: Apache-2.0\n",
            "compatibility: Requires git, docker, jq, and access to the internet\n",
            "metadata:\n",
            "  author: example-org\n",
            "  version: \"1.0\"\n",
            "allowed-tools: Bash(git:*) Bash(jq:*) Read\n",
            "---\n",
            "# PDF processing\n",
        ),
    );
    assert_eq!(
        skill,
        Skill {
            directory: String::new(),
            name: "pdf-processing".to_string(),
            description: "Extract PDF text, fill forms, merge files. Use when handling PDFs."
                .to_string(),
            license: Some("Apache-2.0".to_string()),
            compatibility: Some("Requires git, docker, jq, and access to the internet".to_string()),
            metadata: BTreeMap::from([
                ("author".to_string(), "example-org".to_string()),
                ("version".to_string(), "1.0".to_string()),
            ]),
            allowed_tools: Some("Bash(git:*) Bash(jq:*) Read".to_string()),
        }
    );
}

#[test]
fn the_minimal_skill_has_no_optional_keys() {
    let skill = read(
        "skill-name",
        "---\nname: skill-name\ndescription: A description of what this skill does and when to use it.\n---\n",
    );
    assert_eq!(skill.license, None);
    assert_eq!(skill.compatibility, None);
    assert!(skill.metadata.is_empty());
    assert_eq!(skill.allowed_tools, None);
}

#[test]
fn every_value_is_read_as_text() {
    // Each value is its scalar's text, as the reference validator's
    // strictyaml reads it, so a version is never a number.
    let skill = read(
        "pdf",
        concat!(
            "---\r\n",
            "name: 'pdf'\r\n",
            "description: >\r\n",
            "  Folded\r\n",
            "  lines.\r\n",
            "license: null\r\n",
            "metadata: {version: 1.10, beta: true}\r\n",
            "---  \r\n",
            "body\r\n",
        ),
    );
    assert_eq!(skill.name, "pdf");
    assert_eq!(skill.description, "Folded lines.\n");
    assert_eq!(skill.license.as_deref(), Some("null"));
    assert_eq!(skill.metadata["version"], "1.10");
    assert_eq!(skill.metadata["beta"], "true");
}

#[test]
fn other_keys_are_left_alone() {
    // Claude Code reads keys of its own, and a hook's value is a mapping.
    let skill = read(
        "deploy",
        concat!(
            "---\n",
            "name: deploy\n",
            "description: Deploys.\n",
            "disable-model-invocation: true\n",
            "hooks:\n",
            "  PreToolUse:\n",
            "    - matcher: Bash\n",
            "      hooks: [{type: command, command: ./check.sh}]\n",
            "? [a, complex, key]\n",
            ": with a value\n",
            "x-anchor: &a value\n",
            "x-alias: *a\n",
            "---\n",
        ),
    );
    assert_eq!(skill.name, "deploy");
}

#[test]
fn a_file_with_no_front_matter_is_refused_by_name() {
    let files = tree(&[("skills/pdf/SKILL.md", "# pdf\n\nname: pdf\n")]);
    let error = find(&files, "skills", &[]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "skills/pdf/SKILL.md: has no front matter, which starts on its first line with `---`"
    );
    assert_eq!(problem("pdf", ""), Problem::NoFrontMatter);
    assert_eq!(
        problem("pdf", "\n---\nname: pdf\n---\n"),
        Problem::NoFrontMatter
    );
    assert_eq!(
        problem("pdf", "--- name: pdf\n---\n"),
        Problem::NoFrontMatter
    );
    assert_eq!(
        problem("pdf", "\u{feff}---\nname: pdf\n---\n"),
        Problem::NoFrontMatter
    );
}

#[test]
fn front_matter_must_close() {
    assert_eq!(problem("pdf", "---\nname: pdf\n"), Problem::Unclosed);
    assert_eq!(problem("pdf", "---\nname: pdf\n----\n"), Problem::Unclosed);
    assert_eq!(problem("pdf", "---"), Problem::Unclosed);
}

#[test]
fn front_matter_closes_at_the_end_of_the_file() {
    assert_eq!(
        read("pdf", "---\nname: pdf\ndescription: d\n---").name,
        "pdf"
    );
}

#[test]
fn front_matter_is_bounded() {
    let filler = "#".repeat(MAX_FRONT_MATTER);
    let long = format!("---\nname: pdf\ndescription: d\n{filler}\n---\n");
    assert_eq!(problem("pdf", &long), Problem::TooLong);

    // The bound counts the fences, and a body of any length is fine.
    let fits = MAX_FRONT_MATTER - "---\nname: pdf\ndescription: d\n\n---\n".len();
    let body = "x".repeat(4 * MAX_FRONT_MATTER);
    let text = format!(
        "---\nname: pdf\ndescription: d\n{}\n---\n{body}",
        "#".repeat(fits)
    );
    assert_eq!(read("pdf", &text).name, "pdf");
    let text = format!(
        "---\nname: pdf\ndescription: d\n{}\n---\n",
        "#".repeat(fits + 1)
    );
    assert_eq!(problem("pdf", &text), Problem::TooLong);
}

#[test]
fn front_matter_is_utf8() {
    let files = BTreeMap::from([(
        "SKILL.md".to_string(),
        b"---\nname: pdf\ndescription: \xff\n---\n".to_vec(),
    )]);
    assert!(matches!(
        find(&files, "pdf", &[]),
        Err(Error::Invalid {
            problem: Problem::NotUtf8,
            ..
        })
    ));
    // The body is never read.
    let files = BTreeMap::from([(
        "SKILL.md".to_string(),
        b"---\nname: pdf\ndescription: d\n---\n\xff".to_vec(),
    )]);
    assert_eq!(find(&files, "pdf", &[]).unwrap()[0].name, "pdf");
}

#[test]
fn front_matter_is_one_yaml_mapping() {
    assert!(matches!(
        problem("pdf", "---\nname: [pdf\n---\n"),
        Problem::Yaml(_)
    ));
    assert_eq!(problem("pdf", "---\n---\n"), Problem::NotMapping);
    assert_eq!(problem("pdf", "---\n- name\n---\n"), Problem::NotMapping);
    assert_eq!(problem("pdf", "---\njust text\n---\n"), Problem::NotMapping);
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\n...\nname: two\n---\n"
        ),
        Problem::NotMapping
    );
}

#[test]
fn name_and_description_are_required() {
    assert_eq!(
        problem("pdf", "---\ndescription: d\n---\n"),
        Problem::Missing("name")
    );
    assert_eq!(
        problem("pdf", "---\nname: pdf\n---\n"),
        Problem::Missing("description")
    );
}

#[test]
fn a_key_appears_once() {
    assert_eq!(
        problem("pdf", "---\nname: pdf\nname: pdf\ndescription: d\n---\n"),
        Problem::Repeated("name".to_string())
    );
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nmetadata:\n  a: 1\n  a: 2\n---\n"
        ),
        Problem::Repeated("metadata.a".to_string())
    );
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nhooks: 1\nhooks: 2\n---\n"
        ),
        Problem::Repeated("hooks".to_string())
    );
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nmetadata: {}\nmetadata: {}\n---\n"
        ),
        Problem::Repeated("metadata".to_string())
    );
}

#[test]
fn a_known_key_has_text() {
    assert_eq!(
        problem("pdf", "---\nname: [pdf]\ndescription: d\n---\n"),
        Problem::NotText("name")
    );
    assert_eq!(
        problem("pdf", "---\nname: pdf\ndescription: {a: b}\n---\n"),
        Problem::NotText("description")
    );
    assert_eq!(
        problem("pdf", "---\nx: &n pdf\nname: *n\ndescription: d\n---\n"),
        Problem::NotText("name")
    );
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nallowed-tools: [Read, Bash]\n---\n"
        ),
        Problem::NotText("allowed-tools")
    );
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nlicense:\n  - MIT\n---\n"
        ),
        Problem::NotText("license")
    );
}

#[test]
fn metadata_maps_text_to_text() {
    for metadata in [
        "metadata: text",
        "metadata: [a]",
        "metadata:\n  a: [b]",
        "metadata:\n  [a]: b",
    ] {
        let text = format!("---\nname: pdf\ndescription: d\n{metadata}\n---\n");
        assert_eq!(problem("pdf", &text), Problem::Metadata, "{metadata}");
    }
    let skill = read("pdf", "---\nname: pdf\ndescription: d\nmetadata: {}\n---\n");
    assert!(skill.metadata.is_empty());
}

#[test]
fn name_follows_the_specification() {
    for name in [
        "pdf",
        "pdf-processing",
        "data-analysis",
        "a",
        "x2",
        "0",
        &"a".repeat(64),
    ] {
        let text = format!("---\nname: {name}\ndescription: d\n---\n");
        assert_eq!(read(name, &text).name, name);
    }
    for name in [
        "PDF-Processing",
        "-pdf",
        "pdf-",
        "pdf--processing",
        "pdf_processing",
        "pdf processing",
        "café",
        "pdf.md",
        "''",
    ] {
        let text = format!("---\nname: {name}\ndescription: d\n---\n");
        let shown = name.trim_matches('\'');
        assert_eq!(
            problem(shown, &text),
            Problem::Name(shown.to_string()),
            "{name}"
        );
    }
    let long = "a".repeat(65);
    let text = format!("---\nname: {long}\ndescription: d\n---\n");
    assert_eq!(problem(&long, &text), Problem::Name(long.clone()));
}

#[test]
fn name_matches_the_directory() {
    let text = "---\nname: pdf\ndescription: d\n---\n";
    assert_eq!(
        problem("pdf-tools", text),
        Problem::Directory {
            name: "pdf".to_string(),
            directory: "pdf-tools".to_string()
        }
    );
    let files = tree(&[("skills/docx/SKILL.md", text)]);
    let error = find(&files, "skills", &[]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "skills/docx/SKILL.md: `name` is `pdf`, but the skill's directory is `docx`"
    );
}

#[test]
fn description_is_1_to_1024_characters() {
    let at = |description: &str| format!("---\nname: pdf\ndescription: '{description}'\n---\n");
    assert_eq!(
        problem("pdf", &at("")),
        Problem::Length {
            key: "description",
            max: 1024
        }
    );
    assert_eq!(
        problem("pdf", &at("   ")),
        Problem::Length {
            key: "description",
            max: 1024
        }
    );
    // Characters, not bytes.
    let full = "é".repeat(1024);
    assert_eq!(read("pdf", &at(&full)).description, full);
    assert_eq!(
        problem("pdf", &at(&"a".repeat(1025))),
        Problem::Length {
            key: "description",
            max: 1024
        }
    );
}

#[test]
fn compatibility_is_1_to_500_characters() {
    let at =
        |value: &str| format!("---\nname: pdf\ndescription: d\ncompatibility: '{value}'\n---\n");
    let full = "é".repeat(500);
    assert_eq!(read("pdf", &at(&full)).compatibility, Some(full));
    for value in [String::new(), "a".repeat(501)] {
        assert_eq!(
            problem("pdf", &at(&value)),
            Problem::Length {
                key: "compatibility",
                max: 500
            }
        );
    }
}

#[test]
fn problems_say_what_is_wrong() {
    let shown = |text: &str| {
        find(&tree(&[("skills/pdf/SKILL.md", text)]), "skills", &[])
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        shown("---\nname: pdf\n"),
        "skills/pdf/SKILL.md: its front matter has no closing `---` line"
    );
    assert_eq!(
        shown("---\ndescription: d\n---\n"),
        "skills/pdf/SKILL.md: its front matter has no `name`"
    );
    assert_eq!(
        shown("---\nname: Pdf\ndescription: d\n---\n"),
        concat!(
            "skills/pdf/SKILL.md: `name` is `Pdf`, but a name is 1 to 64 of the letters a to z, ",
            "digits, and single hyphens, with no hyphen at either end"
        )
    );
    assert_eq!(
        shown("---\nname: pdf\ndescription: ''\n---\n"),
        "skills/pdf/SKILL.md: `description` must be 1 to 1024 characters"
    );
    assert_eq!(
        shown("---\nname: pdf\ndescription: [d]\n---\n"),
        "skills/pdf/SKILL.md: `description` isn't text"
    );
    assert_eq!(
        shown("---\nname: pdf\ndescription: d\nmetadata: [d]\n---\n"),
        "skills/pdf/SKILL.md: `metadata` isn't a mapping from text to text"
    );
    assert_eq!(
        shown("---\n- a\n---\n"),
        "skills/pdf/SKILL.md: its front matter isn't one YAML mapping"
    );
    assert!(
        shown("---\nname: [pdf\n---\n")
            .starts_with("skills/pdf/SKILL.md: its front matter isn't valid YAML: ")
    );
}

#[test]
fn a_key_that_readers_resolve_differently_is_refused() {
    // An alias as a key is `allowed-tools` to a YAML reader.
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\nx: &a allowed-tools\n*a : Bash\n---\n"
        ),
        Problem::Key("an alias as a key")
    );
    // A merge key is `license` to a reader that merges.
    assert_eq!(
        problem(
            "pdf",
            "---\nname: pdf\ndescription: d\n<<: {license: MIT}\n---\n"
        ),
        Problem::Key("`<<` as a key")
    );
    // A tag can make any key a merge key.
    for key in [
        "!!merge x",
        "!!merge '<<'",
        "!<tag:yaml.org,2002:merge> x",
        "!!str name",
        "!x license",
    ] {
        let text = format!("---\nname: pdf\ndescription: d\n{key}: {{license: MIT}}\n---\n");
        assert_eq!(
            problem("pdf", &text),
            Problem::Key("a key with a tag"),
            "{key}"
        );
    }
    assert_eq!(
        find(
            &tree(&[(
                "skills/pdf/SKILL.md",
                "---\nname: pdf\ndescription: d\nx: &m {license: MIT}\n<<: *m\n---\n"
            )]),
            "skills",
            &[]
        )
        .unwrap_err()
        .to_string(),
        "skills/pdf/SKILL.md: its front matter has `<<` as a key, which YAML readers read in different ways"
    );
    // Quoted, with no tag, it is only text.
    let skill = read(
        "pdf",
        "---\nname: pdf\ndescription: d\n'<<': {license: MIT}\n---\n",
    );
    assert_eq!(skill.license, None);
}

#[test]
fn a_read_that_stops_past_the_bound_is_enough() {
    // The first `MAX_FRONT_MATTER` bytes end in `---`, but the line goes on.
    let head = "---\nname: pdf\ndescription: d\n";
    let filler = "#".repeat(MAX_FRONT_MATTER - head.len() - "\n---".len());
    let text = format!("{head}{filler}\n---xyz\nlicense: evil\n---\n");
    assert_eq!(
        &text.as_bytes()[MAX_FRONT_MATTER - 4..MAX_FRONT_MATTER],
        b"\n---"
    );
    for limit in [MAX_FRONT_MATTER + 1, text.len()] {
        let read = skill::find("pdf", ["SKILL.md"], &[], |_| {
            Ok(text.as_bytes()[..limit].to_vec())
        });
        assert!(
            matches!(
                read,
                Err(Error::Invalid {
                    problem: Problem::TooLong,
                    ..
                })
            ),
            "{limit}: {read:?}"
        );
    }
}
