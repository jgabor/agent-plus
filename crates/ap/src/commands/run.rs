//! `ap run <agent> [-i] [-p <prompt>]`, from SPEC.md's Run section. It names
//! the run, and starts a run process that boots the agent's layers in a new
//! microVM and starts the harness there as the user `agent`, as
//! [`ap::runs`] says. It streams the harness's output and exits with its
//! status, or with 125 if the harness never started.
//!
//! A run with a prompt is headless, and the harness gets pipes. Any other
//! run, or one with `-i`, is interactive: the harness gets a terminal in the
//! guest, and your terminal is put in raw mode for as long as the run lasts,
//! so every key reaches the harness. Its resizes follow.
//!
//! Only an agent with `NETWORK -` runs. Everything else on #297's list of
//! what isn't built yet fails with 125 and says so, before any VM starts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{DirBuilder, File};
use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfile::{Files, Harness, Instruction, Network, Os, Part, Secrets, Settings, Sign};
use ap::boot::Error;
use ap::filter::Filter;
use ap::runs::{self, End, FromRun, LayerName};
use guest::agent::HOME;
use guest::protocol::{Size, Start};

/// The `PATH` the harness gets, unless `ENV` sets one.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The size an interactive harness's terminal starts at when yours doesn't
/// report one.
const DEFAULT_SIZE: Size = Size {
    rows: 24,
    columns: 80,
};

/// How much of what you type goes to the run at a time.
const KEYS: usize = 4096;

/// The flags SPEC.md gives `ap run` that aren't built yet.
const FLAGS_NOT_BUILT: [&str; 6] = [
    "-t",
    "--share",
    "--network",
    "--resume",
    "--gui",
    "--approve",
];

/// The instructions a run can't honor yet. `ap build` refuses the ones that
/// need a fetch, so an agent that has them can't be built either.
const NOT_BUILT: [&str; 11] = [
    "SHARE",
    "SECRET",
    "AUTH",
    "PERSIST",
    "PORT",
    "PERMISSIONS",
    "REPO",
    "PLUGIN",
    "SKILL",
    "MCP",
    "SEAL",
];

/// The arguments of `ap run`.
pub struct Args {
    agent: OsString,
    /// The prompt from `-p` or `--prompt`.
    prompt: Option<Vec<u8>>,
    /// Whether `-i` forces an interactive run.
    interactive: bool,
    /// The first flag that isn't built yet.
    not_built: Option<&'static str>,
}

/// Reads `ap run`'s arguments: the agent, `-i` at most once, and
/// `-p <prompt>` or `--prompt <prompt>` at most once, in any order. A flag
/// that isn't built yet stops the reading, because `ap run` fails on it
/// anyway. Any other flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut agent = None;
    let mut prompt = None;
    let mut interactive = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let flag = arg.as_bytes();
        let name = flag.split(|byte| *byte == b'=').next().unwrap_or_default();
        if let Some(not_built) = FLAGS_NOT_BUILT
            .iter()
            .find(|known| known.as_bytes() == name)
        {
            return Some(Args {
                agent: agent.unwrap_or_default(),
                prompt,
                interactive,
                not_built: Some(not_built),
            });
        }
        if (arg == "-p" || arg == "--prompt") && prompt.is_none() {
            prompt = Some(args.next()?.as_bytes().to_vec());
        } else if arg == "-i" && !interactive {
            interactive = true;
        } else if flag.starts_with(b"-") || agent.is_some() {
            return None;
        } else {
            agent = Some(arg.clone());
        }
    }
    Some(Args {
        agent: agent?,
        prompt,
        interactive,
        not_built: None,
    })
}

/// Runs the agent, and returns the harness's status, or `ap`'s own.
pub fn run(args: &Args) -> u8 {
    match start(args).and_then(|(root, layers, start)| run_agent(&root, args, &layers, start)) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("ap: {error}");
            error.exit_code()
        }
    }
}

