//! The network process connects what host and IP rules allow to its real
//! destination, and nothing else. Each check runs a real `ap run`, and
//! tries the network from inside the guest with `ap exec`.
//!
//! The checks of your host reach listeners on this host's loopback, as
//! `host.ap.internal`. The check of upstream names runs in a user, network,
//! and mount namespace of its own, where it plays the host's DNS servers,
//! and changes the host's resolver configuration while the run goes on.
//! The last check confines a process as the network process confines
//! itself when it may connect, and makes one attempt in it.

use std::env;
use std::ffi::{c_int, c_ulong};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ap::fetch;
use ap::store::Store;
use guest::net;
use t1::{guest_binary, kernel};

/// How long `ap` may take, in seconds, before `timeout` stops it, and the VM
/// it holds with it.
const TIMEOUT: &str = "240";
/// Set in the namespace, where the check of upstream names runs, to the
/// scratch directory that its agent was built in.
const INSIDE_VARIABLE: &str = "AP_CONNECTS_INSIDE";
const NAMES_TEST: &str = "a_run_keeps_resolving_after_the_host_s_resolver_configuration_changes";

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
    fn new(test: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("connects-{test}-{}", process::id()));
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

    /// Builds `name`, a Claude Code agent with the `NETWORK` lines of
    /// `network`, whose fake harness reports, then sleeps. The static `ap`
    /// goes in the scratch too, for [`Scratch::open`].
    fn build(&self, name: &str, network: &str) {
        fs::copy(&self.ap, self.root.join("ap")).unwrap();
        let fake = guest_binary::fake_harness();
        let sum = Command::new("sha256sum").arg(&fake).output().unwrap();
        let digest = String::from_utf8(sum.stdout).unwrap()[..64].to_owned();
        let cache = self.home().join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(&fake, cache.join(&digest)).unwrap();
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let agentfile =
            format!("AGENT claude:0.0.0\nOS minimal\n{network}ENV FAKE_HARNESS_SLEEP 300\n");
        fs::write(dir.join("Agentfile"), &agentfile).unwrap();
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude 0.0.0 {digest}\n"),
        )
        .unwrap();
        let instructions = agentfile::parse(&agentfile).unwrap();
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

    /// Runs `name` headless, and returns the run once its harness reported,
    /// with the run's name.
    fn run(&self, name: &str) -> (Started, String) {
        let mut run = Started(
            self.command()
                .args(["run", name, "-p", "Try the network."])
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
        let listed = String::from_utf8(self.ap(&["ps"]).stdout).unwrap();
        let row = listed.lines().nth(1).expect("ap ps lists the run");
        (run, row.split_whitespace().next().unwrap().to_owned())
    }

    /// Runs `script` in the guest of the run `name`, and returns its lines.
    fn try_network(&self, name: &str, script: &str) -> Vec<String> {
        let script = format!("{TRY}{script}");
        let tried = self.ap(&["exec", name, "bash", "-c", &script]);
        let stdout = String::from_utf8_lossy(&tried.stdout).into_owned();
        println!("{stdout}{}", String::from_utf8_lossy(&tried.stderr));
        assert_eq!(tried.status.code(), Some(0), "{tried:?}");
        stdout.lines().map(str::to_owned).collect()
    }

    fn stop(&self, name: &str, mut run: Started) {
        let stopped = self.ap(&["stop", name]);
        assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
        let _ = run.0.wait();
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

/// The guest's attempts. Each prints one line: its label and the line the
/// destination sent back, or its label and `failed`. A UDP answer is read
/// with one call, because `read` takes a byte at a time, and the rest of
/// the datagram with it.
const TRY: &str = r#"
tcp() {
  if out=$(timeout 20 bash -c 'exec 3<>"/dev/tcp/$0/$1" && printf "ping\n" >&3 && read -r -t 10 reply <&3 && echo "$reply"' "$2" "$3" 2>/dev/null); then
    echo "$1 $out"
  else
    echo "$1 failed"
  fi
}
udp() {
  if out=$(timeout 20 bash -c 'exec 3<>"/dev/udp/$0/$1" && printf "ping\n" >&3 && reply=$(timeout 5 dd bs=512 count=1 status=none <&3) && [ -n "$reply" ] && echo "$reply"' "$2" "$3" 2>/dev/null); then
    echo "$1 $out"
  else
    echo "$1 failed"
  fi
}
"#;

/// What a listener saw: a line for each connection's or datagram's first
/// line, and `closed` when a connection ended.
type Seen = Arc<Mutex<Vec<String>>>;

/// Answers each TCP connection's first line with `<tag>:<line>`, and notes
/// when the guest closes it.
fn tcp_echo(listener: TcpListener, tag: &'static str) -> Seen {
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let noted = Arc::clone(&noted);
            thread::spawn(move || {
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let _ = reader.read_line(&mut line);
                noted
                    .lock()
                    .unwrap()
                    .push(format!("tcp {}", line.trim_end()));
                let _ = stream.write_all(format!("{tag}:{line}").as_bytes());
                let mut rest = Vec::new();
                let _ = reader.read_to_end(&mut rest);
                noted.lock().unwrap().push("closed".to_owned());
            });
        }
    });
    seen
}

