//! `ap run` itself: a static `ap` builds an agent whose harness is the fake
//! one, then boots it headless and reads what the harness saw.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output, Stdio};
use std::sync::OnceLock;

use ap::fetch;
use ap::store::Store;
use t1::{guest_binary, kernel};

/// The status `ap run` exits with when it fails before the harness starts.
const FAILED: i32 = 125;
/// How long one run may take, in seconds, before `timeout` stops `ap run`,
/// and the VM it holds with it.
const TIMEOUT: &str = "120";
const PROMPT: &str = "Say what you can see.";
const BUILT_PROMPT: &str = "What is here?";
/// The fake harness exits with this, so that a run's status is the harness's.
const STATUS: i32 = 3;

/// Built and fetched once, because the tests in this file run at once, and
/// each build or download would replace a file another test is using.
fn shared() -> &'static (PathBuf, PathBuf, PathBuf) {
    static PATHS: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    PATHS.get_or_init(|| {
        (
            guest_binary::ap(),
            guest_binary::fake_harness(),
            kernel::vmlinux(),
        )
    })
}

/// A home of its own, with the kernel in its store and the fake harness in
/// its download cache, so that `ap` reads none of the owner's files and
/// fetches nothing.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    /// The fake harness's SHA-256 digest.
    harness: String,
}

impl Scratch {
    fn new(test: &str) -> Scratch {
        let root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ap-run-{test}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let store = Store::new(home.join(".local/share/ap"));
        store
            .put(fetch::kernel().sha256, File::open(&shared().2).unwrap())
            .unwrap();
        let fake = &shared().1;
        let harness = sha256(fake);
        let cache = home.join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(fake, cache.join(&harness)).unwrap();
        Scratch {
            root,
            home,
            harness,
        }
    }

    /// Builds the agent `name`, whose harness is the fake one, approving
    /// what this Agentfile asks.
    fn build(&self, name: &str, agentfile: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude 0.0.0 {}\n", self.harness),
        )
        .unwrap();
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let built = self.ap(&[
            "build",
            &format!("--approve={}", approved.display()),
            dir.to_str().unwrap(),
        ]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");
    }

    /// Runs the static `ap` with these arguments and only `HOME` and
    /// `GREETED` in its environment, under a timeout.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args(["-k", "10", TIMEOUT])
            .arg(&shared().0)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("GREETED", "the host")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs the agent headless, and returns the fake harness's one line of
    /// output, which describes what it saw.
    fn run(&self, args: &[&str]) -> String {
        let mut all = vec!["run"];
        all.extend_from_slice(args);
        let output = self.ap(&all);
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        assert_eq!(output.status.code(), Some(STATUS), "{output:?}");
        assert_eq!(stdout.lines().count(), 1, "{output:?}");
        assert!(stdout.starts_with("{\"arguments\":"), "{output:?}");
        stdout
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum failed on {path:?}");
    String::from_utf8(output.stdout).unwrap()[..64].to_owned()
}

/// The items of the array named `name` in the fake harness's report.
fn array<'a>(report: &'a str, name: &str) -> Vec<&'a str> {
    let start = format!("\"{name}\":[");
    let at = report
        .find(&start)
        .unwrap_or_else(|| panic!("no {name}: {report}"));
    let rest = &report[at + start.len()..];
    let items = &rest[..rest.find(']').unwrap()];
    match items {
        "" => Vec::new(),
        items => items.split(',').collect(),
    }
}

fn fake(more: &str) -> String {
    format!(
        "AGENT claude:0.0.0\nOS minimal\nNETWORK -\nENV FAKE_HARNESS_WRITE ran\nENV FAKE_HARNESS_STATUS {STATUS}\n{more}"
    )
}

// spec(Run): `ap run <agent> [-p <prompt>]` starts an agent in a new Linux microVM.
// spec(Run): **Headless or interactive.** A run is headless when it has a non-empty prompt, either `PROMPT` or `-p`/`--prompt` on the command line.
// spec(Run): A headless run streams the harness's output and exits with the harness's status.
// spec(Run): `/home/agent`
// spec(Run): **Ephemeral.** Every run starts from the image with an empty writable overlay, and the overlay is discarded when the run ends.
// spec(Run): **User.** The agent runs as the non-root user `agent`, with `HOME=/home/agent`.
// spec(Guest environment): The agent runs as `agent`, a non-root user.
#[test]
fn a_headless_run_starts_the_harness_as_agent_and_exits_with_its_status() {
    let scratch = Scratch::new("headless");
    scratch.build(
        "fake",
        &fake(&format!(
            "ENV GREETING \"hello, $GREETED\"\nPROMPT {BUILT_PROMPT}\n"
        )),
    );

    let first = scratch.run(&["fake", "-p", PROMPT]);
    assert_eq!(
        array(&first, "arguments"),
        ["\"-p\"", &format!("\"{PROMPT}\"")],
        "{first}"
    );
    for seen in [
        "\"user\":\"agent\"",
        "\"directory\":\"/home/agent\"",
        "\"HOME\":\"/home/agent\"",
        "\"USER\":\"agent\"",
        "\"FAKE_HARNESS_WRITE\":\"ran\"",
        "\"GREETING\":\"hello, the host\"",
    ] {
        assert!(
            first.contains(seen),
            "the harness didn't see {seen}: {first}"
        );
    }
    assert_eq!(array(&first, "interfaces"), ["\"lo\""], "{first}");

    // Without `-p`, the prompt is the one the agent was built with, and the
    // file the first run wrote is gone with its overlay.
    let second = scratch.run(&["fake"]);
    assert_eq!(
        array(&second, "arguments"),
        ["\"-p\"", &format!("\"{BUILT_PROMPT}\"")],
        "{second}"
    );
    assert!(
        !array(&second, "files").contains(&"\"ran\""),
        "a file the first run wrote outlasted it: {second}"
    );
}

#[test]
fn the_harness_starts_in_the_workdir_which_must_be_a_directory() {
    let scratch = Scratch::new("workdir");
    scratch.build("tmp", &fake("WORKDIR /tmp\n"));
    let report = scratch.run(&["tmp", "--prompt", PROMPT]);
    assert!(report.contains("\"directory\":\"/tmp\""), "{report}");

    scratch.build("nowhere", &fake("WORKDIR ~/nowhere\n"));
    let output = scratch.ap(&["run", "nowhere", "-p", PROMPT]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        stderr.contains("the working directory /home/agent/nowhere isn't a directory"),
        "{stderr}"
    );
}

#[test]
fn an_agent_without_a_prompt_fails_because_interactive_runs_are_not_built() {
    let scratch = Scratch::new("interactive");
    scratch.build("fake", &fake(""));
    let output = scratch.ap(&["run", "fake"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
    assert!(
        stderr.contains("interactive runs are not built yet"),
        "{stderr}"
    );
}