/// Names the run, starts its processes, and relays its output, filtered,
/// until it ends.
fn run_agent(root: &Path, args: &Args, layers: &[LayerName], start: Start) -> Result<u8, Error> {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let run = ::store::Runs::new(root)
        .create(&args.agent.to_string_lossy(), started)
        .map_err(fail("can't record the run"))?;

    let dir = runs::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), root);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(fail(&format!("can't create {}", dir.display())))?;
    let opened = File::open(&dir).map_err(fail(&format!("can't open {}", dir.display())))?;
    let socket_name = runs::socket_name(&run.name);
    let socket = runs::short_path(&opened, &socket_name);
    // The cleanup process starts before the socket exists, so that it
    // removes the socket however this process ends. The name is the run's
    // alone, so there's no other socket it could remove.
    let (lifeline, held) = io::pipe().map_err(fail("can't start the run"))?;
    let exe = Path::new("/proc/self/exe");
    Command::new(exe)
        .arg0(runs::CLEANUP)
        .arg(dir.join(&socket_name))
        .stdin(lifeline)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(fail("can't start the run's cleanup"))?;
    let listener = UnixListener::bind(&socket).map_err(fail("can't create the run's socket"))?;
    let (mut stream, theirs) = UnixStream::pair().map_err(fail("can't start the run"))?;
    let mut process = Command::new(exe)
        .arg0(runs::RUN_PROCESS)
        .arg(std::process::id().to_string())
        .arg(&run.name)
        .stdin(OwnedFd::from(theirs))
        .stdout(held.try_clone().map_err(fail("can't start the run"))?)
        .process_group(0)
        .spawn()
        .map_err(fail("can't start the run process"))?;

    let interactive = start.terminal.is_some();
    runs::send_request(&mut stream, layers, start).map_err(fail("lost the run"))?;
    let writer = stream.try_clone().map_err(fail("can't start the run"))?;
    let writer = Arc::new(Mutex::new(writer));
    let execs = runs::Execs::serve(listener, Arc::clone(&writer));
    let raw = match interactive {
        true => {
            // Raw mode comes first, so a signal taken from here on finds the
            // modes to put back, and nothing changes them after.
            let raw = RawMode::start().map_err(fail("can't set up your terminal"))?;
            pass_keys(Arc::clone(&writer), false);
            take_signals(writer);
            Some(raw)
        }
        false => None,
    };
    let end = relay(&mut stream, &execs).map_err(fail("lost the run"));
    drop(raw);
    // The run process exits once it has reported, so this doesn't wait long.
    let exited = process.wait();
    drop(held);
    // A VM that `ap stop` powered off ends the run with no report.
    let killed = exited.is_ok_and(|status| status.code() == Some(runs::KILLED.into()));
    if end.is_err() && killed {
        return Ok(runs::KILLED);
    }
    match end? {
        End::Exited(status) => Ok(status),
        End::Failed(status, _) if status == Error::TimedOut.exit_code() => Err(Error::TimedOut),
        End::Failed(_, reason) => Err(Error::Failed(reason)),
    }
}

/// An error from `ap run`'s own work, in this context.
fn fail(context: &str) -> impl Fn(io::Error) -> Error + '_ {
    move |error| Error::Failed(format!("{context}: {error}"))
}

/// Your terminal's modes before `ap` changed them, for the thread that puts
/// them back if a signal ends `ap`.
static SAVED_MODES: OnceLock<libc::termios> = OnceLock::new();

/// The signals that end `ap run`, which an interactive run takes itself so
/// that it can put your terminal back first. `SIGKILL` can't be taken.
const ENDING: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// Your terminal in raw mode, until this drops: every key goes to the run as
/// it is typed, and nothing turns a key into a signal.
pub(super) struct RawMode(libc::termios);

impl RawMode {
    pub(super) fn start() -> io::Result<RawMode> {
        let mut saved = MaybeUninit::uninit();
        // SAFETY: `tcgetattr` fills the whole struct when it returns 0.
        let saved = unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, saved.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            saved.assume_init()
        };
        let _ = SAVED_MODES.set(saved);
        let mut raw = saved;
        // SAFETY: both calls take a valid struct, which outlives them.
        unsafe {
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(RawMode(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore(&self.0);
    }
}

/// Puts your terminal's modes back. A terminal that went away has nothing to
/// restore.
fn restore(modes: &libc::termios) {
    // SAFETY: the struct is one that `tcgetattr` filled.
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, modes) };
}

