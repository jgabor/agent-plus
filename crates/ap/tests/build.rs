//! `ap build`, from SPEC.md's Build section: the agent's name, its layers and
//! the store, the errors it reports, and what it records. What a layer holds
//! is `ap::add`'s, and its tests check it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use store::{Agent, Agents, Declared, Layers};

/// A test's own directory, with a home directory and a project in it.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("build")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    /// Makes the directory `name` with an Agentfile, and returns its path.
    fn project(&self, name: &str, agentfile: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        dir
    }

    /// Runs `ap` in `dir`, with only `HOME` in its environment.
    fn ap(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Runs `ap build` in `dir`, and fails the test unless it exits 0.
    fn build(&self, dir: &Path, args: &[&str]) -> String {
        let mut all = vec!["build"];
        all.extend_from_slice(args);
        let output = self.ap(dir, &all);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    }

    fn store(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    fn agents(&self) -> Agents {
        Agents::new(self.store())
    }

    fn agent(&self, name: &str) -> Agent {
        self.agents().get(name).unwrap().expect("no agent")
    }

    /// The uncompressed image of an agent's last layer, the harness
    /// configuration.
    fn configuration(&self, name: &str) -> Vec<u8> {
        let agent = self.agent(name);
        let last = agent.layers.last().expect("no layers");
        Layers::new(self.store()).get(&last.digest).unwrap()
    }

    /// The layer files in the store, by name.
    fn layer_files(&self) -> Vec<String> {
        match fs::read_dir(self.store().join("zstd-layers")) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What `ap build` said about each layer: `hit` or `built`, by label.
fn layer_lines(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter(|line| !line.starts_with("agent "))
        .map(|line| {
            let (status, label) = line.split_once(' ').unwrap();
            (status.to_string(), label.to_string())
        })
        .collect()
}

/// Whether `image` holds `text` as one run of bytes, which is how an
/// uncompressed EROFS image keeps a small file.
fn holds(image: &[u8], text: &str) -> bool {
    image
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

fn declared(name: &str, arguments: &[&str]) -> Declared {
    Declared {
        name: name.to_string(),
        arguments: arguments.iter().map(|a| a.to_string()).collect(),
    }
}

/// Every file and directory under `root`, with what a write would change.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, u64, i64, i64)> {
    let mut found = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path).unwrap();
        found.insert(
            path.clone(),
            (meta.ino(), meta.size(), meta.mtime(), meta.mtime_nsec()),
        );
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        }
    }
    found
}

const SIMPLE: &str = "AGENT claude\nADD ./data\n";

// spec(Build): `ap build [path]` reads the Agentfile in `path` (default: the current directory) and writes an image into the local store.
#[test]
fn build_reads_the_agentfile_in_path() {
    let scratch = Scratch::new("in-path");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    let out = scratch.build(&scratch.root, &["notes"]);
    assert!(out.ends_with("agent notes\n"), "{out}");
    let agent = scratch.agent("notes");
    assert_eq!(agent.directory, dir.to_str().unwrap());
    // The `ADD` layer, and the harness configuration.
    assert_eq!(agent.layers.len(), 2);
    assert_eq!(scratch.layer_files().len(), 2);
}

// spec(Build): `ap build [path]` reads the Agentfile in `path` (default: the current directory) and writes an image into the local store.
#[test]
fn build_defaults_to_the_current_directory() {
    let scratch = Scratch::new("current");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    scratch.build(&dir, &[]);
    assert_eq!(scratch.agent("notes").directory, dir.to_str().unwrap());
}

#[test]
fn build_without_an_agentfile_fails() {
    let scratch = Scratch::new("no-agentfile");
    fs::create_dir(scratch.root.join("empty")).unwrap();
    let output = scratch.ap(&scratch.root, &["build", "empty"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no Agentfile"),
        "{}",
        stderr(&output)
    );
}

// spec(Build): **Name.** The image is named after the Agentfile's directory.
// spec(Build): `ap build -t <name>` overrides it.
#[test]
fn the_name_comes_from_the_directory_or_t() {
    let scratch = Scratch::new("name");
    let dir = scratch.project("notes", "AGENT claude\n");
    scratch.build(&dir, &[]);
    scratch.build(&dir, &["-t", "reviewer"]);
    let names: Vec<String> = scratch
        .agents()
        .list()
        .unwrap()
        .into_iter()
        .map(|agent| agent.name)
        .collect();
    assert_eq!(names, ["notes", "reviewer"]);
}

// spec(Build): A directory name that doesn't fit needs `-t`.
#[test]
fn a_directory_name_that_does_not_fit_needs_t() {
    let scratch = Scratch::new("bad-name");
    let dir = scratch.project("My Notes", "AGENT claude\n");
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("-t"), "{}", stderr(&output));
    assert!(scratch.agents().list().unwrap().is_empty());

    scratch.build(&dir, &["-t", "my-notes"]);
    assert_eq!(scratch.agent("my-notes").directory, dir.to_str().unwrap());
}

