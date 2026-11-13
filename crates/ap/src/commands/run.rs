//! `ap run <agent> [-p <prompt>]`, from SPEC.md's Run section. It boots the
//! agent's layers in a new microVM, in this process, and starts the harness
//! there headless, as the user `agent`. It streams the harness's output and
//! exits with its status, or with 125 if the harness never started.
//!
//! Only a headless run of an agent with `NETWORK -` is built. Everything else
//! on #297's list of what isn't built yet fails with 125 and says so, before
//! any VM starts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentfile::{Files, Instruction, Network, Os, Part, Secrets, Settings, Sign};
use ap::boot::{self, Error, Layer};
use guest::agent::HOME;
use guest::protocol::Start;

/// How long a run may last. A headless run ends with its harness, so this
/// only stops one that never does.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

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
    let result = start(args).and_then(|(layers, start)| boot::boot_agent(TIMEOUT, layers, start));
    match result {
        Ok(status) => status,
        Err(error) => {
            eprintln!("ap: {error}");
            error.exit_code()
        }
    }
}

/// Everything the run needs before its VM starts: the agent's layers, and
/// how to start its harness.
fn start(args: &Args) -> Result<(Vec<Layer>, Start), Error> {
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

    let layers = ::store::Layers::new(&root);
    let layers = record
        .layers
        .iter()
        .map(|layer| {
            let disk = layers.get(&layer.digest).map_err(|error| {
                fail(format!(
                    "can't read the layer for {}: {error}",
                    layer.instruction
                ))
            })?;
            Ok(Layer {
                digest: layer.digest.clone(),
                disk,
            })
        })
        .collect::<Result<_, Error>>()?;
    let start = Start {
        argv: argv.into_iter().map(String::into_bytes).collect(),
        env: env.into_iter().collect(),
        directory: directory.into_bytes(),
        prompt,
    };
    Ok((layers, start))
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