/// Answers each datagram with `<tag>:<datagram>`.
fn udp_echo(socket: UdpSocket, tag: &'static str) -> Seen {
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        let mut buffer = [0; 512];
        while let Ok((len, from)) = socket.recv_from(&mut buffer) {
            let text = String::from_utf8_lossy(&buffer[..len]).into_owned();
            noted
                .lock()
                .unwrap()
                .push(format!("udp {}", text.trim_end()));
            let _ = socket.send_to(format!("{tag}:{text}").as_bytes(), from);
        }
    });
    seen
}

/// What a listener saw, once it has noted `want` things or a few seconds
/// have passed.
fn settled(seen: &Seen, want: usize) -> Vec<String> {
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= want {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    seen.lock().unwrap().clone()
}

// spec(NETWORK): **Your host.** Services on your host's loopback interface are reachable only as `host.ap.internal`, and only when a `+` rule names it.
// spec(NETWORK): A connection is attributed to the name behind the address it connects to, which is how host rules apply to traffic without a server name, such as SSH.
// spec(NETWORK): **UDP** follows the same rules, with names attributed through the resolver.
#[test]
fn a_rule_for_your_host_reaches_its_loopback_over_tcp_and_udp() {
    let scratch = Scratch::new("your-host");
    scratch.build("reaching", "NETWORK -\nNETWORK +host.ap.internal\n");
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let udp = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let tcp_seen = tcp_echo(tcp, "host");
    let udp_seen = udp_echo(udp, "host");

    let (run, name) = scratch.run("reaching");
    let lines = scratch.try_network(
        &name,
        &format!("tcp tcp host.ap.internal {port}\nudp udp host.ap.internal {port}\n"),
    );
    assert_eq!(lines, ["tcp host:ping", "udp host:ping"]);
    // The guest's end closed when its shell did, and the close reached the
    // listener.
    assert_eq!(settled(&tcp_seen, 2), ["tcp ping", "closed"]);
    assert_eq!(settled(&udp_seen, 1), ["udp ping"]);
    scratch.stop(&name, run);
}

/// Runs an agent with `network`, which mustn't reach this host's loopback,
/// tries it as `host.ap.internal` and by literal loopback addresses, and
/// fails if a listener there saw anything.
fn unreachable(test: &str, network: &str) {
    let scratch = Scratch::new(test);
    scratch.build(test, network);
    let tcp4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp4.local_addr().unwrap().port();
    let tcp6 = TcpListener::bind(("::1", port)).unwrap();
    let udp4 = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let seen = [
        tcp_echo(tcp4, "host"),
        tcp_echo(tcp6, "host"),
        udp_echo(udp4, "host"),
    ];

    let (run, name) = scratch.run(test);
    let lines = scratch.try_network(
        &name,
        &format!(
            "tcp tcp host.ap.internal {port}\n\
             udp udp host.ap.internal {port}\n\
             tcp tcp-v4 127.0.0.1 {port}\n\
             tcp tcp-mapped ::ffff:127.0.0.1 {port}\n\
             tcp tcp-v6 ::1 {port}\n\
             udp udp-v4 127.0.0.1 {port}\n"
        ),
    );
    assert_eq!(
        lines,
        [
            "tcp failed",
            "udp failed",
            "tcp-v4 failed",
            "tcp-mapped failed",
            "tcp-v6 failed",
            "udp-v4 failed",
        ]
    );
    thread::sleep(Duration::from_millis(500));
    for seen in &seen {
        let seen = seen.lock().unwrap();
        assert!(seen.is_empty(), "the host's loopback saw {seen:?}");
    }
    scratch.stop(&name, run);
}

#[test]
fn your_host_is_unreachable_without_a_rule_that_names_it() {
    unreachable("closed", "NETWORK -\n");
}

#[test]
fn your_host_is_unreachable_under_an_open_default() {
    unreachable("open", "NETWORK +\n");
}

#[test]
fn your_host_is_unreachable_when_a_rule_denies_it() {
    unreachable("denied", "NETWORK +\nNETWORK -host.ap.internal\n");
}

// spec(NETWORK): The proxy refuses any other connection whose real destination is a loopback or unspecified address, including IPv4-mapped forms, and an IP rule can't allow one.
#[test]
fn an_ip_rule_can_t_reach_your_host_s_loopback() {
    unreachable(
        "literal",
        "NETWORK -\nNETWORK +127.0.0.0/8\nNETWORK +[::1]\nNETWORK +[::ffff:127.0.0.0]/104\n",
    );
}

/// The processes `pid` started, and theirs, as `/proc` lists them.
fn descendants(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut next = vec![pid];
    while let Some(pid) = next.pop() {
        let tasks = fs::read_dir(format!("/proc/{pid}/task"));
        for task in tasks.into_iter().flatten() {
            let children = fs::read_to_string(task.unwrap().path().join("children"));
            for child in children.unwrap_or_default().split_whitespace() {
                let child = child.parse().unwrap();
                found.push(child);
                next.push(child);
            }
        }
    }
    found
}

/// Whether a process under `root` that started as the network process
/// still runs, and isn't a zombie.
fn network_process_runs(root: u32) -> bool {
    descendants(root).into_iter().any(|pid| {
        let started = fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|cmdline| cmdline.starts_with(b"ap-network\0"));
        let state = fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let alive = state
            .and_then(|stat| Some(stat.rsplit_once(") ")?.1.chars().next()? != 'Z'))
            .unwrap_or(false);
        started && alive
    })
}

