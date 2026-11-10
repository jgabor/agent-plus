//! `ap build [--strict] [-t name] [dir]`, from SPEC.md's Build section. It
//! checks the Agentfile against the host, turns each `ADD` and the harness
//! configuration into a layer, and records the agent in the store. A layer
//! whose key an agent already recorded is a hit, and isn't built again.
//!
//! Nothing is fetched yet, so the instructions that need a fetch fail, and
//! no lockfile is written. Grants aren't approved here.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};

use agentfile::{
    Error, Files, GuestPath, Harness, Instruction, Network, Policy, Secrets, Settings, Sources,
    Text,
};
use ap::add;
use ap::paths::HostPath;
use graviola::hashing::sha2::Sha256Context;

/// The instructions that need a fetch or a harness, which `ap build` can't
/// build yet.
const NOT_BUILT: [&str; 5] = ["REPO", "PLUGIN", "SKILL", "MCP", "SEAL"];

/// Where the prompt goes in the harness configuration layer. The harness gets
/// it at run time, so it has no file of its own in the harness's directory.
const PROMPT: &str = "/etc/ap/prompt";

/// The arguments of `ap build`.
pub struct Args {
    strict: bool,
    /// The name from `-t`.
    name: Option<String>,
    /// The directory, or `None` for the current one.
    dir: Option<PathBuf>,
}

/// Reads `ap build`'s arguments: `--strict`, `-t <name>`, and a directory,
/// each at most once, in any order. Any other flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut strict = false;
    let mut name = None;
    let mut dir = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--strict" && !strict {
            strict = true;
        } else if arg == "-t" && name.is_none() {
            name = Some(args.next()?.to_str()?.to_string());
        } else if arg.as_encoded_bytes().starts_with(b"-") || dir.is_some() {
            return None;
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    Some(Args { strict, name, dir })
}

/// Builds the agent, and returns the exit status. What each layer did goes
/// to standard output, and warnings and errors to standard error.
pub fn run(args: &Args) -> u8 {
    match build(args) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// One layer the agent needs.
struct Planned {
    /// What `ap build` calls the layer when it reports on it.
    label: String,
    key: String,
    content: Content,
}

enum Content {
    Add(add::Layer),
    Image(Vec<u8>),
}

impl Planned {
    fn image(self) -> io::Result<Vec<u8>> {
        match self.content {
            Content::Add(layer) => layer.image(),
            Content::Image(image) => Ok(image),
        }
    }
}

fn build(args: &Args) -> Result<(), String> {
    let (path, context) = match &args.dir {
        Some(dir) => {
            if !fs::metadata(dir).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(format!("ap: {} isn't a directory", dir.display()));
            }
            (dir.join("Agentfile"), dir.clone())
        }
        None => (PathBuf::from("Agentfile"), PathBuf::from(".")),
    };
    let shown = path.display();
    let bytes = fs::read(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => match &args.dir {
            Some(dir) => format!("ap: no Agentfile in {}", dir.display()),
            None => "ap: no Agentfile in the current directory".to_string(),
        },
        _ => format!("ap: {shown}: {error}"),
    })?;
    let source = String::from_utf8(bytes).map_err(|_| format!("ap: {shown} isn't UTF-8 text"))?;

    let context = context
        .canonicalize()
        .map_err(|error| format!("ap: {}: {error}", context.display()))?;
    let Some(directory) = context.to_str() else {
        return Err(format!("ap: {} isn't UTF-8", context.display()));
    };
    let dir_name = context
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let name = match &args.name {
        Some(name) if !::store::valid_name(name) => {
            return Err(format!("ap: `{name}` isn't a name. {NAME_RULE}"));
        }
        Some(name) => name.clone(),
        None if !::store::valid_name(dir_name) => {
            return Err(format!(
                "ap: the directory name `{dir_name}` can't name an agent, so give it a name with `-t <name>`. {NAME_RULE}"
            ));
        }
        None => dir_name.to_string(),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref().map(Path::as_os_str),
    )
    .ok_or("ap: HOME isn't an absolute path, so there's no place for the store")?;
    let home = home.unwrap_or_default();

    let report = |errors: Vec<Error>| {
        let lines: Vec<String> = errors
            .iter()
            .map(|error| format!("{shown}:{}: {}", error.line, error.message))
            .collect();
        lines.join("\n")
    };
    let instructions = agentfile::parse(&source).map_err(report)?;
    let (planned, warnings) = plan(&instructions, &context, &home, dir_name).map_err(report)?;
    if args.strict && !warnings.is_empty() {
        return Err(report(warnings));
    }
    for warning in &warnings {
        eprintln!("{shown}:{}: warning: {}", warning.line, warning.message);
    }

    let agents = ::store::Agents::new(&root);
    let layers = ::store::Layers::new(&root);
    let old = agents.get(&name).map_err(|error| format!("ap: {error}"))?;
    if let Some(old) = &old
        && old.directory != directory
    {
        return Err(format!(
            "ap: agent {name} was built from {}, not {directory}",
            old.directory
        ));
    }
    let declared: Vec<::store::Declared> = instructions
        .iter()
        .filter(|instruction| instruction.text.is_none())
        .map(|instruction| ::store::Declared {
            name: instruction.name.clone(),
            arguments: instruction.arguments.clone(),
        })
        .collect();

    let mut planned = Some(planned);
    let mut use_cache = true;
    let lines = loop {
        // The first try uses the planned layers. A retry plans them again.
        let current = match planned.take() {
            Some(current) => current,
            None => {
                plan(&instructions, &context, &home, dir_name)
                    .map_err(report)?
                    .0
            }
        };
        let (built, lines) =
            store_layers(&agents, &layers, current, use_cache).map_err(|e| format!("ap: {e}"))?;
        let agent = ::store::Agent {
            name: name.clone(),
            directory: directory.to_string(),
            layers: built,
            declared: declared.clone(),
        };
        if old.as_ref() == Some(&agent) {
            break lines;
        }
        match agents.record(&agent, false) {
            Ok(()) => break lines,
            // Another `ap` freed a layer this build found. Put every layer.
            Err(error) if error.kind() == io::ErrorKind::NotFound && use_cache => {
                use_cache = false;
            }
            Err(error) => return Err(format!("ap: {error}")),
        }
    };
    for line in lines {
        println!("{line}");
    }
    println!("agent {name}");
    Ok(())
}

