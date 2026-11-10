//! What an `ADD` copies into its layer, by SPEC.md's `### ADD`. The layer's
//! image is read back with a small EROFS reader. The `erofs` crate's own
//! tests check the format against the kernel's documentation.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentfile::{Add, GuestPath};
use ap::add::Layer;
use ap::paths::HostPath;

/// A test's own directory, holding a build context and a home directory.
struct Scratch {
    root: PathBuf,
    context: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("add")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let context = root.join("context");
        let home = root.join("home");
        fs::create_dir_all(&context).unwrap();
        fs::create_dir_all(&home).unwrap();
        Scratch {
            root,
            context,
            home,
        }
    }

    fn host(&self, source: &str) -> HostPath {
        HostPath::resolve(source, &self.context, &self.home).unwrap()
    }

    /// Reads `ADD <source> <guest>`.
    fn read(&self, source: &str, guest: &str) -> Layer {
        Layer::read(&add(source, Some(guest)), &self.host(source), guest).unwrap()
    }
}

fn add(source: &str, destination: Option<&str>) -> Add {
    Add {
        line: 1,
        source: source.to_string(),
        destination: destination.map(str::to_string),
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mkfifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the name is a valid C string that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
}

/// What the image holds at one path.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir(u16),
    File(u16, Vec<u8>),
    Symlink(Vec<u8>),
}

fn file(mode: u16, text: &str) -> Entry {
    Entry::File(mode, text.as_bytes().to_vec())
}

fn link(target: &str) -> Entry {
    Entry::Symlink(target.as_bytes().to_vec())
}

/// Every path in the image, the root as `/`.
fn contents(image: &[u8]) -> BTreeMap<String, Entry> {
    let sb = &image[1024..];
    assert_eq!(u32_at(sb, 0), 0xE0F5_E1E2, "magic");
    let reader = Reader {
        image,
        block: 1 << sb[12],
        meta: u32_at(sb, 40) as usize * (1 << sb[12]),
    };
    let mut found = BTreeMap::new();
    reader.inode(u64::from(u16_at(sb, 14)), "/".to_string(), &mut found);
    found
}

/// The paths below `/home/agent`.
fn under_home(image: &[u8]) -> BTreeMap<String, Entry> {
    contents(image)
        .into_iter()
        .filter_map(|(path, entry)| {
            let rest = path.strip_prefix("/home/agent/")?;
            Some((rest.to_string(), entry))
        })
        .collect()
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct Reader<'a> {
    image: &'a [u8],
    block: usize,
    meta: usize,
}

impl Reader<'_> {
    fn inode(&self, nid: u64, path: String, found: &mut BTreeMap<String, Entry>) {
        let at = self.meta + nid as usize * 32;
        let inode = &self.image[at..at + 32];
        let mode = u16_at(inode, 4);
        let size = u32_at(inode, 8) as usize;
        let start = u32_at(inode, 16) as usize * self.block;
        let data = if size == 0 {
            &[][..]
        } else {
            &self.image[start..start + size]
        };
        let perm = mode & 0o7777;
        match mode & 0o170000 {
            0o100000 => {
                found.insert(path, Entry::File(perm, data.to_vec()));
            }
            0o120000 => {
                found.insert(path, Entry::Symlink(data.to_vec()));
            }
            0o040000 => {
                let prefix = if path == "/" {
                    String::new()
                } else {
                    path.clone()
                };
                found.insert(path, Entry::Dir(perm));
                for (name, child) in self.dir(data) {
                    self.inode(child, format!("{prefix}/{name}"), found);
                }
            }
            other => panic!("unexpected file type {other:o}"),
        }
    }

    /// The entries of a directory, without `.` and `..`.
    fn dir(&self, data: &[u8]) -> Vec<(String, u64)> {
        let mut entries = Vec::new();
        for block in data.chunks(self.block) {
            let count = usize::from(u16_at(block, 8)) / 12;
            for k in 0..count {
                let start = usize::from(u16_at(block, k * 12 + 8));
                let end = if k + 1 < count {
                    usize::from(u16_at(block, (k + 1) * 12 + 8))
                } else {
                    block[start..]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(block.len(), |p| start + p)
                };
                let name = String::from_utf8(block[start..end].to_vec()).unwrap();
                if name != "." && name != ".." {
                    entries.push((name, u64_at(block, k * 12)));
                }
            }
        }
        entries
    }
}

