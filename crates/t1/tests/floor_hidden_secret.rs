//! The floor: a hidden secret can't be read from the guest.
//!
//! A real `ap run` gets a hidden secret's value from its environment, with
//! the secret bound to `host.ap.internal`, where the fake upstream answers
//! HTTPS. From inside the guest, with `ap exec`, the check looks for the
//! value in every process's environment and arguments and in every file,
//! and sends the placeholder to the upstream in each place a request can
//! hold it. On the host, it reads the environment and arguments of each
//! process that `ap run` started.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ap::fetch;
use ap::store::Store;
use t1::{guest_binary, kernel, os};

/// How long `ap` may take, in seconds, before `timeout` stops it, and the VM
/// it holds with it.
const TIMEOUT: &str = "240";
/// How long the fake upstream may run, in seconds.
const UPSTREAM_TIMEOUT: &str = "600";
/// The value, which no check below may hand to the guest whole.
const VALUE: &str = "floor-hidden-value-6b2e0d9a41";
/// A pattern that matches the value alone, but doesn't hold it, so that a
/// search for it doesn't find itself.
const PATTERN: &str = "floor-hidden-valu[e]-6b2e0d9a41";

/// A home of its own, with the kernel and the OS layer in its store and the
/// fake harness in its download cache, so that `ap` reads none of the owner's
/// files and fetches nothing.
struct Scratch {
    root: PathBuf,
    ap: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("floor-hidden-secret-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        Store::new(root.join("home/.local/share/ap"))
            .put(
                fetch::kernel().sha256,
                File::open(kernel::vmlinux()).unwrap(),
            )
            .unwrap();
        os::put(&root.join("home/.local/share/ap"));
        fs::create_dir_all(root.join("runtime")).unwrap();
        Scratch {
            root,
            ap: guest_binary::ap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Builds `name`, a Claude Code agent with `lines` too, whose fake
    /// harness reports, then sleeps.
    fn build(&self, name: &str, lines: &str) {
        let fake = guest_binary::fake_harness();
        let sum = Command::new("sha256sum").arg(&fake).output().unwrap();
        let digest = String::from_utf8(sum.stdout).unwrap()[..64].to_owned();
        let cache = self.home().join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(&fake, cache.join(&digest)).unwrap();
        let dir = self.root.join(name);
        fs::create_dir_all(dir.join("notes")).unwrap();
        let agentfile =
            format!("AGENT claude:0.0.0\nOS minimal\n{lines}ENV FAKE_HARNESS_SLEEP 300\n");
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
        let stdout = String::from_utf8_lossy(&built.stdout);
        assert!(stdout.starts_with("hit OS minimal\n"), "{built:?}");
    }

    /// The static `ap`, with only `HOME`, `XDG_RUNTIME_DIR`, and
    /// `SSL_CERT_FILE` in its environment, which names the upstream's
    /// authority, under a timeout.
    fn command(&self) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["-k", "10", TIMEOUT])
            .arg(&self.ap)
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("SSL_CERT_FILE", self.root.join("upstream/ca.pem"));
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

/// A process this test started, killed and reaped when it's dropped, so a
/// failed test leaves no VM behind.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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
fn started_as(root: u32, program: &str) -> u32 {
    let argv0 = format!("{program}\0");
    let found: Vec<u32> = descendants(root)
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline.starts_with(argv0.as_bytes()))
        })
        .collect();
    assert_eq!(found.len(), 1, "{program}: {found:?}");
    found[0]
}

fn holds(bytes: &[u8], wanted: &str) -> bool {
    bytes.windows(wanted.len()).any(|w| w == wanted.as_bytes())
}

fn count(text: &str, wanted: &str) -> usize {
    text.matches(wanted).count()
}

/// The guest's checks. `search` prints, for an extended pattern, how many
/// processes' environments and arguments, and how many files, match it. A
/// file and a process's arguments hold the placeholder too, so the search
/// for it shows that each search can find what it looks for. `send` sends one
/// request over TLS to your host, holding the placeholder in the query,
/// in `Authorization`, `X-Api-Key`, and `X-Other`, and in the body, and
/// prints its label and the answer's status line.
fn script(port: u16) -> String {
    format!(
        r#"
search() {{
  environ=0
  cmdline=0
  for f in /proc/[0-9]*/environ; do
    grep -qaE -- "$2" "$f" 2>/dev/null && environ=$((environ + 1))
  done
  for f in /proc/[0-9]*/cmdline /proc/cmdline; do
    grep -qaE -- "$2" "$f" 2>/dev/null && cmdline=$((cmdline + 1))
  done
  files=$(grep -rlsaE -D skip --exclude-dir=proc --exclude-dir=sys -- "$2" / | wc -l)
  echo "$1 environ $environ cmdline $cmdline files $files"
}}
send() {{
  request=$(printf 'POST /%s?key=%s HTTP/1.1\r\nHost: %s\r\nAuthorization: Bearer %s\r\nX-Api-Key: %s\r\nX-Other: %s\r\nContent-Length: %s\r\n\r\n%s' \
    "$1" "$SECRET" "$2" "$SECRET" "$SECRET" "$SECRET" "${{#SECRET}}" "$SECRET")
  answer=$({{ printf '%s' "$request"; sleep 3; }} |
    timeout 20 openssl s_client -quiet -no_ign_eof -servername host.ap.internal \
      -connect host.ap.internal:{port} 2>/dev/null | head -1 | tr -d '\r')
  echo "$1 $answer"
}}
echo "held ${{#SECRET}}"
printf '%s' "$SECRET" >"$(mktemp)"
bash -c 'sleep 60; true' holder "$SECRET" &
holder=$!
until grep -qa -- "$SECRET" "/proc/$holder/cmdline" 2>/dev/null; do sleep 0.1; done
search placeholder "$SECRET"
search value '{PATTERN}'
kill "$holder"
send bound host.ap.internal:{port}
send other other.example
"#
    )
}