/// Passes what you type to the run, until your input ends. The run goes on,
/// and ends when the harness does. With `end`, the end of your input is
/// passed on too.
pub(super) fn pass_keys(writer: Arc<Mutex<UnixStream>>, end: bool) {
    thread::spawn(move || {
        let mut keys = [0; KEYS];
        let mut stdin = io::stdin();
        loop {
            let count = match stdin.read(&mut keys) {
                Ok(0) => {
                    if end {
                        let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                        let _ = runs::send_end_of_input(&mut *writer);
                    }
                    return;
                }
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            if runs::send_input(&mut *writer, &keys[..count]).is_err() {
                return;
            }
        }
    });
}

/// Takes the signals of [`taken_signals`], which are blocked already, so
/// this thread is the one that gets them. A signal that ends `ap run` puts
/// your terminal back first, then ends it as it would have. A resize goes to
/// a thread of its own, because sending it can wait behind your keys, and
/// this thread must stay free to take the next signal.
pub(super) fn take_signals(writer: Arc<Mutex<UnixStream>>) {
    let (resizes, resized) = mpsc::channel();
    thread::spawn(move || {
        while resized.recv().is_ok() {
            // Only the latest size matters.
            while resized.try_recv().is_ok() {}
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            if runs::send_resize(&mut *writer, terminal_size()).is_err() {
                return;
            }
        }
    });
    thread::spawn(move || {
        let set = taken_signals();
        loop {
            let mut signal = 0;
            // SAFETY: both pointers are valid for the whole call.
            if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
                return;
            }
            if signal != libc::SIGWINCH {
                end_by(signal);
            }
            let _ = resizes.send(());
        }
    });
}

/// Puts your terminal back, and ends `ap run` with `signal`'s own action.
fn end_by(signal: libc::c_int) {
    if let Some(modes) = SAVED_MODES.get() {
        restore(modes);
    }
    let mut only = MaybeUninit::uninit();
    // SAFETY: the set is initialized before it is used, and each call takes
    // only valid pointers. With its default action and unblocked on this
    // thread, the signal ends the process.
    unsafe {
        libc::sigemptyset(only.as_mut_ptr());
        libc::sigaddset(only.as_mut_ptr(), signal);
        libc::signal(signal, libc::SIG_DFL);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, only.as_ptr(), std::ptr::null_mut());
        libc::raise(signal);
    }
}

/// `SIGWINCH`, and the signals that end `ap run`.
pub(super) fn taken_signals() -> libc::sigset_t {
    let mut set = MaybeUninit::uninit();
    // SAFETY: `sigemptyset` initializes the set, and `sigaddset` takes it
    // initialized.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for signal in ENDING.into_iter().chain([libc::SIGWINCH]) {
            libc::sigaddset(set.as_mut_ptr(), signal);
        }
        set.assume_init()
    }
}

/// The size of your terminal, or [`DEFAULT_SIZE`] if it reports none.
pub(super) fn terminal_size() -> Size {
    let mut window = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: `TIOCGWINSZ` writes one `struct winsize` through this pointer,
    // which is valid for the whole call.
    let read = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, window.as_mut_ptr()) };
    // SAFETY: the struct started zeroed, which is a valid `winsize`.
    let window = unsafe { window.assume_init() };
    if read != 0 || window.ws_row == 0 || window.ws_col == 0 {
        return DEFAULT_SIZE;
    }
    Size {
        rows: window.ws_row,
        columns: window.ws_col,
    }
}

/// Writes the run's output to this process's, filtered, and hands each
/// `ap exec` what is for it, until the run ends. Write errors are ignored: a
/// reader that went away shouldn't change what the harness does or how it
/// ends.
fn relay(stream: &mut UnixStream, execs: &runs::Execs) -> io::Result<End> {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    loop {
        match runs::read_from_run(stream)? {
            Some(FromRun::Stdout(bytes)) => {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(&stdout_filter.push(&bytes));
                let _ = stdout.flush();
            }
            Some(FromRun::Stderr(bytes)) => {
                let _ = io::stderr().write_all(&stderr_filter.push(&bytes));
            }
            Some(FromRun::ForExec(id, tag, body)) => execs.deliver(id, tag, body),
            Some(FromRun::End(end)) => return Ok(end),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the run process ended before the run did",
                ));
            }
        }
    }
}

