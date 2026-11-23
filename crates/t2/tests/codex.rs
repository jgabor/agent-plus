//! Real Codex in a guest, with no credentials: a static `ap` builds an agent
//! from OpenAI's release, and Codex runs a tool when the fake upstream, as
//! its model, asks it to.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentfile::Mode;
use t1::started::Started;

mod common;
use common::{Interactive, Scratch};

/// The version that runs every tool through its code-mode host, and the
/// SHA-256 digests of its Linux x86_64 release and of its code-mode host. A
/// pinned one keeps a new release from changing what this check sees, and a
/// cached download needs no network.
const VERSION: &str = "0.171.0";
const DIGEST: &str = "0fd9ae1125788932131a29e296c4fef9995fc4f0c2a2ef6161221786e45ffa8c";
#[allow(dead_code)]
const HOST: &str = "8db4b4899dd36d24defa4a0c4f431e9db527fb689e017c88be9f54ea2949e674";
/// How long the VM may take to start.
const STARTS_WITHIN: Duration = Duration::from_secs(600);
/// How long the fake upstream may run, in seconds.
const UPSTREAM_TIMEOUT: &str = "1200";

/// Builds the fake upstream, which runs on the host, as the journey does:
/// static, for the musl target.
fn fake_upstream() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = "x86_64-unknown-linux-musl";
    let directory = root.join("target/t1-guest");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(&root)
        .args(["build", "--quiet", "--package", "fake-upstream"])
        .args(["--target", target, "--target-dir"])
        .arg(&directory)
        .status()
        .unwrap();
    assert!(status.success(), "cargo couldn't build fake-upstream");
    directory.join(target).join("debug/fake-upstream")
}

/// Starts the fake upstream in `directory`, and returns it with the port
/// its HTTPS listener took.
fn start_upstream(directory: &Path) -> (Started, u16) {
    fs::create_dir_all(directory).unwrap();
    let mut upstream = Started(
        Command::new("timeout")
            .args(["-k", "5", UPSTREAM_TIMEOUT])
            .arg(fake_upstream())
            .arg(directory)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(upstream.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.get(2), Some(&"tls"), "{line}");
    (upstream, words[3].parse().unwrap())
}

// spec(AGENT): A Codex release that publishes a code-mode host, which Codex runs its tools through, has it installed beside Codex, and the lockfile records its digest.
#[test]
fn codex_runs_a_tool_in_the_guest() {
    let scratch = Scratch::new("codex-tool");
    let upstream = scratch.root.join("upstream");
    let (_upstream, port) = start_upstream(&upstream);
    scratch.build(
        "codex",
        &format!(
            "AGENT codex:{VERSION}\nOS minimal\nMODE full\nNETWORK -\nNETWORK +host.ap.internal\n"
        ),
        &format!("HARNESS codex {VERSION} {DIGEST}\n"), // BEFORE
    );
    let mut command = scratch.run("codex");
    command.env("SSL_CERT_FILE", upstream.join("ca.pem"));
    let mut run = Interactive::start(command);
    let deadline = Instant::now() + STARTS_WITHIN;
    let name = loop {
        let listed = String::from_utf8(scratch.ap(&["ps"]).stdout).unwrap();
        if let Some(row) = listed.lines().nth(1) {
            break row.split_whitespace().next().unwrap().to_owned();
        }
        let exited = run.run.try_wait().unwrap();
        assert!(
            exited.is_none(),
            "ap run exited {exited:?}: {}",
            run.shown()
        );
        assert!(Instant::now() < deadline, "the run never started");
        thread::sleep(Duration::from_millis(100));
    };

    // A headless Codex in the run, with the adapter's arguments for
    // `MODE full`, and the fake upstream as its model's provider, which
    // needs no sign-in.
    let provider = format!(
        "model_providers.fake={{name=\"fake\",base_url=\"https://host.ap.internal:{port}/v1\",\
         wire_api=\"responses\"}}"
    );
    let mut args = vec![
        "exec".to_owned(),
        name.clone(),
        "/usr/local/bin/codex".to_owned(),
    ];
    args.extend(ap::adapter::codex::arguments(
        VERSION,
        None,
        None,
        Mode::Full,
        false,
    ));
    args.extend(["--json", "-c", "model_provider=\"fake\"", "-c", &provider].map(str::to_owned));
    args.push("Run the tool.".to_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = scratch.ap(&args);
    let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
    assert_eq!(ran.status.code(), Some(0), "{stdout}{stderr}");
    let wrote = scratch.ap(&["exec", &name, "cat", "tool-ran"]);
    assert_eq!(
        String::from_utf8_lossy(&wrote.stdout),
        "the tool ran\n",
        "{stdout}{stderr}"
    );
    // The model got the tool's output.
    let requests = fs::read_dir(upstream.join("requests")).unwrap();
    let answered = requests.flatten().any(|request| {
        fs::read_to_string(request.path())
            .is_ok_and(|text| text.contains("\"custom_tool_call_output\""))
    });
    assert!(answered, "{stdout}{stderr}");
}