// spec(ADD): `ADD` copies a host file or directory into the image at build time.
#[test]
fn a_file_and_a_directory_are_copied_with_their_modes() {
    let s = Scratch::new("copies");
    write(&s.context.join("notes.txt"), "notes");
    chmod(&s.context.join("notes.txt"), 0o600);
    write(&s.context.join("tools/run.sh"), "#!/bin/sh\n");
    chmod(&s.context.join("tools/run.sh"), 0o755);
    write(&s.context.join("tools/lib/a.txt"), "a");
    chmod(&s.context.join("tools/lib"), 0o750);

    let notes = s.read("./notes.txt", "/opt/notes.txt").image().unwrap();
    let all = contents(&notes);
    assert_eq!(all["/opt/notes.txt"], file(0o600, "notes"));
    assert_eq!(all["/opt"], Entry::Dir(0o755));
    assert_eq!(all["/"], Entry::Dir(0o755));
    assert_eq!(all.len(), 3, "{all:?}");

    let tools = s.read("./tools", "/opt/tools").image().unwrap();
    let all = contents(&tools);
    assert_eq!(all["/opt/tools/run.sh"], file(0o755, "#!/bin/sh\n"));
    assert_eq!(all["/opt/tools/lib"], Entry::Dir(0o750));
    assert_eq!(all["/opt/tools/lib/a.txt"], file(0o644, "a"));
    assert_eq!(all.len(), 6, "{all:?}");
}

#[test]
fn setuid_setgid_and_sticky_bits_are_dropped() {
    let s = Scratch::new("setuid");
    write(&s.context.join("tool"), "x");
    chmod(&s.context.join("tool"), 0o6755);
    fs::create_dir(s.context.join("shared")).unwrap();
    chmod(&s.context.join("shared"), 0o1777);
    let file_image = s.read("./tool", "/opt/tool").image().unwrap();
    assert_eq!(contents(&file_image)["/opt/tool"], file(0o755, "x"));
    let dir_image = s.read("./shared", "/opt/shared").image().unwrap();
    assert_eq!(contents(&dir_image)["/opt/shared"], Entry::Dir(0o777));
}

// spec(ADD): Without `<dest>`, the [mirror rule](#paths) decides where the content lands.
#[test]
fn the_content_lands_at_the_guest_path_the_parser_resolved() {
    let s = Scratch::new("mirror");
    write(&s.context.join("prompts/review.md"), "review");
    let instructions = agentfile::parse("AGENT claude\nADD ./prompts\n").unwrap();
    let guest = GuestPath::read(&instructions, "context").unwrap();
    assert_eq!(guest[0].path, "/home/agent/prompts");
    let layer = Layer::read(
        &add("./prompts", None),
        &s.host("./prompts"),
        &guest[0].path,
    );
    let image = layer.unwrap().image().unwrap();
    assert_eq!(
        under_home(&image),
        BTreeMap::from([
            ("prompts".to_string(), Entry::Dir(0o755)),
            ("prompts/review.md".to_string(), file(0o644, "review")),
        ])
    );
}

// spec(ADD): The content is part of the image.
#[test]
fn the_image_needs_nothing_from_the_host_once_read() {
    let s = Scratch::new("part");
    write(&s.context.join("data/a.txt"), "a");
    let layer = s.read("./data", "/home/agent/data");
    fs::remove_dir_all(&s.context).unwrap();
    let image = layer.image().unwrap();
    assert_eq!(under_home(&image)["data/a.txt"], file(0o644, "a"));
}

// spec(ADD): `<src>` itself is canonicalized like any host path, so a symlink you name is followed.
#[test]
fn a_symlink_you_name_is_followed() {
    let s = Scratch::new("named-link");
    write(&s.context.join("real/a.txt"), "a");
    write(&s.context.join("real.txt"), "real");
    symlink("real", s.context.join("dir-link")).unwrap();
    symlink("real.txt", s.context.join("file-link")).unwrap();

    let dir = s.read("./dir-link", "/home/agent/d").image().unwrap();
    let dir = under_home(&dir);
    assert_eq!(dir["d"], Entry::Dir(0o755));
    assert_eq!(dir["d/a.txt"], file(0o644, "a"));

    let one = s.read("./file-link", "/home/agent/f").image().unwrap();
    assert_eq!(under_home(&one)["f"], file(0o644, "real"));
}

