//! The floor: a closed network stays closed. Every run gets a network
//! process, which ends each of the guest's connections and flows itself, and
//! refuses every one. Nothing the guest sends reaches the host or leaves it,
//! and the network process can't start a process, open a host file, or make
//! a socket of its own.
//!
//! The first check runs a real `ap run`, and tries the network from inside
//! the guest with `ap exec`. The second confines a process as the network
//! process confines itself, and makes one attempt in it, as
//! `floor_vmm_confined` does for the run process.

use std::env;
use std::ffi::{c_int, c_ulong};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, UdpSocket};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use ap::fetch;
use ap::store::Store;
use guest::net;
use t1::started::Started;
use t1::{guest_binary, kernel, os};

/// How long `ap` may take, in seconds, before `timeout` stops it, and the VM
/// it holds with it.
const TIMEOUT: &str = "240";
const SIGSYS: i32 = 31;
const OWNER_TEST: &str = "confined_network_process";
const ATTEMPT_VARIABLE: &str = "AP_FLOOR_ATTEMPT";
const CONFINE_VARIABLE: &str = "AP_FLOOR_CONFINE";
const TARGET_VARIABLE: &str = "AP_FLOOR_TARGET";
const LINK_VARIABLE: &str = "AP_FLOOR_LINK";
const TARGET_CONTENTS: &str = "a host file the network process doesn't need";
/// How long the check waits for a confined process to say something.
const QUIET: Duration = Duration::from_secs(3);

const PR_SET_DUMPABLE: c_int = 4;
const F_SETFD: c_int = 2;

unsafe extern "C" {
    fn fork() -> c_int;
    fn _exit(status: c_int) -> !;
    fn prctl(option: c_int, ...) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, fds: *mut c_int) -> c_int;
}

const AF_UNIX: c_int = 1;
const SOCK_SEQPACKET: c_int = 5;
const SOCK_CLOEXEC: c_int = 0o2_000_000;

/// A link like the one `ap run` makes: a pair of sockets that keep message
/// boundaries, and whose reads end once the other end closes.
fn link() -> (UnixDatagram, UnixDatagram) {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    let made = unsafe { socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
    assert_eq!(made, 0, "{}", io::Error::last_os_error());
    // SAFETY: both descriptors are new, and nothing else owns them. A
    // datagram socket's calls are the ones a seqpacket socket takes.
    unsafe {
        (
            UnixDatagram::from_raw_fd(fds[0]),
            UnixDatagram::from_raw_fd(fds[1]),
        )
    }
}

/// Built and fetched once, because the tests in this file run at once.
fn shared() -> &'static (PathBuf, PathBuf, PathBuf) {
    static PATHS: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    PATHS.get_or_init(|| {
        (
            guest_binary::ap(),
            guest_binary::fake_harness(),
            kernel::vmlinux(),
        )
    })
}

/// A home of its own, with the kernel and the OS layer in its store and the
/// fake harness in its download cache, so that `ap` reads none of the owner's
/// files and fetches nothing.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
}

impl Scratch {
    fn new(test: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("floor-closed-network-{test}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        Store::new(home.join(".local/share/ap"))
            .put(fetch::kernel().sha256, File::open(&shared().2).unwrap())
            .unwrap();
        os::put(&home.join(".local/share/ap"));
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        Scratch {
            root,
            home,
            runtime,
        }
    }

    /// Builds `name`, an agent with a closed network whose fake harness
    /// reports, then sleeps.
    fn build(&self, name: &str) {
        let fake = &shared().1;
        let sum = Command::new("sha256sum").arg(fake).output().unwrap();
        let digest = String::from_utf8(sum.stdout).unwrap()[..64].to_owned();
        let cache = self.home.join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(fake, cache.join(&digest)).unwrap();
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
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let approve = format!("--approve={}", approved.display());
        let built = self.ap(&["build", &approve, dir.to_str().unwrap()]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");
        // The store starts with the pinned layer, so no build downloads it.
        let stdout = String::from_utf8_lossy(&built.stdout);
        assert!(stdout.starts_with("hit OS minimal\n"), "{built:?}");
    }

    /// The static `ap`, with only `HOME` and `XDG_RUNTIME_DIR` in its
    /// environment, under a timeout.
    fn command(&self) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["-k", "10", TIMEOUT])
            .arg(&shared().0)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime);
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
        let _ = fs::remove_dir_all(&self.root);
    }
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