#[test]
fn a_network_that_reaches_nothing_still_answers_the_guest() {
    let scratch = Scratch::new("nothing");
    scratch.build(
        "nothing",
        "NETWORK -\nNETWORK -api.anthropic.com\nNETWORK -claude.ai\n\
         NETWORK -console.anthropic.com\nNETWORK -platform.claude.com\n",
    );
    let (run, name) = scratch.run("nothing");
    let lines = scratch.try_network(
        &name,
        "timeout 10 getent ahostsv4 api.anthropic.com; echo \"lookup $?\"\n\
         tcp tcp 192.0.2.1 443\n",
    );
    // getent's status for a name that doesn't exist, which the resolver
    // said at once, and not the timeout's.
    assert_eq!(lines, ["lookup 2", "tcp failed"]);
    assert!(network_process_runs(run.0.id()), "the network process died");
    scratch.stop(&name, run);
}

/// The port every listener in the namespace takes.
const PORT: u16 = 8000;
/// The host's DNS servers in the namespace, before and after the change.
const FIRST_DNS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 53);
const SECOND_DNS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 54);
/// Where `svc.test` is before and after the change, and where `deny.test`
/// is.
const FIRST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const SECOND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 11);
const DENIED: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 12);

// spec(NETWORK): For an address from the pool, the proxy resolves the name itself and connects only to the addresses it gets.
// spec(NETWORK): It resolves through your host's resolver configuration as it is at each connection, so a run follows your host to another network.
// spec(NETWORK): **IP rules** are checked against the real destination address.
#[test]
fn a_run_keeps_resolving_after_the_host_s_resolver_configuration_changes() {
    if let Some(root) = env::var_os(INSIDE_VARIABLE) {
        inside(&Scratch::open(Path::new(&root)));
        return;
    }
    let scratch = Scratch::new("names");
    scratch.build(
        "resolving",
        &format!(
            "NETWORK -\nNETWORK +svc.test\nNETWORK +loop.test\nNETWORK +deny.test\nNETWORK -{DENIED}\n"
        ),
    );
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--mount"])
        .arg(env::current_exe().unwrap())
        .args([NAMES_TEST, "--exact", "--nocapture", "--test-threads=1"])
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

fn run_ok(program: &str, args: &[&str]) {
    let status = Command::new(program).args(args).status().unwrap();
    assert!(status.success(), "{program} {args:?}: {status:?}");
}

/// The check, in the namespace, on the agent that `scratch` built.
fn inside(scratch: &Scratch) {
    run_ok("ip", &["link", "set", "lo", "up"]);
    for address in [FIRST_DNS, SECOND_DNS, FIRST, SECOND, DENIED] {
        run_ok(
            "ip",
            &["addr", "add", &format!("{address}/32"), "dev", "lo"],
        );
    }
    // The host's resolver configuration is a file of this check's own.
    let resolv = scratch.root.join("resolv.conf");
    fs::write(&resolv, format!("nameserver {FIRST_DNS}\n")).unwrap();
    run_ok(
        "mount",
        &["--bind", resolv.to_str().unwrap(), "/etc/resolv.conf"],
    );
    dns(FIRST_DNS, FIRST);
    dns(SECOND_DNS, SECOND);
    let first = tcp_echo(TcpListener::bind((FIRST, PORT)).unwrap(), "first");
    let second = tcp_echo(TcpListener::bind((SECOND, PORT)).unwrap(), "second");
    let first_udp = udp_echo(UdpSocket::bind((FIRST, PORT)).unwrap(), "first");
    let denied = tcp_echo(TcpListener::bind((DENIED, PORT)).unwrap(), "denied");
    let looped = tcp_echo(TcpListener::bind(("127.0.0.1", PORT)).unwrap(), "loop");

    let (run, name) = scratch.run("resolving");
    let lines = scratch.try_network(
        &name,
        &format!(
            "tcp svc svc.test {PORT}\nudp svc-udp svc.test {PORT}\n\
             tcp loop loop.test {PORT}\ntcp deny deny.test {PORT}\n"
        ),
    );
    assert_eq!(
        lines,
        [
            "svc first:ping",
            "svc-udp first:ping",
            "loop failed",
            "deny failed"
        ]
    );
    // The host moves to another network, with another resolver.
    fs::write(&resolv, format!("nameserver {SECOND_DNS}\n")).unwrap();
    let lines = scratch.try_network(&name, &format!("tcp svc svc.test {PORT}\n"));
    assert_eq!(lines, ["svc second:ping"]);

    assert_eq!(settled(&first, 2), ["tcp ping", "closed"]);
    assert_eq!(settled(&second, 2), ["tcp ping", "closed"]);
    assert_eq!(settled(&first_udp, 1), ["udp ping"]);
    for seen in [&denied, &looped] {
        let seen = seen.lock().unwrap();
        assert!(seen.is_empty(), "a refused destination saw {seen:?}");
    }
    scratch.stop(&name, run);
}

/// A DNS server at `at`, which answers `svc.test` with `svc`, `loop.test`
/// with the loopback, and `deny.test` with [`DENIED`]. Every other name
/// doesn't exist, and every other type of record has no answer.
fn dns(at: Ipv4Addr, svc: Ipv4Addr) {
    let socket = UdpSocket::bind((at, 53)).unwrap();
    thread::spawn(move || {
        let mut buffer = [0; 512];
        while let Ok((len, from)) = socket.recv_from(&mut buffer) {
            if let Some(answer) = dns_answer(&buffer[..len], svc) {
                let _ = socket.send_to(&answer, from);
            }
        }
    });
}

fn dns_answer(query: &[u8], svc: Ipv4Addr) -> Option<Vec<u8>> {
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let length = usize::from(*query.get(at)?);
        if length == 0 {
            break;
        }
        let label = query.get(at + 1..at + 1 + length)?;
        labels.push(String::from_utf8_lossy(label).to_lowercase());
        at += 1 + length;
    }
    let end = at + 5;
    let kind = u16::from_be_bytes([*query.get(at + 1)?, *query.get(at + 2)?]);
    let address = match labels.join(".").as_str() {
        "svc.test" => Some(svc),
        "loop.test" => Some(Ipv4Addr::LOCALHOST),
        "deny.test" => Some(DENIED),
        _ => None,
    };
    let mut answer = query.get(..end)?.to_vec();
    // A response, recursion desired and available, and the name's error.
    answer[2] = 0x81;
    answer[3] = if address.is_some() { 0x80 } else { 0x83 };
    answer[6..12].copy_from_slice(&[0; 6]);
    if let (Some(address), 1) = (address, kind) {
        answer[7] = 1;
        answer.extend([0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 0, 0, 4]);
        answer.extend(address.octets());
    }
    Some(answer)
}