#[test]
fn an_invalid_t_name_fails() {
    let scratch = Scratch::new("bad-t");
    let dir = scratch.project("notes", "AGENT claude\n");
    let output = scratch.ap(&dir, &["build", "-t", "Notes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Notes"), "{}", stderr(&output));
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn bad_usage_exits_2() {
    let scratch = Scratch::new("usage");
    let dir = scratch.project("notes", "AGENT claude\n");
    for args in [
        &["build", "--nope"][..],
        &["build", "-t"],
        &["build", "a", "b"],
        &["build", "--strict", "--strict"],
    ] {
        let output = scratch.ap(&dir, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("ap build"), "{args:?}");
    }
}

#[test]
fn the_usage_message_lists_build() {
    let scratch = Scratch::new("usage-lists");
    let output = scratch.ap(&scratch.root, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("ap build [--strict] [-t name] [dir]"),
        "{}",
        stderr(&output)
    );
}

// spec(Build): The store remembers which directory each name was built from, and building a different directory under an existing name is an error.
#[test]
fn another_directory_under_a_name_fails() {
    let scratch = Scratch::new("taken");
    let first = scratch.project("one", "AGENT claude\n");
    let second = scratch.project("two", "AGENT codex\n");
    scratch.build(&first, &["-t", "agent"]);
    let output = scratch.ap(&second, &["build", "-t", "agent"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("agent"), "{}", stderr(&output));
    assert_eq!(scratch.agent("agent").directory, first.to_str().unwrap());
}

#[test]
fn a_rebuild_with_nothing_changed_is_a_hit_and_writes_nothing() {
    let scratch = Scratch::new("hit");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nSYSTEM ./system.md\nADD ./data\nADD ./src /work\nSHARE ./out\n",
    );
    fs::write(dir.join("system.md"), "Be careful.\n").unwrap();
    fs::write(dir.join("data"), "one").unwrap();
    fs::create_dir_all(dir.join("src/lib")).unwrap();
    fs::write(dir.join("src/lib/main.rs"), "fn main() {}\n").unwrap();
    fs::create_dir(dir.join("out")).unwrap();

    let first = layer_lines(&scratch.build(&dir, &[]));
    assert_eq!(first.len(), 3, "{first:?}");
    assert!(
        first.iter().all(|(status, _)| status == "built"),
        "{first:?}"
    );
    let before = snapshot(&scratch.store());

    let second = layer_lines(&scratch.build(&dir, &[]));
    let labels = |lines: &[(String, String)]| -> Vec<String> {
        lines.iter().map(|(_, label)| label.clone()).collect()
    };
    assert_eq!(labels(&second), labels(&first));
    assert!(
        second.iter().all(|(status, _)| status == "hit"),
        "{second:?}"
    );
    assert_eq!(snapshot(&scratch.store()), before);
}

// spec(Build): Layers are independent, so changing one instruction rebuilds only that layer.
#[test]
fn changing_one_added_file_rebuilds_only_its_layer() {
    let scratch = Scratch::new("one-layer");
    let dir = scratch.project("notes", "AGENT claude\nADD ./a\nADD ./b\n");
    fs::write(dir.join("a"), "one").unwrap();
    fs::write(dir.join("b"), "two").unwrap();
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;

    fs::write(dir.join("b"), "three").unwrap();
    let lines = layer_lines(&scratch.build(&dir, &[]));
    let statuses: Vec<&str> = lines.iter().map(|(status, _)| status.as_str()).collect();
    assert_eq!(statuses, ["hit", "built", "hit"], "{lines:?}");
    let after = scratch.agent("notes").layers;
    assert_eq!(after[0], before[0]);
    assert_ne!(after[1], before[1]);
    assert_eq!(after[2], before[2]);
    // The layer nothing uses any more is freed.
    assert_eq!(scratch.layer_files().len(), 3);
}

// spec(Build): **Store.** Layers are stored by the digest of their uncompressed content and shared across every agent in the store.
#[test]
fn agents_share_their_layers() {
    let scratch = Scratch::new("shared");
    let one = scratch.project("one", SIMPLE);
    let two = scratch.project("two", SIMPLE);
    fs::write(one.join("data"), "same").unwrap();
    fs::write(two.join("data"), "same").unwrap();
    scratch.build(&one, &[]);
    let lines = layer_lines(&scratch.build(&two, &[]));
    assert!(lines.iter().all(|(status, _)| status == "hit"), "{lines:?}");
    assert_eq!(scratch.agent("one").layers, scratch.agent("two").layers);
    assert_eq!(scratch.layer_files().len(), 2);
}

// spec(Text values): The file is read at build time on the host and its content is captured into the image.
#[test]
fn a_text_path_is_read_at_build_time() {
    let scratch = Scratch::new("text-path");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nSYSTEM ./system.md\nPROMPT ./prompt.md\nADD ./data\n",
    );
    fs::write(dir.join("system.md"), "Be careful.\n").unwrap();
    fs::write(dir.join("prompt.md"), "Review the notes.\n").unwrap();
    fs::write(dir.join("data"), "one").unwrap();
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;
    let image = scratch.configuration("notes");
    assert!(holds(&image, "Be careful.\n"));
    assert!(holds(&image, "Review the notes.\n"));

    for file in ["system.md", "prompt.md"] {
        fs::write(dir.join(file), "Something else.\n").unwrap();
        let lines = layer_lines(&scratch.build(&dir, &[]));
        let statuses: Vec<&str> = lines.iter().map(|(status, _)| status.as_str()).collect();
        assert_eq!(statuses, ["hit", "built"], "{file}: {lines:?}");
    }
    let after = scratch.agent("notes").layers;
    assert_eq!(after[0], before[0]);
    assert_ne!(after[1], before[1]);
}

