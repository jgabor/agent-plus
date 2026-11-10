//! The formatter, from SPEC.md's `ap fmt` section. These checks cover what
//! the text alone decides. Reading and writing the file belong to `ap`.

use agentfile::{Instruction, Text, format, parse};

fn formatted(source: &str) -> String {
    format(source).unwrap_or_else(|errors| panic!("{errors:?}"))
}

/// What an Agentfile means: its instructions, without their line numbers.
fn meaning(source: &str) -> Vec<(String, Vec<String>, Option<Text>)> {
    parse(source)
        .unwrap()
        .into_iter()
        .map(
            |Instruction {
                 name,
                 arguments,
                 text,
                 ..
             }| (name, arguments, text),
        )
        .collect()
}

/// Checks the promises that hold for every Agentfile that parses: formatting
/// is stable, keeps the meaning, and keeps comments in their order.
fn assert_formats_well(source: &str) {
    let once = formatted(source);
    assert_eq!(
        formatted(&once),
        once,
        "formatting twice changed\n{source:?}"
    );
    assert_eq!(
        meaning(&once),
        meaning(source),
        "meaning changed\n{source:?}"
    );
    let comments = |text: &str| -> Vec<String> {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(str::trim)
            .filter(|line| line.starts_with('#'))
            .map(String::from)
            .collect()
    };
    assert_eq!(comments(&once), comments(source), "{source:?}");
}

/// Hand-written Agentfiles, formatted and not.
const FIXTURES: [&str; 6] = [
    "AGENT claude\n",
    "agent  claude\r\nshare\t./out rw  \r\n\r\n\r\n# keep\r\n",
    "\n\n  # top  \nAGENT claude\nSYSTEM ```md\nBe brief.  \n\n\n# not a comment\n```\nPROMPT \"/review\"\n\n\n",
    "AGENT claude\nSYSTEM\n````\nAnswer like this:\n\n```json\n{}\n```\n````  \nENV  A \"b  c\"   \n",
    "AGENT claude\rSYSTEM You are a careful reviewer.   \rsystem ./\"My Prompts\"/system.md\r",
    "AGENT claude\n  NETWORK   github.com\n\tSHARE \"./My Notes\"\n",
];

#[test]
fn every_fixture_formats_well() {
    for source in FIXTURES {
        assert_formats_well(source);
    }
}

// spec(ap fmt): Instruction names become uppercase.
#[test]
fn instruction_names_become_uppercase() {
    assert_eq!(
        formatted("agent claude\nShare ./out\nnEtWoRk github.com\n"),
        "AGENT claude\nSHARE ./out\nNETWORK github.com\n"
    );
}

// spec(ap fmt): One space separates an instruction from its arguments.
#[test]
fn one_space_separates_an_instruction_from_its_arguments() {
    assert_eq!(
        formatted("AGENT\t claude\nSHARE   ./out  rw\nSYSTEM \t Be brief.\nENV\tA b\n"),
        "AGENT claude\nSHARE ./out  rw\nSYSTEM Be brief.\nENV A b\n"
    );
}

// spec(ap fmt): Trailing whitespace is removed outside fenced blocks.
#[test]
fn trailing_whitespace_is_removed_outside_fenced_blocks() {
    assert_eq!(
        formatted("AGENT claude \t\n# note  \nSYSTEM\n```\nkeep  \n\t\n```\nPROMPT go \n"),
        "AGENT claude\n# note\nSYSTEM\n```\nkeep  \n\t\n```\nPROMPT go\n"
    );
    // The fences themselves aren't content.
    assert_eq!(
        formatted("AGENT claude\nSYSTEM \n```md  \nkeep  \n```\t\nPROMPT ```  \ngo\n``` \n"),
        "AGENT claude\nSYSTEM\n```md\nkeep  \n```\nPROMPT\n```\ngo\n```\n"
    );
}