/// Everything the run needs before its VM starts: the store's root, the
/// agent's layers, and how to start its harness.
fn start(args: &Args) -> Result<(PathBuf, Vec<LayerName>, Start), Error> {
    let fail = |message: String| Error::Failed(message);
    if let Some(flag) = args.not_built {
        return Err(fail(format!("`ap run {flag}` is not built yet")));
    }
    let agent = args.agent.as_bytes();
    if [&b"./"[..], b"../", b"/", b"~/"]
        .iter()
        .any(|prefix| agent.starts_with(prefix))
    {
        return Err(fail(
            "running an agent from its directory is not built yet. Build it with `ap build`, then run it by name".to_owned(),
        ));
    }
    let name = args.agent.to_string_lossy();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref().map(Path::as_os_str),
    )
    .ok_or_else(|| {
        fail("HOME isn't an absolute path, so there's no place for the store".to_owned())
    })?;
    let missing = || {
        fail(format!(
            "no agent is named {name}. Build one with `ap build`"
        ))
    };
    if !::store::valid_name(&name) {
        return Err(missing());
    }
    let record = ::store::Agents::new(&root)
        .get(&name)
        .map_err(|error| fail(error.to_string()))?
        .ok_or_else(missing)?;

    // A record holds only what a build accepted, so its lines read cleanly.
    let instructions: Vec<Instruction> = (1..)
        .zip(&record.declared)
        .map(|(line, declared)| Instruction {
            line,
            name: declared.name.clone(),
            arguments: declared.arguments.clone(),
            text: None,
        })
        .collect();
    let unreadable = |errors: Vec<agentfile::Error>| {
        let messages: Vec<String> = errors.into_iter().map(|error| error.message).collect();
        fail(format!("agent {name} can't run: {}", messages.join("; ")))
    };
    if let Some(instruction) = instructions
        .iter()
        .find(|instruction| NOT_BUILT.contains(&instruction.name.as_str()))
    {
        return Err(fail(format!("`{}` is not built yet", instruction.name)));
    }
    let settings = Settings::read(&instructions).map_err(unreadable)?;
    let tier = match settings.os {
        Os::Minimal => None,
        Os::Standard => Some("standard"),
        Os::Desktop => Some("desktop"),
    };
    if let Some(tier) = tier {
        return Err(fail(format!("`OS {tier}` is not built yet")));
    }
    let network = Network::read(&instructions).map_err(unreadable)?;
    if network.default != Sign::Deny || !network.rules.is_empty() {
        return Err(fail(
            "network rules are not built yet, so the guest has no network, and `ap run` starts only an agent with `NETWORK -`".to_owned(),
        ));
    }
    // A build records that the agent has a `PROMPT`, though not the text,
    // which only the guest reads.
    let built_prompt = record
        .declared
        .iter()
        .any(|declared| declared.name == "PROMPT");
    let interactive = args.interactive
        || args
            .prompt
            .as_ref()
            .map_or(!built_prompt, |prompt| prompt.is_empty());
    let terminal = match interactive {
        true if !io::stdin().is_terminal() => {
            return Err(fail(
                "an interactive run needs a terminal on standard input. Give a prompt with `-p` to run headless"
                    .to_owned(),
            ));
        }
        true => {
            // Blocked before the size is read, a resize from here on waits
            // for the thread that takes it, as does a signal that ends
            // `ap run`.
            // SAFETY: the set is valid, and a null old set is allowed.
            unsafe {
                libc::pthread_sigmask(libc::SIG_BLOCK, &taken_signals(), std::ptr::null_mut())
            };
            Some(terminal_size())
        }
        false => None,
    };
    let secrets = Secrets::read(&instructions).map_err(unreadable)?;
    let files = Files::read(&instructions).map_err(unreadable)?;

    let mut env = BTreeMap::new();
    env.insert(b"PATH".to_vec(), PATH.as_bytes().to_vec());
    for variable in &secrets.env {
        let value = expand(&variable.value).map_err(|unset| {
            fail(format!(
                "`ENV {}` reads ${unset}, which isn't set",
                variable.key
            ))
        })?;
        env.insert(variable.key.as_bytes().to_vec(), value);
    }
    // SPEC.md fixes these, so `ENV` can't change them.
    for (key, value) in [("HOME", HOME), ("USER", "agent"), ("LOGNAME", "agent")] {
        env.insert(key.as_bytes().to_vec(), value.as_bytes().to_vec());
    }
    let directory = files
        .workdir
        .map_or_else(|| HOME.to_owned(), |workdir| guest_path(&workdir.path));
    let harness = super::build::harness_path(settings.agent.harness);
    // `--` keeps a prompt that starts with `-` from reading as a flag.
    let argv = match settings.agent.harness {
        Harness::Claude => {
            // The harness layer is named for the version the build locked.
            let version = record
                .layers
                .iter()
                .find_map(|layer| ap::lock::labeled_version(&layer.instruction, Harness::Claude))
                .ok_or_else(|| fail(format!("agent {name} has no Claude Code layer")))?;
            // The adapter's variables come after `ENV`'s, so they win.
            for (key, value) in ap::adapter::claude::environment(version, settings.effort) {
                env.insert(key.into_bytes(), value.into_bytes());
            }
            let mut argv = vec![harness];
            if !interactive {
                argv.push("-p".to_owned());
            }
            argv.extend(ap::adapter::claude::arguments(
                version,
                settings.model.as_deref(),
                settings.effort,
                settings.mode,
            ));
            argv.push("--".to_owned());
            argv
        }
        Harness::Codex => {
            // The harness layer is named for the version the build locked.
            let version = record
                .layers
                .iter()
                .find_map(|layer| ap::lock::labeled_version(&layer.instruction, Harness::Codex))
                .ok_or_else(|| fail(format!("agent {name} has no Codex layer")))?;
            let mut argv = vec![harness];
            argv.extend(ap::adapter::codex::arguments(
                version,
                settings.model.as_deref(),
                settings.effort,
                settings.mode,
                interactive,
            ));
            argv.push("--".to_owned());
            argv
        }
    };

    let layers = record
        .layers
        .iter()
        .map(|layer| LayerName {
            digest: layer.digest.clone(),
            instruction: layer.instruction.clone(),
        })
        .collect();
    let start = Start {
        argv: argv.into_iter().map(String::into_bytes).collect(),
        env: env.into_iter().collect(),
        directory: directory.into_bytes(),
        prompt: args.prompt.clone(),
        terminal,
    };
    Ok((root, layers, start))
}

/// An `ENV` value with its variables read from this process's environment,
/// or the name of the first variable that is unset and has no default.
fn expand(value: &[Part]) -> Result<Vec<u8>, String> {
    let mut expanded = Vec::new();
    for part in value {
        match part {
            Part::Literal(text) => expanded.extend_from_slice(text.as_bytes()),
            Part::Variable { name, default } => {
                let found = std::env::var_os(name).map(OsString::into_vec);
                match (found, default) {
                    (Some(found), Some(default)) if found.is_empty() => {
                        expanded.extend_from_slice(default.as_bytes());
                    }
                    (Some(found), _) => expanded.extend_from_slice(&found),
                    (None, Some(default)) => expanded.extend_from_slice(default.as_bytes()),
                    (None, None) => return Err(name.clone()),
                }
            }
        }
    }
    Ok(expanded)
}

/// A guest path as an absolute one. `~/` and relative paths start in the
/// agent's home.
fn guest_path(path: &str) -> String {
    let path = match path.strip_prefix("~/") {
        Some(rest) => rest.trim_start_matches('/'),
        None if path == "~" => "",
        None => path,
    };
    if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        HOME.to_owned()
    } else {
        format!("{HOME}/{path}")
    }
}
