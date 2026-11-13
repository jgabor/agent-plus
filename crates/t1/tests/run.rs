//! `ap run` itself: a static `ap` builds an agent whose harness is the fake
//! one, then boots it headless and reads what the harness saw.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

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
    /// `XDG_RUNTIME_DIR`, where `ap` keeps the sockets of active runs.
    runtime: PathBuf,
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
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        Scratch {
            root,
            home,
            runtime,
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

    /// Runs the static `ap` with these arguments and only `HOME`,
    /// `XDG_RUNTIME_DIR`, and `GREETED` in its environment, under a timeout.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args(["-k", "10", TIMEOUT])
            .arg(&shared().0)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("GREETED", "the host")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The rows `ap ps` prints after its header.
    fn ps(&self) -> Vec<String> {
        let output = self.ap(&["ps"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut lines = stdout.lines().map(str::to_owned);
        let header = lines.next().unwrap_or_default();
        assert!(header.starts_with("NAME "), "{stdout}");
        lines.collect()
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

/// A process this test started, killed and reaped when it's dropped, so a
/// failed test leaves no VM behind.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The processes `pid` started, and theirs, as `/proc` lists them.
fn descendants(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut next = vec![pid];
    while let Some(pid) = next.pop() {
        for task in fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
        {
            let children = fs::read_to_string(task.unwrap().path().join("children"));
            for child in children.unwrap_or_default().split_whitespace() {
                let child = child.parse().unwrap();
                found.push(child);
                next.push(child);
            }
        }
    }
    found
}

/// Whether the process `pid` is running: whether it exists and isn't a
/// zombie that is waiting to be reaped.
fn running(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(") ")?.1.chars().next()?;
            Some(state != 'Z')
        })
        .unwrap_or(false)
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
        ["\"-p\"", "\"--\"", &format!("\"{PROMPT}\"")],
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
        ["\"-p\"", "\"--\"", &format!("\"{BUILT_PROMPT}\"")],
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
    // A prompt that looks like a flag is still the prompt.
    let report = scratch.run(&["tmp", "--prompt", "--help"]);
    assert!(report.contains("\"directory\":\"/tmp\""), "{report}");
    assert_eq!(
        array(&report, "arguments"),
        ["\"-p\"", "\"--\"", "\"--help\""],
        "{report}"
    );

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

// spec(Run): **End.** A run that `ap run` started ends when that `ap run` exits, for any reason, including a closed terminal or a killed `ap run` process, and its VM stops with it.
// spec(ap ps): `ap ps` lists the active runs.
#[test]
fn killing_ap_run_ends_its_run_and_leaves_no_process_or_socket() {
    let scratch = Scratch::new("killed");
    scratch.build("sleepy", &fake("ENV FAKE_HARNESS_SLEEP 60\n"));
    assert_eq!(scratch.ps(), Vec::<String>::new());

    let mut run = Started(
        Command::new(&shared().0)
            .args(["run", "sleepy", "-p", PROMPT])
            .env_clear()
            .env("HOME", &scratch.home)
            .env("XDG_RUNTIME_DIR", &scratch.runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // The harness reports before it sleeps, so the run is active from here
    // until the kill.
    let mut report = String::new();
    BufReader::new(run.0.stdout.take().unwrap())
        .read_line(&mut report)
        .unwrap();
    assert!(report.starts_with("{\"arguments\":"), "{report}");

    let rows = scratch.ps();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let columns: Vec<&str> = rows[0].split_whitespace().collect();
    let name = columns[0];
    assert_eq!(name.len(), 8, "{rows:?}");
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
        "{rows:?}"
    );
    assert_eq!(columns[1], "sleepy", "{rows:?}");
    let socket = scratch.runtime.join("ap").join(format!("{name}.sock"));
    assert!(socket.exists(), "{socket:?}");
    // The run process, which holds the VM, and the cleanup process.
    let processes = descendants(run.0.id());
    assert_eq!(processes.len(), 2, "{processes:?}");

    run.0.kill().unwrap();
    run.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while processes.iter().any(|pid| running(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let left: Vec<u32> = processes.into_iter().filter(|pid| running(*pid)).collect();
    assert_eq!(
        left,
        Vec::<u32>::new(),
        "processes outlived the killed ap run"
    );
    assert_eq!(scratch.ps(), Vec::<String>::new());
    assert!(!socket.exists(), "{socket:?} outlived the run");
}

/// Sends `signal` to the process `pid`, which this test started.
fn signal(signal: &str, pid: u32) {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {signal} {pid}");
}

#[test]
fn a_run_whose_run_process_ended_is_not_listed_while_ap_run_is_stopped() {
    let scratch = Scratch::new("stopped");
    scratch.build("brief", &fake("ENV FAKE_HARNESS_SLEEP 1\n"));
    let mut run = Started(
        Command::new(&shared().0)
            .args(["run", "brief", "-p", PROMPT])
            .env_clear()
            .env("HOME", &scratch.home)
            .env("XDG_RUNTIME_DIR", &scratch.runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(run.0.stdout.take().unwrap());
    let mut report = String::new();
    stdout.read_line(&mut report).unwrap();
    assert_eq!(scratch.ps().len(), 1);

    // As Ctrl-Z would, while the harness finishes.
    signal("-STOP", run.0.id());
    let processes = descendants(run.0.id());
    assert_eq!(processes.len(), 2, "{processes:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while processes.iter().all(|pid| running(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        processes.iter().any(|pid| !running(*pid)),
        "the run process didn't end"
    );
    assert_eq!(scratch.ps(), Vec::<String>::new());

    signal("-CONT", run.0.id());
    assert_eq!(run.0.wait().unwrap().code(), Some(STATUS));
}