const SIGSYS: i32 = 31;
const OWNER_TEST: &str = "connecting_network_process";
const ATTEMPT_VARIABLE: &str = "AP_CONNECTS_ATTEMPT";
const TARGET_VARIABLE: &str = "AP_CONNECTS_TARGET";
const LINK_VARIABLE: &str = "AP_CONNECTS_LINK";
const PEER_VARIABLE: &str = "AP_CONNECTS_PEER";
/// How long the check waits for a confined process to say something.
const QUIET: Duration = Duration::from_secs(3);
const PR_SET_DUMPABLE: c_int = 4;
const F_SETFD: c_int = 2;
const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const SOCK_SEQPACKET: c_int = 5;
const SOCK_CLOEXEC: c_int = 0o2_000_000;

unsafe extern "C" {
    fn fork() -> c_int;
    fn _exit(status: c_int) -> !;
    fn prctl(option: c_int, ...) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, fds: *mut c_int) -> c_int;
}

/// A link like the one `ap run` makes.
fn link() -> (UnixDatagram, UnixDatagram) {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    let made = unsafe { socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
    assert_eq!(made, 0, "{}", io::Error::last_os_error());
    // SAFETY: both descriptors are new, and nothing else owns them.
    unsafe {
        (
            UnixDatagram::from_raw_fd(fds[0]),
            UnixDatagram::from_raw_fd(fds[1]),
        )
    }
}

/// What the confined process tries.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Attempt {
    /// Connects to a listener on this host, which it may.
    Connect,
    Fork,
    Run,
    Open,
    /// Makes a socket that isn't an internet one.
    UnixSocket,
}

