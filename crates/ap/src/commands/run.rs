//! `ap run <agent> [-i] [-p <prompt>] [--share <path>] [--network=<rule>]...`,
//! from SPEC.md's Run section. It names the run, and starts a run process
//! that boots the agent's layers in a new microVM and starts the harness
//! there as the user `agent`, as [`ap::runs`] says. It streams the harness's
//! output and exits with its status, or with 125 if the harness never
//! started. What it shows of the output, it also writes to the run's log.
//!
//! A run with a prompt is headless, and the harness gets pipes. Any other
//! run, or one with `-i`, is interactive: the harness gets a terminal in the
//! guest, and your terminal is put in raw mode for as long as the run lasts,
//! so every key reaches the harness. Its resizes follow.
//!
//! An agent's shares are checked again, then opened, and share servers
//! serve those handles to the run: one for its `rw` shares, and one for the
//! others. A file share is served from the directory that holds it, by its
//! name. A copy-on-write share's changes go to a directory of its own in the
//! run's pending directory, which the run makes in the store and leaves
//! there when it ends. The guest mounts each share at the place SPEC.md's
//! Paths section gives it. The share from `--share` needs no approval. It
//! is copy-on-write, and the guest mounts it at `~/<directory name>`.
//!
//! The guest's network follows the agent's rules in `NETWORK`, merged with
//! the ones from `--network`. A host with URL rules has each request to it
//! decided, as [`ap::network`] says. An instruction in [`NOT_BUILT`], or a
//! flag in [`FLAGS_NOT_BUILT`], fails with 125 and says so, before any VM
//! starts.
//!
//! Each `SECRET`'s value comes from the variable of its name in this
//! process's environment, and a run without one fails with 125, before any
//! VM starts. A `visible` value goes to the harness's environment. A hidden
//! one goes to the network process, which puts it in requests to its bound
//! hosts, and the harness gets a placeholder made for this run instead.
//! No process that `ap run` starts, such as the run process, the network
//! process, a share server, or the cleanup process, starts with a hidden
//! secret's variable.
//!
//! `AUTH key` and `AUTH token` read the variable that the harness's adapter
//! names, in the same way. A key is a hidden secret bound to the hosts the
//! adapter lists for it, and the network process refuses a request to them
//! that doesn't carry its placeholder. A token is visible. `AUTH login`
//! fails with 125, as not built yet.

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
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfile::{
    Auth, Files, GuestPath, Harness, Instruction, Network, Os, Part, Secrets, Settings, Share,
    ShareMode,
};
use ap::boot::Error;
use ap::filter::Filter;
use ap::network::{Hidden, Injection};
use ap::paths::HostPath;
use ap::runs::{self, End, FromRun, LayerName};
use guest::agent::HOME;
use guest::protocol::Share as GuestShare;
use guest::protocol::{GuestFile, Size, Start};

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
const FLAGS_NOT_BUILT: [&str; 4] = ["-t", "--resume", "--gui", "--approve"];

/// The instructions a run can't honor yet. `ap build` refuses the ones that
/// need a fetch, so an agent that has them can't be built either.
const NOT_BUILT: [&str; 8] = [
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
    /// Each rule from `--network=<rule>`, in order.
    network: Vec<OsString>,
    /// The first flag that isn't built yet.
    not_built: Option<&'static str>,
}

/// Reads `ap run`'s arguments: the agent, `-i` at most once,
/// `-p <prompt>` or `--prompt <prompt>` at most once, `--share <path>`, and
/// `--network=<rule>`, in any order. A second `--share` fails the run, not the reading. A flag
/// that isn't built yet stops the reading, because `ap run` fails on it
/// anyway. Any other flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut agent = None;
    let mut prompt = None;
    let mut interactive = false;
    let mut shares = Vec::new();
    let mut network = Vec::new();
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
                network,
                not_built: Some(not_built),
            });
        }
        if (arg == "-p" || arg == "--prompt") && prompt.is_none() {
            prompt = Some(args.next()?.as_bytes().to_vec());
        } else if arg == "--share" {
            shares.push(args.next()?.clone());
        } else if let Some(rule) = flag.strip_prefix(b"--network=") {
            network.push(OsString::from_vec(rule.to_vec()));
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
        network,
        not_built: None,
    })
}