// spec(ap fmt): Line endings become LF, and the file ends with exactly one newline.
#[test]
fn line_endings_become_lf_and_the_file_ends_with_one_newline() {
    assert_eq!(
        formatted("AGENT claude\r\nOS minimal\r"),
        "AGENT claude\nOS minimal\n"
    );
    assert_eq!(formatted("AGENT claude"), "AGENT claude\n");
    assert_eq!(formatted("AGENT claude\n\n\n \n"), "AGENT claude\n");
    assert_eq!(formatted("AGENT claude\n# end"), "AGENT claude\n# end\n");
}

// spec(ap fmt): Outside fenced blocks, a run of blank lines becomes one, and blank lines at the start of the file are removed.
#[test]
fn blank_lines_collapse_outside_fenced_blocks() {
    assert_eq!(
        formatted("\n \n\t\nAGENT claude\n\n\n  \nOS minimal\nSYSTEM\n```\na\n\n\n\nb\n```\n\n\n"),
        "AGENT claude\n\nOS minimal\nSYSTEM\n```\na\n\n\n\nb\n```\n"
    );
}

// spec(ap fmt): The end-of-line fence form becomes the next-line form, as [Text values](#text-values) says.
#[test]
fn the_end_of_line_fence_moves_to_the_next_line() {
    assert_eq!(
        formatted("AGENT claude\nsystem   ```markdown\nBe brief.\n```\nPROMPT ````\nGo.\n````\n"),
        "AGENT claude\nSYSTEM\n```markdown\nBe brief.\n```\nPROMPT\n````\nGo.\n````\n"
    );
}

// spec(Text values): `ap fmt` rewrites the end-of-line form to the next-line form.
#[test]
fn fmt_rewrites_the_end_of_line_fence_form() {
    let source = "AGENT claude\nSYSTEM ```\nBe brief.\n```\n";
    assert_eq!(
        formatted(source),
        "AGENT claude\nSYSTEM\n```\nBe brief.\n```\n"
    );
    // Text followed by backticks is single-line text, and stays on its line.
    let single = "AGENT claude\nSYSTEM Use ``` for code.\n";
    assert_eq!(formatted(single), single);
}

// spec(ap fmt): `ap fmt` never changes an argument, a quote, a text value, the content of a fenced block, a comment's text, or the order of lines.
#[test]
fn fmt_changes_no_argument_quote_value_content_comment_or_order() {
    let source = concat!(
        "  # Keep this   comment, and its \"quotes\".\n",
        "AGENT claude\n",
        "SHARE \"./My Notes\"  ./\"a b\"/c   rw\n",
        "  NETWORK github.com\n",
        "ENV  GREETING \"hi  there\" $HOME\n",
        "SYSTEM Say \"hello\" first.\n",
        "system \"Be concise.\"\n",
        "SYSTEM ./\"My Prompts\"/system.md\n",
        "# middle\n",
        "PROMPT\n",
        "```text\n",
        "  indented  \n",
        "# not a comment\n",
        "\n",
        "$HOME stays\n",
        "```\n",
        "OS minimal\n",
    );
    let expected = concat!(
        "  # Keep this   comment, and its \"quotes\".\n",
        "AGENT claude\n",
        "SHARE \"./My Notes\"  ./\"a b\"/c   rw\n",
        "  NETWORK github.com\n",
        "ENV GREETING \"hi  there\" $HOME\n",
        "SYSTEM Say \"hello\" first.\n",
        "SYSTEM \"Be concise.\"\n",
        "SYSTEM ./\"My Prompts\"/system.md\n",
        "# middle\n",
        "PROMPT\n",
        "```text\n",
        "  indented  \n",
        "# not a comment\n",
        "\n",
        "$HOME stays\n",
        "```\n",
        "OS minimal\n",
    );
    assert_eq!(formatted(source), expected);
    assert_formats_well(source);
}

#[test]
fn text_with_errors_returns_the_errors_and_no_text() {
    let source = "agent claude\nbogus  thing \nSHARE \"./open\nMODEL a\nMODEL b\n";
    assert_eq!(format(source), Err(parse(source).unwrap_err()));
    assert_eq!(format(source).unwrap_err().len(), 3);
    assert!(format("").is_err());
    assert!(format("AGENT claude\nSYSTEM ```\nnever closed\n").is_err());
}