// spec(SYSTEM): The values concatenate in file order, separated by a blank line.
#[test]
fn system_values_concatenate_with_a_blank_line() {
    let scratch = Scratch::new("system-join");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nSYSTEM One.\nSYSTEM ./two.md\nSYSTEM\n```\nThree.\n```\n",
    );
    // A value's own blank lines are kept.
    fs::write(dir.join("two.md"), "Two.\n\n").unwrap();
    scratch.build(&dir, &[]);
    let image = scratch.configuration("notes");
    assert!(holds(&image, "One.\n\nTwo.\n\n\nThree.\n"));
    assert!(holds(&image, "CLAUDE.md"));
}

#[test]
fn literal_text_is_captured_too() {
    let scratch = Scratch::new("text-literal");
    let dir = scratch.project("notes", "AGENT claude\nSYSTEM Be careful.\n");
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;
    fs::write(dir.join("Agentfile"), "AGENT claude\nSYSTEM Be bold.\n").unwrap();
    scratch.build(&dir, &[]);
    assert_ne!(scratch.agent("notes").layers, before);
}

// spec(Text values): A missing file is an error, never silently treated as text.
#[test]
fn a_missing_text_file_is_an_error() {
    let scratch = Scratch::new("text-missing");
    for (instruction, line) in [("SYSTEM", 2), ("PROMPT", 3)] {
        let agentfile = match instruction {
            "SYSTEM" => "AGENT claude\nSYSTEM ./missing.md\n",
            _ => "AGENT claude\nOS minimal\nPROMPT ./missing.md\n",
        };
        let dir = scratch.project(instruction, agentfile);
        let output = scratch.ap(&dir, &["build", "-t", "notes"]);
        assert_eq!(output.status.code(), Some(1), "{instruction}");
        let message = stderr(&output);
        assert!(
            message.starts_with(&format!("Agentfile:{line}: ")),
            "{message}"
        );
        assert!(message.contains("missing.md"), "{message}");
    }
    assert!(scratch.agents().list().unwrap().is_empty());
}