const ATTEMPTS: [Attempt; 5] = [
    Attempt::Connect,
    Attempt::Fork,
    Attempt::Run,
    Attempt::Open,
    Attempt::UnixSocket,
];

impl Attempt {
    fn name(self) -> &'static str {
        match self {
            Attempt::Connect => "connect",
            Attempt::Fork => "fork",
            Attempt::Run => "run",
            Attempt::Open => "open",
            Attempt::UnixSocket => "unix-socket",
        }
    }

    fn from_name(name: &str) -> Self {
        ATTEMPTS
            .into_iter()
            .find(|attempt| attempt.name() == name)
            .unwrap()
    }

    /// Makes the attempt, and says how the call came back. A process the
    /// filter kills never says anything.
    fn make(self, target: &Path, peer: SocketAddr) -> String {
        match self {
            Attempt::Connect => {
                let made = TcpStream::connect(peer).map(drop);
                format!("went through: {made:?}")
            }
            Attempt::Fork => {
                // SAFETY: the child only calls `_exit`, which is safe after
                // a fork.
                match unsafe { fork() } {
                    -1 => format!("went through: {}", io::Error::last_os_error()),
                    0 => unsafe { _exit(0) },
                    child => format!("went through: started process {child}"),
                }
            }
            // The target isn't executable, so a program the kernel starts
            // comes back with an error.
            Attempt::Run => format!("went through: {}", Command::new(target).exec()),
            Attempt::Open => format!("went through: {:?}", File::open(target).map(drop)),
            Attempt::UnixSocket => {
                // SAFETY: a plain call, whose result is only printed.
                let fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
                format!("went through: {fd}")
            }
        }
    }
}

