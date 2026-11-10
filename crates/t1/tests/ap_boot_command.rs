//! `ap boot` itself: a static `ap`, run as a program on the host, boots the
//! pinned kernel with itself as the guest's init.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use ap::fetch;
use ap::store::Store;
use t1::{guest_binary, kernel};

/// The status `ap` exits with when its guest outlives the timeout.
const TIMED_OUT: i32 = 124;
/// The status `ap` exits with on a usage error.
const USAGE: i32 = 2;

/// Fetched once, because the helper's partial file is named per process and
/// these tests share one.
fn vmlinux() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(kernel::vmlinux)
}

/// A home of its own for one test, with the kernel already in its store, so
/// that nothing reads the owner's files or the network.
fn home(test: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&home);
    let store = Store::new(home.join(".local/share/ap"));
    let kernel = File::open(vmlinux()).unwrap();
    store.put(fetch::kernel().sha256, kernel).unwrap();
    home
}

/// Runs `ap boot` with these arguments.
fn ap_boot(test: &str, args: &[&str]) -> Output {
    let output = Command::new(guest_binary::ap())
        .arg("boot")
        .args(args)
        .env_clear()
        .env("HOME", home(test))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    fs::remove_dir_all(Path::new(env!("CARGO_TARGET_TMPDIR")).join(test)).unwrap();
    output
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn host_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap()
        .trim()
        .to_owned()
}

// spec(ap boot): `ap boot [--timeout N] [-- COMMAND...]` boots a bare machine with no agent.
#[test]
fn ap_boot_boots_a_machine_that_holds_no_agent() {
    let booted = ap_boot("bare", &["--timeout", "30"]);
    assert_eq!(booted.status.code(), Some(0), "{booted:?}");
    let release = text(&booted.stdout).trim();
    assert_eq!(release, kernel::release(), "{booted:?}");
    assert_ne!(release, host_release(), "the release is the host's");

    // The machine has `ap` and nothing else, so no harness can start.
    for agent in ["claude", "codex"] {
        let missing = ap_boot(agent, &["--timeout", "30", "--", agent]);
        assert_eq!(missing.status.code(), Some(127), "{missing:?}");
    }

    let stopped = ap_boot("stopped", &["--timeout", "0"]);
    assert_eq!(stopped.status.code(), Some(TIMED_OUT), "{stopped:?}");
}

// spec(ap boot): Without a command, it prints the machine's kernel release and powers off.
#[test]
fn ap_boot_without_a_command_prints_the_release_and_powers_off() {
    let output = ap_boot("release", &["--timeout", "30"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(text(&output.stdout), format!("{}\n", kernel::release()), "{output:?}");
    assert_eq!(text(&output.stderr), "", "{output:?}");
}

// spec(ap boot): With a command, it runs the command in the machine and exits with its status.
#[test]
fn ap_boot_with_a_command_runs_it_in_the_machine_and_exits_with_its_status() {
    let version = ap_boot("version", &["--timeout", "30", "--", "ap", "--version"]);
    assert_eq!(version.status.code(), Some(0), "{version:?}");
    let host = Command::new(guest_binary::ap()).arg("--version").output().unwrap();
    assert_eq!(version.stdout, host.stdout, "{version:?}");

    let bad_flag = ap_boot("bad-flag", &["--timeout", "30", "--", "ap", "--no-such-flag"]);
    assert_eq!(bad_flag.status.code(), Some(USAGE), "{bad_flag:?}");
    assert!(text(&bad_flag.stderr).contains("usage"), "{bad_flag:?}");

    // The host has /dev/kvm, and the machine doesn't, so this ran inside it.
    let nested = ap_boot("nested", &["--timeout", "30", "--", "ap", "boot"]);
    assert!(
        text(&nested.stderr).contains("can't open /dev/kvm"),
        "{nested:?}"
    );
}
