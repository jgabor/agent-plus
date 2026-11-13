//! `ap build [--strict] [--replace] [--approve=file] [-t name] [dir]`, from
//! SPEC.md's Build section. It checks the Agentfile against the host, asks
//! you to approve its grants as `ap::approval` says, turns each `ADD` and the
//! harness configuration into a layer, and records the agent in the store. A
//! layer whose key an agent already recorded is a hit, and isn't built again.
//!
//! The agent's bottom layer is the pinned `OS minimal` layer, downloaded only
//! if the store doesn't have it. The harness layer comes from the file that
//! `Agentfile.lock` pins. After approval, a lock that doesn't resolve `AGENT`
//! resolves again, the file is downloaded into the download cache unless it
//! holds it, and a changed lock is written, as `ap::lock::settle` says. The
//! other tiers, and the instructions that need any other fetch, fail.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Cursor, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use agentfile::{
    Error, Files, GuestPath, Harness, Instruction, Network, Os, Policy, Secrets, Settings, Sources,
    Text,
};
use ap::add;
use ap::fetch::{self, Client, Pin};
use ap::lock::{self, Lock, Locked};
use ap::paths::HostPath;
use graviola::hashing::sha2::Sha256Context;
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

/// The instructions that need a fetch or a harness, which `ap build` can't
/// build yet.
const NOT_BUILT: [&str; 5] = ["REPO", "PLUGIN", "SKILL", "MCP", "SEAL"];

/// Where the prompt goes in the harness configuration layer. The harness gets
/// it at run time, so it has no file of its own in the harness's directory.
const PROMPT: &str = "/etc/ap/prompt";

