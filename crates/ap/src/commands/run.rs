//! `ap run <agent> [-p <prompt>]`, from SPEC.md's Run section. It names the
//! run, and starts a run process that boots the agent's layers in a new
//! microVM and starts the harness there headless, as the user `agent`, as
//! [`ap::runs`] says. It streams the harness's output and exits with its
//! status, or with 125 if the harness never started.
//!
//! Only a headless run of an agent with `NETWORK -` is built. Everything else
//! on #297's list of what isn't built yet fails with 125 and says so, before
//! any VM starts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::fs::{DirBuilder, File};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use agentfile::{Files, Instruction, Network, Os, Part, Secrets, Settings, Sign};
use ap::boot::Error;
use ap::filter::Filter;
use ap::runs::{self, End, FromRun, LayerName};
use guest::agent::HOME;
use guest::protocol::Start;

/// The `PATH` the harness gets, unless `ENV` sets one.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The flags SPEC.md gives `ap run` that aren't built yet.
const FLAGS_NOT_BUILT: [&str; 7] = [
    "-i",
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
    /// The first flag that isn't built yet.
    not_built: Option<&'static str>,
}

/// Reads `ap run`'s arguments: the agent, and `-p <prompt>` or
/// `--prompt <prompt>` at most once, in either order. A flag that isn't built
/// yet stops the reading, because `ap run` fails on it anyway. Any other flag
/// is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut agent = None;
    let mut prompt = None;
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
                not_built: Some(not_built),
            });
        }
        if (arg == "-p" || arg == "--prompt") && prompt.is_none() {
            prompt = Some(args.next()?.as_bytes().to_vec());
        } else if flag.starts_with(b"-") || agent.is_some() {
            return None;
        } else {
            agent = Some(arg.clone());
        }
    }
    Some(Args {
        agent: agent?,
        prompt,
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
    let (run, record) = ::store::Runs::new(root)
        .create(&args.agent.to_string_lossy(), started)
        .map_err(fail("can't record the run"))?;
    // The lock lasts as long as this process, and marks the run active.
    record.lock().map_err(fail("can't lock the run's record"))?;

    let dir = runs::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), root);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(fail(&format!("can't create {}", dir.display())))?;
    let opened = File::open(&dir).map_err(fail(&format!("can't open {}", dir.display())))?;
    let socket_name = runs::socket_name(&run.name);
    let socket = runs::short_path(&opened, &socket_name);
    let listener = UnixListener::bind(&socket).map_err(fail("can't create the run's socket"))?;
    // Connecting before the run process starts makes this connection the
    // first it accepts.
    let mut stream = UnixStream::connect(&socket).map_err(fail("can't connect to the run"))?;

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
    let mut process = Command::new(exe)
        .arg0(runs::RUN_PROCESS)
        .arg(std::process::id().to_string())
        .stdin(OwnedFd::from(listener))
        .stdout(held.try_clone().map_err(fail("can't start the run"))?)
        .process_group(0)
        .spawn()
        .map_err(fail("can't start the run process"))?;

    let lost = fail("lost the run");
    runs::send_request(&mut stream, layers, start).map_err(lost)?;
    let end = relay(&mut stream).map_err(fail("lost the run"));
    // The run process exits once it has reported, so this doesn't wait long.
    let _ = process.wait();
    drop(held);
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

/// Writes the run's output to this process's, filtered, until the run ends.
/// Write errors are ignored: a reader that went away shouldn't change what
/// the harness does or how it ends.
fn relay(stream: &mut UnixStream) -> io::Result<End> {
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
    let prompt = match &args.prompt {
        Some(prompt) if prompt.is_empty() => {
            return Err(fail(
                "an empty prompt opens the interactive interface, which is not built yet"
                    .to_owned(),
            ));
        }
        prompt => prompt.clone(),
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
    let argv = match settings.agent.harness {
        // `--` keeps a prompt that starts with `-` from reading as a flag.
        agentfile::Harness::Claude => vec![harness, "-p".to_owned(), "--".to_owned()],
        agentfile::Harness::Codex => vec![harness, "exec".to_owned(), "--".to_owned()],
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
        prompt,
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