/// The one process under `root` with this `argv[0]`.
fn started_as(root: u32, program: &[u8]) -> u32 {
    let found: Vec<u32> = descendants(root)
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline.starts_with(program))
        })
        .collect();
    assert_eq!(found.len(), 1, "{program:?}: {found:?}");
    found[0]
}

/// Whether the process `pid` exists and isn't a zombie.
fn running(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(") ")?.1.chars().next()? != 'Z'))
        .unwrap_or(false)
}

fn status_field(status: &str, field: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|value| value.trim().parse().ok())
}

/// Every socket that `pid` holds, by the inode `/proc` names it by.
fn sockets(pid: u32) -> Vec<u64> {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|fd| {
            let target = fs::read_link(fd.unwrap().path()).ok()?;
            let inode = target.to_str()?.strip_prefix("socket:[")?;
            inode.strip_suffix(']')?.parse().ok()
        })
        .collect()
}

/// The inodes of every Unix socket on the host.
fn unix_sockets() -> Vec<u64> {
    fs::read_to_string("/proc/net/unix")
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(6)?.parse().ok())
        .collect()
}

/// The guest's attempts, each one line that names it and says how it came
/// back. `port` is where the host's loopback listens.
fn script(port: u16) -> String {
    let gateway4 = net::IPV4_GATEWAY;
    let gateway6 = net::IPV6_GATEWAY;
    format!(
        r#"
tcp() {{
  if out=$(timeout 15 bash -c "exec 3<>/dev/tcp/$2/$3" 2>&1); then
    echo "$1 connected"
  else
    echo "$1 ${{out##*: }}"
  fi
}}
udp() {{
  if exec 3<>"/dev/udp/$2/$3" && printf 'query' >&3; then
    if read -r -t 3 answer <&3; then echo "$1 answered"; else echo "$1 no answer"; fi
    exec 3<&-
  else
    echo "$1 not sent"
  fi
}}
tcp tcp-gateway-v4 {gateway4} {port}
tcp tcp-gateway-v6 {gateway6} {port}
tcp tcp-literal-v4 192.0.2.1 443
tcp tcp-literal-v6 2001:db8::1 443
udp udp-gateway-v4 {gateway4} {port}
udp udp-dns {gateway4} 53
udp udp-dns-v6 {gateway6} 53
udp udp-quic 192.0.2.1 443
timeout 60 getent ahostsv4 example.com >/dev/null
echo "lookup $?"
"#
    )
}

// spec(NETWORK): A bare `NETWORK -` denies every destination that no rule allows.
#[test]
fn floor_closed_network() {
    let scratch = Scratch::new("floor");
    scratch.build("closed");
    // The host's loopback listens, on both families, for anything the run
    // sends it.
    let tcp4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp4.local_addr().unwrap().port();
    let tcp6 = TcpListener::bind(("::1", port)).unwrap();
    let udp4 = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    for listener in [&tcp4, &tcp6] {
        listener.set_nonblocking(true).unwrap();
    }
    udp4.set_nonblocking(true).unwrap();

    let mut run = Started(
        scratch
            .command()
            .args(["run", "closed", "-p", "Try the network."])
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

    let network = started_as(run.0.id(), b"ap-network\0");
    let vm = started_as(run.0.id(), b"ap-run-process\0");
    let status = fs::read_to_string(format!("/proc/{network}/status")).unwrap();
    assert_eq!(status_field(&status, "Seccomp"), Some(2), "{status}");
    assert_eq!(status_field(&status, "NoNewPrivs"), Some(1), "{status}");

    let started = Instant::now();
    let tried = scratch.ap(&["exec", &name, "bash", "-c", &script(port)]);
    let stdout = String::from_utf8_lossy(&tried.stdout).into_owned();
    println!(
        "{stdout}{}took {:?}",
        String::from_utf8_lossy(&tried.stderr),
        started.elapsed()
    );
    assert_eq!(tried.status.code(), Some(0), "{tried:?}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            // The network process resets each connection at its SYN.
            "tcp-gateway-v4 Connection refused",
            "tcp-gateway-v6 Connection refused",
            "tcp-literal-v4 Connection refused",
            "tcp-literal-v6 Connection refused",
            // And drops each datagram, with no answer.
            "udp-gateway-v4 no answer",
            "udp-dns no answer",
            "udp-dns-v6 no answer",
            "udp-quic no answer",
            // getent's status for a name it couldn't look up.
            "lookup 2",
        ],
        "{stdout}"
    );

    // Nothing reached the host's loopback.
    for listener in [&tcp4, &tcp6] {
        let accepted = listener.accept().map(|(_, from)| from);
        assert!(
            matches!(&accepted, Err(error) if error.kind() == ErrorKind::WouldBlock),
            "the host's loopback got a connection: {accepted:?}"
        );
    }
    let mut buffer = [0; 64];
    let got = udp4.recv_from(&mut buffer).map(|(_, from)| from);
    assert!(
        matches!(&got, Err(error) if error.kind() == ErrorKind::WouldBlock),
        "the host's loopback got a datagram: {got:?}"
    );
    // The two processes that see the guest's frames hold only Unix
    // sockets, so no packet of the run's leaves the host. Each is confined,
    // so neither can make another.
    let unix = unix_sockets();
    for pid in [network, vm] {
        let held = sockets(pid);
        assert!(!held.is_empty(), "{pid} holds no socket");
        let other: Vec<_> = held.iter().filter(|inode| !unix.contains(inode)).collect();
        assert!(
            other.is_empty(),
            "{pid} holds a socket that isn't a Unix socket: {other:?}"
        );
    }

    let stopped = scratch.ap(&["stop", &name]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
    let _ = run.0.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(network) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!running(network), "the network process outlived its run");
}

