//! `ap run` itself: a static `ap` builds an agent whose harness is the fake
//! one, then boots it, headless or in a terminal, and reads what the harness
//! saw.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ap::fetch;
use ap::store::Store;
use guest::linux::{open_terminal, program_has_ended, resize, take_terminal};
use guest::protocol::Size;
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
    /// what this Agentfile asks. The lock gives the fake harness the version
    /// that `AGENT claude:<version>` names.
    fn build(&self, name: &str, agentfile: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        let version = agentfile
            .lines()
            .find_map(|line| line.strip_prefix("AGENT claude:"))
            .expect("no `AGENT claude:<version>`");
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude {version} {}\n", self.harness),
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

    // The fake harness is Claude Code 0.0.0, older than `auto`, so the
    // default mode starts it in Claude Code's manual mode.
    let first = scratch.run(&["fake", "-p", PROMPT]);
    assert_eq!(
        array(&first, "arguments"),
        [
            "\"-p\"",
            "\"--permission-mode\"",
            "\"default\"",
            "\"--\"",
            &format!("\"{PROMPT}\"")
        ],
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
        [
            "\"-p\"",
            "\"--permission-mode\"",
            "\"default\"",
            "\"--\"",
            &format!("\"{BUILT_PROMPT}\"")
        ],
        "{second}"
    );
    assert!(
        !array(&second, "files").contains(&"\"ran\""),
        "a file the first run wrote outlasted it: {second}"
    );
}

