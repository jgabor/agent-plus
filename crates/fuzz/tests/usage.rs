//! The fuzzer's command line, which the nightly job relies on.

use std::process::{Command, Output};

fn fuzz(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fuzz"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn bad_usage_exits_2() {
    let output = fuzz(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: fuzz"));
}

#[test]
fn a_clean_run_prints_its_seed_and_exits_0() {
    let output = fuzz(&["0", "9"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("fuzz: seed 9\n"));
}
