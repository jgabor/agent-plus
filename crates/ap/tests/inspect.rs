//! `ap inspect`, from SPEC.md's `ap inspect` section: an agent's layers, in
//! order, each with its digest and the instruction that made it, and the
//! grants you approved for its Agentfile. `ap inspect --grants` prints only
//! the grants, in the text form that `--approve=<file>` reads.
//!
//! Git credentials and the harness's own hosts aren't shown yet, because
//! nothing fetches a source or resolves a harness.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use store::Agents;

/// A test's own directory, with two home directories in it, so that a test
/// can build in a second store.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    other_home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("inspect")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let other_home = root.join("other-home");
        fs::create_dir_all(&home).unwrap();
        common::seed_os(&home.join(".local/share/ap"));
        fs::create_dir_all(&other_home).unwrap();
        common::seed_os(&other_home.join(".local/share/ap"));
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
            other_home: other_home.canonicalize().unwrap(),
        }
    }

    /// Runs `ap` in `dir` with `home` as `HOME`, with nothing else in its
    /// environment and no terminal.
    fn ap_with(&self, home: &Path, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ap(&self, args: &[&str]) -> Output {
        self.ap_with(&self.home, &self.root, args)
    }

    /// Makes the directory `path` under the root with this Agentfile, and
    /// returns its path.
    fn project(&self, path: &str, agentfile: &str) -> PathBuf {
        let dir = self.root.join(path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        common::pin_harness(&self.home, &dir);
        common::pin_harness(&self.other_home, &dir);
        dir
    }

    /// Builds `dir` in the store under `home`, approving the grants in
    /// `grants`, and fails the test unless it builds.
    fn build(&self, home: &Path, dir: &Path, grants: &Path) {
        let approve = format!("--approve={}", grants.display());
        let output = self.ap_with(home, dir, &["build", &approve]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    /// The digests of the agent's layers, in order, from its record.
    fn digests(&self, name: &str) -> Vec<String> {
        let store = self.home.join(".local/share/ap");
        let agent = Agents::new(store).get(name).unwrap().unwrap();
        agent.layers.into_iter().map(|layer| layer.digest).collect()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// An Agentfile with two layers before the harness configuration, and a
/// grant of each kind that `ap build` can build.
const NOTES: &str = "AGENT claude\nOS minimal\n\
                     ADD ./src /work/src\n\
                     ADD ./docs\n\
                     SHARE ../shared /work/shared\n\
                     SHARE ./out /work/out rw\n\
                     ENV NAME $USER\n\
                     SECRET GITHUB_TOKEN @api.github.com\n\
                     AUTH key\n\
                     NETWORK -\n\
                     NETWORK +github.com\n\
                     POLICY -\n\
                     POLICY +reviewer\n\
                     SYSTEM Be brief.\n";

/// Makes the notes project under the root, with what it adds and shares,
/// and returns it with the grants it asks for.
fn notes(scratch: &Scratch) -> (PathBuf, String) {
    let dir = scratch.project("notes", NOTES);
    for path in ["src", "docs", "out"] {
        fs::create_dir_all(dir.join(path)).unwrap();
        fs::write(dir.join(path).join("file"), path).unwrap();
    }
    fs::create_dir_all(scratch.root.join("shared")).unwrap();
    let grants = format!(
        "AGENT claude\n\
         PATH {}/notes/out rw\n\
         PATH {}/shared\n\
         VARIABLE USER\n\
         SECRET GITHUB_TOKEN @api.github.com\n\
         AUTH key\n\
         NETWORK -\n\
         NETWORK +github.com\n\
         POLICY -\n\
         POLICY +reviewer\n",
        scratch.root.display(),
        scratch.root.display()
    );
    (dir, grants)
}

// spec(ap inspect): `ap inspect <agent>` shows an agent's layers and its approved grants: shares, host environment variables, secrets, git credentials, network policy, and the agents it may call.
#[test]
fn inspect_shows_the_layers_in_order_and_the_approved_grants() {
    let scratch = Scratch::new("shows");
    let (dir, grants) = notes(&scratch);
    let file = scratch.root.join("grants");
    fs::write(&file, &grants).unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let digests = scratch.digests("notes");
    assert_eq!(digests.len(), 5);
    let grants: String = grants.lines().map(|line| format!("  {line}\n")).collect();
    assert_eq!(
        stdout(&output),
        format!(
            "layers:\n  \
             {} OS minimal\n  \
             {} AGENT claude:0.0.0\n  \
             {} ADD ./src /work/src\n  \
             {} ADD ./docs\n  \
             {} harness configuration\n\
             grants:\n{grants}",
            digests[0], digests[1], digests[2], digests[3], digests[4]
        )
    );
}

// spec(Grants): `--approve=<file>`, on `ap build` and `ap run`, approves exactly the grants listed in `<file>`, in the format that `ap inspect --grants` prints, and fails if the Agentfile asks for more.
#[test]
fn inspect_grants_prints_what_approves_a_fresh_build() {
    let scratch = Scratch::new("grants");
    let (dir, grants) = notes(&scratch);
    let file = scratch.root.join("grants");
    fs::write(&file, &grants).unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    assert_eq!(stdout(&output), grants);

    // What it prints approves a build of the same Agentfile in another store.
    let printed = scratch.root.join("printed");
    fs::write(&printed, &output.stdout).unwrap();
    scratch.build(&scratch.other_home, &dir, &printed);
    // The flag goes before or after the name.
    let after = scratch.ap(&["inspect", "notes", "--grants"]);
    assert_eq!(stdout(&after), grants);
}

#[test]
fn inspect_grants_escapes_what_it_prints() {
    let scratch = Scratch::new("escapes");
    let dir = scratch.project(
        "notes",
        "AGENT claude\nOS minimal\nSHARE ./a\u{202e}b /work/x rw\n",
    );
    fs::create_dir_all(dir.join("a\u{202e}b")).unwrap();
    let file = scratch.root.join("grants");
    fs::write(
        &file,
        format!(
            "PATH {}/notes/a\\u{{202e}}b rw\nNETWORK +\nPOLICY +\n",
            scratch.root.display()
        ),
    )
    .unwrap();
    scratch.build(&scratch.home, &dir, &file);

    let output = scratch.ap(&["inspect", "--grants", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), fs::read_to_string(&file).unwrap());
}

#[test]
fn inspect_shows_the_approval_of_the_agentfile_the_build_read() {
    let scratch = Scratch::new("symlink");
    let file = scratch.root.join("grants");
    let wide = scratch.project("wide", "AGENT claude\nOS minimal\n");
    fs::write(&file, "NETWORK +\nPOLICY +\n").unwrap();
    scratch.build(&scratch.home, &wide, &file);
    let real = scratch.project("real", "AGENT claude\nOS minimal\nNETWORK -\nPOLICY -\n");
    let dir = scratch.root.join("notes");
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(real.join("Agentfile"), dir.join("Agentfile")).unwrap();
    common::pin_harness(&scratch.home, &dir);
    fs::write(&file, "AGENT claude\nNETWORK -\nPOLICY -\n").unwrap();
    scratch.build(&scratch.home, &dir, &file);

    // The link now leads to an Agentfile whose approval is wider, and then
    // nowhere.
    fs::remove_file(dir.join("Agentfile")).unwrap();
    std::os::unix::fs::symlink(wide.join("Agentfile"), dir.join("Agentfile")).unwrap();
    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(
        stdout(&output),
        "AGENT claude\nNETWORK -\nPOLICY -\n",
        "{}",
        stderr(&output)
    );
    fs::remove_file(dir.join("Agentfile")).unwrap();
    let output = scratch.ap(&["inspect", "--grants", "notes"]);
    assert_eq!(
        stdout(&output),
        "AGENT claude\nNETWORK -\nPOLICY -\n",
        "{}",
        stderr(&output)
    );
}

#[test]
fn inspect_an_unknown_agent_exits_1_and_names_it() {
    let scratch = Scratch::new("unknown");
    for args in [
        &["inspect", "missing"][..],
        &["inspect", "--grants", "missing"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "ap: no agent named missing\n");
    }
    // A name that can't name an agent is escaped, too.
    let output = scratch.ap(&["inspect", "No\u{202e}pe"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: no agent named No\\u{202e}pe\n");
}

#[test]
fn inspect_takes_one_agent_and_only_grants() {
    let scratch = Scratch::new("arguments");
    for args in [
        &["inspect"][..],
        &["inspect", "--grants"],
        &["inspect", "a", "b"],
        &["inspect", "--grants", "--grants", "a"],
        &["inspect", "--layers", "a"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("ap inspect [--grants | --network=destination] agent\n"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn usage_lists_inspect() {
    let scratch = Scratch::new("usage");
    let output = scratch.ap(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("ap inspect [--grants | --network=destination] agent\n"),
        "{}",
        stderr(&output)
    );
}

/// Builds the agent `name`, a Claude Code agent with these `NETWORK` rules,
/// and approves them.
fn network_agent(scratch: &Scratch, name: &str, rules: &[&str]) {
    let lines: String = rules
        .iter()
        .map(|rule| format!("NETWORK {rule}\n"))
        .collect();
    let dir = scratch.project(
        name,
        &format!("AGENT claude\nOS minimal\n{lines}POLICY -\n"),
    );
    let file = scratch.root.join(format!("{name}.grants"));
    fs::write(&file, format!("AGENT claude\n{lines}POLICY -\n")).unwrap();
    scratch.build(&scratch.home, &dir, &file);
}

/// Asks whether `agent` may reach `destination`, and returns the exit
/// status, standard output, and standard error.
fn ask(scratch: &Scratch, agent: &str, destination: &str) -> (Option<i32>, String, String) {
    let flag = format!("--network={destination}");
    let output = scratch.ap(&["inspect", agent, &flag]);
    (output.status.code(), stdout(&output), stderr(&output))
}

/// Fails unless `agent` answers each destination with the line that
/// follows it, and the status of its sign.
fn answers(scratch: &Scratch, agent: &str, expected: &[(&str, &str)]) {
    for (destination, line) in expected {
        let (status, out, err) = ask(scratch, agent, destination);
        let sign = if line.starts_with('+') { 0 } else { 1 };
        assert_eq!(out, format!("{line}\n"), "{destination}: {err}");
        assert_eq!(status, Some(sign), "{destination}: {err}");
    }
}

/// The rules of the example under NETWORK in SPEC.md.
const EXAMPLE: &[&str] = &[
    "-",
    "+github.com",
    "-gist.github.com",
    "+https://codeberg.org/forgejo/forgejo",
];

// spec(ap inspect): It prints one line: `+` or `-`, the destination as you wrote it, and the rule that decides, named as `ap logs` names it for a network decision.
// spec(ap inspect): It exits 0 for `+` and 1 for `-`.
// spec(ap inspect): It decides from the agent's approved rules and its harness's hosts, as a run with no `--network` flag does.
#[test]
fn inspect_network_answers_with_the_sign_and_the_rule_that_decides() {
    let scratch = Scratch::new("network-example");
    network_agent(&scratch, "example", EXAMPLE);
    answers(
        &scratch,
        "example",
        &[
            (
                "https://github.com/torvalds/linux",
                "+ https://github.com/torvalds/linux  NETWORK +github.com",
            ),
            (
                "https://gist.github.com/x",
                "- https://gist.github.com/x  NETWORK -gist.github.com",
            ),
            (
                "https://codeberg.org/forgejo/forgejo/issues",
                "+ https://codeberg.org/forgejo/forgejo/issues  NETWORK +https://codeberg.org/forgejo/forgejo",
            ),
            (
                "https://codeberg.org/other/repo",
                "- https://codeberg.org/other/repo  the default",
            ),
            (
                "https://api.anthropic.com/v1/messages",
                "+ https://api.anthropic.com/v1/messages  the harness's hosts",
            ),
            (
                "https://api.openai.com/v1/responses",
                "- https://api.openai.com/v1/responses  the default",
            ),
            ("GitHub.com", "+ GitHub.com  NETWORK +github.com"),
        ],
    );
}

// spec(ap inspect): A destination is written as a `NETWORK` rule writes one, without the sign: a URL, a host, a host with a port, or an address with or without a port.
#[test]
fn inspect_network_reads_each_form_of_destination() {
    let scratch = Scratch::new("network-forms");
    network_agent(
        &scratch,
        "forms",
        &["-", "+github.com", "+1.1.1.1", "+[2606:4700::1111]"],
    );
    answers(
        &scratch,
        "forms",
        &[
            (
                "https://github.com/x",
                "+ https://github.com/x  NETWORK +github.com",
            ),
            (
                "http://github.com",
                "+ http://github.com  NETWORK +github.com",
            ),
            ("github.com", "+ github.com  NETWORK +github.com"),
            ("github.com:22", "+ github.com:22  NETWORK +github.com"),
            ("1.1.1.1", "+ 1.1.1.1  NETWORK +1.1.1.1"),
            ("1.1.1.1:53", "+ 1.1.1.1:53  NETWORK +1.1.1.1"),
            (
                "[2606:4700::1111]",
                "+ [2606:4700::1111]  NETWORK +[2606:4700::1111]",
            ),
            (
                "[2606:4700::1111]:443",
                "+ [2606:4700::1111]:443  NETWORK +[2606:4700::1111]",
            ),
            ("1.0.0.1:53", "- 1.0.0.1:53  the default"),
        ],
    );
}

// spec(ap inspect): A destination with a sign is bad usage, and the message says that `ap run --network` is where a rule goes.
#[test]
fn inspect_network_refuses_a_rule_with_its_sign() {
    let scratch = Scratch::new("network-sign");
    network_agent(&scratch, "signed", EXAMPLE);
    for destination in ["+github.com", "-gist.github.com", "+", "-"] {
        let (status, out, err) = ask(&scratch, "signed", destination);
        assert_eq!(status, Some(2), "{destination}: {err}");
        assert_eq!(out, "");
        assert!(err.contains("`ap run --network`"), "{destination}: {err}");
    }
}

// spec(ap inspect): A destination it can't read is bad usage, and so is `--network` with `--grants`.
#[test]
fn inspect_network_refuses_what_it_can_t_read() {
    let scratch = Scratch::new("network-unreadable");
    network_agent(&scratch, "unread", EXAMPLE);
    for destination in [
        "",
        "ftp://github.com/x",
        "https://",
        "https://github.com:0/x",
        "https://1.1.1.1/x",
        "https://github.com/a b",
        "kagi..com",
        "github.com:99999",
        "10.0.0.0/8",
        "[2606:4700::]/32",
        "2606:4700::1",
        "github.com/x",
    ] {
        let (status, out, err) = ask(&scratch, "unread", destination);
        assert_eq!(status, Some(2), "{destination:?}: {err}");
        assert_eq!(out, "", "{destination:?}");
        assert!(err.starts_with("ap: "), "{destination:?}: {err}");
    }
    for args in [
        &["inspect", "--grants", "unread", "--network=github.com"][..],
        &[
            "inspect",
            "unread",
            "--network=github.com",
            "--network=x.com",
        ],
        &["inspect", "unread", "--network"],
    ] {
        let output = scratch.ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert!(
            stderr(&output).contains("ap inspect [--grants | --network=destination] agent\n"),
            "{}",
            stderr(&output)
        );
    }
}

// spec(ap inspect): It starts no VM, resolves no name, and connects nowhere.
#[test]
fn inspect_network_answers_from_the_rules_alone() {
    let scratch = Scratch::new("network-alone");
    network_agent(&scratch, "alone", &["+", "+host.ap.internal"]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let host = format!("host.ap.internal:{port}");
    // A run would find `localhost` at your host's loopback, and refuse it.
    answers(
        &scratch,
        "alone",
        &[
            (&host, &format!("+ {host}  NETWORK +host.ap.internal")),
            ("localhost", "+ localhost  the default"),
        ],
    );
    let accepted = listener.accept();
    assert!(
        matches!(&accepted, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "{accepted:?}"
    );
    let runs = scratch.ap(&["ps", "-a"]);
    assert_eq!(stdout(&runs).lines().count(), 1, "{}", stdout(&runs));
}

// spec(ap inspect): A destination without a port meets only the rules for every port.
// spec(ap inspect): A URL without a port has its scheme's port.
#[test]
fn inspect_network_without_a_port_meets_the_rules_for_every_port() {
    let scratch = Scratch::new("network-ports");
    network_agent(
        &scratch,
        "ports",
        &["-", "+github.com:22", "+example.com", "-example.com:443"],
    );
    answers(
        &scratch,
        "ports",
        &[
            ("github.com", "- github.com  the default"),
            ("github.com:22", "+ github.com:22  NETWORK +github.com:22"),
            ("example.com", "+ example.com  NETWORK +example.com"),
            (
                "example.com:443",
                "- example.com:443  NETWORK -example.com:443",
            ),
            (
                "https://example.com/",
                "- https://example.com/  NETWORK -example.com:443",
            ),
            (
                "http://example.com/",
                "+ http://example.com/  NETWORK +example.com",
            ),
            (
                "http://example.com:443/",
                "- http://example.com:443/  NETWORK -example.com:443",
            ),
        ],
    );
}

// spec(ap inspect): For a URL on a host with URL rules, it treats the path as the proxy does, so a path the proxy denies for its form is denied here too.
#[test]
fn inspect_network_treats_a_url_path_as_the_proxy_does() {
    let scratch = Scratch::new("network-paths");
    network_agent(
        &scratch,
        "paths",
        &["-", "+https://github.com/jgabor", "+gitlab.com"],
    );
    let path = "the request path";
    answers(
        &scratch,
        "paths",
        &[
            (
                "https://github.com/jgabor/agent-plus",
                "+ https://github.com/jgabor/agent-plus  NETWORK +https://github.com/jgabor",
            ),
            (
                "https://github.com/%6Agabor/x",
                "+ https://github.com/%6Agabor/x  NETWORK +https://github.com/jgabor",
            ),
            (
                "https://github.com/jgabor-evil",
                "- https://github.com/jgabor-evil  the default",
            ),
            (
                "https://github.com/jgabor/../x",
                &format!("- https://github.com/jgabor/../x  {path}"),
            ),
            (
                "https://github.com/jgabor%2Fx",
                &format!("- https://github.com/jgabor%2Fx  {path}"),
            ),
            (
                "https://github.com/jgabor//x",
                &format!("- https://github.com/jgabor//x  {path}"),
            ),
            (
                "https://github.com/jgabor/a;b",
                &format!("- https://github.com/jgabor/a;b  {path}"),
            ),
            // A host without URL rules has no path to check.
            (
                "https://gitlab.com/a//b",
                "+ https://gitlab.com/a//b  NETWORK +gitlab.com",
            ),
        ],
    );
}

// spec(ap inspect): For an address, it gives what the IP rules and your host's loopback say.
#[test]
fn inspect_network_decides_an_address_by_the_ip_rules_and_loopback() {
    let scratch = Scratch::new("network-addresses");
    network_agent(
        &scratch,
        "addresses",
        &["+", "-10.0.0.0/8", "+10.1.0.0/16", "+127.0.0.1"],
    );
    let loopback = "your host's loopback, which only host.ap.internal reaches";
    answers(
        &scratch,
        "addresses",
        &[
            ("10.1.2.3", "+ 10.1.2.3  NETWORK +10.1.0.0/16"),
            ("10.2.0.1:22", "- 10.2.0.1:22  NETWORK -10.0.0.0/8"),
            (
                "[::ffff:10.2.0.1]",
                "- [::ffff:10.2.0.1]  NETWORK -10.0.0.0/8",
            ),
            ("192.0.2.1", "+ 192.0.2.1  the default"),
            ("127.0.0.1:8080", &format!("- 127.0.0.1:8080  {loopback}")),
            ("[::1]", &format!("- [::1]  {loopback}")),
        ],
    );
}

// spec(ap inspect): For a name, it gives the decision by name.
// spec(ap inspect): When that is `+`, it says on standard error that a run can still deny the connection: by an IP rule or your host's loopback, for the address the name resolves to, or by the name check.
#[test]
fn inspect_network_decides_a_name_by_name_and_says_what_a_run_still_checks() {
    let scratch = Scratch::new("network-names");
    network_agent(&scratch, "names", &["+", "-1.1.1.1", "-evil.net"]);
    let (status, out, err) = ask(&scratch, "names", "one.one.one.one");
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "+ one.one.one.one  the default\n")
    );
    assert_eq!(
        err,
        "ap: a run can still deny a connection to one.one.one.one: by an IP rule or your host's loopback, for the address it resolves to, or by the name check\n"
    );
    for (destination, line) in [
        ("api.evil.net", "- api.evil.net  NETWORK -evil.net"),
        ("1.1.1.1", "- 1.1.1.1  NETWORK -1.1.1.1"),
        ("1.0.0.1", "+ 1.0.0.1  the default"),
    ] {
        let (status, out, err) = ask(&scratch, "names", destination);
        assert_eq!(out, format!("{line}\n"), "{destination}");
        assert!(status.is_some(), "{destination}");
        assert_eq!(err, "", "{destination}");
    }
}

// spec(ap inspect): If the agent doesn't exist, or has no approved grants, it says so on standard error and exits with status 125.
#[test]
fn inspect_network_exits_125_when_it_can_t_answer() {
    let scratch = Scratch::new("network-unknown");
    let (status, out, err) = ask(&scratch, "missing", "github.com");
    assert_eq!((status, out.as_str()), (Some(125), ""));
    assert_eq!(err, "ap: no agent named missing\n");
    network_agent(&scratch, "unapproved", EXAMPLE);
    fs::remove_dir_all(scratch.home.join(".local/share/ap/approvals")).unwrap();
    let (status, out, err) = ask(&scratch, "unapproved", "github.com");
    assert_eq!((status, out.as_str()), (Some(125), ""));
    assert!(err.starts_with("ap: no grants are approved for "), "{err}");
}
