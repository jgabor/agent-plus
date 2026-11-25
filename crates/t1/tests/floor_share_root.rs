//! The floor: a share can't reach outside its root.
//!
//! A real guest mounts an `ro` share through virtio-fs, served by a real
//! share server, and tries to reach a host file beside the share: through
//! symlinks inside it, relative and absolute, through `..`, and by writing.
//! It reads every file it can find under the share, and none of it is the
//! file beside the share. The guest runs through the API `ap boot` uses, in
//! its own process, because the process that runs a guest confines itself
//! for the rest of its life. The share server's own rules against a guest
//! that sends FUSE requests its kernel never would are the share crate's
//! tests.

use ap::boot::{self, Guest, Report, Shares};
use ap::fetch;
use ap::store::Store;
use guest::protocol::ToGuest;
use share::{Mode, Root};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::Duration;
use t1::{guest_binary, kernel};
use vmm::initramfs::Entry;

const OWNER_TEST: &str = "share_owner_process";
const BASE_VARIABLE: &str = "AP_FLOOR_SHARE_BASE";
const AP_VARIABLE: &str = "AP_FLOOR_SHARE_AP";
const KERNEL_VARIABLE: &str = "AP_FLOOR_SHARE_KERNEL";
const PROBE_VARIABLE: &str = "AP_FLOOR_SHARE_PROBE";
const TIMEOUT: Duration = Duration::from_secs(30);
const CANARY: &str = "a host file beside the share";
const ENOENT: &str = "error 2";
const EROFS: &str = "error 30";

/// The share's root, and a host file beside it, in a directory named per
/// process, because another run of this suite may share the target
/// directory.
fn tree() -> PathBuf {
    let base =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("floor-share-root-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("root");
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::create_dir_all(base.join("outside")).unwrap();
    let secret = base.join("outside/secret");
    fs::write(&secret, CANARY).unwrap();
    fs::write(root.join("hello.txt"), "inside").unwrap();
    fs::write(root.join("dir/nested.txt"), "nested").unwrap();
    symlink("hello.txt", root.join("link")).unwrap();
    symlink("../outside/secret", root.join("escape")).unwrap();
    symlink(&secret, root.join("absolute")).unwrap();
    symlink("../outside", root.join("dirlink")).unwrap();
    base
}

/// Starts the share server and boots the guest, which runs the probe, and
/// logs what the probe printed and how the machine ended. It does nothing in
/// a normal test run.
#[test]
fn share_owner_process() {
    let Some(base) = env::var_os(BASE_VARIABLE) else {
        return;
    };
    let base = PathBuf::from(base);
    let ap = PathBuf::from(env::var_os(AP_VARIABLE).unwrap());
    let kernel = fs::read(env::var_os(KERNEL_VARIABLE).unwrap()).unwrap();
    let probe = fs::read(env::var_os(PROBE_VARIABLE).unwrap()).unwrap();
    let init = fs::read(&ap).unwrap();
    // Opened before the filter, which allows writes to what is already open.
    let mut log = File::create(base.join("log")).unwrap();
    // The VMM's end of the socket closes with the machine, and the share
    // server ends on its own.
    let root = File::open(base.join("root")).unwrap();
    let servers = share::start(&ap, &[Root::new(root.as_fd(), Mode::ReadOnly)], &[], None).unwrap();
    drop(root);
    let files = [Entry::File {
        path: "share-probe",
        mode: 0o755,
        contents: &probe,
    }];
    let guest = Guest {
        kernel: (&kernel).into(),
        init: (&init).into(),
        files: &files,
        layers: Vec::new(),
        request: Some(ToGuest::Run {
            argv: vec![b"/share-probe".to_vec()],
        }),
        timeout: TIMEOUT,
        confine: true,
        input: None,
    };
    let shares = Shares {
        sockets: servers
            .started
            .into_iter()
            .map(|server| server.socket)
            .collect(),
        serving: servers.serving,
    };
    let result = boot::boot_sharing(guest, Some(shares), |report| match report {
        Report::Stdout(bytes) | Report::Stderr(bytes) => log.write_all(bytes).unwrap(),
        Report::Ready(_) | Report::Exec(..) | Report::ExecTook(_) => {}
    });
    let result = result.map_err(|error| error.to_string());
    log.write_all(format!("booted: {result:?}\n").as_bytes())
        .unwrap();
}

/// Boots the probe with the share, and returns what it printed.
fn probe(base: &Path) -> String {
    let exit = Command::new(env::current_exe().unwrap())
        .args([OWNER_TEST, "--exact", "--test-threads=1"])
        .env(BASE_VARIABLE, base)
        .env(AP_VARIABLE, guest_binary::ap())
        .env(KERNEL_VARIABLE, kernel::vmlinux())
        .env(PROBE_VARIABLE, guest_binary::build("share-probe"))
        .stdout(Stdio::null())
        .status()
        .unwrap();
    let log = fs::read_to_string(base.join("log")).unwrap_or_default();
    assert!(exit.success(), "the owner process failed: {exit:?}\n{log}");
    log
}

/// The result the probe printed for `name`.
fn result<'a>(log: &'a str, name: &str) -> &'a str {
    log.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
        .unwrap_or_else(|| panic!("the probe printed nothing for {name}:\n{log}"))
}