/// Runs the agent, and returns the harness's status, or `ap`'s own.
pub fn run(args: &Args) -> u8 {
    let started = start(args).and_then(|(root, layers, shares, start, network, injection)| {
        run_agent(&root, args, &layers, shares, start, &network, injection)
    });
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
    mut start: Start,
    network: &[OsString],
    injection: Injection,
) -> Result<u8, Error> {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let name = args.agent.to_string_lossy();
    // Held from before the run is recorded until its record is locked and
    // its run process has opened the layers, so `ap rm` and rebuilds wait,
    // and never find the run finished or free a layer it is about to read.
    let digests: Vec<&str> = layers.iter().map(|layer| layer.digest.as_str()).collect();
    let held_layers = ::store::Agents::new(root)
        .hold_layers(&name, &digests)
        .map_err(fail("can't read the agent"))?
        .ok_or_else(|| {
            Error::Failed(format!(
                "agent {name} was removed or rebuilt while the run started"
            ))
        })?;
    let runs = ::store::Runs::new(root);
    let run = runs
        .create(&name, started)
        .map_err(fail("can't record the run"))?;
    let shares = keep_pending(&runs, &run.name, shares)?;
    // No process this one starts gets a hidden value, not even through the
    // environment it would inherit.
    let names: Vec<String> = (injection.secrets.iter())
        .map(|secret| secret.name.clone())
        .collect();

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
    let mut cleanup = Command::new(exe);
    for secret in &names {
        cleanup.env_remove(secret);
    }
    cleanup
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
    // The network process and the share servers end once the run process,
    // which holds the only other end of each one's socket, does.
    let ap::network::Started {
        process: network_process,
        link: network_link,
        lookups,
        authority,
    } = ap::network::start(exe, network, &injection)
        .map_err(fail("can't start the network process"))?;
    start.authority = authority;
    // The values live in the network process alone from here.
    drop(injection);
    // The run is active from here until the run process and each share
    // server have exited, because each holds a copy of what this locked.
    let record = runs
        .lock(&run.name)
        .map_err(fail("can't lock the run's record"))?;
    let roots: Vec<_> = shares.iter().map(share::Root::as_fd).collect();
    let share::Servers { started, serving } =
        share::start(exe, &roots, &names, Some(record.as_fd()))
            .map_err(fail("can't start the share server"))?;
    let share_sockets = (started.iter())
        .map(|server| share::inheritable(server.socket.as_fd()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(fail("can't start the share server"))?;
    let mut servers: Vec<Child> = started.into_iter().map(|server| server.process).collect();
    // Made after the share servers start, so that only the run process
    // inherits it.
    let link = share::inheritable(network_link.as_fd())
        .map_err(fail("can't start the network process"))?;
    drop(network_link);
    // Your keys have a pair of their own, so a harness that doesn't read
    // them holds back nothing else.
    let (keys, keys_end) = UnixStream::pair().map_err(fail("can't start the run"))?;
    let their_keys = share::inheritable(keys_end.as_fd()).map_err(fail("can't start the run"))?;
    drop(keys_end);
    let their_layers =
        share::inheritable(held_layers.as_fd()).map_err(fail("can't start the run"))?;
    drop(held_layers);
    let their_record = share::inheritable(record.as_fd()).map_err(fail("can't start the run"))?;
    drop(record);
    drop(network_process);
    let mut command = Command::new(exe);
    for secret in &names {
        command.env_remove(secret);
    }
    command
        .arg0(runs::RUN_PROCESS)
        .arg(std::process::id().to_string())
        .arg(&run.name)
        .arg(link.as_raw_fd().to_string())
        .arg(their_keys.as_raw_fd().to_string())
        .arg(their_layers.as_raw_fd().to_string())
        .arg(their_record.as_raw_fd().to_string());
    for server in serving {
        command.arg(share_sockets[server].as_raw_fd().to_string());
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
    drop(their_keys);
    drop(their_layers);
    drop(their_record);
    drop(share_sockets);
    // The log is made only once the run process runs, which records how
    // the run ends, so `ap ps -a` never lists a run that couldn't start.
    let mut log = runs
        .create_log(&run.name)
        .map_err(fail("can't create the run's log"))?;
    // The guest starts only once the request is sent, so the network
    // process decides nothing before its decisions have a log to go to.
    let keeper = log
        .try_clone()
        .map_err(fail("can't create the run's log"))?;
    let keeper = ap::network::Keeper::new(network, keeper);
    let lookups = ap::network::answer_lookups(lookups, ap::network::asks(network), Some(keeper));

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
            let keys = Arc::new(Mutex::new(keys));
            pass_keys(Arc::clone(&keys), false);
            take_signals(keys);
            Some(raw)
        }
        false => None,
    };
    let end = relay(&mut stream, &execs, &mut log).map_err(fail("lost the run"));
    drop(raw);
    // The run process exits once it has reported, so this doesn't wait long.
    let exited = process.wait();
    // Each share server ends once it has answered what the run process
    // sent, and the run is active until then, so `ap run` exits after it.
    for server in &mut servers {
        let _ = server.wait();
    }
    drop(held);
    // SPEC.md's Run, under **Blocked hosts**. The network process ends once
    // the run process has, and its reports end with it.
    let reports = lookups.reports(REPORTS_WAIT);
    if let Some(report) = ap::network::report(network, &reports) {
        // As with the run's own output, a failed write doesn't change how
        // the run ends.
        let _ = io::stderr().write_all(report.as_bytes());
    }
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

/// How long a run that has ended waits for the network process to end, for
/// the hosts it blocked.
const REPORTS_WAIT: Duration = Duration::from_secs(2);

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
/// it is typed, and nothing turns a key into a signal. A run that `ap stop`
/// reaches while `ap run` waits in the background of your terminal leaves
/// its modes as they are, and holds none.
pub(super) struct RawMode(Option<libc::termios>);

impl RawMode {
    pub(super) fn start() -> io::Result<RawMode> {
        while !foreground() && !runs::stopping() {
            // From the background, this waits as setting the modes would:
            // job control suspends `ap run` until it is in the foreground,
            // or until it ignores job control, and it changes nothing.
            // SAFETY: `tcdrain` takes and returns plain integers.
            if unsafe { libc::tcdrain(libc::STDIN_FILENO) } == 0 {
                // `ap run` is in the foreground now, or ignores job control
                // for a stop, or job control doesn't hold it, as when it
                // started with `SIGTTOU` ignored, and waiting again would
                // never end.
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        if runs::stopping() {
            return Ok(RawMode(None));
        }
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
        Ok(RawMode(Some(saved)))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            restore(saved);
        }
    }
}

/// Whether `ap run` is in the foreground of your terminal, or of one with no
/// job control, which isn't its controlling terminal.
fn foreground() -> bool {
    // SAFETY: both calls take and return plain integers.
    let (terminal, ours) = unsafe { (libc::tcgetpgrp(libc::STDIN_FILENO), libc::getpgrp()) };
    terminal == -1 || terminal == ours
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

/// Writes the run's output, filtered, to the run's log and then to this
/// process's, as [`show`] does, and hands each `ap exec` what is for it,
/// until the run ends. Write errors are ignored: a reader that went away,
/// or a full disk, shouldn't change what the harness does or how it ends.
fn relay(stream: &mut UnixStream, execs: &runs::Execs, log: &mut File) -> io::Result<End> {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    loop {
        match runs::read_from_run(stream)? {
            Some(FromRun::Stdout(bytes)) => {
                show(log, false, &stdout_filter.push(&bytes), &mut io::stdout());
            }
            Some(FromRun::Stderr(bytes)) => {
                show(log, true, &stderr_filter.push(&bytes), &mut io::stderr());
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

/// Writes output to the run's log, then shows it on `terminal`, so that
/// `ap logs` has all you saw. Errors are ignored, as [`relay`] says.
fn show(log: &mut impl Write, stderr: bool, shown: &[u8], terminal: &mut impl Write) {
    let _ = runs::write_log(log, stderr, shown);
    let _ = terminal.write_all(shown);
    let _ = terminal.flush();
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

/// The store's root, the agent's layers, its shares' roots, how to start
/// its harness, the network process's arguments, and what it puts in
/// requests.
type Prepared = (
    PathBuf,
    Vec<LayerName>,
    Vec<Opened>,
    Start,
    Vec<OsString>,
    Injection,
);

/// How many random bytes a placeholder has. It holds them in hex.
const PLACEHOLDER_BYTES: usize = 16;

/// A new placeholder: random bytes from the kernel, in lowercase hex.
fn random_placeholder() -> io::Result<Vec<u8>> {
    let mut bytes = [0u8; PLACEHOLDER_BYTES];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the call writes at most the bytes left in `bytes`.
        let drawn = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        match drawn {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            -1 => return Err(io::Error::last_os_error()),
            drawn => filled += drawn as usize,
        }
    }
    Ok(bytes
        .iter()
        .flat_map(|byte| format!("{byte:02x}").into_bytes())
        .collect())
}

/// Everything the run needs before its VM starts.
fn start(args: &Args) -> Result<Prepared, Error> {
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
    // SPEC.md's SECRET: each value comes from the host environment variable
    // of the secret's name, and a run without one fails before it asks
    // anything else.
    let secrets = Secrets::read(&instructions).map_err(unreadable)?;
    let mut given = Vec::new();
    let mut hidden = Vec::new();
    for (at, secret) in secrets.secrets.iter().enumerate() {
        let same = |other: &&agentfile::Secret| other.name == secret.name;
        if secrets.secrets[..at].iter().any(|other| same(&other)) {
            continue;
        }
        // A name on several lines is visible if any of them says so, as
        // its grant is.
        let visible = secrets
            .secrets
            .iter()
            .filter(same)
            .any(|other| other.visible);
        let value = std::env::var_os(&secret.name).ok_or_else(|| {
            fail(format!(
                "`SECRET {0}` has no value. Set the environment variable `{0}`",
                secret.name
            ))
        })?;
        let value = value.into_vec();
        if visible {
            given.push((secret.name.as_bytes().to_vec(), value));
            continue;
        }
        let placeholder = random_placeholder()
            .map_err(|error| fail(format!("can't make a placeholder: {error}")))?;
        given.push((secret.name.as_bytes().to_vec(), placeholder.clone()));
        // A name on several lines is bound to each of their hosts.
        let mut hosts: Vec<String> = Vec::new();
        for host in secrets
            .secrets
            .iter()
            .filter(same)
            .flat_map(|other| &other.hosts)
        {
            if !hosts.contains(host) {
                hosts.push(host.clone());
            }
        }
        hidden.push(Hidden {
            name: secret.name.clone(),
            value,
            placeholder,
            hosts,
            required: false,
        });
    }
    // SPEC.md's AUTH: `key` and `token` read their variable as a secret
    // does. A key is hidden, and every request to its hosts must carry its
    // placeholder. A token is visible.
    let mut placeholder_given = None;
    let harness = Settings::read(&instructions)
        .map_err(unreadable)?
        .agent
        .harness;
    match secrets.auth {
        None => {}
        Some(Auth::Login) => return Err(fail("`AUTH login` is not built yet".to_owned())),
        Some(auth) => {
            let method = if auth == Auth::Key { "key" } else { "token" };
            let sign_in = ap::adapter::sign_in(harness, auth)
                .ok_or_else(|| fail(format!("agent {name} can't run: `AUTH {method}`")))?;
            let variable = sign_in.variable;
            let value = std::env::var_os(variable).ok_or_else(|| {
                fail(format!(
                    "`AUTH {method}` has no value. Set the environment variable `{variable}`"
                ))
            })?;
            let value = value.into_vec();
            if sign_in.hosts.is_empty() {
                given.push((variable.as_bytes().to_vec(), value));
            } else {
                let placeholder = random_placeholder()
                    .map_err(|error| fail(format!("can't make a placeholder: {error}")))?;
                given.push((variable.as_bytes().to_vec(), placeholder.clone()));
                // Only a key has hosts.
                placeholder_given = Some(placeholder.clone());
                hidden.push(Hidden {
                    name: variable.to_owned(),
                    value,
                    placeholder,
                    hosts: sign_in.hosts.iter().map(|host| host.to_string()).collect(),
                    required: true,
                });
            }
        }
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
    // SPEC.md's NETWORK: rules from `--network` are for this run only, and
    // need no approval.
    let rules: Vec<String> = (args.network.iter())
        .map(|rule| {
            rule.to_str().map(str::to_owned).ok_or_else(|| {
                fail(format!(
                    "`--network={}` isn't UTF-8",
                    rule.to_string_lossy()
                ))
            })
        })
        .collect::<Result<_, _>>()?;
    let merged = agentfile::with_typed_network(&instructions, &rules).map_err(unreadable)?;
    Network::read(&merged).map_err(unreadable)?;
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
    // A visible secret's value, and a hidden one's placeholder.
    env.extend(given);
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
    let mut written = ap::adapter::run_files(harness, &directory, placeholder_given.as_deref());
    // SPEC.md's SECRET: git signs in to `github.com` with a token bound to
    // it.
    let bound = |token: &&str| {
        (secrets.secrets.iter()).any(|secret| {
            secret.name == *token && secret.hosts.iter().any(|host| host == "github.com")
        })
    };
    if let Some(token) = ap::adapter::GIT_TOKENS.into_iter().find(bound) {
        let placeholder = (hidden.iter())
            .find(|secret| secret.name == token)
            .map(|secret| secret.placeholder.as_slice());
        written.push((
            ap::adapter::GIT_CONFIG,
            ap::adapter::git_config(token, placeholder),
        ));
    }
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
        files: (written.into_iter())
            .map(|(path, contents)| GuestFile {
                path: path.as_bytes().to_vec(),
                contents,
            })
            .collect(),
        // The network process makes the run's authority, once it starts.
        authority: Vec::new(),
        prompt: args.prompt.clone(),
        terminal,
    };
    let hosts = ap::adapter::reserved(settings.agent.harness).hosts;
    let network = ap::network::arguments(hosts, &merged);
    let injection = Injection {
        headers: (ap::adapter::headers(settings.agent.harness).iter())
            .map(|header| header.to_string())
            .collect(),
        secrets: hidden,
    };
    Ok((root, layers, roots, start, network, injection))
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// A log that a terminal can look at.
    struct Log(Rc<RefCell<Vec<u8>>>);

    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A terminal that, at each write, keeps what the log held then.
    struct Terminal {
        log: Rc<RefCell<Vec<u8>>>,
        logged: Vec<Vec<u8>>,
    }

    impl Write for Terminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.logged.push(self.log.borrow().clone());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // spec(ap logs): It prints the run's output from its start, for an active run up to that moment, and exits 0.
    #[test]
    fn output_reaches_the_log_before_your_terminal() {
        let written = Rc::new(RefCell::new(Vec::new()));
        let mut log = Log(Rc::clone(&written));
        for stderr in [false, true] {
            let mut terminal = Terminal {
                log: Rc::clone(&written),
                logged: Vec::new(),
            };
            show(&mut log, stderr, b"shown\n", &mut terminal);
            assert_eq!(terminal.logged, [written.borrow().clone()]);
            let mut logged = &written.borrow()[..];
            let mut last = None;
            while let Some(part) = runs::read_log(&mut logged).unwrap() {
                last = Some(part);
            }
            let shown = b"shown\n".to_vec();
            let expected = if stderr {
                runs::Logged::Stderr(shown)
            } else {
                runs::Logged::Stdout(shown)
            };
            assert_eq!(last, Some(expected));
        }
    }
}