const NAME_RULE: &str = "A name is 1 to 63 lowercase letters, digits, and `-`, starting and ending with a letter or digit";

/// Puts each layer in the store, unless an agent already recorded its key,
/// and returns the agent's layers with a line for each.
fn store_layers(
    agents: &::store::Agents,
    layers: &::store::Layers,
    planned: Vec<Planned>,
    use_cache: bool,
) -> io::Result<(Vec<::store::Layer>, Vec<String>)> {
    let mut built = Vec::new();
    let mut lines = Vec::new();
    for layer in planned {
        let found = if use_cache {
            agents.layer(&layer.key)?
        } else {
            None
        };
        let label = layer.label.clone();
        let key = layer.key.clone();
        let (status, digest) = match found {
            Some(digest) => ("hit", digest),
            None => ("built", layers.put(&layer.image()?)?),
        };
        lines.push(format!("{status} {label}"));
        built.push(::store::Layer { key, digest });
    }
    Ok((built, lines))
}

/// Checks the Agentfile against the host, and plans its layers: one for each
/// `ADD`, in line order, and the harness configuration last. It returns
/// every error it finds, or the layers and the warnings.
fn plan(
    instructions: &[Instruction],
    context: &Path,
    home: &Path,
    dir_name: &str,
) -> Result<(Vec<Planned>, Vec<Error>), Vec<Error>> {
    let mut errors = Vec::new();
    let settings = take(&mut errors, Settings::read(instructions));
    let sources = take(&mut errors, Sources::read(instructions));
    let files = take(&mut errors, Files::read(instructions));
    take(&mut errors, Secrets::read(instructions));
    take(&mut errors, Network::read(instructions));
    take(&mut errors, Policy::read(instructions));
    let guest = take(&mut errors, GuestPath::read(instructions, dir_name));
    let (Some(settings), Some(sources), Some(files), Some(guest)) =
        (settings, sources, files, guest)
    else {
        errors.sort_by_key(|error| error.line);
        errors.dedup();
        return Err(errors);
    };

    let mut warnings = Vec::new();
    let mut planned = Vec::new();
    let mut system = Vec::new();
    let mut prompt = None;
    let fail = |line: usize, message: String| Error { line, message };
    for instruction in instructions {
        let line = instruction.line;
        if NOT_BUILT.contains(&instruction.name.as_str()) {
            errors.push(fail(
                line,
                format!("`{}` is not built yet", instruction.name),
            ));
            continue;
        }
        let Some(text) = &instruction.text else {
            continue;
        };
        let value = match text {
            Text::Literal(value) => value.as_bytes().to_vec(),
            Text::Path(path) => match read_text(path, context, home) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(fail(line, error.to_string()));
                    continue;
                }
            },
        };
        if instruction.name == "SYSTEM" {
            system.push(value);
        } else {
            prompt = Some(value);
        }
    }
    for add in &sources.add {
        let Some(guest) = guest.iter().find(|path| path.line == add.line) else {
            continue;
        };
        let layer = HostPath::resolve(&add.source, context, home)
            .and_then(|host| add::Layer::read(add, &host, &guest.path));
        match layer {
            Ok(layer) => {
                warnings.extend(layer.warnings.iter().map(|w| fail(add.line, w.clone())));
                let mut label = "ADD".to_string();
                label.push(' ');
                label.push_str(&agentfile::print_argument(&add.source));
                if let Some(destination) = &add.destination {
                    label.push(' ');
                    label.push_str(&agentfile::print_argument(destination));
                }
                planned.push(Planned {
                    label,
                    key: layer.key.clone(),
                    content: Content::Add(layer),
                });
            }
            Err(error) => errors.push(fail(add.line, error.to_string())),
        }
    }
    for share in &files.share {
        if let Err(error) = HostPath::resolve(&share.source, context, home) {
            errors.push(fail(share.line, error.to_string()));
        }
    }
    if !errors.is_empty() {
        errors.sort_by_key(|error| error.line);
        return Err(errors);
    }
    let image = configuration(settings.agent.harness, &system, prompt.as_deref())
        .map_err(|error| vec![fail(1, error.to_string())])?;
    planned.push(Planned {
        label: "harness configuration".to_string(),
        key: configuration_key(&image),
        content: Content::Image(image),
    });
    warnings.sort_by_key(|warning| warning.line);
    Ok((planned, warnings))
}