/// What the confined process tries.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Attempt {
    /// Proves that a confined process still answers on its link.
    Nothing,
    Fork,
    Exec,
    Open,
    Socket,
}

const ATTEMPTS: [Attempt; 5] = [
    Attempt::Nothing,
    Attempt::Fork,
    Attempt::Exec,
    Attempt::Open,
    Attempt::Socket,
];

impl Attempt {
    fn name(self) -> &'static str {
        match self {
            Attempt::Nothing => "nothing",
            Attempt::Fork => "fork",
            Attempt::Exec => "exec",
            Attempt::Open => "open",
            Attempt::Socket => "socket",
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
    fn make(self, target: &Path) -> String {
        match self {
            Attempt::Nothing => "did nothing".to_owned(),
            Attempt::Fork => {
                // SAFETY: the child only calls `_exit`, which is safe after
                // a fork.
                match unsafe { fork() } {
                    -1 => format!("went through: {}", io::Error::last_os_error()),
                    0 => unsafe { _exit(0) },
                    child => format!("went through: started process {child}"),
                }
            }
            // The target isn't executable, so an exec the kernel runs comes
            // back with an error.
            Attempt::Exec => format!("went through: {}", Command::new(target).exec()),
            Attempt::Open => format!("went through: {:?}", File::open(target).map(drop)),
            Attempt::Socket => {
                let made = UdpSocket::bind("127.0.0.1:0").map(drop);
                format!("went through: {made:?}")
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

/// The process that confines itself as the network process does, makes one
/// attempt, and then serves its link. It says what it does on the link,
/// because it can't write anywhere else once confined. It does nothing in a
/// normal test run.
#[test]
fn confined_network_process() {
    let Some(fd) = env::var_os(LINK_VARIABLE) else {
        return;
    };
    // The filter kills this process on purpose, so it doesn't dump core.
    // SAFETY: the call matches the prctl signature for this option.
    assert_eq!(unsafe { prctl(PR_SET_DUMPABLE, 0 as c_ulong) }, 0);
    let attempt = Attempt::from_name(&env::var(ATTEMPT_VARIABLE).unwrap());
    let target = PathBuf::from(env::var_os(TARGET_VARIABLE).unwrap());
    let fd: i32 = fd.to_str().unwrap().parse().unwrap();
    // SAFETY: the parent passed this descriptor for this process alone.
    let link = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut stack = ap::network::stack();
    if env::var(CONFINE_VARIABLE).unwrap() == "on" {
        ap::network::confine(link.as_fd()).unwrap();
    }
    let attempting = format!("attempting {}", attempt.name());
    (&link).write_all(attempting.as_bytes()).unwrap();
    let outcome = attempt.make(&target);
    (&link).write_all(outcome.as_bytes()).unwrap();
    // A megabyte held in small blocks grows the heap, as serving a busy
    // guest does.
    let pages: Vec<Vec<u8>> = (0..256).map(|_| vec![1; 4096]).collect();
    std::hint::black_box(&pages);
    let mut resolver = ap::network::resolver(&["--".into(), "-".into()]).unwrap();
    ap::network::serve(&link, &mut stack, &mut resolver).unwrap();
}

/// Makes one attempt in its own process, and returns how that process
/// ended, and what it said on its link, with `answered ARP` once it answers
/// the request this side sends after its outcome.
fn attempt(directory: &Path, attempt: Attempt, confine: bool) -> (ExitStatus, Vec<String>) {
    let (mine, theirs) = link();
    mine.set_read_timeout(Some(QUIET)).unwrap();
    let fd = theirs.as_raw_fd();
    let target = directory.join("unrelated");
    fs::write(&target, TARGET_CONTENTS).unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([OWNER_TEST, "--exact", "--test-threads=1"])
        .env(ATTEMPT_VARIABLE, attempt.name())
        .env(CONFINE_VARIABLE, if confine { "on" } else { "off" })
        .env(TARGET_VARIABLE, &target)
        .env(LINK_VARIABLE, fd.to_string())
        // The real network process has one thread, whose heap grows with
        // `brk`. This one serves on a test's thread, whose own heap would
        // grow with `mprotect`, which the filter refuses, so every thread
        // shares the first heap.
        .env("GLIBC_TUNABLES", "glibc.malloc.arena_max=1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure only clears close-on-exec on a descriptor this
    // process holds, which is safe between fork and exec.
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
            // A process that died since gets no request.
            let _ = mine.send(&arp_request());
        }
    }
    // Without its link, the process ends.
    drop(mine);
    (child.wait().unwrap(), said)
}

/// Decides one attempt from how its process ended and what it said.
fn judge(attempt: Attempt, exit: ExitStatus, said: &[String]) -> Result<(), String> {
    let name = attempt.name();
    if said.first().map(String::as_str) != Some(&format!("attempting {name}")) {
        return Err(format!(
            "{name}: the process ended before the attempt: {exit:?}, said {said:?}"
        ));
    }
    if attempt == Attempt::Nothing {
        return match said {
            [_, outcome, answer] if outcome == "did nothing" && answer == "answered ARP" => Ok(()),
            _ => Err(format!(
                "{name}: the confined process didn't serve its link: {exit:?}, said {said:?}"
            )),
        };
    }
    if exit.signal() == Some(SIGSYS) && said.len() == 1 {
        return Ok(());
    }
    Err(format!(
        "{name}: the attempt went through: {exit:?}, said {said:?}"
    ))
}

/// Makes each attempt in its own process, and returns every failure.
fn check(label: &str, confine: bool) -> Result<(), String> {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("floor-closed-network-{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let mut failures = Vec::new();
    for each in ATTEMPTS {
        let (exit, said) = attempt(&directory, each, confine);
        println!("{}: {exit:?}, {said:?}", each.name());
        if let Err(failure) = judge(each, exit, &said) {
            failures.push(failure);
        }
    }
    let _ = fs::remove_dir_all(&directory);
    match failures.is_empty() {
        true => Ok(()),
        false => Err(failures.join("\n")),
    }
}

#[test]
fn the_network_process_can_t_start_a_process_open_a_file_or_make_a_socket() {
    check("confined", true).unwrap();
}

#[test]
fn the_check_fails_when_the_process_is_not_confined() {
    let error = check("unconfined", false).unwrap_err();
    for attempt in ["fork", "exec", "open", "socket"] {
        assert!(
            error.contains(&format!("{attempt}: the attempt went through")),
            "{error}"
        );
    }
    assert!(!error.contains("nothing:"), "{error}");
}

#[test]
fn the_judge_passes_only_a_process_killed_at_the_attempt() {
    let killed = ExitStatus::from_raw(SIGSYS);
    let said = |lines: &[&str]| {
        lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
    };
    let attempting = said(&["attempting fork"]);
    assert_eq!(judge(Attempt::Fork, killed, &attempting), Ok(()));
    assert!(judge(Attempt::Fork, killed, &[]).is_err());
    let through = said(&["attempting open", "went through: Ok(())"]);
    assert!(judge(Attempt::Open, killed, &through).is_err());
    let quiet = said(&["attempting nothing", "did nothing"]);
    assert!(judge(Attempt::Nothing, ExitStatus::from_raw(0), &quiet).is_err());
}
