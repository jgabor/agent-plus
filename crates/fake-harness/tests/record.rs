//! Runs the fake harness, and reads what it reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn empty_directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn record(directory: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_fake-harness"))
        .args(args)
        .current_dir(directory)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn it_records_its_arguments() {
    let directory = empty_directory("arguments");
    let out = record(&directory, &["-p", "two words"], &[]);
    assert!(out.contains(r#""arguments":["-p","two words"]"#), "{out}");
}

#[test]
fn it_records_its_environment_in_sorted_order() {
    let directory = empty_directory("environment");
    let out = record(&directory, &[], &[("B", "2"), ("A", "1")]);
    assert!(out.contains(r#""environment":{"A":"1","B":"2"}"#), "{out}");
}

#[test]
fn it_records_the_files_below_its_working_directory() {
    let directory = empty_directory("files");
    fs::create_dir_all(directory.join("a")).unwrap();
    fs::write(directory.join("a/b.txt"), "").unwrap();
    fs::write(directory.join("c.txt"), "").unwrap();
    let out = record(&directory, &[], &[]);
    assert!(out.contains(r#""files":["a/b.txt","c.txt"]"#), "{out}");
}

#[test]
fn it_records_its_network_view() {
    let directory = empty_directory("network");
    let out = record(&directory, &[], &[]);
    assert!(out.contains(r#""network":{"interfaces":["#), "{out}");
    assert!(out.contains(r#""resolvers":["#), "{out}");
}

#[test]
fn it_escapes_what_it_records() {
    let directory = empty_directory("escapes");
    let out = record(&directory, &["a\"b\\c\nd"], &[]);
    assert!(out.contains(r#""a\"b\\c\nd""#), "{out}");
}

#[test]
fn it_prints_one_line() {
    let directory = empty_directory("one-line");
    let out = record(&directory, &[], &[]);
    assert_eq!(out.matches('\n').count(), 1, "{out}");
    assert!(out.ends_with("}\n"), "{out}");
}
