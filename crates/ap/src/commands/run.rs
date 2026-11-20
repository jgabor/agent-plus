//! `ap run <agent> [-i] [-p <prompt>]`, from SPEC.md's Run section. It names
//! the run, and starts a run process that boots the agent's layers in a new
//! microVM and starts the harness there as the user `agent`, as
//! [`ap::runs`] says. It streams the harness's output and exits with its
//! status, or with 125 if the harness never started. What it shows of the
//! output, it also writes to the run's log.
//!
//! A run with a prompt is headless, and the harness gets pipes. Any other
//! run, or one with `-i`, is interactive: the harness gets a terminal in the
//! guest, and your terminal is put in raw mode for as long as the run lasts,
//! so every key reaches the harness. Its resizes follow.
//!
//! An agent's shares are checked again, then opened, and a share server
//! serves those handles to the run. A file share is served from the
//! directory that holds it, by its name. A copy-on-write share's changes go
//! to a directory of its own in the run's pending directory, which the run
//! makes in the store and leaves there when it ends. The guest mounts each
//! share at the place SPEC.md's Paths section gives it.
//!
//! Only an agent with `NETWORK -` runs. Everything else on #297's list of
//! what isn't built yet fails with 125 and says so, before any VM starts.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, OsString};
use std::fs::{DirBuilder, File};
use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfile::{
    Files, GuestPath, Harness, Instruction, Network, Os, Part, Secrets, Settings, Share, ShareMode,
    Sign,
};
use ap::boot::Error;
use ap::filter::Filter;
use ap::paths::HostPath;
use ap::runs::{self, End, FromRun, LayerName};
use guest::agent::HOME;
use guest::protocol::Share as GuestShare;
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
const FLAGS_NOT_BUILT: [&str; 5] = ["-t", "--network", "--resume", "--gui", "--approve"];

/// The instructions a run can't honor yet. `ap build` refuses the ones that
/// need a fetch, so an agent that has them can't be built either.
const NOT_BUILT: [&str; 10] = [
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
    /// Each directory from `--share`, as typed.
    shares: Vec<OsString>,
    /// The first flag that isn't built yet.
    not_built: Option<&'static str>,
}

/// Reads `ap run`'s arguments: the agent, `-i` at most once,
/// `-p <prompt>` or `--prompt <prompt>` at most once, and `--share <path>`,
/// in any order. A second `--share` fails the run, not the reading. A flag
/// that isn't built yet stops the reading, because `ap run` fails on it
/// anyway. Any other flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut agent = None;
    let mut prompt = None;
    let mut interactive = false;
    let mut shares = Vec::new();
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
                shares,
                not_built: Some(not_built),
            });
        }
        if (arg == "-p" || arg == "--prompt") && prompt.is_none() {
            prompt = Some(args.next()?.as_bytes().to_vec());
        } else if arg == "--share" {
            shares.push(args.next()?.clone());
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
        shares,
        not_built: None,
    })
}

/// Runs the agent, and returns the harness's status, or `ap`'s own.
pub fn run(args: &Args) -> u8 {
    let started = start(args)
        .and_then(|(root, layers, shares, start)| run_agent(&root, args, &layers, shares, start));
    match started {
        Ok(status) => status,
        Err(error) => {
            eprintln!("ap: {error}");
            error.exit_code()
        }
    }
}