// spec(MODE): The adapter sets the mode on the harness's command line, with flags or Codex's `-c` overrides, every time it starts the harness, including on `--resume`, so a project's settings or an earlier session can't pick a different one.
// spec(MODEL): `<model>` is passed to the harness unchanged, so it accepts whatever names or aliases the harness accepts.
// spec(AGENT): which first-run state it prepares, so a new VM doesn't repeat the harness's setup
#[test]
fn claude_code_starts_with_its_adapters_configuration() {
    let scratch = Scratch::new("adapter");
    scratch.build(
        "configured",
        &format!(
            "AGENT claude:2.1.323\nOS minimal\nNETWORK -\nENV FAKE_HARNESS_STATUS {STATUS}\nSYSTEM Be careful.\nMODEL claude-opus-5-5\nEFFORT xhigh\nMODE full\n"
        ),
    );
    let report = scratch.run(&["configured", "-p", PROMPT]);
    assert_eq!(
        array(&report, "arguments"),
        [
            "\"-p\"",
            "\"--model\"",
            "\"claude-opus-5-5\"",
            "\"--effort\"",
            "\"xhigh\"",
            "\"--permission-mode\"",
            "\"bypassPermissions\"",
            "\"--\"",
            &format!("\"{PROMPT}\""),
        ],
        "{report}"
    );
    let files = array(&report, "files");
    for file in [".claude.json", ".claude/CLAUDE.md", ".claude/settings.json"] {
        assert!(
            files.contains(&format!("\"{file}\"").as_str()),
            "the harness can't see {file}: {report}"
        );
    }

    // A version older than `--effort` gets the effort from its variable,
    // which `ENV` can't change.
    scratch.build(
        "older",
        &format!(
            "AGENT claude:2.1.37\nOS minimal\nNETWORK -\nENV FAKE_HARNESS_STATUS {STATUS}\nENV CLAUDE_CODE_EFFORT_LEVEL low\nEFFORT max\n"
        ),
    );
    let report = scratch.run(&["older", "-p", PROMPT]);
    assert_eq!(
        array(&report, "arguments"),
        [
            "\"-p\"",
            "\"--permission-mode\"",
            "\"default\"",
            "\"--\"",
            &format!("\"{PROMPT}\""),
        ],
        "{report}"
    );
    assert!(
        report.contains("\"CLAUDE_CODE_EFFORT_LEVEL\":\"max\""),
        "{report}"
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
        [
            "\"-p\"",
            "\"--permission-mode\"",
            "\"default\"",
            "\"--\"",
            "\"--help\""
        ],
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

/// How long an interactive run may take to show what a check waits for.
const SHOWN_WITHIN: Duration = Duration::from_secs(60);

/// `ap run` in a terminal of its own, which this test uses as you would.
struct Interactive {
    run: Started,
    controller: File,
    /// Everything the terminal has shown.
    shown: Arc<Mutex<Vec<u8>>>,
    /// Closing this stops the reader, which lets go of its side of the
    /// terminal.
    stop_reading: PipeWriter,
    reader: thread::JoinHandle<()>,
    /// The other side, kept open so the terminal outlasts `ap run`.
    _terminal: File,
}

impl Interactive {
    fn start(scratch: &Scratch, args: &[&str], size: Size) -> Interactive {
        let (controller, terminal) = open_terminal(size).unwrap();
        let mut command = Command::new(&shared().0);
        command
            .arg("run")
            .args(args)
            .env_clear()
            .env("HOME", &scratch.home)
            .env("XDG_RUNTIME_DIR", &scratch.runtime)
            .stdin(terminal.try_clone().unwrap())
            .stdout(terminal.try_clone().unwrap())
            .stderr(terminal.try_clone().unwrap());
        // SAFETY: `take_terminal` only makes system calls.
        unsafe { command.pre_exec(take_terminal) };
        let run = Started(command.spawn().unwrap());
        // The terminal must end with `ap run`'s last use of it.
        drop(command);
        let shown = Arc::new(Mutex::new(Vec::new()));
        let mut side = controller.try_clone().unwrap();
        let screen = Arc::clone(&shown);
        let (stopped, stop_reading) = io::pipe().unwrap();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(false) = program_has_ended(side.as_fd(), stopped.as_fd()) {
                match side.read(&mut buffer) {
                    Ok(count @ 1..) => screen.lock().unwrap().extend_from_slice(&buffer[..count]),
                    _ => return,
                }
            }
        });
        Interactive {
            run,
            controller,
            shown,
            stop_reading,
            reader,
            _terminal: terminal,
        }
    }

    /// Closes the terminal, as closing its window would.
    fn close(self) -> Started {
        drop(self.stop_reading);
        self.reader.join().unwrap();
        drop(self.controller);
        self.run
    }

    /// Waits for `ap run` to exit, for as long as it may take to show
    /// something.
    fn exit_status(&mut self) -> ExitStatus {
        wait_within(&mut self.run, SHOWN_WITHIN)
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.shown.lock().unwrap()).into_owned()
    }

    /// Waits until the terminal has shown `text`, and returns all it showed.
    fn wait_for(&self, text: &str) -> String {
        let deadline = Instant::now() + SHOWN_WITHIN;
        loop {
            let screen = self.screen();
            if screen.contains(text) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "the terminal never showed {text:?}: {screen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn type_line(&mut self, line: &str) {
        self.controller
            .write_all(format!("{line}\r").as_bytes())
            .unwrap();
    }
}

/// The status of `run`, once it exits within `limit`.
fn wait_within(run: &mut Started, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = run.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "ap run didn't exit");
        thread::sleep(Duration::from_millis(10));
    }
}

fn size(rows: u16, columns: u16) -> Size {
    Size { rows, columns }
}

