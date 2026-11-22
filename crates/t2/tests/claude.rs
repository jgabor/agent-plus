//! Real Claude Code in a guest, with no credentials: a static `ap` builds an
//! agent from Anthropic's release, and runs it in a terminal, as you would.

use std::fs::{self, File};
use std::io::{self, PipeWriter, Read};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ap::fetch;
use ap::store::Store;
use guest::linux::{open_terminal, program_has_ended, take_terminal};
use guest::protocol::Size;
use t1::{guest_binary, kernel, os};

/// The version the adapter's first-run state was read from, and the SHA-256
/// digest of its Linux x64 release. A pinned one keeps a new release from
/// changing what this check sees, and a cached download needs no network.
const VERSION: &str = "2.1.323";
const DIGEST: &str = "604d29a6869409106b6a1110bdadcc8b72e8654a5a865d23258f938a696e348e";
/// How long `ap build` may take, in seconds, downloading Claude Code
/// included.
const BUILDS_WITHIN: &str = "600";
/// How long Claude Code may take to start in a guest. A start took 38 to 132
/// seconds on a loaded machine in journey step 8.
const STARTS_WITHIN: Duration = Duration::from_secs(600);

/// A home of its own, with the kernel and the OS layer in its store, so that
/// `ap` reads none of the owner's files. Downloads go to a cache that
/// outlasts it, so that Claude Code is downloaded once per tree.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    cache: PathBuf,
    /// `XDG_RUNTIME_DIR`, where `ap` keeps the sockets of active runs.
    runtime: PathBuf,
    ap: PathBuf,
}

impl Scratch {
    fn new(test: &str) -> Scratch {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let root = tmp.join(format!("t2-{test}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let store = Store::new(home.join(".local/share/ap"));
        store
            .put(
                fetch::kernel().sha256,
                File::open(kernel::vmlinux()).unwrap(),
            )
            .unwrap();
        os::put(&home.join(".local/share/ap"));
        let cache = tmp.join("t2-cache");
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        Scratch {
            root,
            home,
            cache,
            runtime,
            ap: guest_binary::ap(),
        }
    }

    /// `ap run` of the agent `name`, with only `HOME`, `XDG_CACHE_HOME`, and
    /// `XDG_RUNTIME_DIR` in its environment. It has no `timeout`, because
    /// that would take it out of its terminal's foreground.
    fn run(&self, name: &str) -> Command {
        let mut command = Command::new(&self.ap);
        command
            .args(["run", name])
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_RUNTIME_DIR", &self.runtime);
        command
    }

    /// Builds the agent `name` from this Agentfile, approving what it asks,
    /// and downloading Claude Code unless the cache has it. The lock pins
    /// [`VERSION`] by its digest.
    fn build(&self, name: &str, agentfile: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude {VERSION} {DIGEST}\n"),
        )
        .unwrap();
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let built = Command::new("timeout")
            .args(["-k", "10", BUILDS_WITHIN])
            .arg(&self.ap)
            .args([
                "build",
                &format!("--approve={}", approved.display()),
                dir.to_str().unwrap(),
            ])
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(built.status.code(), Some(0), "{built:?}");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `ap run` in a terminal of its own. Dropping it kills `ap run`, which
/// stops the VM, so a failed check leaves none behind.
struct Interactive {
    run: Child,
    /// Everything the terminal has shown.
    shown: Arc<Mutex<Vec<u8>>>,
    /// Closing this stops the reader, which lets go of its side of the
    /// terminal.
    stop_reading: Option<PipeWriter>,
    reader: Option<thread::JoinHandle<()>>,
    /// Both sides, kept open until the run is killed.
    _terminal: (File, File),
}

impl Interactive {
    fn start(scratch: &Scratch, name: &str) -> Interactive {
        let size = Size {
            rows: 40,
            columns: 120,
        };
        let (controller, terminal) = open_terminal(size).unwrap();
        let mut command = scratch.run(name);
        command
            .stdin(terminal.try_clone().unwrap())
            .stdout(terminal.try_clone().unwrap())
            .stderr(terminal.try_clone().unwrap());
        // SAFETY: `take_terminal` only makes system calls.
        unsafe { command.pre_exec(take_terminal) };
        let shown = Arc::new(Mutex::new(Vec::new()));
        let mut side = controller.try_clone().unwrap();
        let screen = Arc::clone(&shown);
        let (stopped, stop_reading) = io::pipe().unwrap();
        // Nothing between here and the guard below can panic.
        let run = command.spawn().unwrap();
        drop(command);
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
            shown,
            stop_reading: Some(stop_reading),
            reader: Some(reader),
            _terminal: (controller, terminal),
        }
    }

    /// The text the terminal has shown, without escape sequences or
    /// whitespace, because Claude Code moves the cursor in place of most
    /// spaces.
    fn text(&self) -> String {
        visible(&String::from_utf8_lossy(&self.shown.lock().unwrap()))
    }
}

impl Drop for Interactive {
    fn drop(&mut self) {
        let _ = self.run.kill();
        let _ = self.run.wait();
        drop(self.stop_reading.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// `screen` without escape sequences and whitespace.
fn visible(screen: &str) -> String {
    let mut text = String::new();
    let mut characters = screen.chars();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.next() {
                // An escape sequence ends at the first byte after its
                // intermediate bytes, as in ESC `(` `B`.
                Some(' '..='/') => {
                    for next in characters.by_ref() {
                        if !(' '..='/').contains(&next) {
                            break;
                        }
                    }
                }
                // A control sequence ends at its final byte.
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                // A string ends at BEL, or at ESC `\`.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            characters.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            character if character.is_whitespace() || character.is_control() => {}
            character => text.push(character),
        }
    }
    text
}

// spec(Run): The Claude Code adapter marks `/` as trusted (`projects["/"].hasTrustDialogAccepted` in `~/.claude.json`), so an interactive run whose working directory isn't in a git repository doesn't ask whether to trust it.
#[test]
fn an_interactive_claude_code_run_reaches_its_prompt_with_no_dialog() {
    let scratch = Scratch::new("dialogs");
    scratch.build(
        "claude",
        &format!("AGENT claude:{VERSION}\nOS minimal\nNETWORK -\nMODE auto\n"),
    );
    let mut run = Interactive::start(&scratch, "claude");
    // The prompt's status line names the mode. Claude Code draws it only
    // once no dialog is in the way.
    let deadline = Instant::now() + STARTS_WITHIN;
    loop {
        let exited = run.run.try_wait().unwrap();
        let text = run.text();
        assert!(
            !text.contains("Itrustthisfolder"),
            "Claude Code asked whether to trust its working directory: {text}"
        );
        if text.contains("⏵⏵automodeon") {
            break;
        }
        assert!(exited.is_none(), "ap run exited {exited:?}: {text}");
        assert!(
            Instant::now() < deadline,
            "Claude Code never showed its prompt: {text}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn visible_text_drops_escape_sequences_and_whitespace() {
    let screen = "\x1b[?25l\x1b]0;title\x07⏵⏵ auto\x1b[1Cmode\x1b(B on\r\n\x1b]8;;x\x1b\\on";
    assert_eq!(visible(screen), "⏵⏵automodeonon");
}