/// The zstd level the store keeps the OS and harness layers at, which
/// `COMPRESSION` doesn't change, so that every agent shares them.
const BASE_LEVEL: i8 = 3;

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
    /// The locked harness, from its file in the download cache.
    Harness(Locked, PathBuf),
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
    let mut lock = read_lock(&path.with_extension("lock"), &context)?;
    let cache = lock::cache(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        Some(home.as_os_str()),
    );
    let mut resolved = Resolved {
        context: context.clone(),
        home: home.clone(),
        paths: HashMap::new(),
    };
    let cached = Cached {
        lock: &lock,
        cache: cache.as_deref(),
    };
    let (planned, warnings) =
        plan(&instructions, &mut resolved, &cached, dir_name).map_err(report)?;
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

    let agent_line = agent_line(&instructions);
    let agent = Settings::read(&instructions).map_err(report)?.agent;
    let cache = cache.ok_or("ap: HOME isn't an absolute path, so there's no download cache")?;
    let changed = lock::settle(
        &context.join("Agentfile.lock"),
        &mut lock,
        &agent,
        &Client::new(),
        lock::base(agent.harness),
        &cache,
        |locked| {
            stored(&agents, &layers, &harness_key(locked), BASE_LEVEL)
                .is_ok_and(|found| found.is_some())
        },
    )
    .map_err(|error| format!("{shown}:{agent_line}: {error}"))?;
    let cached = Cached {
        lock: &lock,
        cache: Some(&cache),
    };

    // A changed lock plans the layers again, with the harness it locks.
    let mut planned = (!changed).then_some(planned);
    let mut use_cache = true;
    let lines = loop {
        // The first try uses the planned layers. A retry plans them again.
        let current = match planned.take() {
            Some(current) => current,
            None => {
                plan(&instructions, &mut resolved, &cached, dir_name)
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
        if use_cache && let Some(digest) = stored(agents, layers, &key, level).map_err(io)? {
            made.push((key, label, Made::Hit(digest)));
            continue;
        }
        let image = match content {
            Content::Add(add) => add.image(),
            Content::Image(image) => Ok(image),
            Content::Harness(locked, path) => harness(&locked, &path),
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

/// The digest of the layer an agent recorded with `key`, if the store keeps
/// it at `level`. Agents at other levels can share a layer, and the last to
/// put it set its level.
fn stored(
    agents: &::store::Agents,
    layers: &::store::Layers,
    key: &str,
    level: i8,
) -> io::Result<Option<String>> {
    match agents.layer(key)? {
        Some(digest) if layers.level(&digest)? == Some(level) => Ok(Some(digest)),
        _ => Ok(None),
    }
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

/// What the lockfile pins, and the download cache that holds it.
struct Cached<'a> {
    lock: &'a Lock,
    /// `None` when there is no place for the cache.
    cache: Option<&'a Path>,
}

/// Reads the lockfile at `path`, shown as it is, through its canonical path
/// in `context`. A missing lockfile pins nothing.
fn read_lock(path: &Path, context: &Path) -> Result<Lock, String> {
    let shown = path.display();
    let name = path.file_name().unwrap_or_default();
    let host = match HostPath::resolve(&name.to_string_lossy(), context, Path::new("/")) {
        Ok(host) => host,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Lock::default()),
        Err(error) => return Err(format!("ap: {shown}: {error}")),
    };
    let mut bytes = Vec::new();
    host.open()
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| format!("ap: {shown}: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("ap: {shown} isn't UTF-8 text"))?;
    Lock::read(&text).map_err(|error| format!("{shown}:{}: {}", error.line, error.message))
}

/// Checks the Agentfile against the host, and plans its layers: the OS first,
/// then the locked harness, one for each `ADD`, in line order, and the harness
/// configuration last. It returns every error it finds, or the layers and the
/// warnings.
fn plan(
    instructions: &[Instruction],
    resolved: &mut Resolved,
    cached: &Cached,
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
        level: BASE_LEVEL,
        content: Content::Fetch(fetch::os()),
    });
    let agent_line = agent_line(instructions);
    let locked = (cached.lock.harness.as_ref()).filter(|locked| locked.resolves(&settings.agent));
    match (locked, cached.cache) {
        (_, None) => errors.push(fail(
            agent_line,
            "HOME isn't an absolute path, so there's no download cache to take the harness from"
                .to_string(),
        )),
        (Some(locked), Some(cache)) => {
            planned.push(Planned {
                line: agent_line,
                label: locked.label(),
                key: harness_key(locked),
                level: BASE_LEVEL,
                content: Content::Harness(locked.clone(), cache.join(&locked.sha256)),
            });
        }
        // `ap build` resolves the harness after approval, and plans again.
        (None, Some(_)) => {}
    }
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

/// The line of the `AGENT` instruction, or 1 if there is none.
fn agent_line(instructions: &[Instruction]) -> usize {
    instructions
        .iter()
        .find(|instruction| instruction.name == "AGENT")
        .map_or(1, |instruction| instruction.line)
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
        insert(&mut root, file, text_file(text));
    }
    if let Some(prompt) = prompt {
        insert(&mut root, PROMPT, text_file(prompt.to_vec()));
    }
    let mut image = Vec::new();
    erofs::write(root, &mut image).map_err(|error| match error {
        erofs::Error::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
    })?;
    Ok(image)
}

/// A file that anyone can read, holding `data`.
fn text_file(data: Vec<u8>) -> erofs::Node {
    erofs::Node::File(erofs::File {
        mode: 0o644,
        size: data.len() as u64,
        content: Box::new(Cursor::new(data)),
    })
}

/// Puts `node` at the absolute `path` in `root`, with the directories above
/// it.
fn insert(root: &mut erofs::Dir, path: &str, node: erofs::Node) {
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
    dir.entries.insert(last.as_bytes().to_vec(), node);
}

/// Where the harness layer puts the harness, which its adapter starts.
pub fn harness_path(harness: Harness) -> String {
    format!("/usr/local/bin/{}", lock::name(harness))
}

/// The harness layer, as an uncompressed EROFS image: the binary in the
/// cached file at `path`, as an executable at [`harness_path`]. Claude
/// Code's file is the binary, and Codex's holds it in one zstd frame. The
/// file is hashed as it is read, and a digest that isn't the locked one is
/// an error.
fn harness(locked: &Locked, path: &Path) -> io::Result<Vec<u8>> {
    let shown = path.display();
    // Nonblocking, so that a FIFO fails the check below instead of waiting
    // for a writer. A regular file reads the same either way.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => io::Error::new(
                error.kind(),
                format!(
                    "`{}` isn't in the download cache at {shown}",
                    locked.label()
                ),
            ),
            _ => io::Error::new(error.kind(), format!("{shown}: {error}")),
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{shown} isn't a file"),
        ));
    }
    let hash = Rc::new(RefCell::new(Sha256Context::new()));
    let mut root = erofs::Dir {
        mode: 0o755,
        entries: BTreeMap::new(),
    };
    let content = Hashing {
        file,
        hash: Rc::clone(&hash),
    };
    let (size, content): (u64, Box<dyn Read>) = match locked.harness {
        Harness::Claude => (metadata.len(), Box::new(content)),
        Harness::Codex => {
            // The frame is read whole and checked before it is unpacked, so
            // only the locked file is. Unpacking it once first gives the
            // binary's size without holding the binary.
            let mut frame = Vec::new();
            { content }.read_to_end(&mut frame)?;
            check_digest(locked, path, &hash)?;
            let unpacking = |error: io::Error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{shown}: unpacking `{}`: {error}", locked.label()),
                )
            };
            let mut counting = unzstd(&frame[..]).map_err(unpacking)?;
            let size = io::copy(&mut counting, &mut io::sink()).map_err(unpacking)?;
            (
                size,
                Box::new(unzstd(Cursor::new(frame)).map_err(unpacking)?),
            )
        }
    };
    insert(
        &mut root,
        &harness_path(locked.harness),
        erofs::Node::File(erofs::File {
            mode: 0o755,
            size,
            content,
        }),
    );
    let mut image = Vec::new();
    erofs::write(root, &mut image).map_err(|error| match error {
        erofs::Error::Io(error) => io::Error::new(error.kind(), format!("{shown}: {error}")),
        error => io::Error::new(io::ErrorKind::InvalidData, format!("{shown}: {error}")),
    })?;
    if locked.harness == Harness::Claude {
        check_digest(locked, path, &hash)?;
    }
    Ok(image)
}

/// Checks that the file at `path`, read whole into `hash`, has the digest
/// `locked` names.
fn check_digest(locked: &Locked, path: &Path, hash: &RefCell<Sha256Context>) -> io::Result<()> {
    let actual: String = hash
        .replace(Sha256Context::new())
        .finish()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != locked.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} has digest {actual}, not the {} that Agentfile.lock locks",
                path.display(),
                locked.sha256
            ),
        ));
    }
    Ok(())
}

/// A reader of the binary in the zstd frame that `frame` reads.
fn unzstd<R: Read>(frame: R) -> io::Result<StreamingDecoder<R, FrameDecoder>> {
    StreamingDecoder::new(frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// A file that adds what is read from it to a hash.
struct Hashing {
    file: fs::File,
    hash: Rc<RefCell<Sha256Context>>,
}

impl Read for Hashing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read(buf)?;
        self.hash.borrow_mut().update(&buf[..read]);
        Ok(read)
    }
}

/// The harness layer's key. Its image is a function of the harness and the
/// file's digest.
fn harness_key(locked: &Locked) -> String {
    let mut hash = Sha256Context::new();
    hash.update(b"harness\0");
    hash.update(lock::name(locked.harness).as_bytes());
    hash.update(b"\0");
    hash.update(locked.sha256.as_bytes());
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
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