// spec(ap fmt): Formatting a formatted file changes nothing.
#[test]
fn formatting_a_formatted_file_changes_nothing() {
    for source in FIXTURES {
        let once = formatted(source);
        assert_eq!(formatted(&once), once);
    }
    let tidy = "# top\nAGENT claude\n\nSYSTEM\n```\na  \n\n\nb\n```\n  SHARE ./x\n";
    assert_eq!(formatted(tidy), tidy);
}

/// A small, seeded generator, so a failure reproduces.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        choices[self.below(choices.len())]
    }
}

/// An Agentfile built from random pieces, in random case and spacing. Some
/// have errors on purpose.
fn random_agentfile(random: &mut Random) -> String {
    const SPACE: [&str; 5] = ["", " ", "  ", "\t", " \t "];
    const BREAK: [&str; 3] = ["\n", "\r\n", "\r"];
    const NAMES: [&str; 8] = [
        "SHARE", "NETWORK", "ENV", "SYSTEM", "PROMPT", "MODEL", "PERSIST", "BOGUS",
    ];
    const ARGUMENTS: [&str; 7] = [
        "./out",
        "./out rw",
        "\"./My Notes\"",
        "A \"b  c\" $HOME",
        "fix #12 # now",
        "./\"a b\"/c",
        "\"/review\"",
    ];
    const CONTENT: [&str; 7] = ["Be brief.", "", "  indented  ", "# hash", "``", "$X", "\t"];
    fn case(random: &mut Random, name: &str) -> String {
        match random.below(3) {
            0 => name.to_string(),
            1 => name.to_ascii_lowercase(),
            _ => name
                .chars()
                .map(|c| {
                    if random.below(2) == 0 {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect(),
        }
    }
    let mut out = String::new();
    for _ in 0..random.below(3) {
        out += random.pick(&SPACE);
        out += random.pick(&BREAK);
    }
    out += random.pick(&SPACE);
    out += &case(random, "AGENT");
    out += " ";
    out += random.pick(&SPACE);
    out += "claude";
    out += random.pick(&SPACE);
    out += random.pick(&BREAK);
    for _ in 0..random.below(12) {
        let indent = if random.below(4) == 0 {
            random.pick(&SPACE)
        } else {
            ""
        };
        match random.below(6) {
            0 => out += random.pick(&SPACE),
            1 => {
                out += indent;
                out += "# note";
                out += random.pick(&SPACE);
            }
            2 => {
                let name = random.pick(&["SYSTEM", "PROMPT"]);
                let name = case(random, name);
                let ticks = random.pick(&["```", "````"]);
                out += indent;
                out += &name;
                if random.below(2) == 0 {
                    out += " ";
                    out += random.pick(&SPACE);
                } else {
                    out += random.pick(&BREAK);
                }
                out += ticks;
                out += random.pick(&["", "md", "json "]);
                out += random.pick(&BREAK);
                for _ in 0..1 + random.below(4) {
                    out += random.pick(&CONTENT);
                    out += random.pick(&BREAK);
                }
                out += ticks;
                out += random.pick(&SPACE);
            }
            _ => {
                let name = random.pick(&NAMES);
                out += indent;
                out += &case(random, name);
                out += " ";
                out += random.pick(&SPACE);
                out += random.pick(&ARGUMENTS);
                out += random.pick(&SPACE);
            }
        }
        out += random.pick(&BREAK);
    }
    if random.below(2) == 0 {
        let end = out.trim_end_matches(['\n', '\r']).len();
        out.truncate(end);
    }
    out
}

#[test]
fn random_agentfiles_format_well() {
    let mut random = Random(0x5EED_F0A6_E7E7);
    let (mut good, mut bad) = (0, 0);
    for _ in 0..5000 {
        let source = random_agentfile(&mut random);
        match parse(&source) {
            Ok(_) => {
                assert_formats_well(&source);
                good += 1;
            }
            Err(errors) => {
                assert_eq!(format(&source), Err(errors), "{source:?}");
                bad += 1;
            }
        }
    }
    assert!(good > 500 && bad > 500, "good {good}, bad {bad}");
}
