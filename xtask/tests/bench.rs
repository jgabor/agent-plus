//! Runs `bench` in real git repositories, on a workspace small enough to build
//! in moments. A fake `gh` on `PATH` stands in for GitHub, so no test reaches
//! it. The band is wide and the planted sleep long, so a busy machine doesn't
//! change a result.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const BUDGETS: &str = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n\n[[budget]]\nkey = \"binary_bytes\"\nname = \"b\"\n\n[[budget]]\nkey = \"run_ms\"\nname = \"run\"\ntarget = 150.0\n";
const BOOT_BUDGETS: &str = "[[budget]]\nkey = \"boot_ms\"\nname = \"boot\"\ntarget = 150.0\nband_pct = 10.0\nband_abs = 25.0\n";
const BOOTS: &str = "fn main() {\n    if std::env::args().nth(1).as_deref() == Some(\"boot\") {\n        println!(\"7.3.3-ap.1\");\n    }\n}\n";
const SLOW_BOOT: &str = "fn main() {\n    if std::env::args().nth(1).as_deref() == Some(\"boot\") {\n        std::thread::sleep(std::time::Duration::from_millis(150));\n        println!(\"7.3.3-ap.1\");\n    }\n}\n";
const SLOW_EXIT: &str = "fn main() {\n    if std::env::args().nth(1).as_deref() == Some(\"boot\") {\n        println!(\"7.3.3-ap.1\");\n        std::thread::sleep(std::time::Duration::from_millis(150));\n    }\n}\n";
const COUNTS_RUNS: &str = "use std::io::Write;\nfn main() {\n    if std::env::args().nth(1).as_deref() == Some(\"boot\") {\n        let home = std::env::var(\"HOME\").unwrap();\n        let mut runs = std::fs::OpenOptions::new().create(true).append(true).open(format!(\"{home}/runs\")).unwrap();\n        writeln!(runs, \"run\").unwrap();\n        println!(\"7.3.3-ap.1\");\n    }\n}\n";
const NO_BOOT: &str = "fn main() -> std::process::ExitCode {\n    if std::env::args().nth(1).as_deref() == Some(\"--version\") {\n        return std::process::ExitCode::SUCCESS;\n    }\n    eprintln!(\"usage: ap --version\");\n    std::process::ExitCode::from(2)\n}\n";
const BOOT_FAILS: &str = "fn main() -> std::process::ExitCode {\n    if std::env::args().nth(1).as_deref() == Some(\"--version\") {\n        return std::process::ExitCode::SUCCESS;\n    }\n    eprintln!(\"ap: no kvm\");\n    std::process::ExitCode::from(1)\n}\n";
const FAST: &str = "fn main() {}\n";
const BUILD_BUDGETS: &str = "[[budget]]\nkey = \"noop_build_ms\"\nname = \"build\"\ntarget = 100.0\nband_pct = 10.0\nband_abs = 25.0\n";
/// An `ap build` that fails unless it has an approval file and a directory
/// with an Agentfile, and counts its runs in its `HOME`.
const BUILDS: &str = "use std::io::Write;\nfn main() -> std::process::ExitCode {\n    let args: Vec<String> = std::env::args().skip(1).collect();\n    if args.first().map(String::as_str) != Some(\"build\") {\n        return std::process::ExitCode::SUCCESS;\n    }\n    let approved = args.iter().find_map(|a| a.strip_prefix(\"--approve=\")).and_then(|f| std::fs::read_to_string(f).ok());\n    let dir = std::path::Path::new(args.last().unwrap());\n    if !approved.is_some_and(|a| a.contains(\"POLICY +\")) || !dir.join(\"Agentfile\").is_file() {\n        eprintln!(\"ap: needs approval {args:?}\");\n        return std::process::ExitCode::from(1);\n    }\n    // sleep\n    let home = std::env::var(\"HOME\").unwrap();\n    let mut runs = std::fs::OpenOptions::new().create(true).append(true).open(format!(\"{home}/builds\")).unwrap();\n    writeln!(runs, \"build\").unwrap();\n    std::process::ExitCode::SUCCESS\n}\n";
const SLOW: &str =
    "fn main() {\n    std::thread::sleep(std::time::Duration::from_millis(150));\n}\n";