// spec(Run): A run without a prompt opens the harness's interactive interface in your terminal.
// spec(Run): **Terminal output.** Bytes from the guest are filtered before they reach your terminal.
// spec(Run): Escape sequences that write or read your clipboard (OSC 52), or that make your terminal reply with text the guest chose, such as a window title report, are removed.
#[test]
fn a_run_without_a_prompt_opens_the_harness_in_a_terminal_that_follows_yours() {
    let scratch = Scratch::new("interactive");
    scratch.build("open", &fake(""));
    let mut run = Interactive::start(&scratch, &["open"], size(30, 100));

    let screen = run.wait_for("\"user\":");
    assert!(screen.contains("\"terminal\":\"30x100\""), "{screen:?}");
    assert!(screen.contains("\"user\":\"agent\""), "{screen:?}");
    // Interactive, with nothing to submit.
    assert_eq!(
        array(&screen, "arguments"),
        ["\"--permission-mode\"", "\"default\"", "\"--\""],
        "{screen:?}"
    );

    // A resize of your terminal reaches the harness's. It travels apart
    // from the keys, so the question is asked until the answer changes.
    resize(run.controller.as_fd(), size(40, 120)).unwrap();
    let deadline = Instant::now() + SHOWN_WITHIN;
    while !run.screen().contains("size 40x120") {
        assert!(Instant::now() < deadline, "{:?}", run.screen());
        run.type_line("size");
        thread::sleep(Duration::from_millis(100));
    }

    // The harness repeats this line unescaped, and the clipboard write in
    // it never reaches the terminal.
    run.type_line("\x1b]52;c;aGk=\x07clipped");
    let screen = run.wait_for("typed clipped");
    assert!(!screen.contains("\x1b]52"), "{screen:?}");

    run.type_line("exit");
    assert_eq!(run.exit_status().code(), Some(STATUS));
}

#[test]
fn a_signal_that_ends_an_interactive_run_puts_the_terminal_back() {
    let scratch = Scratch::new("terminated");
    scratch.build("open", &fake(""));
    let mut run = Interactive::start(&scratch, &["open"], size(24, 80));
    run.wait_for("\"user\":");
    signal("-TERM", run.run.0.id());
    let status = run.exit_status();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    // Raw mode echoes nothing. Put back, the terminal echoes what is typed.
    run.type_line("echoed");
    run.wait_for("echoed");
}

// spec(Run): `ap run -i` forces interactive and submits the prompt as the first message.
#[test]
fn ap_run_i_submits_the_prompt_as_the_first_message() {
    let scratch = Scratch::new("forced");
    scratch.build("prompted", &fake(&format!("PROMPT {BUILT_PROMPT}\n")));
    for (args, prompt) in [
        (&["prompted", "-i"][..], BUILT_PROMPT),
        (&["-i", "prompted", "-p", PROMPT], PROMPT),
    ] {
        let mut run = Interactive::start(&scratch, args, size(24, 80));
        let screen = run.wait_for("\"user\":");
        assert!(screen.contains("\"terminal\":\"24x80\""), "{screen:?}");
        assert_eq!(
            array(&screen, "arguments"),
            [
                "\"--permission-mode\"",
                "\"default\"",
                "\"--\"",
                &format!("\"{prompt}\"")
            ],
            "{args:?}: {screen:?}"
        );
        run.type_line("exit");
        assert_eq!(run.exit_status().code(), Some(STATUS));
    }
}

// spec(Run): **End.** A run that `ap run` started ends when that `ap run` exits, for any reason, including a closed terminal or a killed `ap run` process, and its VM stops with it.
#[test]
fn closing_the_terminal_of_an_interactive_run_ends_it_and_leaves_nothing() {
    let scratch = Scratch::new("closed");
    scratch.build("open", &fake(""));
    let run = Interactive::start(&scratch, &["open"], size(24, 80));
    run.wait_for("\"user\":");
    assert_eq!(scratch.ps().len(), 1);
    let processes = descendants(run.run.0.id());
    assert_eq!(processes.len(), 2, "{processes:?}");

    let mut run = run.close();
    let status = wait_within(&mut run, Duration::from_secs(10));
    assert!(!status.success(), "{status:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while processes.iter().any(|pid| running(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let left: Vec<u32> = processes.into_iter().filter(|pid| running(*pid)).collect();
    assert_eq!(
        left,
        Vec::<u32>::new(),
        "processes outlived the closed terminal"
    );
    assert_eq!(scratch.ps(), Vec::<String>::new());
    let sockets = fs::read_dir(scratch.runtime.join("ap")).unwrap().count();
    assert_eq!(sockets, 0, "a socket outlived the run");
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
