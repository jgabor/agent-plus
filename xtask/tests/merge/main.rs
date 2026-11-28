//! Runs `merge` against a bare origin. Wrappers on `PATH` for `git` and `cargo`
//! and a fake `gh` log every call, and can kill `xtask` at a chosen call, so
//! that each step of the merge is tested for recovery. No test reaches GitHub.

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[path = "../common/mod.rs"]
mod common;

mod basics;
mod counts;
mod floor;
mod land;
mod mains_xtask;
mod prepare;
mod refresh;
mod slot;
mod timeout;
mod tools;
mod waits;

const SPEC: &str = "# A\n\nOne. Two.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";
const NO_CHECK: &str = "pub fn f() {}\n";
const FAST: &str = "fn main() {}\n";
const SLOW: &str =
    "fn main() {\n    std::thread::sleep(std::time::Duration::from_millis(150));\n}\n";
const NO_BOOT: &str = "fn main() -> std::process::ExitCode {\n    if std::env::args().nth(1).as_deref() == Some(\"--version\") {\n        return std::process::ExitCode::SUCCESS;\n    }\n    eprintln!(\"usage: ap --version\");\n    std::process::ExitCode::from(2)\n}\n";
const BOOTS: &str = "fn main() {\n    if std::env::args().nth(1).as_deref() == Some(\"boot\") {\n        println!(\"7.3.3-ap.1\");\n    }\n}\n";
const JOURNEY: &str = "[[milestone]]\nname = \"m\"\nsummary = \"s\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"s\"\nmilestone = \"m\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const BUDGETS_EDITED: &str =
    "# edited\n[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";
const BUDGETS: &str =
    "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";
const BRANCH: &str = "7-x";
const HILL_NONE: &str = "Closes #7.\n\nHill: none, tooling.\n";
/// What a merge says when another run holds the slot.
const SLOT_WAIT: &str = "merge: PR 9 waits for the merge slot, which another run holds";
const NO_REBASE: &str = "the branch doesn't rebase onto `main`. Rebase it by hand, and push it";

/// Far longer than any command in these tests runs, timeouts and all, and far
/// shorter than the timeout of a merge whose proof runs these tests. So a
/// merge that hangs past its own timeouts fails its test, and doesn't stop
/// the merge that runs it.
const HUNG: Duration = Duration::from_secs(5 * 60);

/// Runs `command` as `Command::output` does, until it has ended and closed its
/// output, or until `hung` says that it hangs. Then it kills the command, and
/// gives what the command had written as the error. A merge's supervisor that
/// dies stops what the merge started.
fn output_unless(command: &mut Command, mut hung: impl FnMut() -> bool) -> Result<Output, Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (said, heard) = mpsc::channel();
    let pipes: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for (which, mut pipe) in pipes.into_iter().enumerate() {
        let said = said.clone();
        thread::spawn(move || {
            let mut chunk = [0; 4096];
            while let Ok(read @ 1..) = pipe.read(&mut chunk) {
                if said.send((which, chunk[..read].to_vec())).is_err() {
                    return;
                }
            }
        });
    }
    drop(said);
    let mut written = [Vec::new(), Vec::new()];
    let mut open = true;
    let hangs = loop {
        if open {
            match heard.recv_timeout(Duration::from_millis(100)) {
                Ok((which, chunk)) => written[which].extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
            }
        } else {
            thread::sleep(Duration::from_millis(10));
        }
        if !open && child.try_wait().unwrap().is_some() {
            break false;
        }
        if hung() {
            let _ = child.kill();
            break true;
        }
    };
    let status = child.wait().unwrap();
    for (which, chunk) in heard.try_iter() {
        written[which].extend(chunk);
    }
    let [stdout, stderr] = written;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    if hangs { Err(output) } else { Ok(output) }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn quoted(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn labels_json(labels: &[&str]) -> String {
    let items: Vec<String> = labels
        .iter()
        .map(|label| format!("{{\"name\":{}}}", quoted(label)))
        .collect();
    format!("[{}]", items.join(","))
}

fn record(proven: u32, passing: u32, budgets: &str) -> String {
    format!(
        "issue = 3\npr = 2\ncommit = \"c\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = {proven}, promises = 2 }}\njourney = {{ passing = {passing}, steps = 1 }}\nreason = \"\"\n\n[budgets]\n{budgets}"
    )
}

fn real(program: &str) -> String {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .unwrap();
    stdout(&output).trim().to_string()
}

struct Pr<'a> {
    state: &'a str,
    body: &'a str,
    labels: &'a [&'a str],
    base: &'a str,
    closes: &'a [u64],
    elsewhere: &'a [u64],
    reviews: &'a [(&'a str, &'a str)],
    fork: bool,
}

