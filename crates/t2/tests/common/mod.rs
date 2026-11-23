//! What the T2 checks share: a home of their own for a static `ap`, and an
//! `ap run` in a terminal of its own.

use std::fs::{self, File};
use std::io::{self, PipeWriter, Read};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use ap::fetch;
use ap::store::Store;
use guest::linux::{open_terminal, program_has_ended, take_terminal};
use guest::protocol::Size;
use t1::{guest_binary, kernel, os};

/// How long `ap build` may take, in seconds, downloading the harness
/// included, and how long any other `ap` command may take.
const WITHIN: &str = "600";

/// A home of its own, with the kernel and the OS layer in its store, so that
/// `ap` reads none of the owner's files. Downloads go to a cache that
/// outlasts it, so that a harness is downloaded once per tree.
pub struct Scratch {
    pub root: PathBuf,
    home: PathBuf,
    cache: PathBuf,
    /// `XDG_RUNTIME_DIR`, where `ap` keeps the sockets of active runs.
    runtime: PathBuf,
    ap: PathBuf,
}

impl Scratch {
    pub fn new(test: &str) -> Scratch {
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

    /// The static `ap` as `program` starts it, with only `HOME`,
    /// `XDG_CACHE_HOME`, and `XDG_RUNTIME_DIR` in its environment.
    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_RUNTIME_DIR", &self.runtime);
        command
    }

    /// `ap run` of the agent `name`. It has no `timeout`, because that would
    /// take it out of its terminal's foreground.
    pub fn run(&self, name: &str) -> Command {
        let mut command = self.command(&self.ap);
        command.args(["run", name]);
        command
    }

    /// `ap` with `args`, under a timeout, with no input.
    pub fn ap(&self, args: &[&str]) -> Output {
        (self.command(Path::new("timeout")))
            .args(["-k", "10", WITHIN])
            .arg(&self.ap)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Builds the agent `name` from this Agentfile and lockfile, approving
    /// what it asks, and downloading the harness unless the cache has it.
    pub fn build(&self, name: &str, agentfile: &str, lock: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        fs::write(dir.join("Agentfile.lock"), lock).unwrap();
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let approve = format!("--approve={}", approved.display());
        let built = self.ap(&["build", &approve, dir.to_str().unwrap()]);
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
pub struct Interactive {
    pub run: Child,
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
    /// Starts `command`, an `ap run`, in a terminal.
    pub fn start(mut command: Command) -> Interactive {
        let size = Size {
            rows: 40,
            columns: 120,
        };
        let (controller, terminal) = open_terminal(size).unwrap();
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

    /// Everything the terminal has shown, escape sequences included.
    pub fn shown(&self) -> String {
        String::from_utf8_lossy(&self.shown.lock().unwrap()).into_owned()
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
