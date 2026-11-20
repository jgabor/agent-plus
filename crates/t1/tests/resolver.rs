//! The guest's only resolver is `ap`'s network process, which answers from
//! the run's rules and never asks upstream. The check runs a real `ap run`
//! under `NETWORK -`, and looks names up from inside the guest with
//! `ap exec`.
//!
//! It runs in a user and network namespace of its own, where the host has
//! only a loopback interface, so that it can listen on the loopback's DNS
//! port, and so that no answer can come from anywhere but `ap`.

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Output, Stdio};

use ap::fetch;
use ap::store::Store;
use guest::net;
use t1::{guest_binary, kernel};

/// How long `ap` may take, in seconds, before `timeout` stops it, and the VM
/// it holds with it.
const TIMEOUT: &str = "240";
/// Set in the namespace, where the check runs, to the scratch directory
/// that the agent was built in. The namespace can't fetch what a build
/// needs.
const INSIDE_VARIABLE: &str = "AP_RESOLVER_INSIDE";
const TEST: &str = "the_guest_s_only_resolver_is_ap";

/// A home of its own, with the kernel in its store and the fake harness in
/// its download cache, so that `ap` reads none of the owner's files and
/// fetches nothing.
struct Scratch {
    root: PathBuf,
    ap: PathBuf,
    /// Whether this process made it, and removes it.
    owned: bool,
}

impl Scratch {
    fn new() -> Scratch {
        let root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("resolver-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        Store::new(root.join("home/.local/share/ap"))
            .put(
                fetch::kernel().sha256,
                File::open(kernel::vmlinux()).unwrap(),
            )
            .unwrap();
        fs::create_dir_all(root.join("runtime")).unwrap();
        Scratch {
            root,
            ap: guest_binary::ap(),
            owned: true,
        }
    }

    /// The scratch that another process made at `root`.
    fn open(root: &Path) -> Scratch {
        Scratch {
            root: root.to_owned(),
            ap: root.join("ap"),
            owned: false,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Builds `name`, a Claude Code agent with a closed network, whose fake
    /// harness reports, then sleeps. The static `ap` goes in the scratch
    /// too, for [`Scratch::open`].
    fn build(&self, name: &str) {
        fs::copy(&self.ap, self.root.join("ap")).unwrap();
        let fake = guest_binary::fake_harness();
        let sum = Command::new("sha256sum").arg(&fake).output().unwrap();
        let digest = String::from_utf8(sum.stdout).unwrap()[..64].to_owned();
        let cache = self.home().join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(&fake, cache.join(&digest)).unwrap();
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let agentfile = "AGENT claude:0.0.0\nOS minimal\nNETWORK -\nENV FAKE_HARNESS_SLEEP 300\n";
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude 0.0.0 {digest}\n"),
        )
        .unwrap();
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home())
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let approve = format!("--approve={}", approved.display());
        let built = self.ap(&["build", &approve, dir.to_str().unwrap()]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");
    }

    /// The static `ap`, with only `HOME` and `XDG_RUNTIME_DIR` in its
    /// environment, under a timeout.
    fn command(&self) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["-k", "10", TIMEOUT])
            .arg(&self.ap)
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"));
        command
    }