impl Default for Pr<'_> {
    fn default() -> Self {
        Pr {
            state: "OPEN",
            body: HILL_NONE,
            labels: &[],
            base: "main",
            closes: &[7],
            elsewhere: &[],
            reviews: &[],
            fork: false,
        }
    }
}

fn pr_json(pr: &Pr, state: &str) -> String {
    let issue = |repository: &str, number: &u64| {
        format!(
            "{{\"number\":{number},\"url\":\"https://github.com/{repository}/issues/{number}\"}}"
        )
    };
    let here = pr.closes.iter().map(|number| issue("o/r", number));
    let there = pr.elsewhere.iter().map(|number| issue("x/y", number));
    let closes: Vec<String> = here.chain(there).collect();
    let reviews: Vec<String> = pr
        .reviews
        .iter()
        .map(|(author, state)| {
            format!(
                "{{\"author\":{{\"login\":{}}},\"state\":{}}}",
                quoted(author),
                quoted(state)
            )
        })
        .collect();
    format!(
        "{{\"number\":9,\"state\":{},\"url\":\"https://github.com/o/r/pull/9\",\"isCrossRepository\":{},\"headRefName\":{},\"baseRefName\":{},\"body\":{},\"labels\":{},\"closingIssuesReferences\":[{}],\"reviews\":[{}]}}\n",
        quoted(state),
        pr.fork,
        quoted(BRANCH),
        quoted(pr.base),
        quoted(pr.body),
        labels_json(pr.labels),
        closes.join(","),
        reviews.join(",")
    )
}

/// Where these tests keep their repositories, variants, and tools. Each test
/// deletes its own directory first, which a run of the tests through `xtask`
/// can do, because it holds its target directory alone.
fn tmp() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

/// Where a variant of this xtask is built.
fn variant_binary(name: &str) -> PathBuf {
    tmp().join("variants").join(name).join("target/debug/xtask")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// This xtask with one edit, built once per run of these tests. It judges a
/// PR differently from the xtask that `main` holds in a test.
fn variant(name: &str, file: &str, from: &str, to: &str) -> PathBuf {
    let directory = tmp().join("variants").join(name);
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &directory.join("src"),
    );
    let manifest = directory.join("Cargo.toml");
    fs::write(
        &manifest,
        "[package]\nname = \"xtask\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    let source = directory.join("src").join(file);
    let text = fs::read_to_string(&source).unwrap();
    assert_eq!(text.matches(from).count(), 1, "{file}: {from}");
    fs::write(&source, text.replace(from, to)).unwrap();
    let output = Command::new("cargo")
        .env("PATH", common::path(&[]))
        .env("CARGO_HOME", directory.join("cargo-home"))
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(directory.join("target"))
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let binary = variant_binary(name);
    assert!(binary.is_file(), "{}", binary.display());
    binary
}

/// An xtask that lets a change to `.github/` through.
fn unprotected() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        variant(
            "unprotected",
            "waits.rs",
            "const WORKFLOWS: &str = \".github/\";",
            "const WORKFLOWS: &str = \".nothing/\";",
        )
    })
}

/// An xtask whose bench says that it waits for the activity lock once it holds
/// it, writes the file that `LIED` names, and then keeps it for two minutes.
fn lying() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        variant(
            "lying",
            "bench.rs",
            "    waiting(true)?;\n    let measuring = lock::exclusive(root);\n    waiting(false)?;\n",
            "    let measuring = lock::exclusive(root);\n    waiting(true)?;\n    let _ = std::fs::write(std::env::var_os(\"LIED\").unwrap_or_default(), \"\");\n    std::thread::sleep(std::time::Duration::from_secs(120));\n",
        )
    })
}

/// An xtask whose journey runner fails every step that passes.
fn failing() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        variant(
            "failing",
            "journey.rs",
            "Ok(status.success())",
            "Ok(!status.success())",
        )
    })
}

struct Repo {
    root: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
    temp: PathBuf,
}

impl Repo {
    /// `main` holds a base record with no proven promises and one passing
    /// step. Branch `7-x` adds `docs.txt`, and is pushed and checked out.
    fn new(name: &str) -> Repo {
        let base = record(0, 1, "");
        Repo::with(
            name,
            &[("harness/ledger/3.toml", base.as_str())],
            &[("docs.txt", "x\n")],
        )
    }

