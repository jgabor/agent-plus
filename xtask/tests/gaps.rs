//! Runs `cargo xtask gaps` against throwaway repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SPEC: &str = "# A\n\nZulu. Yankee.\n\n# B\n\nAlpha.\n";
const CHECK_YANKEE: &str = "// spec(A): Yankee.\n#[test]\nfn yankee() {}\n";
const JOURNEY: &str = "[[milestone]]\nname = \"Early\"\ncommands = []\n\n[[milestone]]\nname = \"Later on\"\ncommands = []\n\n[[step]]\nid = 1\nname = \"first\"\nmilestone = \"Early\"\nneeds = []\nscript = \"harness/journey/ok.sh\"\n\n[[step]]\nid = 2\nname = \"second\"\nmilestone = \"Later on\"\nneeds = [1]\nscript = \"harness/journey/bad.sh\"\n\n[[step]]\nid = 3\nname = \"third\"\nmilestone = \"Early\"\nneeds = [2]\nscript = \"harness/journey/ok.sh\"\n";
const BUDGETS: &str = "[[budget]]\nkey = \"version_ms\"\nname = \"Version\"\ntarget = 5\n\n[[budget]]\nkey = \"run_ms\"\nname = \"Run\"\ntarget = 1000\n\n[[budget]]\nkey = \"boot_ms\"\nname = \"Boot\"\ntarget = 150\n\n[[budget]]\nkey = \"binary_bytes\"\nname = \"Size\"\n";
const RECORD: &str = "issue = 3\npr = 2\ncommit = \"c\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = { proven = 0, promises = 3 }\njourney = { passing = 1, steps = 3 }\nreason = \"\"\n\n[budgets]\nrun_ms = 2000\nversion_ms = 1\n";

fn repository(name: &str, extra: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gaps")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n")
    };
    let (ap, demo) = (manifest("ap"), manifest("demo"));
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        ),
        (".gitignore", "/target\n"),
        ("SPEC.md", SPEC),
        ("crates/ap/Cargo.toml", &ap),
        ("crates/ap/src/main.rs", "fn main() {}\n"),
        ("crates/demo/Cargo.toml", &demo),
        ("crates/demo/src/lib.rs", CHECK_YANKEE),
        ("harness/budgets.toml", BUDGETS),
        ("harness/journey.toml", JOURNEY),
        ("harness/journey/ok.sh", "exit 0\n"),
        ("harness/journey/bad.sh", "exit 1\n"),
    ];
    for (path, content) in files.iter().chain(extra) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
    };
    let lockfile = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(lockfile.status.success());
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "start"]);
    root
}

fn gaps(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gaps")
        .args(args)
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("XTASK_ACTIVITY_HELD")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn lists_steps_then_budgets_then_promises_in_order() {
    let root = repository("order", &[("harness/ledger/3.toml", RECORD)]);
    let output = gaps(&root, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout(&output),
        "milestone Early\n\
         step 2 fail Later on: second\n\
         step 3 skip Early: third\n\
         budget run_ms 2000 of 1000 Run\n\
         budget boot_ms none of 150 Boot\n\
         promise A: Zulu.\n\
         promise B: Alpha.\n"
    );
}

#[test]
fn json_carries_the_same_list() {
    let root = repository("json", &[("harness/ledger/3.toml", RECORD)]);
    let output = gaps(&root, &["--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        "{\"milestone\":\"Early\",\
         \"steps\":[{\"id\":2,\"name\":\"second\",\"milestone\":\"Later on\",\"state\":\"fail\"},\
         {\"id\":3,\"name\":\"third\",\"milestone\":\"Early\",\"state\":\"skip\"}],\
         \"budgets\":[{\"key\":\"run_ms\",\"name\":\"Run\",\"target\":1000,\"value\":2000},\
         {\"key\":\"boot_ms\",\"name\":\"Boot\",\"target\":150,\"value\":null}],\
         \"promises\":[{\"heading\":\"A\",\"sentence\":\"Zulu.\"},\
         {\"heading\":\"B\",\"sentence\":\"Alpha.\"}]}\n"
    );
}

/// The milestone to work on is the first in roadmap order with a step that
/// doesn't pass. When every step passes, there is none.
#[test]
fn no_milestone_is_named_once_every_step_passes() {
    let root = repository(
        "all-pass",
        &[
            ("harness/ledger/3.toml", RECORD),
            ("harness/journey/bad.sh", "exit 0\n"),
        ],
    );
    let text = stdout(&gaps(&root, &[]));
    assert!(!text.contains("milestone"), "{text}");
    assert!(!text.contains("step "), "{text}");
    let json = stdout(&gaps(&root, &["--json"]));
    assert!(
        json.starts_with("{\"milestone\":null,\"steps\":[],"),
        "{json}"
    );
}

#[test]
fn a_waived_sentence_is_not_a_gap() {
    let root = repository(
        "waived",
        &[
            ("harness/ledger/3.toml", RECORD),
            ("harness/waived.txt", "A\tZulu.\tnot a promise\n"),
        ],
    );
    let output = stdout(&gaps(&root, &[]));
    assert!(!output.contains("Zulu"), "{output}");
    assert!(output.contains("promise B: Alpha."), "{output}");
}

#[test]
fn without_a_record_every_budget_with_a_target_is_missed() {
    let root = repository("no-record", &[]);
    let output = stdout(&gaps(&root, &[]));
    for line in [
        "budget version_ms none of 5 Version\n",
        "budget run_ms none of 1000 Run\n",
        "budget boot_ms none of 150 Boot\n",
    ] {
        assert!(output.contains(line), "{output}");
    }
    assert!(!output.contains("binary_bytes"), "{output}");
}

#[test]
fn a_broken_quote_fails_as_it_does_in_score() {
    let root = repository(
        "broken-quote",
        &[(
            "crates/demo/src/lib.rs",
            "// spec(A): Nope.\n#[test]\nfn nope() {}\n",
        )],
    );
    for args in [&[][..], &["--json"][..]] {
        let output = gaps(&root, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("spec: "));
        assert!(stdout(&output).contains("Zulu."), "{args:?}");
    }
}

#[test]
fn a_stray_argument_exits_2() {
    let root = repository("stray", &[]);
    let output = gaps(&root, &["--wat"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("`gaps` takes"));
}