// spec(SECRET): `SECRET` gives the agent a credential that, by default, it can use but never read.
// spec(SECRET): The real value is never sent to any other host.
#[test]
fn floor_hidden_secret() {
    let scratch = Scratch::new();
    let directory = scratch.root.join("upstream");
    let (_upstream, port) = start_upstream(&directory);
    scratch.build(
        "hidden",
        "NETWORK -\nNETWORK +host.ap.internal\n\
         SECRET SECRET @host.ap.internal\nSHARE ./notes ro\n",
    );

    let mut run = Started(
        scratch
            .command()
            .env("SECRET", VALUE)
            .args(["run", "hidden", "-p", "Use the secret."])
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
    assert!(!report.contains(VALUE), "{report}");
    let listed = String::from_utf8(scratch.ap(&["ps"]).stdout).unwrap();
    let row = listed.lines().nth(1).expect("ap ps lists the run");
    let name = row.split_whitespace().next().unwrap().to_owned();

    // On the host, no process that `ap run` started holds the value in its
    // arguments or its environment. Each environment holds `HOME`, so an
    // unreadable one doesn't pass for one without the value.
    let ap_run = started_as(run.0.id(), scratch.ap.to_str().unwrap());
    let environment = fs::read(format!("/proc/{ap_run}/environ")).unwrap();
    assert!(holds(&environment, VALUE), "the test passed no value");
    for program in [
        "ap-run-process",
        "ap-share-server",
        "ap-network",
        "ap-run-cleanup",
    ] {
        let pid = started_as(ap_run, program);
        let environment = fs::read(format!("/proc/{pid}/environ")).unwrap();
        assert!(holds(&environment, "HOME="), "{program} has no environment");
        for (file, bytes) in [
            ("cmdline", fs::read(format!("/proc/{pid}/cmdline")).unwrap()),
            ("environ", environment),
        ] {
            assert!(!holds(&bytes, VALUE), "{program}'s {file} holds the value");
        }
    }

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
    assert_eq!(lines.len(), 5, "{stdout}");
    // The guest holds a placeholder in the secret's variable, which is in
    // the environment of the processes that have it, and in the arguments
    // and the file the script gave it.
    assert_eq!(lines[0], "held 32", "{stdout}");
    let placeholder: Vec<usize> = (lines[1].split_whitespace())
        .filter_map(|word| word.parse().ok())
        .collect();
    assert!(lines[1].starts_with("placeholder "), "{stdout}");
    assert!(placeholder.iter().all(|&found| found > 0), "{stdout}");
    // And the value nowhere.
    assert_eq!(lines[2], "value environ 0 cmdline 0 files 0", "{stdout}");
    assert_eq!(lines[3], "bound HTTP/1.1 404 Not Found", "{stdout}");
    assert_eq!(lines[4], "other HTTP/1.1 404 Not Found", "{stdout}");

    let stopped = scratch.ap(&["stop", &name]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
    let _ = run.0.wait();

    // The bound host got the value in the allowed headers, and the
    // placeholder in the query, `X-Other`, and the body. A request whose
    // `Host` names another host got the placeholder in every place.
    let requests = kept(&directory.join("requests"), 2);
    let bound = (requests.iter())
        .find(|request| request.starts_with("POST /bound?"))
        .unwrap_or_else(|| panic!("{requests:?}"));
    assert!(
        bound.contains(&format!("\r\nAuthorization: Bearer {VALUE}\r\n")),
        "{bound}"
    );
    assert!(
        bound.contains(&format!("\r\nX-Api-Key: {VALUE}\r\n")),
        "{bound}"
    );
    assert_eq!(count(bound, VALUE), 2, "{bound}");
    let other = (requests.iter())
        .find(|request| request.starts_with("POST /other?"))
        .unwrap_or_else(|| panic!("{requests:?}"));
    assert_eq!(count(other, VALUE), 0, "{other}");
    let held = (other.split("key=").nth(1).unwrap())
        .split(' ')
        .next()
        .unwrap();
    assert_eq!(held.len(), 32, "{other}");
    assert_ne!(held, VALUE);
    assert_eq!(count(bound, held), 3, "{bound}");
    assert_eq!(count(other, held), 5, "{other}");
}

/// The requests the upstream kept, once there are `want` of them.
fn kept(directory: &Path, want: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut requests = Vec::new();
        for entry in fs::read_dir(directory).unwrap() {
            let mut text = String::new();
            File::open(entry.unwrap().path())
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            requests.push(text);
        }
        if requests.len() >= want || Instant::now() > deadline {
            assert_eq!(requests.len(), want, "{requests:?}");
            return requests;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