    fn with(name: &str, on_main: &[(&str, &str)], on_branch: &[(&str, &str)]) -> Repo {
        let parent = tmp().join("merge").join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            origin: parent.join("origin.git"),
            bin: parent.join("bin"),
            temp: parent.join("tmp"),
        };
        fs::create_dir_all(&repo.root).unwrap();
        fs::create_dir_all(&repo.bin).unwrap();
        fs::create_dir_all(&repo.temp).unwrap();
        repo.write_tools();
        repo.set_pr(&Pr::default());
        repo.set_issue(&["auto:merge"]);
        let ap = "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let demo = "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", SPEC),
            ("crates/ap/Cargo.toml", ap),
            ("crates/ap/src/main.rs", FAST),
            ("crates/demo/Cargo.toml", demo),
            ("crates/demo/src/lib.rs", NO_CHECK),
            ("harness/budgets.toml", BUDGETS),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/one.sh", "#!/bin/sh\nexit 0\n"),
        ];
        for (path, content) in files.iter().chain(on_main) {
            repo.write(path, content);
        }
        let output = repo
            .command("cargo")
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let origin = repo.origin.display().to_string();
        let output = repo
            .command("git")
            .args(["init", "-q", "--bare", "-b", "main", &origin])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["remote", "add", "origin", &origin]);
        repo.commit();
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", BRANCH]);
        for (path, content) in on_branch {
            repo.write(path, content);
        }
        repo.commit();
        repo.git(&["push", "-q", "origin", BRANCH]);
        fs::write(repo.bin.join("calls"), "").unwrap();
        repo
    }
    fn set_pr(&self, pr: &Pr) {
        fs::write(self.bin.join("pr.json"), pr_json(pr, pr.state)).unwrap();
        fs::write(self.bin.join("pr-merged.json"), pr_json(pr, "MERGED")).unwrap();
    }

    fn set_next_pr(&self, pr: &Pr) {
        fs::write(self.bin.join("pr-next.json"), pr_json(pr, pr.state)).unwrap();
    }

    fn set_issue(&self, labels: &[&str]) {
        fs::write(
            self.bin.join("issue.json"),
            format!(
                "{{\"number\":7,\"state\":\"OPEN\",\"body\":\"\",\"labels\":{},\"comments\":[]}}\n",
                labels_json(labels)
            ),
        )
        .unwrap();
    }

    /// An issue that `thread` claimed, with a lease that ends at `lease`.
    fn set_held_issue(&self, labels: &[&str], thread: &str, lease: &str) {
        let body = format!(
            "<!-- status -->\n**State:** working\n**Thread:** {thread}\n**Branch:** `7-x`\n**Lease until:** {lease}\n"
        );
        fs::write(
            self.bin.join("issue.json"),
            format!(
                "{{\"number\":7,\"state\":\"OPEN\",\"body\":\"\",\"labels\":{},\"comments\":[{{\"url\":\"https://github.com/o/r/issues/7#issuecomment-40\",\"author\":{{\"login\":\"o\"}},\"body\":{}}}]}}\n",
                labels_json(labels),
                quoted(&body)
            ),
        )
        .unwrap();
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("PATH", common::path(&[&self.bin]))
            .env("TMPDIR", &self.temp)
            // Every cargo on the machine shares the package-cache lock in the
            // owner's `~/.cargo`, and a proof that waits for it can reach its
            // timeout. The fixtures have no dependencies, so an empty home
            // serves.
            .env("CARGO_HOME", self.temp.join("cargo-home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD");
        // The gate runs these tests inside a GitHub Actions run, whose page a
        // status would otherwise link to.
        for name in [
            "GITHUB_ACTIONS",
            "GITHUB_SERVER_URL",
            "GITHUB_REPOSITORY",
            "GITHUB_RUN_ID",
            "GITHUB_STEP_SUMMARY",
        ] {
            command.env_remove(name);
        }
        command
    }

    /// What `command` wrote, as `Command::output` gives it. A command that
    /// runs for `HUNG` is killed, and fails the test.
    #[track_caller]
    fn output(&self, command: &mut Command) -> Output {
        let started = Instant::now();
        match output_unless(command, || started.elapsed() > HUNG) {
            Ok(output) => output,
            Err(output) => panic!(
                "killed after {HUNG:?}, because it hung\nstdout: {}\nstderr: {}\ncalls:\n{}",
                stdout(&output),
                stderr(&output),
                self.calls()
            ),
        }
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[track_caller]
    fn git(&self, args: &[&str]) -> String {
        let output = self.command("git").args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "change"]);
    }

    fn change_branch(&self, path: &str, content: &str) {
        self.write(path, content);
        self.commit();
        self.git(&["push", "-q", "origin", BRANCH]);
        self.forget_calls();
    }

    fn forget_calls(&self) {
        fs::write(self.bin.join("calls"), "").unwrap();
    }

    fn advance_main(&self, path: &str, content: &str) {
        self.git(&["checkout", "-q", "main"]);
        self.write(path, content);
        self.commit();
        self.git(&["push", "-q", "origin", "main"]);
        self.git(&["checkout", "-q", BRANCH]);
        self.forget_calls();
    }

    fn origin(&self, args: &[&str]) -> String {
        let directory = self.origin.display().to_string();
        let output = Command::new(real("git"))
            .args(["--git-dir", &directory])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output).trim().to_string()
    }

    fn head(&self) -> String {
        self.origin(&["rev-parse", "refs/heads/7-x"])
    }

    /// A shell command that moves a ref on the origin.
    fn update_ref(&self, name: &str, value: &str) -> String {
        format!(
            "'{}' --git-dir '{}' update-ref {name} {value}",
            real("git"),
            self.origin.display()
        )
    }

    /// The branch's commits, at the head that was squash-merged.
    fn commits_merged(&self) -> usize {
        let merged = fs::read_to_string(self.bin.join("merged")).unwrap();
        self.origin(&[
            "rev-list",
            "--count",
            &format!("refs/heads/main..{}", merged.trim()),
        ])
        .parse()
        .unwrap()
    }

    fn commits_on_branch(&self) -> usize {
        self.origin(&["rev-list", "--count", "refs/heads/main..refs/heads/7-x"])
            .parse()
            .unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.output(self.command(env!("CARGO_BIN_EXE_xtask")).args(args))
    }

    fn merge(&self) -> Output {
        self.xtask(&["merge", "9"])
    }

    fn prepare(&self) -> Output {
        self.xtask(&["prepare", "9"])
    }

    fn land(&self) -> Output {
        self.xtask(&["land", "9"])
    }

    /// Where `land` runs from: a checkout of the newest `main`.
    fn on_main(self) -> Repo {
        self.git(&["checkout", "-q", "main"]);
        self.forget_calls();
        self
    }

    fn land_running(&self, on: &str, command: &str) -> Output {
        self.output(
            self.command(env!("CARGO_BIN_EXE_xtask"))
                .args(["land", "9"])
                .env("RUN_ON", on)
                .env("RUN_COMMAND", command),
        )
    }

    /// The comments `prepare` posted on the PR, each ending in a blank line.
    fn pr_comments(&self) -> String {
        fs::read_to_string(self.bin.join("pr-comments")).unwrap_or_default()
    }

    /// What merges have left in the temporary directory.
    fn merge_trees(&self) -> Vec<String> {
        fs::read_dir(&self.temp)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("xtask-merge"))
            .collect()
    }

    fn merge_running(&self, on: &str, command: &str) -> Output {
        self.output(
            self.command(env!("CARGO_BIN_EXE_xtask"))
                .args(["merge", "9"])
                .env("RUN_ON", on)
                .env("RUN_COMMAND", command),
        )
    }

    fn merge_killed(&self, on: &str, when: &str) -> Output {
        self.output(
            self.command(env!("CARGO_BIN_EXE_xtask"))
                .args(["merge", "9"])
                .env("KILL_ON", on)
                .env("KILL_WHEN", when),
        )
    }
}

#[track_caller]
fn assert_merged(repo: &Repo, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert!(repo.bin.join("merged").exists(), "gh pr merge never ran");
}

#[track_caller]
fn assert_prepared(repo: &Repo, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert_left_prepared(repo);
}

/// The record is pushed and green, and nothing was squashed.
#[track_caller]
fn assert_left_prepared(repo: &Repo) {
    let head = repo.head();
    assert_eq!(
        repo.origin(&["diff-tree", "--no-commit-id", "--name-only", "-r", &head]),
        "harness/ledger/7.toml"
    );
    let calls = repo.calls();
    assert!(
        calls.contains(&format!(
            "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/gate"
        )),
        "{calls}"
    );
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!repo.bin.join("merged").exists());
}

#[track_caller]
fn assert_refused(repo: &Repo, output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(stderr(output).contains(message), "{}", stderr(output));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
}
