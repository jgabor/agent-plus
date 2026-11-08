use std::process::Command;

fn ap(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ap"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn version_prints_the_name_and_version() {
    let output = ap(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ap {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn anything_else_prints_usage_and_exits_2() {
    for args in [&[][..], &["--nope"][..], &["--version", "x"][..]] {
        let output = ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
    }
}
