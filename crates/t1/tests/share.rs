//! `ap run` with `ro` and `rw` shares: a static `ap` builds an agent whose
//! harness is the fake one, boots it with its shares mounted, and looks at
//! them from inside the guest with `ap exec`.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Output, Stdio};
use std::sync::OnceLock;

use ap::fetch;
use ap::store::Store;
use t1::{guest_binary, kernel};

/// How long one run may take, in seconds, before `timeout` stops `ap run`,
/// and the VM it holds with it.
const TIMEOUT: &str = "120";
const PROMPT: &str = "Look at the shares.";
/// The fake harness exits with this, so that a run's status is the harness's.
const STATUS: i32 = 3;

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

/// A home of its own, with the kernel in its store and the fake harness in
/// its download cache, so that `ap` reads none of the owner's files and
/// fetches nothing.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
    harness: String,
}

impl Scratch {
    fn new(test: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ap-share-{test}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Share sources are canonical, so the test's paths are too.
        let root = root.canonicalize().unwrap();
        let home = root.join("home");
        let store = Store::new(home.join(".local/share/ap"));
        store
            .put(fetch::kernel().sha256, File::open(&shared().2).unwrap())
            .unwrap();
        let harness = sha256(&shared().1);
        let cache = home.join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(&shared().1, cache.join(&harness)).unwrap();
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        Scratch {
            root,
            home,
            runtime,
            harness,
        }
    }

    /// The build context of the agent `name`.
    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Builds the agent `name` from the fake harness and `more`, approving
    /// what it asks.
    fn build(&self, name: &str, more: &str) {
        let agentfile = format!(
            "AGENT claude:0.0.0\nOS minimal\nNETWORK -\nENV FAKE_HARNESS_STATUS {STATUS}\n{more}"
        );
        let dir = self.dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), &agentfile).unwrap();
        fs::write(
            dir.join("Agentfile.lock"),
            format!("HARNESS claude 0.0.0 {}\n", self.harness),
        )
        .unwrap();
        let instructions = agentfile::parse(&agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let built = self.ap(&[
            "build",
            &format!("--approve={}", approved.display()),
            dir.to_str().unwrap(),
        ]);
        assert_eq!(built.status.code(), Some(0), "{built:?}");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["-k", "10", TIMEOUT])
            .arg(&shared().0)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null());
        command
    }

    fn ap(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs the agent headless, and returns the fake harness's report.
    fn run(&self, name: &str) -> String {
        let output = self.ap(&["run", name, "-p", PROMPT]);
        assert_eq!(output.status.code(), Some(STATUS), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.starts_with("{\"arguments\":"), "{stdout}");
        stdout
    }

    /// Starts a headless run of the agent `name` that lasts, and returns it,
    /// the fake harness's report, and the run's name.
    fn start(&self, name: &str) -> (Started, String, String) {
        let mut run = Started(
            self.command(&["run", name, "-p", PROMPT])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut report = String::new();
        BufReader::new(run.0.stdout.take().unwrap())
            .read_line(&mut report)
            .unwrap();
        let ps = String::from_utf8(self.ap(&["ps"]).stdout).unwrap();
        let line = ps.lines().nth(1).unwrap_or_default();
        let name = line.split_whitespace().next().unwrap_or_default();
        (run, report, name.to_owned())
    }

    /// Runs `sh -c script` in the run `name`.
    fn sh(&self, name: &str, script: &str) -> Output {
        self.ap(&["exec", name, "sh", "-c", script])
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

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum failed on {path:?}");
    String::from_utf8(output.stdout).unwrap()[..64].to_owned()
}

fn printed(output: &Output, stdout: &str) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        stdout,
        "{output:?}"
    );
}

fn failed(output: &Output, says: &str) {
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(says), "{output:?}");
}

