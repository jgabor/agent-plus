//! An agent built with `OS minimal` boots on the pinned OS layer, which `ap
//! build` downloads from its release once.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::Duration;

use ap::boot::{self, Guest, Layer, Report};
use ap::fetch;
use guest::protocol::ToGuest;
use store::{Agents, Layers};
use t1::{guest_binary, kernel};

const TIMEOUT: Duration = Duration::from_secs(60);
const AGENTFILE: &str = "AGENT claude\nOS minimal\nNETWORK -\nPOLICY -\n";
const GRANTS: &str = "NETWORK -\nPOLICY -\n";
/// The SHA-256 digest of the empty file, which stands in for the harness that
/// each agent locks. The guest command doesn't start it.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// A shell runs git and a coreutils program.
const COMMAND: &[&str] = &[
    "/bin/sh",
    "-c",
    "PATH=/usr/bin:/bin && git --version && ls --version",
];

/// Builds the agent in `dir` with a static `ap`, with only `HOME` in its
/// environment, and returns what it printed.
fn build(home: &Path, dir: &Path, grants: &Path) -> String {
    let mut approve = std::ffi::OsString::from("--approve=");
    approve.push(grants);
    let output = Command::new(guest_binary::ap())
        .arg("build")
        .arg(approve)
        .arg(dir)
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// An agent's directory, with a lockfile that pins the harness that the
/// download cache under `home` holds.
fn project(root: &Path, home: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Agentfile"), AGENTFILE).unwrap();
    fs::write(
        dir.join("Agentfile.lock"),
        format!("HARNESS claude 0.0.0 {EMPTY}\n"),
    )
    .unwrap();
    let cache = home.join(".cache/ap");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join(EMPTY), "").unwrap();
    dir
}

// spec(Build): A kernel, OS, or harness layer is downloaded once, however many agents use it.
// spec(OS): | `minimal` | The harness, a shell, git, and coreutils |
#[test]
fn an_os_minimal_agent_boots_with_a_shell_git_and_coreutils() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("os-{}", process::id()));
    let _ = fs::remove_dir_all(&root);
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let grants = root.join("grants");
    fs::write(&grants, GRANTS).unwrap();
    let (one, two) = (project(&root, &home, "one"), project(&root, &home, "two"));

    let first = build(&home, &one, &grants);
    assert!(first.starts_with("fetched OS minimal\n"), "{first}");
    let second = build(&home, &two, &grants);
    assert!(second.starts_with("hit OS minimal\n"), "{second}");

    let store = home.join(".local/share/ap");
    let agents = Agents::new(&store);
    let layers: Vec<_> = ["one", "two"]
        .map(|name| agents.get(name).unwrap().unwrap().layers)
        .into();
    let pin = fetch::os();
    assert_eq!(layers[0][0].digest, pin.sha256);
    assert_eq!(layers[1][0].digest, pin.sha256);
    let stored = Layers::new(&store);
    let disks: Vec<Layer> = layers[0]
        .iter()
        .map(|layer| Layer {
            digest: layer.digest.clone(),
            disk: stored.get(&layer.digest).unwrap(),
        })
        .collect();
    fs::remove_dir_all(&root).unwrap();

    let kernel = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::ap()).unwrap();
    let guest = Guest {
        kernel: &kernel,
        init: &init,
        files: &[],
        layers: disks,
        request: Some(ToGuest::Run {
            argv: COMMAND.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
        }),
        timeout: TIMEOUT,
        confine: false,
        input: None,
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = boot::boot(guest, |report| match report {
        Report::Ready(_) | Report::Exec(..) | Report::ExecTook(_) => {}
        Report::Stdout(bytes) => stdout.extend_from_slice(bytes),
        Report::Stderr(bytes) => stderr.extend_from_slice(bytes),
    });
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(
        status.unwrap(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.starts_with("git version 2."), "{stdout}");
    assert!(stdout.contains("ls (GNU coreutils) "), "{stdout}");
}
