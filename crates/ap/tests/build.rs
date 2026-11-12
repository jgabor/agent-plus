//! `ap build`, from SPEC.md's Build section: the agent's name, its layers and
//! the store, the errors it reports, and what it records. What a layer holds
//! is `ap::add`'s, and its tests check it.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
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
        common::seed_os(&home.join(".local/share/ap"));
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
        common::pin_harness(&self.home, &dir);
        dir
    }

    /// Runs `ap` in `dir`, with only `HOME` in its environment. A build
    /// approves exactly the grants its Agentfile asks for, since approval
    /// has tests of its own.
    fn ap(&self, dir: &Path, args: &[&str]) -> Output {
        let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        if args.first().is_some_and(|command| command == "build")
            && let Some(file) = self.approval(dir, &args[1..])
        {
            args.push(format!("--approve={}", file.display()));
        }
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Writes the grants that the Agentfile `ap build` would read asks for,
    /// and returns the file, or `None` if they can't be read.
    fn approval(&self, dir: &Path, args: &[String]) -> Option<PathBuf> {
        let mut target = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "-t" {
                args.next()?;
            } else if !arg.starts_with('-') {
                target = Some(dir.join(arg));
            }
        }
        let context = target
            .unwrap_or_else(|| dir.to_path_buf())
            .canonicalize()
            .ok()?;
        let source = fs::read_to_string(context.join("Agentfile")).ok()?;
        let instructions = agentfile::parse(&source).ok()?;
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &context, &self.home)
        })
        .ok()?;
        let file = self.root.join("approved-grants");
        fs::write(&file, grants.to_string()).unwrap();
        Some(file)
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