const JOURNEY: &str = "[[milestone]]\nname = \"m\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"s\"\nmilestone = \"m\"\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
const RESULT: &str = "target/bench/result.toml";

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn record_text(budgets: &str) -> String {
    format!(
        "issue = 5\npr = 9\ncommit = \"old\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = 0, promises = 1 }}\njourney = {{ passing = 0, steps = 1 }}\nreason = \"\"\n\n[budgets]\n{budgets}"
    )
}

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    fn new(name: &str, budgets: &str) -> Repo {
        Repo::with(name, budgets, FAST, &[])
    }

    fn with(name: &str, budgets: &str, ap: &str, extra: &[(&str, &str)]) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("bench")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.bin).unwrap();
        repo.set_gh("echo \"unexpected gh call: $*\" >&2\nexit 3\n");
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", "# A\n\nOne.\n"),
            (
                "crates/ap/Cargo.toml",
                "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
            ),
            ("crates/ap/src/main.rs", ap),
            ("harness/budgets.toml", budgets),
            ("harness/journey.toml", JOURNEY),
            ("harness/journey/one.sh", "#!/bin/sh\nexit 0\n"),
        ];
        for (path, content) in files.iter().chain(extra) {
            repo.write(path, content);
        }
        let output = repo
            .command("cargo")
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        repo.git(&["init", "-q", "-b", "main"]);
        repo.commit();
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(&["checkout", "-q", "-b", "work"]);
        repo
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            env::var("PATH").expect("PATH is set")
        );
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("PATH", path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_ACTIVITY_HELD");
        command
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[track_caller]
    fn git(&self, args: &[&str]) {
        let output = self.command("git").args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "change",
        ]);
    }

    fn plant(&self, ap: &str) {
        self.write("crates/ap/src/main.rs", ap);
        self.commit();
    }

    fn set_gh(&self, body: &str) {
        let path = self.bin.join("gh");
        common::write_tool(&path, &format!("#!/bin/sh\n{body}"));
    }

    /// Gives this branch one open PR with this text as its body.
    fn set_pr_body(&self, body: &str) {
        let body = body.replace('\n', "\\n");
        self.set_gh(&format!("cat <<'END'\n[{{\"body\":\"{body}\"}}]\nEND\n"));
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .unwrap()
    }

    fn bench(&self) -> Output {
        self.xtask(&["bench"])
    }

    fn record(&self) -> String {
        let output = self.xtask(&["record", "--issue", "6", "--pr", "7"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        fs::read_to_string(self.root.join("harness/ledger/6.toml")).unwrap()
    }

    fn result_exists(&self) -> bool {
        self.root.join(RESULT).exists()
    }
}

#[track_caller]
fn assert_passes(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stdout(output).ends_with("bench: pass\n"),
        "{}",
        stdout(output)
    );
}

#[track_caller]
fn assert_problem(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(2), "{}", stdout(output));
    assert!(stderr(output).contains(message), "{}", stderr(output));
}

#[test]
fn level_builds_pass() {
    let repo = Repo::new("level", BUDGETS);
    let output = repo.bench();
    assert_passes(&output);
    let out = stdout(&output);
    assert!(out.contains("pass version_ms "), "{out}");
    assert!(out.contains("recorded binary_bytes "), "{out}");
    assert!(out.contains("skip run_ms\n"), "{out}");
}

#[test]
fn boot_is_measured_for_the_base_and_the_candidate() {
    let repo = Repo::with("boot", BOOT_BUDGETS, BOOTS, &[]);
    let output = repo.bench();
    assert_passes(&output);
    let out = stdout(&output);
    assert!(out.contains("pass boot_ms base "), "{out}");
    assert!(out.contains(" candidate "), "{out}");
    assert!(repo.record().contains("boot_ms = "));
}