// spec(ADD): Inside a directory, symlinks are copied as symlinks and never followed.
#[test]
fn symlinks_inside_a_directory_are_copied_as_symlinks() {
    let s = Scratch::new("inner-links");
    write(&s.root.join("secret.txt"), "secret");
    write(&s.root.join("outside/b.txt"), "b");
    write(&s.context.join("data/a.txt"), "a");
    symlink("a.txt", s.context.join("data/to-a")).unwrap();
    symlink(s.root.join("secret.txt"), s.context.join("data/to-secret")).unwrap();
    symlink(s.root.join("outside"), s.context.join("data/to-dir")).unwrap();
    symlink("missing", s.context.join("data/dangling")).unwrap();

    let image = s.read("./data", "/home/agent/data").image().unwrap();
    let mut want = BTreeMap::from([
        ("data".to_string(), Entry::Dir(0o755)),
        ("data/a.txt".to_string(), file(0o644, "a")),
        ("data/to-a".to_string(), link("a.txt")),
        ("data/dangling".to_string(), link("missing")),
    ]);
    let secret = s.root.join("secret.txt");
    let outside = s.root.join("outside");
    want.insert("data/to-secret".into(), link(secret.to_str().unwrap()));
    want.insert("data/to-dir".into(), link(outside.to_str().unwrap()));
    assert_eq!(under_home(&image), want);
}

// spec(ADD): Special files (devices, sockets, FIFOs) are skipped with a warning.
#[test]
fn special_files_are_skipped_with_a_warning() {
    let s = Scratch::new("special");
    write(&s.context.join("data/a.txt"), "a");
    mkfifo(&s.context.join("data/pipe"));
    let _socket = UnixListener::bind(s.context.join("data/sock")).unwrap();

    let layer = s.read("./data", "/home/agent/data");
    let mut warnings = layer.warnings.clone();
    warnings.sort();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("data/pipe"), "{warnings:?}");
    assert!(warnings[1].contains("data/sock"), "{warnings:?}");
    let image = layer.image().unwrap();
    assert_eq!(
        under_home(&image).into_keys().collect::<Vec<_>>(),
        ["data", "data/a.txt"]
    );

    // Named on its own, a FIFO is skipped too, and reading it doesn't hang.
    let named = s.read("./data/pipe", "/home/agent/pipe");
    assert_eq!(named.warnings.len(), 1, "{:?}", named.warnings);
    let image = named.image().unwrap();
    assert!(under_home(&image).is_empty());
}

// spec(ADD): When `<src>` is a directory, files ignored by git (according to the `.gitignore` files inside it) are skipped.
#[test]
fn files_git_ignores_are_skipped_in_a_directory() {
    let s = Scratch::new("ignored");
    write(&s.context.join("data/.gitignore"), "*.log\nbuild/\n");
    write(&s.context.join("data/keep.txt"), "keep");
    write(&s.context.join("data/debug.log"), "log");
    write(&s.context.join("data/build/out.bin"), "out");
    write(&s.context.join("data/sub/.gitignore"), "local.txt\n");
    write(&s.context.join("data/sub/local.txt"), "local");
    write(&s.context.join("data/sub/other.log"), "log");
    write(&s.context.join("data/sub/kept.txt"), "kept");
    symlink("keep.txt", s.context.join("data/link.log")).unwrap();
    // Only the `.gitignore` files inside the directory count.
    write(&s.context.join(".gitignore"), "keep.txt\n");

    let image = s.read("./data", "/home/agent/data").image().unwrap();
    assert_eq!(
        under_home(&image).into_keys().collect::<Vec<_>>(),
        [
            "data",
            "data/.gitignore",
            "data/keep.txt",
            "data/sub",
            "data/sub/.gitignore",
            "data/sub/kept.txt",
        ]
    );
}

// spec(ADD): A file named explicitly is always copied.
#[test]
fn a_file_named_explicitly_is_always_copied() {
    let s = Scratch::new("explicit");
    write(&s.context.join("data/.gitignore"), "*.log\n");
    write(&s.context.join("data/debug.log"), "log");
    write(&s.context.join(".gitignore"), "data/\n");
    let image = s
        .read("./data/debug.log", "/home/agent/debug.log")
        .image()
        .unwrap();
    assert_eq!(under_home(&image)["debug.log"], file(0o644, "log"));
}

