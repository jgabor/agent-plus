//! `ap build [--strict] [--replace] [--approve=file] [-t name] [dir]`, from
//! SPEC.md's Build section. It checks the Agentfile against the host, asks
//! you to approve its grants as `ap::approval` says, turns each `ADD` and the
//! harness configuration into a layer, and records the agent in the store. A
//! layer whose key an agent already recorded is a hit, and isn't built again.
//!
//! The agent's bottom layer is the pinned `OS minimal` layer, downloaded only
//! if the store doesn't have it. The other tiers, and the instructions that
//! need any other fetch, fail, and no lockfile is written.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Cursor, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agentfile::{
    Error, Files, GuestPath, Harness, Instruction, Network, Os, Policy, Secrets, Settings, Sources,
    Text,
};
use ap::add;
use ap::fetch::{self, Client, Pin};
use ap::paths::HostPath;
use graviola::hashing::sha2::Sha256Context;

/// The instructions that need a fetch or a harness, which `ap build` can't
/// build yet.
const NOT_BUILT: [&str; 5] = ["REPO", "PLUGIN", "SKILL", "MCP", "SEAL"];

/// Where the prompt goes in the harness configuration layer. The harness gets
/// it at run time, so it has no file of its own in the harness's directory.
const PROMPT: &str = "/etc/ap/prompt";

/// The zstd level the store keeps the OS layer at, which `COMPRESSION`
/// doesn't change.
const OS_LEVEL: i8 = 3;

/// The arguments of `ap build`.
pub struct Args {
    strict: bool,
    /// Whether the build may take the name over from another directory.
    replace: bool,
    /// The file from `--approve=<file>`.
    approve: Option<PathBuf>,
    /// The name from `-t`.
    name: Option<String>,
    /// The directory, or `None` for the current one.
    dir: Option<PathBuf>,
}

/// Reads `ap build`'s arguments: `--strict`, `--replace`, `--approve=<file>`,
/// `-t <name>`, and a directory, each at most once, in any order. Any other
/// flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut strict = false;
    let mut replace = false;
    let mut approve = None;
    let mut name = None;
    let mut dir = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--strict" && !strict {
            strict = true;
        } else if arg == "--replace" && !replace {
            replace = true;
        } else if let Some(file) = arg.as_encoded_bytes().strip_prefix(b"--approve=")
            && !file.is_empty()
            && approve.is_none()
        {
            approve = Some(PathBuf::from(OsStr::from_bytes(file)));
        } else if arg == "-t" && name.is_none() {
            name = Some(args.next()?.to_str()?.to_string());
        } else if arg.as_encoded_bytes().starts_with(b"-") || dir.is_some() {
            return None;
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    Some(Args {
        strict,
        replace,
        approve,
        name,
        dir,
    })
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
    /// The line of the instruction the layer comes from.
    line: usize,
    /// What `ap build` calls the layer when it reports on it.
    label: String,
    key: String,
    /// The zstd level the store keeps the layer at.
    level: i8,
    content: Content,
}

enum Content {
    Add(add::Layer),
    Image(Vec<u8>),
    /// A layer to download, unless the store has it.
    Fetch(Pin<'static>),
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
    // The approval is kept with the canonical path, so the Agentfile is read
    // from there, and not through a symlink swapped in since.
    let missing = |error: io::Error| match error.kind() {
        io::ErrorKind::NotFound => match &args.dir {
            Some(dir) => format!("ap: no Agentfile in {}", dir.display()),
            None => "ap: no Agentfile in the current directory".to_string(),
        },
        _ => format!("ap: {error}"),
    };
    let agentfile = HostPath::resolve("Agentfile", &context, Path::new("/")).map_err(missing)?;
    let mut bytes = Vec::new();
    agentfile
        .open()
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| format!("ap: {shown}: {error}"))?;
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
    let mut resolved = Resolved {
        context: context.clone(),
        home: home.clone(),
        paths: HashMap::new(),
    };
    let (planned, warnings) = plan(&instructions, &mut resolved, dir_name).map_err(report)?;
    if args.strict && !warnings.is_empty() {
        return Err(report(warnings));
    }
    for warning in &warnings {
        eprintln!("{shown}:{}: warning: {}", warning.line, warning.message);
    }

    let agents = ::store::Agents::new(&root);
    let layers = ::store::Layers::new(&root);
    if let Some(old) = agents.get(&name).map_err(|error| format!("ap: {error}"))?
        && old.directory != directory
        && !args.replace
    {
        return Err(format!(
            "ap: agent {name} was built from {}, not {directory}. If you moved it, build with `--replace` to take over the name",
            old.directory
        ));
    }
    // The grants come from the resolutions the layers are made from, so a
    // path that changes on the host can't put what you didn't approve in a
    // layer.
    let grants = ap::approval::grants(&instructions, |text| resolved.get(text)).map_err(report)?;
    let approver = match &args.approve {
        Some(file) => ap::approval::Approver::File(file),
        None => ap::approval::Approver::Terminal,
    };
    let approvals = ::store::Approvals::new(&root);
    ap::approval::approve(
        &approvals,
        agentfile.path(),
        &shown.to_string(),
        &grants,
        approver,
    )?;