#[test]
fn a_planted_boot_slowdown_fails() {
    let repo = Repo::with("boot-slow", BOOT_BUDGETS, BOOTS, &[]);
    repo.set_gh("echo '[]'\n");
    repo.plant(SLOW_BOOT);
    let output = repo.bench();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("fail boot_ms "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn boot_is_timed_to_the_ready_line_and_not_to_the_exit() {
    let repo = Repo::with("boot-exit", BOOT_BUDGETS, BOOTS, &[]);
    repo.plant(SLOW_EXIT);
    assert_passes(&repo.bench());
}

#[test]
fn each_build_gets_one_warm_up_and_all_runs_share_a_home() {
    let repo = Repo::with("boot-warm", BOOT_BUDGETS, COUNTS_RUNS, &[]);
    assert_passes(&repo.bench());
    let runs = fs::read_to_string(repo.root.join("target/bench/home/runs")).unwrap();
    assert_eq!(runs.lines().count(), 2 * (1 + 15));
}

#[test]
fn a_base_with_no_boot_command_is_no_comparison() {
    let repo = Repo::with("boot-none", BOOT_BUDGETS, NO_BOOT, &[]);
    repo.plant(BOOTS);
    let output = repo.bench();
    assert_passes(&output);
    let out = stdout(&output);
    assert!(out.contains("no-comparison boot_ms candidate "), "{out}");
    assert!(repo.record().contains("boot_ms = "));
}

/// Old builds can't change, so the exit status decides, and not the wording.
#[test]
fn a_base_that_exits_with_the_usage_status_is_no_comparison_whatever_it_says() {
    let reworded = NO_BOOT.replace("usage: ap --version", "try `ap --version`");
    let repo = Repo::with("boot-reworded", BOOT_BUDGETS, &reworded, &[]);
    repo.plant(BOOTS);
    let output = repo.bench();
    assert_passes(&output);
    assert!(
        stdout(&output).contains("no-comparison boot_ms candidate "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_failed_boot_that_prints_usage_text_is_a_problem() {
    let confused = BOOT_FAILS
        .replace("ap: no kvm", "usage: no kvm")
        .replace("ExitCode::from(1)", "ExitCode::from(125)");
    let repo = Repo::with("boot-confused", BOOT_BUDGETS, &confused, &[]);
    repo.plant(BOOTS);
    assert_problem(&repo.bench(), "no kvm");
}

#[test]
fn a_candidate_with_no_boot_command_is_a_problem() {
    let repo = Repo::with("boot-lost", BOOT_BUDGETS, BOOTS, &[]);
    repo.plant(NO_BOOT);
    assert_problem(&repo.bench(), "boot");
}

#[test]
fn a_boot_that_writes_more_errors_than_a_pipe_holds_is_a_problem() {
    let loud = BOOT_FAILS.replace(
        "eprintln!(\"ap: no kvm\");",
        "eprintln!(\"ap: no kvm {}\", \"x\".repeat(1 << 17));",
    );
    let repo = Repo::with("boot-loud", BOOT_BUDGETS, &loud, &[]);
    repo.plant(BOOTS);
    assert_problem(&repo.bench(), "no kvm");
}

#[test]
fn a_base_whose_boot_fails_is_a_problem() {
    let repo = Repo::with("boot-broken", BOOT_BUDGETS, BOOT_FAILS, &[]);
    repo.plant(BOOTS);
    assert_problem(&repo.bench(), "no kvm");
}

fn slow_build() -> String {
    BUILDS.replace(
        "// sleep",
        "std::thread::sleep(std::time::Duration::from_millis(150));",
    )
}

#[test]
fn noop_build_is_measured_for_the_base_and_the_candidate() {
    let repo = Repo::with("build", BUILD_BUDGETS, BUILDS, &[]);
    let output = repo.bench();
    assert_passes(&output);
    let out = stdout(&output);
    assert!(out.contains("pass noop_build_ms base "), "{out}");
    assert!(!out.contains("skip noop_build_ms"), "{out}");
    assert!(repo.record().contains("noop_build_ms = "));
}

#[test]
fn a_planted_noop_build_slowdown_fails() {
    let repo = Repo::with("build-slow", BUILD_BUDGETS, BUILDS, &[]);
    repo.set_gh("echo '[]'\n");
    repo.plant(&slow_build());
    let output = repo.bench();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("fail noop_build_ms "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn each_binary_builds_once_to_warm_up_in_a_home_of_its_own() {
    let repo = Repo::with("build-warm", BUILD_BUDGETS, BUILDS, &[]);
    assert_passes(&repo.bench());
    for binary in ["base", "candidate"] {
        let path = repo.root.join("target/bench/noop/home").join(binary);
        let runs = fs::read_to_string(path.join("builds")).unwrap();
        assert_eq!(runs.lines().count(), 1 + 15, "{binary}");
    }
}

#[test]
fn a_base_with_no_build_command_is_no_comparison() {
    let repo = Repo::with("build-none", BUILD_BUDGETS, NO_BOOT, &[]);
    repo.plant(BUILDS);
    let output = repo.bench();
    assert_passes(&output);
    let out = stdout(&output);
    assert!(
        out.contains("no-comparison noop_build_ms candidate "),
        "{out}"
    );
    assert!(repo.record().contains("noop_build_ms = "));
}

#[test]
fn a_candidate_with_no_build_command_is_a_problem() {
    let repo = Repo::with("build-lost", BUILD_BUDGETS, BUILDS, &[]);
    repo.plant(NO_BOOT);
    assert_problem(&repo.bench(), "build");
}

#[test]
fn a_base_whose_build_fails_is_a_problem() {
    let broken = BUILDS.replace(
        "// sleep",
        "eprintln!(\"ap: broken store\");\n    return std::process::ExitCode::from(1);",
    );
    let repo = Repo::with("build-broken", BUILD_BUDGETS, &broken, &[]);
    repo.plant(BUILDS);
    assert_problem(&repo.bench(), "broken store");
}

#[test]
fn a_planted_sleep_fails() {
    let repo = Repo::new("sleep", BUDGETS);
    repo.set_gh("echo '[]'\n");
    repo.plant(SLOW);
    let output = repo.bench();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("fail version_ms "), "{out}");
    assert!(out.ends_with("bench: fail\n"), "{out}");
    assert!(
        stderr(&output).contains("Budget reason:"),
        "{}",
        stderr(&output)
    );
    assert!(!repo.result_exists());
}

#[test]
fn a_faster_build_passes() {
    let repo = Repo::with("faster", BUDGETS, SLOW, &[]);
    repo.plant(FAST);
    assert_passes(&repo.bench());
}

#[test]
fn a_slowdown_with_a_reason_passes() {
    let repo = Repo::new("reason", BUDGETS);
    repo.plant(SLOW);
    repo.set_pr_body("Some text\nBudget reason: the new check is worth it\nMore text");
    let output = repo.bench();
    assert_passes(&output);
    assert!(
        stdout(&output).contains("over version_ms "),
        "{}",
        stdout(&output)
    );
    let text = repo.record();
    assert!(
        text.contains("reason = \"the new check is worth it\"\n"),
        "{text}"
    );
}

#[test]
fn an_empty_reason_does_not_count() {
    let repo = Repo::new("empty-reason", BUDGETS);
    repo.plant(SLOW);
    repo.set_pr_body("Budget reason:   \n");
    assert_eq!(repo.bench().status.code(), Some(1));
}

#[test]
fn a_slowdown_with_no_open_pr_fails() {
    let repo = Repo::new("no-pr", BUDGETS);
    repo.plant(SLOW);
    repo.set_gh("echo '[]'\n");
    assert_eq!(repo.bench().status.code(), Some(1));
}

#[test]
fn a_budget_with_no_band_can_not_fail() {
    let budgets = "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\n";
    let repo = Repo::new("no-band", budgets);
    repo.plant(SLOW);
    let output = repo.bench();
    assert_passes(&output);
    assert!(
        stdout(&output).contains("recorded version_ms "),
        "{}",
        stdout(&output)
    );
}

/// Git gives an exported file its commit's date, so a base committed before
/// an earlier build looks unchanged to Cargo.
#[test]
fn a_new_base_older_than_the_last_build_is_built() {
    let repo = Repo::new("old-base", BUDGETS);
    assert_passes(&repo.bench());
    repo.write("crates/ap/src/main.rs", SLOW);
    repo.git(&["add", "-A"]);
    let output = repo
        .command("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "old",
        ])
        .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    assert_passes(&repo.bench());
}

#[test]
fn the_base_does_not_build_with_the_candidates_cargo_config() {
    let ap = "fn main() {\n    #[cfg(candidate_only)]\n    std::thread::sleep(std::time::Duration::from_millis(150));\n}\n";
    let repo = Repo::with("config", BUDGETS, ap, &[]);
    repo.set_gh("echo '[]'\n");
    repo.write(
        ".cargo/config.toml",
        "[build]\nrustflags = [\"--cfg\", \"candidate_only\"]\n",
    );
    repo.commit();
    let output = repo.bench();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail version_ms "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_temp_directory_inside_the_checkout_is_a_problem() {
    let repo = Repo::new("tmpdir", BUDGETS);
    let temp = repo.root.join("target/tmp");
    fs::create_dir_all(&temp).unwrap();
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("bench")
        .env("TMPDIR", &temp)
        .output()
        .unwrap();
    assert_problem(&output, "TMPDIR");
}

#[test]
fn a_commit_made_during_the_bench_gets_no_result() {
    let repo = Repo::new("moved", BUDGETS);
    let real = Command::new("sh")
        .args(["-c", "command -v cargo"])
        .output()
        .unwrap();
    let real = stdout(&real);
    common::write_tool(
        &repo.bin.join("cargo"),
        &format!(
            "#!/bin/sh\n'{}' \"$@\"\ncode=$?\nif [ \"$1\" = build ]; then\n  git -C '{}' -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m moved\nfi\nexit $code\n",
            real.trim(),
            repo.root.display()
        ),
    );
    assert_passes(&repo.bench());
    assert!(!repo.result_exists());
}

#[test]
fn no_origin_main_is_a_problem() {
    let repo = Repo::new("no-base", BUDGETS);
    repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    assert_problem(&repo.bench(), "origin/main");
}

#[test]
fn a_missing_budget_file_is_a_problem() {
    let repo = Repo::new("no-file", BUDGETS);
    fs::remove_file(repo.root.join("harness/budgets.toml")).unwrap();
    assert_problem(&repo.bench(), "harness/budgets.toml");
}

#[test]
fn a_bad_budget_file_is_a_problem() {
    let repo = Repo::new("bad-file", BUDGETS);
    for text in [
        "[[budget]]\nname = \"no key\"\n",
        "[[budget]]\nkey = \"version_ms\"\n",
        "[[budget]]\nkey = \"version_msx\"\nname = \"mistyped key\"\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = \"20\"\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_abs = true\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\ntarget = \"1\"\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = -1.0\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nbandpct = 20.0\n",
        "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\n\n[[budget]]\nkey = \"version_ms\"\nname = \"v\"\n",
        "[[other]]\n",
    ] {
        repo.write("harness/budgets.toml", text);
        assert_problem(&repo.bench(), "harness/budgets.toml");
    }
}

#[test]
fn a_base_that_does_not_build_is_a_problem() {
    let repo = Repo::with("broken-base", BUDGETS, "fn main( {\n", &[]);
    repo.plant(FAST);
    assert_problem(&repo.bench(), "base");
}

#[test]
fn a_candidate_that_does_not_build_is_a_problem() {
    let repo = Repo::new("broken-candidate", BUDGETS);
    repo.plant("fn main( {\n");
    assert_problem(&repo.bench(), "candidate");
}

#[test]
fn a_build_that_exits_with_a_failure_is_a_problem() {
    let repo = Repo::new("exits", BUDGETS);
    repo.plant("fn main() {\n    std::process::exit(3);\n}\n");
    assert_problem(&repo.bench(), "--version");
}

#[test]
fn the_bench_can_not_run_under_xtask_run() {
    let repo = Repo::new("nested", BUDGETS);
    let output = repo.xtask(&["run", env!("CARGO_BIN_EXE_xtask"), "bench"]);
    assert_ne!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("under `cargo xtask run`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn bench_takes_no_arguments() {
    let repo = Repo::new("args", BUDGETS);
    assert_problem(&repo.xtask(&["bench", "extra"]), "takes no arguments");
}

#[test]
fn record_takes_the_measured_values_and_carries_the_rest() {
    let previous = record_text("run_ms = 141.2\nversion_ms = 99999\n");
    let repo = Repo::with(
        "takes",
        BUDGETS,
        FAST,
        &[("harness/ledger/5.toml", previous.as_str())],
    );
    assert_passes(&repo.bench());
    let text = repo.record();
    assert!(text.contains("run_ms = 141.2\n"), "{text}");
    assert!(text.contains("binary_bytes = "), "{text}");
    assert!(text.contains("version_ms = "), "{text}");
    assert!(!text.contains("version_ms = 99999\n"), "{text}");
    assert!(text.contains("reason = \"\"\n"), "{text}");
}

#[test]
fn record_ignores_a_bench_of_another_commit() {
    let previous = record_text("version_ms = 99999\n");
    let repo = Repo::with(
        "stale",
        BUDGETS,
        FAST,
        &[("harness/ledger/5.toml", previous.as_str())],
    );
    assert_passes(&repo.bench());
    repo.commit();
    let text = repo.record();
    assert!(text.contains("version_ms = 99999\n"), "{text}");
}

#[test]
fn a_bench_of_a_dirty_tree_leaves_nothing_to_record() {
    let repo = Repo::new("dirty", BUDGETS);
    assert_passes(&repo.bench());
    assert!(repo.result_exists());
    repo.write("notes.txt", "uncommitted\n");
    assert_passes(&repo.bench());
    assert!(!repo.result_exists());
}

#[test]
fn a_failed_bench_removes_an_earlier_result() {
    let repo = Repo::new("failed", BUDGETS);
    assert_passes(&repo.bench());
    assert!(repo.result_exists());
    repo.set_gh("echo '[]'\n");
    repo.plant(SLOW);
    assert_eq!(repo.bench().status.code(), Some(1));
    assert!(!repo.result_exists());
}

#[test]
fn score_counts_the_budgets_met() {
    for (name, value, met) in [("met", "100", 1), ("missed", "200", 0), ("equal", "150", 0)] {
        let previous = record_text(&format!("run_ms = {value}\n"));
        let repo = Repo::with(
            &format!("score-{name}"),
            BUDGETS,
            FAST,
            &[("harness/ledger/5.toml", previous.as_str())],
        );
        let output = repo.xtask(&["score"]);
        assert_eq!(
            stdout(&output),
            format!("spec 0 of 1\njourney 0 of 1\nbudgets {met} of 1\n")
        );
        let output = repo.xtask(&["score", "--json"]);
        assert_eq!(
            stdout(&output),
            format!(
                "{{\"spec\":{{\"proven\":0,\"promises\":1}},\"journey\":{{\"passing\":0,\"steps\":1}},\"budgets\":{{\"met\":{met},\"targets\":1}}}}\n"
            )
        );
    }
}

#[test]
fn score_counts_a_budget_with_no_value_as_missed() {
    let previous = record_text("");
    let repo = Repo::with(
        "score-unmeasured",
        BUDGETS,
        FAST,
        &[("harness/ledger/5.toml", previous.as_str())],
    );
    assert!(stdout(&repo.xtask(&["score"])).ends_with("budgets 0 of 1\n"));
}