// spec(SHARE): The agent sees only what is under the share's root.
// spec(SHARE): A symlink inside the share appears to the guest as a symlink and is resolved inside the guest, never on your host, so it can't reach host files outside the share.
#[test]
fn floor_share_root() {
    let base = tree();
    let log = probe(&base);
    println!("{log}");
    let secret = base.join("outside/secret");
    assert_eq!(result(&log, "booted"), "Ok(Some(0))", "{log}");
    assert_eq!(result(&log, "mount"), "ok ");
    assert_eq!(result(&log, "hello.txt"), "ok inside");
    assert_eq!(result(&log, "dir/nested.txt"), "ok nested");
    assert_eq!(result(&log, "readlink link"), "ok hello.txt");
    assert_eq!(result(&log, "via link"), "ok inside");
    assert_eq!(result(&log, "readlink escape"), "ok ../outside/secret");
    for path in ["via escape", "via absolute", "via dirlink", "via dotdot"] {
        assert_eq!(result(&log, path), ENOENT, "{path}");
    }
    for write in ["create", "change", "mkdir", "remove", "rename"] {
        assert_eq!(result(&log, write), EROFS, "{write}");
    }
    let walked = log.lines().filter(|line| line.starts_with("walk ")).count();
    assert_eq!(walked, 6, "{log}");
    assert!(!log.contains(CANARY), "{log}");
    // Nothing the guest did changed the host.
    assert_eq!(
        fs::read_to_string(base.join("root/hello.txt")).unwrap(),
        "inside"
    );
    assert_eq!(fs::read_to_string(&secret).unwrap(), CANARY);
    assert!(!base.join("root/new.txt").exists());
    let _ = fs::remove_dir_all(&base);
}

/// The children of every thread of `pid`.
fn children(pid: u32) -> Vec<String> {
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn a_machine_with_no_share_starts_no_share_server() {
    let home =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("floor-share-none-{}", process::id()));
    let _ = fs::remove_dir_all(&home);
    let store = Store::new(home.join(".local/share/ap"));
    store
        .put(
            fetch::kernel().sha256,
            File::open(kernel::vmlinux()).unwrap(),
        )
        .unwrap();
    let mut ap = Command::new(guest_binary::ap())
        .args(["boot", "--timeout", "30", "--", "/init", "--version"])
        .env_clear()
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut started = Vec::new();
    let exit = loop {
        started.extend(children(ap.id()));
        if let Some(exit) = ap.try_wait().unwrap() {
            break exit;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = fs::remove_dir_all(&home);
    assert!(exit.success(), "{exit:?}");
    assert_eq!(started, Vec::<String>::new());
}