    fn ap(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// A process this test started, killed and reaped when it's dropped, so a
/// failed test leaves no VM behind.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the guest tries: its resolver configuration, and each name looked
/// up, as the name and the first address, or the name and `getent`'s
/// status.
const SCRIPT: &str = r#"
grep -v '^#' /etc/resolv.conf
look() {
  if out=$(timeout 30 getent "$1" "$2"); then
    echo "$1 $2 ${out%% *}"
  else
    echo "$1 $2 $?"
  fi
}
look ahostsv4 api.anthropic.com
look ahostsv4 API.Anthropic.com.
look ahostsv4 claude.ai
look ahosts claude.ai
look ahostsv4 example.com
look ahostsv4 anthropic.com
"#;

// spec(NETWORK): **DNS.** The guest's only resolver is inside `ap`.
// spec(NETWORK): For a name it doesn't answer, it says that the name doesn't exist.
#[test]
fn the_guest_s_only_resolver_is_ap() {
    if let Some(root) = env::var_os(INSIDE_VARIABLE) {
        inside(&Scratch::open(Path::new(&root)));
        return;
    }
    let scratch = Scratch::new();
    scratch.build("resolving");
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net"])
        .arg(env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(INSIDE_VARIABLE, &scratch.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    println!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{:?}", output.status);
}

/// The check, in the namespace, on the agent that `scratch` built.
fn inside(scratch: &Scratch) {
    let up = Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .status()
        .unwrap();
    assert!(up.success(), "{up:?}");
    // The host's loopback listens for DNS, on both families and both
    // transports, as a resolver on the host would.
    let udp = [
        UdpSocket::bind("127.0.0.1:53").unwrap(),
        UdpSocket::bind("[::1]:53").unwrap(),
    ];
    let tcp = [
        TcpListener::bind("127.0.0.1:53").unwrap(),
        TcpListener::bind("[::1]:53").unwrap(),
    ];
    for socket in &udp {
        socket.set_nonblocking(true).unwrap();
    }
    for listener in &tcp {
        listener.set_nonblocking(true).unwrap();
    }

    let mut run = Started(
        scratch
            .command()
            .args(["run", "resolving", "-p", "Look names up."])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut report = String::new();
    BufReader::new(run.0.stdout.take().unwrap())
        .read_line(&mut report)
        .unwrap();
    assert!(report.starts_with("{\"arguments\":"), "{report}");
    let listed = String::from_utf8(scratch.ap(&["ps"]).stdout).unwrap();
    let row = listed.lines().nth(1).expect("ap ps lists the run");
    let name = row.split_whitespace().next().unwrap().to_owned();

    let tried = scratch.ap(&["exec", &name, "bash", "-c", SCRIPT]);
    let stdout = String::from_utf8_lossy(&tried.stdout).into_owned();
    println!("{stdout}{}", String::from_utf8_lossy(&tried.stderr));
    assert_eq!(tried.status.code(), Some(0), "{tried:?}");
    let lines: Vec<&str> = stdout.lines().collect();
    let resolvers = [
        format!("nameserver {}", net::IPV4_GATEWAY),
        format!("nameserver {}", net::IPV6_GATEWAY),
    ];
    assert_eq!(lines[..2], resolvers, "{stdout}");
    // Each harness host gets one address of its own from the pool, in any
    // case, and `getent ahosts` takes the empty `AAAA` answer.
    let address = |at: usize, lookup: &str| {
        let (seen, address) = lines[at].rsplit_once(' ').unwrap();
        assert_eq!(seen, lookup, "{stdout}");
        let address: Ipv4Addr = address
            .parse()
            .unwrap_or_else(|_| panic!("{lookup} didn't resolve: {stdout}"));
        assert_eq!(address.octets()[..2], [198, 18], "{stdout}");
        address
    };
    let api = address(2, "ahostsv4 api.anthropic.com");
    assert_eq!(address(3, "ahostsv4 API.Anthropic.com."), api);
    let claude = address(4, "ahostsv4 claude.ai");
    assert_ne!(claude, api);
    assert_eq!(address(5, "ahosts claude.ai"), claude);
    // Any other name doesn't exist, and `getent` says so with 2.
    assert_eq!(
        lines[6..],
        ["ahostsv4 example.com 2", "ahostsv4 anthropic.com 2"],
        "{stdout}"
    );

    // Nothing asked the host's loopback.
    let mut buffer = [0; 512];
    for socket in &udp {
        let got = socket.recv_from(&mut buffer).map(|(_, from)| from);
        assert!(
            matches!(&got, Err(error) if error.kind() == ErrorKind::WouldBlock),
            "the host's resolver got a query: {got:?}"
        );
    }
    for listener in &tcp {
        let got = listener.accept().map(|(_, from)| from);
        assert!(
            matches!(&got, Err(error) if error.kind() == ErrorKind::WouldBlock),
            "the host's resolver got a connection: {got:?}"
        );
    }

    let stopped = scratch.ap(&["stop", &name]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
    let _ = run.0.wait();
}