    let Some(agentfile_path) = agentfile.path().to_str().map(str::to_string) else {
        return Err(format!("ap: {} isn't UTF-8", agentfile.path().display()));
    };
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
                plan(&instructions, &mut resolved, dir_name)
                    .map_err(report)?
                    .0
            }
        };
        let (built, lines) = store_layers(&agents, &layers, current, use_cache, &report)?;
        let agent = ::store::Agent {
            name: name.clone(),
            directory: directory.to_string(),
            agentfile: agentfile_path.clone(),
            layers: built,
            declared: declared.clone(),
        };
        // A build that put no layer leaves an unchanged record as it is. The
        // record is read again, because another `ap` may have changed it.
        let unchanged = || -> Result<bool, String> {
            let current = agents.get(&name).map_err(|error| format!("ap: {error}"))?;
            Ok(current.as_ref() == Some(&agent))
        };
        if lines.iter().all(|line| line.starts_with("hit ")) && unchanged()? {
            break lines;
        }
        match agents.record(&agent, args.replace) {
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

/// What putting one layer in the store takes.
enum Made {
    /// The store has the layer with this digest.
    Hit(String),
    /// An image to put at a zstd level.
    Image(Vec<u8>, i8),
    /// A layer to download at a zstd level, unless the store has it.
    Fetch(Pin<'static>, i8),
}

/// Puts each layer in the store, unless an agent already recorded its key
/// and the store keeps it at the layer's level, and returns the agent's layers
/// with a line for each. Every image is made before any download, and every
/// download before any image is put, so a layer that can't be made leaves the
/// store as it was, and its error names its line.
fn store_layers(
    agents: &::store::Agents,
    layers: &::store::Layers,
    planned: Vec<Planned>,
    use_cache: bool,
    report: &dyn Fn(Vec<Error>) -> String,
) -> Result<(Vec<::store::Layer>, Vec<String>), String> {
    let io = |error: io::Error| format!("ap: {error}");
    // Each layer's key and label, with what putting it takes.
    let mut made = Vec::new();
    let mut errors = Vec::new();
    for layer in planned {
        let (line, label, key, level) = (layer.line, layer.label, layer.key, layer.level);
        let content = match layer.content {
            Content::Fetch(pin) => {
                made.push((key, label, Made::Fetch(pin, level)));
                continue;
            }
            content => content,
        };
        let found = match use_cache {
            true => agents.layer(&key).map_err(io)?,
            false => None,
        };
        // Agents at other levels can share a layer, and the last to put it
        // set its level.
        if let Some(digest) = found
            && layers.level(&digest).map_err(io)? == Some(level)
        {
            made.push((key, label, Made::Hit(digest)));
            continue;
        }
        let image = match content {
            Content::Add(add) => add.image(),
            Content::Image(image) => Ok(image),
            Content::Fetch(_) => unreachable!("a fetch was made above"),
        };
        match image {
            Ok(image) => made.push((key, label, Made::Image(image, level))),
            Err(error) => errors.push(Error {
                line,
                message: error.to_string(),
            }),
        }
    }
    if !errors.is_empty() {
        return Err(report(errors));
    }
    let mut built = Vec::new();
    let mut lines = Vec::new();
    let client = Client::new();
    let mut put = Vec::new();
    for (key, label, content) in made {
        let (status, digest) = match content {
            Made::Hit(digest) => ("hit", digest),
            Made::Fetch(pin, level) => {
                let fetched = fetch::layer(layers, &client, &pin, level)
                    .map_err(|error| format!("ap: {label}: {error}"))?;
                (
                    if fetched { "fetched" } else { "hit" },
                    pin.sha256.to_string(),
                )
            }
            Made::Image(image, level) => {
                put.push((built.len(), image, level));
                ("built", String::new())
            }
        };
        lines.push(format!("{status} {label}"));
        built.push(::store::Layer {
            key,
            digest,
            instruction: label,
        });
    }
    for (index, image, level) in put {
        built[index].digest = layers.put(&image, level).map_err(io)?;
    }
    Ok((built, lines))
}

/// Each host path a build resolved, by its text, so that approval and every
/// plan of the layers use the same resolution. A layer opens its path
/// without following a symlink swapped in since.
struct Resolved {
    context: PathBuf,
    home: PathBuf,
    paths: HashMap<String, Result<HostPath, String>>,
}

impl Resolved {
    /// Resolves `text` as [`HostPath::resolve`] does, the first time only.
    fn get(&mut self, text: &str) -> io::Result<HostPath> {
        let (context, home) = (&self.context, &self.home);
        let resolved = self.paths.entry(text.to_string()).or_insert_with(|| {
            HostPath::resolve(text, context, home).map_err(|error| error.to_string())
        });
        resolved.clone().map_err(io::Error::other)
    }
}

/// Checks the Agentfile against the host, and plans its layers: the OS first,
/// one for each `ADD`, in line order, and the harness configuration last. It returns
/// every error it finds, or the layers and the warnings.
fn plan(
    instructions: &[Instruction],
    resolved: &mut Resolved,
    dir_name: &str,
) -> Result<(Vec<Planned>, Vec<Error>), Vec<Error>> {
    let mut errors = Vec::new();
    let settings = take(&mut errors, Settings::read(instructions));
    // The line of the `OS` instruction, if there is one.
    let os_line = instructions
        .iter()
        .find(|instruction| instruction.name == "OS")
        .map(|instruction| instruction.line);
    let unbuilt = match settings.as_ref().map(|settings| settings.os) {
        Some(Os::Standard) => Some("standard"),
        Some(Os::Desktop) => Some("desktop"),
        Some(Os::Minimal) | None => None,
    };
    if let Some(tier) = unbuilt {
        let message = match os_line {
            Some(_) => format!("`OS {tier}` is not built yet"),
            None => format!("`OS {tier}`, the default, is not built yet"),
        };
        errors.push(Error {
            line: os_line.unwrap_or(1),
            message,
        });
    }
    let sources = take(&mut errors, Sources::read(instructions));
    let files = take(&mut errors, Files::read(instructions));
    take(&mut errors, Secrets::read(instructions));
    take(&mut errors, Network::read(instructions));
    take(&mut errors, Policy::read(instructions));
    let guest = take(&mut errors, GuestPath::read(instructions, dir_name));

    let mut warnings = Vec::new();
    let mut planned = Vec::new();
    // The line of the first `SYSTEM` or `PROMPT`, which the harness
    // configuration comes from.
    let mut text_line = None;
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
        text_line.get_or_insert(line);
        let value = match text {
            Text::Literal(value) => value.as_bytes().to_vec(),
            Text::Path(path) => match resolved.get(path).and_then(|host| read_text(&host)) {
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
    // Checking `ADD` and `SHARE` on the host needs their typed values.
    let (Some(settings), Some(sources), Some(files), Some(guest)) =
        (settings, sources, files, guest)
    else {
        errors.sort_by_key(|error| error.line);
        errors.dedup();
        return Err(errors);
    };
    let level = settings.compression.level();
    planned.push(Planned {
        line: os_line.unwrap_or(1),
        label: "OS minimal".to_string(),
        key: os_key(fetch::os().sha256),
        level: OS_LEVEL,
        content: Content::Fetch(fetch::os()),
    });
    for add in &sources.add {
        let Some(guest) = guest.iter().find(|path| path.line == add.line) else {
            continue;
        };
        let layer = resolved
            .get(&add.source)
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
                    line: add.line,
                    label,
                    key: layer.key.clone(),
                    level,
                    content: Content::Add(layer),
                });
            }
            Err(error) => errors.push(fail(add.line, error.to_string())),
        }
    }
    for share in &files.share {
        if let Err(error) = resolved.get(&share.source) {
            errors.push(fail(share.line, error.to_string()));
        }
    }
    if !errors.is_empty() {
        errors.sort_by_key(|error| error.line);
        return Err(errors);
    }
    let line = text_line.unwrap_or(1);
    let image = configuration(settings.agent.harness, &system, prompt.as_deref())
        .map_err(|error| vec![fail(line, error.to_string())])?;
    planned.push(Planned {
        line,
        label: "harness configuration".to_string(),
        key: configuration_key(&image),
        level,
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
fn read_text(host: &HostPath) -> io::Result<Vec<u8>> {
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
        let mut text: Vec<u8> = Vec::new();
        for value in system {
            if !text.is_empty() {
                if !text.ends_with(b"\n") {
                    text.push(b'\n');
                }
                text.push(b'\n');
            }
            text.extend_from_slice(value);
        }
        if !text.ends_with(b"\n") {
            text.push(b'\n');
        }
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

/// The OS layer's key, from the digest it is pinned to.
fn os_key(sha256: &str) -> String {
    let mut hash = Sha256Context::new();
    hash.update(b"OS minimal\0");
    hash.update(sha256.as_bytes());
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// The harness configuration's key. The image is a function of everything it
/// was rendered from, so its hash keys it.
fn configuration_key(image: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(b"harness configuration\0");
    hash.update(image);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}
