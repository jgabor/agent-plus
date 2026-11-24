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
/// SHA-256 digests of its Linux x86_64 release, of its code-mode host, and
/// of its bubblewrap. A pinned one keeps a new release from changing what
/// this check sees, and a cached download needs no network.
const VERSION: &str = "0.171.0";
const DIGEST: &str = "0fd9ae1125788932131a29e296c4fef9995fc4f0c2a2ef6161221786e45ffa8c";
const HOST: &str = "8db4b4899dd36d24defa4a0c4f431e9db527fb689e017c88be9f54ea2949e674";
const BWRAP: &str = "6f122933bd4cb7113d11aec2db1b8ba2977ff11b961b26fec65950057aafe28c";
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
    runs_the_tool("codex-tool", "full", Mode::Full);
}

// spec(MODE): Every Codex mode except `full` relies on Codex's own sandbox, so the guest kernel has to support that sandbox.
#[test]
fn codex_edits_its_workspace_in_its_sandbox() {
    runs_the_tool("codex-sandbox", "edit", Mode::Edit);
}

/// Builds a Codex agent with `MODE <word>`, and starts it with `ap run` in a
/// terminal, with the fake upstream in `upstream` reachable at
/// `host.ap.internal`. Returns the run, and its name, once `ap ps` lists it.
fn start_codex(scratch: &Scratch, upstream: &Path, word: &str) -> (Interactive, String) {
    scratch.build(
        "codex",
        &format!(
            "AGENT codex:{VERSION}\nOS minimal\nMODE {word}\nNETWORK -\nNETWORK +host.ap.internal\n\
             SECRET UPSTREAM @host.ap.internal\n"
        ),
        &format!("HARNESS codex {VERSION} {DIGEST} {HOST} {BWRAP}\n"),
    );
    // The hidden secret makes the proxy decrypt TLS to the fake upstream,
    // with the run's authority, which the guest trusts. The proxy trusts
    // the upstream's.
    let mut command = scratch.run("codex");
    command
        .env("SSL_CERT_FILE", upstream.join("ca.pem"))
        .env("UPSTREAM", "unused");
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
    (run, name)
}

/// Runs a headless Codex in the run `name`, with `arguments` from the
/// adapter, and the fake upstream at `port` as its model's provider, which
/// needs no sign-in. Returns what it wrote to standard output and error,
/// once it exits 0.
fn exec_codex(scratch: &Scratch, name: &str, port: u16, arguments: Vec<String>) -> String {
    let provider = format!(
        "model_providers.fake={{name=\"fake\",base_url=\"https://host.ap.internal:{port}/v1\",\
         wire_api=\"responses\"}}"
    );
    let mut args = vec![
        "exec".to_owned(),
        name.to_owned(),
        "/usr/local/bin/codex".to_owned(),
    ];
    args.extend(arguments);
    args.extend(["--json", "-c", "model_provider=\"fake\"", "-c", &provider].map(str::to_owned));
    args.push("Run the tool.".to_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = scratch.ap(&args);
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(ran.status.code(), Some(0), "{shown}");
    shown
}

/// The tool's outputs that requests to the fake upstream in `upstream`
/// gave the model, which say why a tool failed.
fn tool_outputs(upstream: &Path) -> Vec<String> {
    let requests = fs::read_dir(upstream.join("requests")).unwrap();
    requests
        .flatten()
        .filter_map(|request| fs::read_to_string(request.path()).ok())
        .filter(|text| text.contains("\"custom_tool_call_output\""))
        .collect()
}

/// Builds a Codex agent with `MODE <word>`, and has a headless Codex in its
/// run, with the adapter's arguments for `mode`, run the tool the fake
/// upstream, as its model, asks for. Fails unless the tool wrote `tool-ran`
/// in Codex's working directory, which is in its workspace, and the model
/// got the tool's output.
fn runs_the_tool(test: &str, word: &str, mode: Mode) {
    let scratch = Scratch::new(test);
    let upstream = scratch.root.join("upstream");
    let (_upstream, port) = start_upstream(&upstream);
    let (_run, name) = start_codex(&scratch, &upstream, word);
    let arguments = ap::adapter::codex::arguments(VERSION, None, None, mode, false);
    let shown = exec_codex(&scratch, &name, port, arguments);
    let answers = tool_outputs(&upstream);
    assert!(!answers.is_empty(), "{shown}");
    let wrote = scratch.ap(&["exec", &name, "cat", "tool-ran"]);
    assert_eq!(
        String::from_utf8_lossy(&wrote.stdout),
        "the tool ran\n",
        "{shown}{}",
        answers.concat()
    );
}

/// A rule that allows every command Codex runs through its shell.
const ALLOW_RULE: &str = r#"prefix_rule(pattern = ["/bin/sh", "-lc"], decision = "allow")"#;

// spec(MODE): With `read`, `ask`, `edit`, or `auto`, when the working directory is a share, a rule in its `.codex/rules` that allows a command lets Codex run that command outside its sandbox without asking.
#[test]
fn a_trusted_codex_projects_rule_runs_a_command_outside_the_sandbox() {
    let scratch = Scratch::new("codex-rules");
    let upstream = scratch.root.join("upstream");
    let (_upstream, port) = start_upstream(&upstream);
    let (_run, name) = start_codex(&scratch, &upstream, "full");
    let mut runs = 0;
    for mode in [Mode::Read, Mode::Ask, Mode::Edit, Mode::Auto] {
        for rule in [false, true] {
            // The project is the working directory, and the command writes
            // outside it, where no mode's sandbox lets it.
            let project = format!("/home/agent/{mode:?}-{rule}");
            let wrote = format!("{project}-ran");
            fs::write(upstream.join("command"), format!("echo ran > {wrote}")).unwrap();
            let mut set_up = format!("mkdir -p {project}/.codex/rules");
            if rule {
                set_up.push_str(&format!(
                    " && echo '{ALLOW_RULE}' > {project}/.codex/rules/allow.rules"
                ));
            }
            let made = scratch.ap(&["exec", &name, "sh", "-c", &set_up]);
            assert_eq!(made.status.code(), Some(0), "{made:?}");
            let mut arguments = ap::adapter::codex::arguments(VERSION, None, None, mode, false);
            arguments.extend(["--cd".to_owned(), project.clone()]);
            arguments.extend(ap::adapter::codex::workspace(&[&project]));
            let shown = exec_codex(&scratch, &name, port, arguments);
            // The model asked for the command, and got the tool's output.
            runs += 1;
            let answers = tool_outputs(&upstream);
            assert_eq!(answers.len(), runs, "{shown}");
            let ran = scratch.ap(&["exec", &name, "test", "-e", &wrote]);
            assert_eq!(
                ran.status.code(),
                Some(if rule { 0 } else { 1 }),
                "{mode:?} with the rule {rule}: {shown}{}",
                answers.concat()
            );
        }
    }
}