// spec(Build): To tell directories from files, `ap build` checks every share source, so a missing source is a build error (see [SHARE](#share)).
#[test]
fn a_missing_share_source_is_an_error() {
    let scratch = Scratch::new("share-missing");
    let dir = scratch.project("notes", "AGENT claude\nSHARE ./here\nSHARE ./gone\n");
    fs::create_dir(dir.join("here")).unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:3: "), "{message}");
    assert!(message.contains("gone"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_missing_add_source_is_an_error() {
    let scratch = Scratch::new("add-missing");
    let dir = scratch.project("notes", "AGENT claude\nADD ./gone\n");
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:2: "), "{message}");
    assert!(message.contains("gone"), "{message}");
}

#[test]
fn instructions_that_are_not_built_yet_fail() {
    let scratch = Scratch::new("not-yet");
    let dir = scratch.project(
        "notes",
        "AGENT claude\n\
         REPO jgabor/agent-plus\n\
         PLUGIN anthropics/claude-plugins-official/external_plugins/context7\n\
         SKILL anthropics/skills/skills/algorithmic-art\n\
         MCP anthropics/claude-plugins-official/external_plugins/github\n\
         SEAL GITHUB_TOKEN @api.github.com\n",
    );
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 5, "{message}");
    let names = ["REPO", "PLUGIN", "SKILL", "MCP", "SEAL"];
    for ((line, name), number) in lines.iter().zip(names).zip(2..) {
        assert!(line.starts_with(&format!("Agentfile:{number}: ")), "{line}");
        assert!(line.contains(&format!("`{name}`")), "{line}");
        assert!(line.contains("not built yet"), "{line}");
    }
    assert!(scratch.agents().list().unwrap().is_empty());
}

// spec(Overview): `ap build` reports every error it can find in the Agentfile itself.
#[test]
fn build_reports_every_error() {
    let scratch = Scratch::new("every-error");
    let dir = scratch.project("notes", "AGENT claude\nNOPE x\nMODEL opus\nMODEL sonnet\n");
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 2, "{message}");
    assert!(lines[0].starts_with("Agentfile:2: "), "{message}");
    assert!(lines[1].starts_with("Agentfile:4: "), "{message}");

    // Errors that need the host are reported together, in line order.
    fs::write(
        dir.join("Agentfile"),
        "AGENT claude\nSHARE ./gone\nSYSTEM ./missing.md\nADD ./absent\nREPO jgabor/agent-plus\n",
    )
    .unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 4, "{message}");
    for (line, number) in lines.iter().zip(2..) {
        assert!(
            line.starts_with(&format!("Agentfile:{number}: ")),
            "{message}"
        );
    }
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_destination_error_does_not_hide_other_errors() {
    let scratch = Scratch::new("destination-and-more");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nADD ./data /etc/data\nREPO jgabor/agent-plus\nSYSTEM ./missing.md\n",
    );
    fs::write(dir.join("data"), "one").unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 3, "{message}");
    for (line, number) in lines.iter().zip(2..) {
        assert!(
            line.starts_with(&format!("Agentfile:{number}: ")),
            "{message}"
        );
    }
}

#[test]
fn a_layer_that_can_not_be_made_writes_nothing() {
    let scratch = Scratch::new("bad-image");
    let long = "x".repeat(256);
    let dir = scratch.project(
        "notes",
        &format!("AGENT claude\nADD ./a\nADD ./b /work/{long}\n"),
    );
    fs::write(dir.join("a"), "one").unwrap();
    fs::write(dir.join("b"), "two").unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:3: "), "{message}");
    assert!(scratch.layer_files().is_empty());
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn errors_name_the_agentfile_by_its_path() {
    let scratch = Scratch::new("error-path");
    scratch.project("notes", "AGENT claude\nNOPE x\n");
    let output = scratch.ap(&scratch.root, &["build", "notes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("notes/Agentfile:2: "),
        "{}",
        stderr(&output)
    );
}

// spec(Build): `ap build --strict` turns warnings into errors.
#[test]
fn strict_turns_a_warning_into_an_error() {
    let scratch = Scratch::new("strict");
    let dir = scratch.project("notes", "AGENT claude\nADD ./data\n");
    fs::create_dir(dir.join("data")).unwrap();
    let _socket = UnixListener::bind(dir.join("data/socket")).unwrap();

    let output = scratch.ap(&dir, &["build", "--strict"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:2: "), "{message}");
    assert!(!message.contains("warning"), "{message}");
    assert!(message.contains("socket"), "{message}");
    assert!(scratch.agents().list().unwrap().is_empty());
    assert!(scratch.layer_files().is_empty());

    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:2: warning: "), "{message}");
    assert!(message.contains("socket"), "{message}");
    scratch.agent("notes");
}

#[test]
fn settings_and_run_instructions_are_recorded() {
    let scratch = Scratch::new("recorded");
    let dir = scratch.project(
        "notes",
        "AGENT claude:2.1.319\n\
         OS minimal\n\
         SYSTEM Be careful.\n\
         MODEL opus\n\
         EFFORT high\n\
         MODE auto\n\
         ADD ./data\n\
         ENV LOG_LEVEL debug\n\
         SHARE ./out rw\n\
         WORKDIR ~/out\n\
         NETWORK +example.com\n",
    );
    fs::write(dir.join("data"), "one").unwrap();
    fs::create_dir(dir.join("out")).unwrap();
    scratch.build(&dir, &[]);
    let agent = scratch.agent("notes");
    assert_eq!(
        agent.declared,
        [
            declared("AGENT", &["claude:2.1.319"]),
            declared("OS", &["minimal"]),
            declared("MODEL", &["opus"]),
            declared("EFFORT", &["high"]),
            declared("MODE", &["auto"]),
            declared("ADD", &["./data"]),
            declared("ENV", &["LOG_LEVEL debug"]),
            declared("SHARE", &["./out", "rw"]),
            declared("WORKDIR", &["~/out"]),
            declared("NETWORK", &["+example.com"]),
        ]
    );
    // Nothing is resolved, so nothing is locked.
    assert!(!dir.join("Agentfile.lock").exists());
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["Agentfile", "data", "out"]);
}