/// The value of `result`, or `None` after adding its errors to `errors`.
fn take<T>(errors: &mut Vec<Error>, result: Result<T, Vec<Error>>) -> Option<T> {
    result.map_err(|found| errors.extend(found)).ok()
}

/// Reads the file a `SYSTEM` or `PROMPT` path names, through the handle that
/// its checked path opens.
fn read_text(path: &str, context: &Path, home: &Path) -> io::Result<Vec<u8>> {
    let host = HostPath::resolve(path, context, home)?;
    let mut value = Vec::new();
    host.open()?.read_to_end(&mut value).map_err(|error| {
        io::Error::new(error.kind(), format!("{}: {error}", host.path().display()))
    })?;
    Ok(value)
}

/// The harness configuration layer, as an uncompressed EROFS image. `SYSTEM`
/// goes to the harness's own file, its values separated by a blank line, and
/// the prompt to [`PROMPT`].
fn configuration(
    harness: Harness,
    system: &[Vec<u8>],
    prompt: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut root = erofs::Dir {
        mode: 0o755,
        entries: BTreeMap::new(),
    };
    if !system.is_empty() {
        let file = match harness {
            Harness::Claude => "/home/agent/.claude/CLAUDE.md",
            Harness::Codex => "/home/agent/.codex/AGENTS.md",
        };
        let mut text = Vec::new();
        for value in system {
            if !text.is_empty() {
                text.extend_from_slice(b"\n\n");
            }
            let end = value
                .iter()
                .rposition(|b| *b != b'\n')
                .map_or(0, |at| at + 1);
            text.extend_from_slice(&value[..end]);
        }
        text.push(b'\n');
        insert(&mut root, file, text);
    }
    if let Some(prompt) = prompt {
        insert(&mut root, PROMPT, prompt.to_vec());
    }
    let mut image = Vec::new();
    erofs::write(root, &mut image).map_err(|error| match error {
        erofs::Error::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
    })?;
    Ok(image)
}

/// Puts a file at the absolute `path` in `root`, with the directories above it.
fn insert(root: &mut erofs::Dir, path: &str, data: Vec<u8>) {
    let mut names: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
    let Some(last) = names.pop() else {
        return;
    };
    let mut dir = root;
    for name in names {
        let node = dir
            .entries
            .entry(name.as_bytes().to_vec())
            .or_insert_with(|| {
                erofs::Node::Dir(erofs::Dir {
                    mode: 0o755,
                    entries: BTreeMap::new(),
                })
            });
        let erofs::Node::Dir(next) = node else {
            unreachable!("only directories are inserted above a file");
        };
        dir = next;
    }
    dir.entries.insert(
        last.as_bytes().to_vec(),
        erofs::Node::File(erofs::File {
            mode: 0o644,
            size: data.len() as u64,
            content: Box::new(Cursor::new(data)),
        }),
    );
}

/// The harness configuration's key. The image is a function of everything it
/// was rendered from, so its hash keys it.
fn configuration_key(image: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(b"harness configuration\0");
    hash.update(image);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}