/// Names the run, starts its processes, and relays its output, filtered,
/// until it ends. `shares` are its shares, opened, in share order.
fn run_agent(
    root: &Path,
    args: &Args,
    layers: &[LayerName],
    shares: Vec<Opened>,
    start: Start,
) -> Result<u8, Error> {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let runs = ::store::Runs::new(root);
    let run = runs
        .create(&args.agent.to_string_lossy(), started)
        .map_err(fail("can't record the run"))?;
    let shares = keep_pending(&runs, &run.name, shares)?;

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
    // The network process and the share server end once the run process,
    // which holds the only other end of each one's socket, does.
    let network = ap::network::start(exe).map_err(fail("can't start the network process"))?;
    let share_socket = match shares.is_empty() {
        true => None,
        false => {
            let roots: Vec<_> = shares.iter().map(share::Root::as_fd).collect();
            let server = share::start(exe, &roots).map_err(fail("can't start the share server"))?;
            let inherited = share::inheritable(server.socket.as_fd())
                .map_err(fail("can't start the share server"))?;
            Some(inherited)
        }
    };
    // Made after the share server starts, so that only the run process
    // inherits it.
    let link = share::inheritable(network.link.as_fd())
        .map_err(fail("can't start the network process"))?;
    drop(network);
    let mut command = Command::new(exe);
    command
        .arg0(runs::RUN_PROCESS)
        .arg(std::process::id().to_string())
        .arg(&run.name)
        .arg(link.as_raw_fd().to_string());
    if let Some(socket) = &share_socket {
        command.arg(socket.as_raw_fd().to_string());
    }
    let mut process = command
        .stdin(OwnedFd::from(theirs))
        .stdout(held.try_clone().map_err(fail("can't start the run"))?)
        .process_group(0)
        .spawn()
        .map_err(fail("can't start the run process"))?;
    // The command holds this process's copy of the run process's end of the
    // socket, which must close so that the run's end reads as an end.
    drop(command);
    drop(link);
    drop(share_socket);
    // The log is made only once the run process runs, which records how
    // the run ends, so `ap ps -a` never lists a run that couldn't start.
    let mut log = runs
        .create_log(&run.name)
        .map_err(fail("can't create the run's log"))?;

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
    let end = relay(&mut stream, &execs, &mut log).map_err(fail("lost the run"));
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

/// Writes the run's output to this process's and to the run's log,
/// filtered, and hands each `ap exec` what is for it, until the run ends.
/// Write errors are ignored: a reader that went away, or a full disk,
/// shouldn't change what the harness does or how it ends.
fn relay(stream: &mut UnixStream, execs: &runs::Execs, log: &mut File) -> io::Result<End> {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    loop {
        match runs::read_from_run(stream)? {
            Some(FromRun::Stdout(bytes)) => {
                let shown = stdout_filter.push(&bytes);
                let mut stdout = io::stdout();
                let _ = stdout.write_all(&shown);
                let _ = stdout.flush();
                let _ = runs::write_log(log, false, &shown);
            }
            Some(FromRun::Stderr(bytes)) => {
                let shown = stderr_filter.push(&bytes);
                let _ = io::stderr().write_all(&shown);
                let _ = runs::write_log(log, true, &shown);
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

/// A share of a run, opened: its root, as [`share::Root`] has it, and the
/// path of the share's directory on your host.
struct Opened {
    directory: File,
    file: Option<CString>,
    mode: share::Mode<()>,
    host: PathBuf,
}

/// The roots of a run's shares, in share order. Each copy-on-write share
/// gets a directory of its own in the run's pending directory, which this
/// makes first, if the run has one.
fn keep_pending(
    runs: &::store::Runs,
    run: &str,
    shares: Vec<Opened>,
) -> Result<Vec<share::Root<File>>, Error> {
    let kept = fail("can't keep the run's changes");
    let mut made = false;
    let mut roots = Vec::new();
    for opened in shares {
        let mode = match opened.mode {
            share::Mode::ReadOnly => share::Mode::ReadOnly,
            share::Mode::ReadWrite => share::Mode::ReadWrite,
            share::Mode::CopyOnWrite(()) => {
                if !made {
                    runs.create_pending(run).map_err(&kept)?;
                    made = true;
                }
                let dir = (runs.create_pending_share(run, &opened.host)).map_err(&kept)?;
                share::Mode::CopyOnWrite(File::open(dir).map_err(&kept)?)
            }
        };
        roots.push(share::Root {
            directory: opened.directory,
            file: opened.file,
            mode,
        });
    }
    Ok(roots)
}

/// Everything the run needs before its VM starts: the store's root, the
/// agent's layers, its shares' roots, and how to start its harness.
fn start(args: &Args) -> Result<(PathBuf, Vec<LayerName>, Vec<Opened>, Start), Error> {
    let fail = |message: String| Error::Failed(message);
    if let Some(flag) = args.not_built {
        return Err(fail(format!("`ap run {flag}` is not built yet")));
    }
    let typed = match &args.shares[..] {
        [] => None,
        [only] => Some(only),
        _ => {
            return Err(fail("`ap run --share` can appear only once".to_owned()));
        }
    };
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
    if let Some(instruction) =
        (instructions.iter()).find(|instruction| NOT_BUILT.contains(&instruction.name.as_str()))
    {
        return Err(fail(format!("`{}` is not built yet", instruction.name)));
    }
    let files = Files::read(&instructions).map_err(unreadable)?;
    let home_dir = home.clone().unwrap_or_default();
    let checked = check_shares(&root, &record, &instructions, &files.share, &home_dir)?;
    let mut roots = Vec::new();
    for (share, (host, directory)) in files.share.iter().zip(&checked) {
        let shown = format!("`SHARE {}`", agentfile::print_argument(&share.source));
        let mode = match share.mode {
            ShareMode::CopyOnWrite if !directory => {
                return Err(fail(format!(
                    "{shown}: a copy-on-write file `SHARE` is not built yet. Give it `ro` or `rw`"
                )));
            }
            ShareMode::CopyOnWrite => share::Mode::CopyOnWrite(()),
            ShareMode::ReadOnly => share::Mode::ReadOnly,
            ShareMode::ReadWrite => share::Mode::ReadWrite,
        };
        let opened = |error: io::Error| fail(format!("{shown}: {error}"));
        // The share server serves this handle, so a symlink swapped in
        // since the check fails the run instead of being followed. A file
        // is served from the directory that holds it, by its name.
        let (directory, file) = match directory {
            true => (host.open().map_err(opened)?, None),
            false => {
                let (parent, name) = host.open_parent().map_err(opened)?;
                (parent, Some(name))
            }
        };
        roots.push(Opened {
            directory,
            file,
            mode,
            host: host.path().to_owned(),
        });
    }
    let context_name = Path::new(&record.directory)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let placed = GuestPath::read(&instructions, context_name).map_err(unreadable)?;
    // Each share's guest path, in share order.
    let mut shares: Vec<String> = (files.share.iter())
        .map(|share| {
            (placed.iter())
                .find(|placed| placed.line == share.line)
                .map(|placed| placed.path.clone())
                .ok_or_else(|| fail(format!("line {} has no guest path", share.line)))
        })
        .collect::<Result<_, _>>()?;
    // SPEC.md's `SHARE`: the share from `ap run --share` is copy-on-write,
    // needs no approval, and maps to `~/<directory name>`.
    let typed = match typed {
        None => None,
        Some(typed) => {
            let shown = format!("`--share {}`", typed.to_string_lossy());
            let opened = |error: io::Error| fail(format!("{shown}: {error}"));
            let host = HostPath::resolve_typed(Path::new(typed)).map_err(opened)?;
            if !host.is_dir().map_err(opened)? {
                return Err(fail(format!("{shown}: it isn't a directory")));
            }
            let name = (host.path().file_name())
                .ok_or_else(|| fail(format!("{shown}: it has no name to map to")))?
                .to_str()
                .ok_or_else(|| fail(format!("{shown}: its name isn't UTF-8")))?;
            let path = GuestPath::for_run_share(name, &placed)
                .map_err(|message| fail(format!("{shown}: {message}")))?;
            roots.push(Opened {
                directory: host.open().map_err(opened)?,
                file: None,
                mode: share::Mode::CopyOnWrite(()),
                host: host.path().to_owned(),
            });
            Some(path)
        }
    };
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
    // SPEC.md's Run section: the share from `--share`, then `WORKDIR`, then
    // the only directory share, then the agent's home.
    let directory_shares: Vec<&String> = (shares.iter().zip(&checked))
        .filter(|(_, (_, directory))| *directory)
        .map(|(path, _)| path)
        .collect();
    let directory = match (&typed, files.workdir, &directory_shares[..]) {
        (Some(typed), _, _) => typed.clone(),
        (None, Some(workdir), _) => guest_path(&workdir.path),
        (None, None, [only]) => (*only).clone(),
        (None, None, _) => HOME.to_owned(),
    };
    shares.extend(typed);
    // SPEC.md's MODE section: every share joins the harness's workspace. The
    // share from `--share` is a directory.
    let kinds = (checked.iter().map(|(_, directory)| *directory)).chain(std::iter::repeat(true));
    let workspace = ap::adapter::workspace(shares.iter().map(String::as_str).zip(kinds));
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
            argv.extend(ap::adapter::claude::workspace(&workspace));
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
            argv.extend(ap::adapter::codex::workspace(&workspace));
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
        shares: (shares.into_iter().zip(&roots))
            .map(|(path, opened)| GuestShare {
                path: path.into_bytes(),
                file: (opened.file.as_ref()).map(|name| name.as_bytes().to_vec()),
            })
            .collect(),
        prompt: args.prompt.clone(),
        terminal,
    };
    Ok((root, layers, roots, start))
}

/// Checks each `SHARE` again, as SPEC.md's SHARE and Grants sections say a
/// run does: a relative source needs the build context to be where it was,
/// every source must still exist and be the kind it was at build time, and
/// what it resolves to now must be approved, or you are asked again. It
/// returns each source as it resolves now, and whether it is a directory,
/// in share order.
fn check_shares(
    root: &Path,
    record: &::store::Agent,
    instructions: &[Instruction],
    shares: &[Share],
    home: &Path,
) -> Result<Vec<(HostPath, bool)>, Error> {
    let fail = |message: String| Error::Failed(message);
    if shares.is_empty() {
        return Ok(Vec::new());
    }
    let context = Path::new(&record.directory);
    let relative =
        |source: &str| !(source.starts_with('/') || source == "~" || source.starts_with("~/"));
    if shares.iter().any(|share| relative(&share.source)) {
        let now = context.canonicalize().map_err(|error| {
            fail(format!(
                "the agent was built from {}, which can't be found now: {error}",
                context.display()
            ))
        })?;
        if now != context {
            return Err(fail(format!(
                "the agent was built from {}, which is now {}. Build it again where it is",
                context.display(),
                now.display()
            )));
        }
    }
    let word = |directory: bool| match directory {
        true => "directory",
        false => "file",
    };
    // Each source resolves once, so the shares that name it are served
    // what its approval checks, even if it moves in between.
    let mut resolved: HashMap<&str, (bool, HostPath)> = HashMap::new();
    let mut checked = Vec::new();
    for share in shares {
        if let Some((directory, host)) = resolved.get(share.source.as_str()) {
            checked.push((host.clone(), *directory));
            continue;
        }
        let shown = format!("`SHARE {}`", agentfile::print_argument(&share.source));
        let host = HostPath::resolve_pinned(&share.source, context, home)
            .and_then(|host| Ok((host.is_dir()?, host)));
        let (directory, host) = host.map_err(|error| fail(format!("{shown}: {error}")))?;
        let built = (record.kinds.iter())
            .find(|kind| kind.source == share.source)
            .ok_or_else(|| {
                fail(format!(
                    "{shown}: the agent was built before `ap` recorded what its shares are. Build it again"
                ))
            })?;
        if built.directory != directory {
            return Err(fail(format!(
                "{shown}: it was a {} when the agent was built, and is a {} now",
                word(built.directory),
                word(directory)
            )));
        }
        resolved.insert(share.source.as_str(), (directory, host.clone()));
        checked.push((host, directory));
    }
    let only_shares: Vec<Instruction> = (instructions.iter())
        .filter(|instruction| instruction.name == "SHARE")
        .cloned()
        .collect();
    let grants = ap::approval::grants(&only_shares, |text| {
        resolved
            .get(text)
            .map(|(_, host)| host.clone())
            .ok_or_else(|| io::Error::other(format!("{text} wasn't resolved")))
    })
    .map_err(|errors| {
        let messages: Vec<String> = errors.into_iter().map(|error| error.message).collect();
        fail(messages.join("; "))
    })?;
    let approvals = ::store::Approvals::new(root);
    ap::approval::approve_shares(
        &approvals,
        Path::new(&record.agentfile),
        &record.agentfile,
        grants.paths,
    )
    .map_err(fail)?;
    Ok(checked)
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