/// An ARP request from the guest for the gateway's address.
fn arp_request() -> Vec<u8> {
    let mut frame = net::GATEWAY_MAC.to_vec();
    frame.extend(net::MAC);
    frame.extend([0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1]);
    frame.extend(net::MAC);
    frame.extend(net::IPV4.octets());
    frame.extend([0; 6]);
    frame.extend(net::IPV4_GATEWAY.octets());
    frame
}

/// The process that confines itself as the network process does when it
/// may connect, makes one attempt, and then serves its link. It does
/// nothing in a normal test run.
#[test]
fn connecting_network_process() {
    let Some(fd) = env::var_os(LINK_VARIABLE) else {
        return;
    };
    // The filter kills this process on purpose, so it doesn't dump core.
    // SAFETY: the call matches the prctl signature for this option.
    assert_eq!(unsafe { prctl(PR_SET_DUMPABLE, 0 as c_ulong) }, 0);
    let attempt = Attempt::from_name(&env::var(ATTEMPT_VARIABLE).unwrap());
    let target = PathBuf::from(env::var_os(TARGET_VARIABLE).unwrap());
    let peer: SocketAddr = env::var(PEER_VARIABLE).unwrap().parse().unwrap();
    let fd: i32 = fd.to_str().unwrap().parse().unwrap();
    // SAFETY: the parent passed this descriptor for this process alone.
    let link = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut stack = ap::network::stack();
    ap::network::confine_connecting(link.as_fd()).unwrap();
    let attempting = format!("attempting {}", attempt.name());
    (&link).write_all(attempting.as_bytes()).unwrap();
    let outcome = attempt.make(&target, peer);
    (&link).write_all(outcome.as_bytes()).unwrap();
    let mut resolver = ap::network::resolver(&["--".into(), "+".into()]).unwrap();
    ap::network::serve(&link, &mut stack, &mut resolver).unwrap();
}

/// Makes one attempt in its own process, and returns how that process
/// ended, and what it said on its link, with `answered ARP` once it answers
/// the request this side sends after its outcome.
fn attempt(directory: &Path, attempt: Attempt, peer: SocketAddr) -> (ExitStatus, Vec<String>) {
    let (mine, theirs) = link();
    mine.set_read_timeout(Some(QUIET)).unwrap();
    let fd = theirs.as_raw_fd();
    let target = directory.join("unrelated");
    fs::write(&target, "a host file the network process doesn't need").unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([OWNER_TEST, "--exact", "--test-threads=1"])
        .env(ATTEMPT_VARIABLE, attempt.name())
        .env(TARGET_VARIABLE, &target)
        .env(PEER_VARIABLE, peer.to_string())
        .env(LINK_VARIABLE, fd.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure only clears close-on-exec on a descriptor this
    // process holds, which is safe between fork and the new program.
    unsafe {
        command.pre_exec(move || match fcntl(fd, F_SETFD, 0) {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        });
    }
    let mut child = command.spawn().unwrap();
    drop(theirs);
    let mut said = Vec::new();
    let mut buffer = [0; 2048];
    // A read of nothing is the end of the link: the process has died.
    while let Ok(len @ 1..) = mine.recv(&mut buffer) {
        let message = &buffer[..len];
        if message.get(12..14) == Some(&[0x08, 0x06]) {
            said.push("answered ARP".to_owned());
            break;
        }
        said.push(String::from_utf8_lossy(message).into_owned());
        if said.len() == 2 {
            let _ = mine.send(&arp_request());
        }
    }
    drop(mine);
    (child.wait().unwrap(), said)
}

#[test]
fn a_network_process_that_may_connect_still_can_t_start_a_process_or_open_a_file() {
    let directory =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("connects-confined-{}", process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = listener.local_addr().unwrap();
    let mut failures = Vec::new();
    for each in ATTEMPTS {
        let (exit, said) = attempt(&directory, each, peer);
        println!("{}: {exit:?}, {said:?}", each.name());
        let name = each.name();
        let attempting = format!("attempting {name}");
        let passed = match each {
            Attempt::Connect => {
                said == [attempting.as_str(), "went through: Ok(())", "answered ARP"]
            }
            _ => exit.signal() == Some(SIGSYS) && said == [attempting.as_str()],
        };
        if !passed {
            failures.push(format!("{name}: {exit:?}, said {said:?}"));
        }
    }
    let _ = fs::remove_dir_all(&directory);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
