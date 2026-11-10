//! Host paths, from the host side of SPEC.md's Paths section: how a path from
//! an Agentfile resolves, when it counts as outside the build context, and
//! how it is opened after the check. The guest side is the `agentfile`
//! crate's, and its tests check it.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

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
            .join("paths")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let context = root.join("context");
        let home = root.join("home");
        fs::create_dir_all(&context).unwrap();
        fs::create_dir_all(&home).unwrap();
        // Canonical, so tests can compare with what `resolve` returns.
        let root = root.canonicalize().unwrap();
        Scratch {
            context: root.join("context"),
            home: root.join("home"),
            root,
        }
    }

    fn resolve(&self, text: &str) -> std::io::Result<HostPath> {
        HostPath::resolve(text, &self.context, &self.home)
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn contents(host: &HostPath) -> String {
    let mut text = String::new();
    host.open().unwrap().read_to_string(&mut text).unwrap();
    text
}

// spec(Paths): A relative path resolves against the directory that contains the Agentfile.
#[test]
fn a_relative_path_resolves_against_the_build_context() {
    let s = Scratch::new("relative");
    write(&s.context.join("data/notes.txt"), "notes");
    let host = s.resolve("./data/notes.txt").unwrap();
    assert_eq!(host.path(), s.context.join("data/notes.txt"));
    assert!(!host.outside());
    assert_eq!(s.resolve("data/notes.txt").unwrap().path(), host.path());
    assert_eq!(s.resolve(".").unwrap().path(), s.context);
}

// spec(Paths): `~/` is your home directory on the host.
#[test]
fn a_tilde_path_resolves_under_home() {
    let s = Scratch::new("tilde");
    write(&s.home.join("notes/today.md"), "today");
    let host = s.resolve("~/notes/today.md").unwrap();
    assert_eq!(host.path(), s.home.join("notes/today.md"));
    assert!(host.outside(), "home is outside the build context");
    assert_eq!(contents(&host), "today");
}

// spec(Paths): Absolute paths are used as-is.
#[test]
fn an_absolute_path_is_used_as_is() {
    let s = Scratch::new("absolute");
    let file = s.root.join("elsewhere/output.log");
    write(&file, "log");
    let host = s.resolve(file.to_str().unwrap()).unwrap();
    assert_eq!(host.path(), file);
    assert!(host.outside());

    let inside = s.context.join("inside.txt");
    write(&inside, "in");
    let host = s.resolve(inside.to_str().unwrap()).unwrap();
    assert_eq!(host.path(), inside);
    assert!(!host.outside());
}

// spec(Paths): Before any check, host paths are canonicalized: `.` and `..` are resolved and symlinks are followed, including a symlink you name directly.
#[test]
fn dots_are_resolved_before_the_check() {
    let s = Scratch::new("dots");
    write(&s.context.join("a/file.txt"), "a");
    write(&s.root.join("shared/file.txt"), "shared");

    let host = s.resolve("./a/./b/../file.txt");
    // `a/b` doesn't exist, so `..` can't be resolved through it.
    assert!(host.is_err());
    fs::create_dir_all(s.context.join("a/b")).unwrap();
    let host = s.resolve("./a/./b/../file.txt").unwrap();
    assert_eq!(host.path(), s.context.join("a/file.txt"));
    assert!(!host.outside());

    let host = s.resolve("../shared/file.txt").unwrap();
    assert_eq!(host.path(), s.root.join("shared/file.txt"));
    assert!(host.outside(), "`..` leaves the build context");
}

// spec(Paths): Before any check, host paths are canonicalized: `.` and `..` are resolved and symlinks are followed, including a symlink you name directly.
#[test]
fn symlinks_are_followed_including_one_named_directly() {
    let s = Scratch::new("follow");
    write(&s.context.join("real/file.txt"), "real");
    symlink("real", s.context.join("link")).unwrap();
    symlink("real/file.txt", s.context.join("direct")).unwrap();

    let host = s.resolve("./link/file.txt").unwrap();
    assert_eq!(host.path(), s.context.join("real/file.txt"));
    assert_eq!(contents(&host), "real");

    let host = s.resolve("./direct").unwrap();
    assert_eq!(host.path(), s.context.join("real/file.txt"));
    assert_eq!(contents(&host), "real");
}

// spec(Paths): Before any check, host paths are canonicalized: `.` and `..` are resolved and symlinks are followed, including a symlink you name directly.
#[test]
fn the_build_context_is_canonicalized_too() {
    let s = Scratch::new("context-link");
    write(&s.context.join("file.txt"), "in");
    let alias = s.root.join("alias");
    symlink("context", &alias).unwrap();
    let host = HostPath::resolve("./file.txt", &alias, &s.home).unwrap();
    assert_eq!(host.path(), s.context.join("file.txt"));
    assert!(!host.outside());
}

// spec(Paths): A symlink inside the build context that points outside it counts as outside.
#[test]
fn a_symlink_that_points_outside_counts_as_outside() {
    let s = Scratch::new("escape");
    write(&s.root.join("secret/key"), "key");
    symlink(s.root.join("secret"), s.context.join("inner")).unwrap();
    symlink("../secret/key", s.context.join("key")).unwrap();

    let host = s.resolve("./inner/key").unwrap();
    assert_eq!(host.path(), s.root.join("secret/key"));
    assert!(host.outside());

    let host = s.resolve("./key").unwrap();
    assert_eq!(host.path(), s.root.join("secret/key"));
    assert!(host.outside());
}

#[test]
fn a_sibling_with_a_longer_name_is_outside() {
    let s = Scratch::new("prefix");
    write(&s.root.join("context-other/file.txt"), "x");
    let host = s.resolve("../context-other/file.txt").unwrap();
    assert!(host.outside(), "a shared name prefix isn't containment");
}

#[test]
fn a_missing_path_is_an_error() {
    let s = Scratch::new("missing");
    let error = s.resolve("./nothing").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

// spec(Paths): After the check, `ap` opens the canonical path it checked, refusing symlinks along the way, and reads or shares it only through that open handle.
#[test]
fn the_open_handle_is_the_checked_file() {
    let s = Scratch::new("open");
    write(&s.context.join("data/file.txt"), "checked");
    let host = s.resolve("./data/file.txt").unwrap();
    let mut file = host.open().unwrap();
    // A rename after the open doesn't change what the handle reads.
    fs::rename(s.context.join("data"), s.context.join("moved")).unwrap();
    write(&s.context.join("data/file.txt"), "swapped");
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "checked");

    let dir = s.resolve("./data").unwrap();
    assert!(dir.open().unwrap().metadata().unwrap().is_dir());
}

// spec(Paths): After the check, `ap` opens the canonical path it checked, refusing symlinks along the way, and reads or shares it only through that open handle.
// spec(Paths): If a symlink was swapped in after the check, the build or run fails instead of following it.
#[test]
fn a_directory_swapped_for_a_symlink_after_the_check_fails_the_open() {
    let s = Scratch::new("swap-directory");
    write(&s.context.join("data/file.txt"), "checked");
    write(&s.root.join("secret/file.txt"), "secret");
    let host = s.resolve("./data/file.txt").unwrap();
    assert!(!host.outside());

    fs::rename(s.context.join("data"), s.context.join("moved")).unwrap();
    symlink(s.root.join("secret"), s.context.join("data")).unwrap();
    assert!(host.open().is_err(), "the open must not follow the symlink");
}

// spec(Paths): If a symlink was swapped in after the check, the build or run fails instead of following it.
#[test]
fn a_file_swapped_for_a_symlink_after_the_check_fails_the_open() {
    let s = Scratch::new("swap-file");
    write(&s.context.join("file.txt"), "checked");
    write(&s.root.join("secret"), "secret");
    let host = s.resolve("./file.txt").unwrap();

    fs::remove_file(s.context.join("file.txt")).unwrap();
    symlink(s.root.join("secret"), s.context.join("file.txt")).unwrap();
    assert!(host.open().is_err(), "the open must not follow the symlink");

    let dir = s.resolve(".").unwrap();
    fs::rename(&s.context, s.root.join("old")).unwrap();
    symlink(s.root.join("old"), &s.context).unwrap();
    assert!(dir.open().is_err(), "nor a symlink named directly");
}

#[test]
fn a_tilde_path_with_repeated_slashes_stays_under_home() {
    let s = Scratch::new("tilde-slashes");
    write(&s.home.join("etc/notes"), "home");
    let host = s.resolve("~//etc/notes").unwrap();
    assert_eq!(host.path(), s.home.join("etc/notes"));
}

#[test]
fn a_directory_that_can_be_searched_but_not_listed_can_be_walked() {
    let s = Scratch::new("search-only");
    write(&s.context.join("locked/file.txt"), "inside");
    let locked = s.context.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o111)).unwrap();
    let opened = s.resolve("./locked/file.txt").map(|host| contents(&host));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(opened.unwrap(), "inside");
}

#[test]
fn a_file_swapped_for_a_fifo_after_the_check_does_not_hang_the_open() {
    let s = Scratch::new("swap-fifo");
    write(&s.context.join("file.txt"), "checked");
    let host = s.resolve("./file.txt").unwrap();
    fs::remove_file(s.context.join("file.txt")).unwrap();
    let path = std::ffi::CString::new(s.context.join("file.txt").as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(host.open().map(|_| ()));
    });
    let opened = receiver.recv_timeout(std::time::Duration::from_secs(5));
    assert!(
        opened.is_ok(),
        "the open must return, not wait for a writer"
    );
}