// spec(SHARE): `SHARE` gives the agent a live view of a host file or directory while it runs.
// spec(SHARE): Without `<dest>`, the [mirror rule](#paths) decides the guest path.
// spec(SHARE): In `rw` shares, the files your host acts on are read-only to the agent.
#[test]
fn a_run_mounts_its_ro_and_rw_shares_at_their_places() {
    let scratch = Scratch::new("modes");
    let dir = scratch.dir("modes");
    fs::create_dir_all(dir.join("notes")).unwrap();
    fs::create_dir_all(dir.join("work/.git")).unwrap();
    fs::write(dir.join("notes/read.txt"), "from ro\n").unwrap();
    fs::write(dir.join("work/read.txt"), "from rw\n").unwrap();
    fs::write(dir.join("work/Agentfile"), "keep\n").unwrap();
    fs::write(dir.join("work/.git/config"), "keep\n").unwrap();
    scratch.build(
        "modes",
        "ENV FAKE_HARNESS_SLEEP 300\nSHARE ./notes /srv/notes ro\nSHARE ./work rw\n",
    );
    let (_run, report, name) = scratch.start("modes");
    let name = name.as_str();
    // With two shares, the harness starts in the agent's home.
    assert!(report.contains("\"directory\":\"/home/agent\""), "{report}");

    // The guest reads both, the `rw` share by the mirror rule.
    printed(&scratch.sh(name, "cat /srv/notes/read.txt"), "from ro\n");
    printed(&scratch.sh(name, "cat ~/work/read.txt"), "from rw\n");
    // Each is live: a change on the host shows at once.
    fs::write(dir.join("notes/read.txt"), "changed\n").unwrap();
    printed(&scratch.sh(name, "cat /srv/notes/read.txt"), "changed\n");

    // A write lands on the host only through `rw`.
    failed(
        &scratch.sh(name, "echo new > /srv/notes/new.txt"),
        "Read-only file system",
    );
    assert!(!dir.join("notes/new.txt").exists());
    printed(&scratch.sh(name, "echo new > ~/work/new.txt"), "");
    assert_eq!(
        fs::read_to_string(dir.join("work/new.txt")).unwrap(),
        "new\n"
    );

    // What the host acts on stays as it was.
    for path in ["~/work/.git/config", "~/work/Agentfile"] {
        failed(
            &scratch.sh(name, &format!("echo bad > {path}")),
            "Operation not permitted",
        );
    }
    assert_eq!(
        fs::read_to_string(dir.join("work/Agentfile")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("work/.git/config")).unwrap(),
        "keep\n"
    );
    let stopped = scratch.ap(&["stop", name]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
}

// spec(Run): **Working directory.** The harness starts in the first of these that applies.
// spec(Run): the [`WORKDIR`](#workdir)
// spec(Run): the Agentfile's only directory `SHARE` or `REPO`
#[test]
fn a_run_starts_in_its_workdir_or_its_only_directory_share() {
    let scratch = Scratch::new("workdir");
    for name in ["only", "deeper"] {
        fs::create_dir_all(scratch.dir(name).join("project/sub")).unwrap();
        fs::write(scratch.dir(name).join("project/sub/inner.txt"), "").unwrap();
    }
    scratch.build("only", "SHARE ./project ro\n");
    let report = scratch.run("only");
    assert!(
        report.contains("\"directory\":\"/home/agent/project\""),
        "{report}"
    );
    assert!(report.contains("\"files\":[\"sub/inner.txt\"]"), "{report}");

    scratch.build("deeper", "SHARE ./project ro\nWORKDIR ~/project/sub\n");
    let report = scratch.run("deeper");
    assert!(
        report.contains("\"directory\":\"/home/agent/project/sub\""),
        "{report}"
    );
    assert!(report.contains("\"files\":[\"inner.txt\"]"), "{report}");
}

// spec(SHARE): `SHARE` gives the agent a live view of a host file or directory while it runs.
// spec(SHARE): Without `<dest>`, the [mirror rule](#paths) decides the guest path.
#[test]
fn a_run_shows_a_shared_file_and_nothing_beside_it() {
    let scratch = Scratch::new("file");
    let dir = scratch.dir("file");
    fs::create_dir_all(dir.join("notes")).unwrap();
    fs::write(dir.join("notes/read.txt"), "from ro\n").unwrap();
    fs::write(dir.join("notes/beside.txt"), "beside\n").unwrap();
    fs::write(dir.join("work.txt"), "from rw\n").unwrap();
    scratch.build(
        "file",
        "ENV FAKE_HARNESS_SLEEP 300\nSHARE ./notes/read.txt /srv/read.txt ro\nSHARE ./work.txt rw\n",
    );
    let (_run, report, name) = scratch.start("file");
    let name = name.as_str();
    // A file share is no directory to start in.
    assert!(report.contains("\"directory\":\"/home/agent\""), "{report}");

    // The guest reads both, the `rw` share by the mirror rule, and nothing
    // beside either one.
    printed(&scratch.sh(name, "cat /srv/read.txt"), "from ro\n");
    printed(&scratch.sh(name, "cat ~/work.txt"), "from rw\n");
    printed(&scratch.sh(name, "ls -A /srv"), "read.txt\n");
    printed(
        &scratch.sh(
            name,
            "ls -A / ~ | grep -e beside -e notes -e Agentfile -e ap-share",
        ),
        "",
    );
    // Each is live: a change on the host shows at once.
    fs::write(dir.join("notes/read.txt"), "changed\n").unwrap();
    printed(&scratch.sh(name, "cat /srv/read.txt"), "changed\n");

    // A write lands on the host only through `rw`.
    failed(
        &scratch.sh(name, "echo new > /srv/read.txt"),
        "Read-only file system",
    );
    assert_eq!(
        fs::read_to_string(dir.join("notes/read.txt")).unwrap(),
        "changed\n"
    );
    printed(&scratch.sh(name, "echo new > ~/work.txt"), "");
    assert_eq!(fs::read_to_string(dir.join("work.txt")).unwrap(), "new\n");
    let stopped = scratch.ap(&["stop", name]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
}