/// Binds a Unix socket at `path`, however long the checkout's path is.
///
/// A socket address holds about 104 bytes, fewer than a deep checkout's
/// target directory needs, so this binds through a short link in `/tmp` to the
/// socket's directory, and removes the link afterwards.
fn bind(path: &Path) -> UnixListener {
    let link = PathBuf::from(format!("/tmp/ap-build-{}", std::process::id()));
    let _ = fs::remove_file(&link);
    symlink(path.parent().unwrap(), &link).unwrap();
    let socket = UnixListener::bind(link.join(path.file_name().unwrap()));
    fs::remove_file(&link).unwrap();
    socket.unwrap()
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

const SIMPLE: &str = "AGENT claude\nOS minimal\nADD ./data\n";

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
    // The OS, the harness, the `ADD` layer, and the harness configuration.
    assert_eq!(agent.layers.len(), 4);
    assert_eq!(scratch.layer_files().len(), 4);
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
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\n");
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
    let dir = scratch.project("My Notes", "AGENT claude\nOS minimal\n");
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
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\n");
    let output = scratch.ap(&dir, &["build", "-t", "Notes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Notes"), "{}", stderr(&output));
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn bad_usage_exits_2() {
    let scratch = Scratch::new("usage");
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\n");
    for args in [
        &["build", "--nope"][..],
        &["build", "-t"],
        &["build", "a", "b"],
        &["build", "--strict", "--strict"],
        &["build", "--replace", "--replace"],
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
        stderr(&output)
            .contains("ap build [--strict] [--replace] [--approve=file] [-t name] [dir]"),
        "{}",
        stderr(&output)
    );
}

// spec(Build): The store remembers which directory each name was built from, and building a different directory under an existing name is an error.
#[test]
fn another_directory_under_a_name_fails() {
    let scratch = Scratch::new("taken");
    let first = scratch.project("one", "AGENT claude\nOS minimal\n");
    let second = scratch.project("two", "AGENT codex\nOS minimal\n");
    scratch.build(&first, &["-t", "agent"]);
    let output = scratch.ap(&second, &["build", "-t", "agent"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("agent"), "{}", stderr(&output));
    assert_eq!(scratch.agent("agent").directory, first.to_str().unwrap());
}

// spec(Build): If you moved the directory, `ap build --replace` takes over the name and keeps the agent's persisted state, and history and pending changes for paths under the old directory move to the new one.
#[test]
fn replace_takes_over_a_name_from_a_moved_directory() {
    let scratch = Scratch::new("replace");
    let old = scratch.project("old", "AGENT claude\nOS minimal\nSYSTEM \"before\"\n");
    scratch.build(&old, &["-t", "agent"]);
    let before = scratch.agent("agent");
    let moved = scratch.root.join("moved");
    fs::rename(&old, &moved).unwrap();
    fs::write(
        moved.join("Agentfile"),
        "AGENT claude\nOS minimal\nSYSTEM \"after\"\n",
    )
    .unwrap();

    let output = scratch.ap(&moved, &["build", "-t", "agent"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--replace"), "{}", stderr(&output));
    assert_eq!(scratch.agent("agent"), before);

    let out = scratch.build(&moved, &["--replace", "-t", "agent"]);
    assert!(out.ends_with("agent agent\n"), "{out}");
    let after = scratch.agent("agent");
    assert_eq!(after.directory, moved.to_str().unwrap());
    assert!(holds(&scratch.configuration("agent"), "after"));
    // The old configuration layer is freed, and only the new one is kept.
    assert_eq!(scratch.layer_files().len(), after.layers.len());

    // The name now belongs to the new directory, so a plain build is a hit.
    let out = scratch.build(&moved, &["-t", "agent"]);
    assert!(
        layer_lines(&out).iter().all(|(verb, _)| verb == "hit"),
        "{out}"
    );
}

#[test]
fn replace_on_a_new_name_just_builds() {
    let scratch = Scratch::new("replace-new");
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\n");
    scratch.build(&dir, &["--replace"]);
    assert_eq!(scratch.agent("notes").directory, dir.to_str().unwrap());
}

#[test]
fn a_rebuild_with_nothing_changed_is_a_hit_and_writes_nothing() {
    let scratch = Scratch::new("hit");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nSYSTEM ./system.md\nADD ./data\nADD ./src /work\nSHARE ./out\n",
    );
    fs::write(dir.join("system.md"), "Be careful.\n").unwrap();
    fs::write(dir.join("data"), "one").unwrap();
    fs::create_dir_all(dir.join("src/lib")).unwrap();
    fs::write(dir.join("src/lib/main.rs"), "fn main() {}\n").unwrap();
    fs::create_dir(dir.join("out")).unwrap();

    let first = layer_lines(&scratch.build(&dir, &[]));
    assert_eq!(first.len(), 5, "{first:?}");
    // The store already has the OS layer.
    assert_eq!(first[0], ("hit".to_string(), "OS minimal".to_string()));
    assert!(
        first[1..].iter().all(|(status, _)| status == "built"),
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
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\nADD ./a\nADD ./b\n");
    fs::write(dir.join("a"), "one").unwrap();
    fs::write(dir.join("b"), "two").unwrap();
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;

    fs::write(dir.join("b"), "three").unwrap();
    let lines = layer_lines(&scratch.build(&dir, &[]));
    let statuses: Vec<&str> = lines.iter().map(|(status, _)| status.as_str()).collect();
    assert_eq!(statuses, ["hit", "hit", "hit", "built", "hit"], "{lines:?}");
    let after = scratch.agent("notes").layers;
    assert_eq!(after[..3], before[..3]);
    assert_ne!(after[3], before[3]);
    assert_eq!(after[4], before[4]);
    // The layer nothing uses any more is freed.
    assert_eq!(scratch.layer_files().len(), 5);
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
    assert_eq!(scratch.layer_files().len(), 4);
}

// spec(Text values): The file is read at build time on the host and its content is captured into the image.
#[test]
fn a_text_path_is_read_at_build_time() {
    let scratch = Scratch::new("text-path");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nSYSTEM ./system.md\nPROMPT ./prompt.md\nADD ./data\n",
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
        assert_eq!(statuses, ["hit", "hit", "hit", "built"], "{file}: {lines:?}");
    }
    let after = scratch.agent("notes").layers;
    assert_eq!(after[..3], before[..3]);
    assert_ne!(after[3], before[3]);
}

// spec(SYSTEM): The values concatenate in file order, separated by a blank line.
#[test]
fn system_values_concatenate_with_a_blank_line() {
    let scratch = Scratch::new("system-join");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nSYSTEM One.\nSYSTEM ./two.md\nSYSTEM\n```\nThree.\n```\n",
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
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\nSYSTEM Be careful.\n");
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;
    fs::write(
        dir.join("Agentfile"),
        "AGENT claude\nOS minimal\nSYSTEM Be bold.\n",
    )
    .unwrap();
    scratch.build(&dir, &[]);
    assert_ne!(scratch.agent("notes").layers, before);
}

// spec(Text values): A missing file is an error, never silently treated as text.
#[test]
fn a_missing_text_file_is_an_error() {
    let scratch = Scratch::new("text-missing");
    for (instruction, line) in [("SYSTEM", 3), ("PROMPT", 3)] {
        let agentfile = match instruction {
            "SYSTEM" => "AGENT claude\nOS minimal\nSYSTEM ./missing.md\n",
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
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nSHARE ./here\nSHARE ./gone\n",
    );
    fs::create_dir(dir.join("here")).unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:4: "), "{message}");
    assert!(message.contains("gone"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_missing_add_source_is_an_error() {
    let scratch = Scratch::new("add-missing");
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\nADD ./gone\n");
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:3: "), "{message}");
    assert!(message.contains("gone"), "{message}");
}

#[test]
fn instructions_that_are_not_built_yet_fail() {
    let scratch = Scratch::new("not-yet");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\n\
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
    for ((line, name), number) in lines.iter().zip(names).zip(3..) {
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
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nNOPE x\nMODEL opus\nMODEL sonnet\n",
    );
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 2, "{message}");
    assert!(lines[0].starts_with("Agentfile:3: "), "{message}");
    assert!(lines[1].starts_with("Agentfile:5: "), "{message}");

    // Errors that need the host are reported together, in line order.
    fs::write(
        dir.join("Agentfile"),
        "AGENT claude\nOS minimal\nSHARE ./gone\nSYSTEM ./missing.md\nADD ./absent\nREPO jgabor/agent-plus\n",
    )
    .unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 4, "{message}");
    for (line, number) in lines.iter().zip(3..) {
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
        "AGENT claude\nOS minimal\nADD ./data /etc/data\nREPO jgabor/agent-plus\nSYSTEM ./missing.md\n",
    );
    fs::write(dir.join("data"), "one").unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 3, "{message}");
    for (line, number) in lines.iter().zip(3..) {
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
        &format!("AGENT claude\nOS minimal\nADD ./a\nADD ./b /work/{long}\n"),
    );
    fs::write(dir.join("a"), "one").unwrap();
    fs::write(dir.join("b"), "two").unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:4: "), "{message}");
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn errors_name_the_agentfile_by_its_path() {
    let scratch = Scratch::new("error-path");
    scratch.project("notes", "AGENT claude\nOS minimal\nNOPE x\n");
    let output = scratch.ap(&scratch.root, &["build", "notes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("notes/Agentfile:3: "),
        "{}",
        stderr(&output)
    );
}

// spec(Build): `ap build --strict` turns warnings into errors.
#[test]
fn strict_turns_a_warning_into_an_error() {
    let scratch = Scratch::new("strict");
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\nADD ./data\n");
    fs::create_dir(dir.join("data")).unwrap();
    let _socket = bind(&dir.join("data/socket"));

    let output = scratch.ap(&dir, &["build", "--strict"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:3: "), "{message}");
    assert!(!message.contains("warning"), "{message}");
    assert!(message.contains("socket"), "{message}");
    assert!(scratch.agents().list().unwrap().is_empty());
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);

    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:3: warning: "), "{message}");
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
    let lock = fs::read_to_string(dir.join("Agentfile.lock")).unwrap();
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
    // Nothing is resolved, so the lockfile is only read.
    assert_eq!(
        fs::read_to_string(dir.join("Agentfile.lock")).unwrap(),
        lock
    );
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["Agentfile", "Agentfile.lock", "data", "out"]);
}

/// The line `ap build` printed for the harness layer.
fn harness_line(out: &str) -> (String, String) {
    layer_lines(out)
        .into_iter()
        .find(|(_, label)| label.starts_with("AGENT "))
        .expect("no harness layer")
}

// spec(Build): A kernel, OS, or harness layer is downloaded once, however many agents use it.
#[test]
fn agents_that_lock_one_harness_share_its_layer() {
    let scratch = Scratch::new("harness-shared");
    let one = scratch.project("one", "AGENT claude\nOS minimal\nSYSTEM One.\n");
    let two = scratch.project("two", "AGENT claude\nOS minimal\nSYSTEM Two.\n");
    let first = scratch.build(&one, &[]);
    assert_eq!(
        harness_line(&first),
        ("built".to_string(), "AGENT claude:0.0.0".to_string())
    );
    let second = scratch.build(&two, &[]);
    assert_eq!(
        harness_line(&second),
        ("hit".to_string(), "AGENT claude:0.0.0".to_string())
    );
    let (one, two) = (scratch.agent("one").layers, scratch.agent("two").layers);
    assert_eq!(one[1], two[1]);
    assert_eq!(one[1].instruction, "AGENT claude:0.0.0");
    // The OS, one harness layer, and each agent's own configuration.
    assert_eq!(scratch.layer_files().len(), 4);
    let image = Layers::new(scratch.store()).get(&one[1].digest).unwrap();
    assert!(holds(&image, std::str::from_utf8(common::HARNESS).unwrap()));
    assert!(holds(&image, "claude"));
}

#[test]
fn a_rebuild_reuses_the_locked_harness() {
    let scratch = Scratch::new("harness-rebuild");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    scratch.build(&dir, &[]);
    let before = scratch.agent("notes").layers;
    // The store has the layer, so the build doesn't need the cached file.
    fs::remove_file(common::cached(
        &scratch.home,
        &common::digest(common::HARNESS),
    ))
    .unwrap();
    let out = scratch.build(&dir, &[]);
    assert_eq!(harness_line(&out).0, "hit", "{out}");
    assert_eq!(scratch.agent("notes").layers, before);
}

#[test]
fn a_cached_harness_with_the_wrong_digest_is_refused() {
    let scratch = Scratch::new("harness-digest");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    let path = common::cached(&scratch.home, &common::digest(common::HARNESS));
    fs::write(&path, "something else\n").unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:1: "), "{message}");
    assert!(
        message.contains(&common::digest(b"something else\n")),
        "{message}"
    );
    assert!(
        message.contains(&common::digest(common::HARNESS)),
        "{message}"
    );
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_build_with_no_locked_harness_fails() {
    let scratch = Scratch::new("harness-unlocked");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    fs::remove_file(dir.join("Agentfile.lock")).unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:1: "), "{message}");
    assert!(message.contains("Agentfile.lock"), "{message}");
    assert!(message.contains("not built yet"), "{message}");
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_lock_for_another_harness_or_version_is_not_reused() {
    let scratch = Scratch::new("harness-stale");
    for (name, agentfile) in [
        ("other", "AGENT codex\nOS minimal\n"),
        ("version", "AGENT claude:1.2.3\nOS minimal\n"),
    ] {
        let dir = scratch.project(name, "AGENT claude\nOS minimal\n");
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        let output = scratch.ap(&dir, &["build"]);
        assert_eq!(output.status.code(), Some(1), "{name}");
        let message = stderr(&output);
        assert!(message.starts_with("Agentfile:1: "), "{message}");
        assert!(message.contains("claude:0.0.0"), "{message}");
        assert!(message.contains("not built yet"), "{message}");
    }
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_locked_harness_missing_from_the_cache_fails() {
    let scratch = Scratch::new("harness-uncached");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    let path = common::cached(&scratch.home, &common::digest(common::HARNESS));
    fs::remove_file(&path).unwrap();
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("Agentfile:1: "), "{message}");
    assert!(message.contains(path.to_str().unwrap()), "{message}");
    assert!(message.contains("not built yet"), "{message}");
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

#[test]
fn a_bad_lockfile_fails() {
    let scratch = Scratch::new("harness-bad-lock");
    let dir = scratch.project("notes", "AGENT claude\nOS minimal\n");
    for lock in [
        "HARNESS claude 0.0.0\n",
        "HARNESS claude 0.0.0 abc\n",
        "HARNESS cursor 0.0.0 0000000000000000000000000000000000000000000000000000000000000000\n",
        "NOPE x\n",
    ] {
        fs::write(dir.join("Agentfile.lock"), lock).unwrap();
        let output = scratch.ap(&dir, &["build"]);
        assert_eq!(output.status.code(), Some(1), "{lock}");
        let message = stderr(&output);
        assert!(
            message.starts_with("Agentfile.lock:1: "),
            "{lock}: {message}"
        );
    }
    let sha256 = common::digest(common::HARNESS);
    fs::write(
        dir.join("Agentfile.lock"),
        format!("HARNESS claude 0.0.0 {sha256}\nHARNESS claude 0.0.0 {sha256}\n"),
    )
    .unwrap();
    let message = stderr(&scratch.ap(&dir, &["build"]));
    assert!(message.starts_with("Agentfile.lock:2: "), "{message}");
    assert_eq!(scratch.layer_files(), [ap::fetch::os().sha256]);
    assert!(scratch.agents().list().unwrap().is_empty());
}

/// Only `OS minimal` is built. Any other tier fails before anything is
/// downloaded, and the error names the tier.
#[test]
fn other_os_tiers_fail_before_any_download() {
    let scratch = Scratch::new("os-tiers");
    // With no OS layer in the store, a build that got as far as the OS
    // would download it.
    fs::remove_dir_all(scratch.store()).unwrap();
    for (agentfile, error) in [
        (
            "AGENT claude\nOS standard\n",
            "Agentfile:2: `OS standard` is not built yet\n",
        ),
        (
            "AGENT claude\nOS desktop\n",
            "Agentfile:2: `OS desktop` is not built yet\n",
        ),
        (
            "AGENT claude\n",
            "Agentfile:1: `OS desktop`, the default, is not built yet\n",
        ),
    ] {
        let dir = scratch.project("notes", agentfile);
        let output = scratch.ap(&dir, &["build"]);
        assert_eq!(output.status.code(), Some(1), "{agentfile}");
        assert_eq!(stderr(&output), error);
        assert_eq!(stdout(&output), "");
        assert!(!scratch.store().join("zstd-layers").exists());
        assert!(scratch.agents().list().unwrap().is_empty());
    }
}

/// The pinned OS layer is every agent's bottom layer, which a build finds in
/// the store and doesn't download again.
#[test]
fn the_os_layer_is_the_bottom_layer() {
    let scratch = Scratch::new("os-bottom");
    let dir = scratch.project("notes", SIMPLE);
    fs::write(dir.join("data"), "one").unwrap();
    let lines = layer_lines(&scratch.build(&dir, &[]));
    assert_eq!(lines[0], ("hit".to_string(), "OS minimal".to_string()));
    let bottom = &scratch.agent("notes").layers[0];
    assert_eq!(bottom.digest, ap::fetch::os().sha256);
    assert_eq!(bottom.instruction, "OS minimal");
}