#[test]
fn the_key_covers_the_instruction_its_arguments_and_every_file() {
    let s = Scratch::new("key");
    write(&s.context.join("data/a.txt"), "aaaa");
    write(&s.context.join("data/sub/b.txt"), "bbbb");
    let key = |source: &str, destination: Option<&str>, guest: &str| {
        Layer::read(&add(source, destination), &s.host(source), guest)
            .unwrap()
            .key
    };
    let first = key("./data", Some("/opt/data"), "/opt/data");
    assert_eq!(first.len(), 64);
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(key("./data", Some("/opt/data"), "/opt/data"), first);

    // Touching a file without changing it keeps the key.
    let later = SystemTime::now() + Duration::from_secs(3600);
    let b = fs::File::options()
        .write(true)
        .open(s.context.join("data/sub/b.txt"))
        .unwrap();
    b.set_modified(later).unwrap();
    drop(b);
    assert_eq!(key("./data", Some("/opt/data"), "/opt/data"), first);

    // The arguments as written count, even when they resolve the same.
    assert_ne!(key("data", Some("/opt/data"), "/opt/data"), first);
    assert_ne!(key("./data", None, "/opt/data"), first);
    assert_ne!(key("./data", Some("/opt/data"), "/srv/data"), first);

    // One byte of one file changes it.
    write(&s.context.join("data/sub/b.txt"), "bbbc");
    let changed = key("./data", Some("/opt/data"), "/opt/data");
    assert_ne!(changed, first);

    // So does a mode, a name, or a symlink target.
    chmod(&s.context.join("data/sub/b.txt"), 0o600);
    let moded = key("./data", Some("/opt/data"), "/opt/data");
    assert_ne!(moded, changed);
    fs::rename(
        s.context.join("data/sub/b.txt"),
        s.context.join("data/sub/c.txt"),
    )
    .unwrap();
    let renamed = key("./data", Some("/opt/data"), "/opt/data");
    assert_ne!(renamed, moded);
    symlink("a.txt", s.context.join("data/l")).unwrap();
    let linked = key("./data", Some("/opt/data"), "/opt/data");
    assert_ne!(linked, renamed);
    fs::remove_file(s.context.join("data/l")).unwrap();
    symlink("b.txt", s.context.join("data/l")).unwrap();
    assert_ne!(key("./data", Some("/opt/data"), "/opt/data"), linked);
}

#[test]
fn the_same_input_gives_the_same_image() {
    let s = Scratch::new("same");
    write(&s.context.join("data/z.txt"), "z");
    write(&s.context.join("data/a/b.txt"), "b");
    symlink("z.txt", s.context.join("data/m")).unwrap();
    let first = s.read("./data", "/home/agent/data").image().unwrap();
    let later = SystemTime::now() + Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(s.context.join("data/z.txt"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    // A copy elsewhere, made in a different order, gives the same bytes.
    let other = Scratch::new("same-copy");
    symlink("z.txt", other.context.join("data-m")).unwrap();
    write(&other.context.join("data/a/b.txt"), "b");
    write(&other.context.join("data/z.txt"), "z");
    fs::rename(other.context.join("data-m"), other.context.join("data/m")).unwrap();
    let second = other.read("./data", "/home/agent/data").image().unwrap();
    assert_eq!(first, second);
    assert_eq!(s.read("./data", "/home/agent/data").image().unwrap(), first);
}

#[test]
fn files_are_read_through_the_handle_the_path_module_opened() {
    let s = Scratch::new("swapped");
    write(&s.context.join("data/a.txt"), "a");
    write(&s.root.join("elsewhere/secret.txt"), "secret");
    let host = s.host("./data");
    // Swapped for a symlink after the check.
    fs::rename(s.context.join("data"), s.context.join("old")).unwrap();
    symlink(s.root.join("elsewhere"), s.context.join("data")).unwrap();
    let Err(error) = Layer::read(&add("./data", None), &host, "/home/agent/data") else {
        panic!("a swapped symlink was followed");
    };
    assert!(
        error.to_string().contains("changed after it was checked"),
        "{error}"
    );
}
