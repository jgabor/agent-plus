//! `ap inspect`, from SPEC.md's `ap inspect` section: an agent's layers, in
//! order, each with its digest and the instruction that made it, and the
//! grants you approved for its Agentfile. `ap inspect --grants` prints only
//! the grants, in the text form that `--approve=<file>` reads.
//!
//! Git credentials and the harness's own hosts aren't shown yet, because
//! nothing fetches a source or resolves a harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use store::Agents;

/// A test's own directory, with two home directories in it, so that a test
/// can build in a second store.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    other_home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("inspect")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let other_home = root.join("other-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&other_home).unwrap();
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
            other_home: other_home.canonicalize().unwrap(),
        }
    }

    /// Runs `ap` in `dir` with `home` as `HOME`, with nothing else in its
    /// environment and no terminal.
    fn ap_with(&self, home: &Path, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ap(&self, args: &[&str]) -> Output {
        self.ap_with(&self.home, &self.root, args)
    }

    /// Makes the directory `path` under the root with this Agentfile, and
    /// returns its path.
    fn project(&self, path: &str, agentfile: &str) -> PathBuf {
        let dir = self.root.join(path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        dir
    }

    /// Builds `dir` in the store under `home`, approving the grants in
    /// `grants`, and fails the test unless it builds.
    fn build(&self, home: &Path, dir: &Path, grants: &Path) {
        let approve = format!("--approve={}", grants.display());
        let output = self.ap_with(home, dir, &["build", &approve]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    /// The digests of the agent's layers, in order, from its record.
    fn digests(&self, name: &str) -> Vec<String> {
        let store = self.home.join(".local/share/ap");
        let agent = Agents::new(store).get(name).unwrap().unwrap();
        agent.layers.into_iter().map(|layer| layer.digest).collect()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// An Agentfile with two layers before the harness configuration, and a
/// grant of each kind that `ap build` can build.
const NOTES: &str = "AGENT claude\n\
                     ADD ./src /work/src\n\
                     ADD ./docs\n\
                     SHARE ../shared /work/shared\n\
                     SHARE ./out /work/out rw\n\
                     ENV NAME $USER\n\
                     SECRET GITHUB_TOKEN @api.github.com\n\
                     AUTH key\n\
                     NETWORK -\n\
                     NETWORK +github.com\n\
                     POLICY -\n\
                     POLICY +reviewer\n\
                     SYSTEM Be brief.\n";

/// Makes the notes project under the root, with what it adds and shares,
/// and returns it with the grants it asks for.
fn notes(scratch: &Scratch) -> (PathBuf, String) {
    let dir = scratch.project("notes", NOTES);
    for path in ["src", "docs", "out"] {
        fs::create_dir_all(dir.join(path)).unwrap();
        fs::write(dir.join(path).join("file"), path).unwrap();
    }
    fs::create_dir_all(scratch.root.join("shared")).unwrap();
    let grants = format!(
        "PATH {}/notes/out rw\n\
         PATH {}/shared\n\
         VARIABLE USER\n\
         SECRET GITHUB_TOKEN @api.github.com\n\
         AUTH key\n\
         NETWORK -\n\
         NETWORK +github.com\n\
         POLICY -\n\
         POLICY +reviewer\n",
        scratch.root.display(),
        scratch.root.display()
    );
    (dir, grants)
}

// spec(ap inspect): `ap inspect <agent>` shows an agent's layers and its approved grants: shares, host environment variables, secrets, git credentials, network policy, and the agents it may call.
#[test]
fn inspect_shows_the_layers_in_order_and_the_approved_grants() {
    let scratch = Scratch::new("shows");
    let (dir, grants) = notes(&scratch);
    let file = scratch.root.join("grants");
    fs::write(&file, &grants).unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let digests = scratch.digests("notes");
    assert_eq!(digests.len(), 3);
    let grants: String = grants.lines().map(|line| format!("  {line}\n")).collect();
    assert_eq!(
        stdout(&output),
        format!(
            "layers:\n  \
             {} ADD ./src /work/src\n  \
             {} ADD ./docs\n  \
             {} harness configuration\n\
             grants:\n{grants}",
            digests[0], digests[1], digests[2]
        )
    );
}

// spec(Grants): `--approve=<file>`, on `ap build` and `ap run`, approves exactly the grants listed in `<file>`, in the format that `ap inspect --grants` prints, and fails if the Agentfile asks for more.
#[test]
fn inspect_grants_prints_what_approves_a_fresh_build() {
    let scratch = Scratch::new("grants");
    let (dir, grants) = notes(&scratch);
    let file = scratch.root.join("grants");
    fs::write(&file, &grants).unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    assert_eq!(stdout(&output), grants);

    // What it prints approves a build of the same Agentfile in another store.
    let printed = scratch.root.join("printed");
    fs::write(&printed, &output.stdout).unwrap();
    scratch.build(&scratch.other_home, &dir, &printed);
    // The flag goes before or after the name.
    let after = scratch.ap(&["inspect", "notes", "--grants"]);
    assert_eq!(stdout(&after), grants);
}

#[test]
fn inspect_grants_escapes_what_it_prints() {
    let scratch = Scratch::new("escapes");
    let dir = scratch.project("notes", "AGENT claude\nSHARE ./a\u{202e}b /work/x rw\n");
    fs::create_dir_all(dir.join("a\u{202e}b")).unwrap();
    let file = scratch.root.join("grants");
    fs::write(
        &file,
        format!(
            "PATH {}/notes/a\\u{{202e}}b rw\nNETWORK +\nPOLICY +\n",
            scratch.root.display()
        ),
    )
    .unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "--grants", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), fs::read_to_string(&file).unwrap());
}

#[test]
fn inspect_shows_the_approval_of_the_agentfile_the_build_read() {
    let scratch = Scratch::new("symlink");
    let file = scratch.root.join("grants");
    let wide = scratch.project("wide", "AGENT claude\n");
    fs::write(&file, "NETWORK +\nPOLICY +\n").unwrap();
    scratch.build(&scratch.home, &wide, &file);
    let real = scratch.project("real", "AGENT claude\nNETWORK -\nPOLICY -\n");
    let dir = scratch.root.join("notes");
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(real.join("Agentfile"), dir.join("Agentfile")).unwrap();
    fs::write(&file, "NETWORK -\nPOLICY -\n").unwrap();
    scratch.build(&scratch.home, &dir, &file);

    // The link now leads to an Agentfile whose approval is wider, and then
    // nowhere.
    fs::remove_file(dir.join("Agentfile")).unwrap();
    std::os::unix::fs::symlink(wide.join("Agentfile"), dir.join("Agentfile")).unwrap();
    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(
        stdout(&output),
        "NETWORK -\nPOLICY -\n",
        "{}",
        stderr(&output)
    );
    fs::remove_file(dir.join("Agentfile")).unwrap();
    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(
        stdout(&output),
        "NETWORK -\nPOLICY -\n",
        "{}",
        stderr(&output)
    );
}

#[test]
fn inspect_an_unknown_agent_exits_1_and_names_it() {
    let scratch = Scratch::new("unknown");
    for args in [
        &["inspect", "missing"][..],
        &["inspect", "--grants", "missing"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "ap: no agent named missing\n");
    }
    // A name that can't name an agent is escaped, too.
    let output = scratch.ap(&["inspect", "No\u{202e}pe"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: no agent named No\\u{202e}pe\n");
}

#[test]
fn inspect_takes_one_agent_and_only_grants() {
    let scratch = Scratch::new("arguments");
    for args in [
        &["inspect"][..],
        &["inspect", "--grants"],
        &["inspect", "a", "b"],
        &["inspect", "--grants", "--grants", "a"],
        &["inspect", "--layers", "a"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("ap inspect [--grants] agent\n"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn usage_lists_inspect() {
    let scratch = Scratch::new("usage");
    let output = scratch.ap(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("ap inspect [--grants] agent\n"),
        "{}",
        stderr(&output)
    );
}
