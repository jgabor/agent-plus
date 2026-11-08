//! Runs the real gate against workspaces with planted dependencies. A fixture
//! crate named `cc` stands in for the real one, so these checks work offline.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LIB: &str = "pub fn answer() -> u32 {\n    42\n}\n";
const EMPTY_MAIN: &str = "fn main() {}\n";

/// A workspace whose crate `demo` has `extra` appended to its manifest, plus
/// any further files.
fn workspace(name: &str, extra: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("floors")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    let demo =
        format!("[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n{extra}");
    let root_manifest = "[workspace]\nmembers = [\"demo\"]\nresolver = \"3\"\n";
    let base: [(&str, &str); 4] = [
        ("ws/SPEC.md", "# A\n"),
        ("ws/Cargo.toml", root_manifest),
        ("ws/demo/Cargo.toml", &demo),
        ("ws/demo/src/lib.rs", LIB),
    ];
    for (path, content) in base.iter().chain(files) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let root = root.join("ws");
    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}

/// A crate outside the workspace, which is how a third-party package looks.
fn package(name: &str, extra: &str) -> String {
    format!(
        "[workspace]\n\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{extra}"
    )
}

fn gate(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gate")
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CC")
        .env_remove("CXX")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_floor_fails(output: &Output, culprit: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    assert!(stdout(output).contains("fail floors"), "{}", stdout(output));
    assert!(stderr(output).contains(culprit), "{}", stderr(output));
}

#[test]
fn a_clean_workspace_passes_the_floors() {
    let output = gate(&workspace("clean", "", &[]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("pass floors"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_crate_that_builds_c_fails() {
    for builder in ["cc", "cmake", "bindgen", "pkg-config"] {
        let extra = format!(
            "\n[build-dependencies]\n{builder} = {{ path = \"../../vendor/{builder}\" }}\n"
        );
        let manifest_path = format!("vendor/{builder}/Cargo.toml");
        let lib_path = format!("vendor/{builder}/src/lib.rs");
        let manifest = package(builder, "");
        let name = format!("builder-{builder}");
        let root = workspace(
            &name,
            &extra,
            &[(&manifest_path, &manifest), (&lib_path, LIB)],
        );
        assert_floor_fails(&gate(&root), builder);
    }
}

#[test]
fn a_links_key_fails() {
    let root = workspace(
        "links",
        "\n[dependencies]\ndep = { path = \"../../vendor/dep\" }\n",
        &[
            ("vendor/dep/Cargo.toml", &package("dep", "links = \"z\"\n")),
            ("vendor/dep/src/lib.rs", LIB),
            ("vendor/dep/build.rs", EMPTY_MAIN),
        ],
    );
    assert_floor_fails(&gate(&root), "links");
}

#[test]
fn our_own_build_script_fails() {
    let root = workspace("own-build", "", &[("ws/demo/build.rs", EMPTY_MAIN)]);
    assert_floor_fails(&gate(&root), "demo");
}

const DEP_WITH_BUILD_SCRIPT: &str = "\n[dependencies]\ndep = { path = \"../../vendor/dep\" }\n";

#[test]
fn an_unaudited_build_script_fails() {
    let root = workspace(
        "unaudited",
        DEP_WITH_BUILD_SCRIPT,
        &[
            ("vendor/dep/Cargo.toml", &package("dep", "")),
            ("vendor/dep/src/lib.rs", LIB),
            ("vendor/dep/build.rs", EMPTY_MAIN),
        ],
    );
    assert_floor_fails(&gate(&root), "dep");
}

#[test]
fn an_audited_build_script_passes() {
    let root = workspace(
        "audited",
        DEP_WITH_BUILD_SCRIPT,
        &[
            ("vendor/dep/Cargo.toml", &package("dep", "")),
            ("vendor/dep/src/lib.rs", LIB),
            ("vendor/dep/build.rs", EMPTY_MAIN),
            (
                "ws/harness/build-scripts.txt",
                "# name version reason\ndep 0.1.0 only prints rerun lines\n",
            ),
        ],
    );
    let output = gate(&root);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("pass floors"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn an_audit_of_another_version_fails() {
    let root = workspace(
        "audit-version",
        DEP_WITH_BUILD_SCRIPT,
        &[
            ("vendor/dep/Cargo.toml", &package("dep", "")),
            ("vendor/dep/src/lib.rs", LIB),
            ("vendor/dep/build.rs", EMPTY_MAIN),
            ("ws/harness/build-scripts.txt", "dep 0.2.0 an older audit\n"),
        ],
    );
    assert_floor_fails(&gate(&root), "dep");
}

#[test]
fn a_malformed_audit_line_is_an_environment_error() {
    let root = workspace(
        "audit-malformed",
        "",
        &[("ws/harness/build-scripts.txt", "dep-with-no-version\n")],
    );
    let output = gate(&root);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("build-scripts.txt"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn foreign_source_and_prebuilt_files_fail() {
    for extension in [
        "c", "C", "h", "cc", "cpp", "c++", "hxx", "m", "mm", "S", "s", "sx", "asm", "a", "o", "so",
        "so.1",
    ] {
        let file = format!("vendor/dep/native/thing.{extension}");
        let name = format!("foreign-{extension}");
        let root = workspace(
            &name,
            DEP_WITH_BUILD_SCRIPT,
            &[
                ("vendor/dep/Cargo.toml", &package("dep", "")),
                ("vendor/dep/src/lib.rs", LIB),
                (&file, "planted\n"),
            ],
        );
        assert_floor_fails(&gate(&root), &format!("thing.{extension}"));
    }
}

#[test]
fn a_dependency_only_on_another_platform_family_is_ignored() {
    let root = workspace(
        "windows-only",
        "\n[target.'cfg(windows)'.dependencies]\ncc = { path = \"../../vendor/cc\" }\n",
        &[
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    );
    let output = gate(&root);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn a_dependency_only_on_macos_still_fails() {
    let root = workspace(
        "macos-only",
        "\n[target.'cfg(target_os = \"macos\")'.dependencies]\ncc = { path = \"../../vendor/cc\" }\n",
        &[
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn building_with_a_c_compiler_fails() {
    // The build script succeeds unless the gate has pointed CC at a program
    // that fails. The script also fails the floors, so this checks `tests`.
    let script = "use std::{env, process::Command};\n\nfn main() {\n    let cc = env::var(\"CC\").unwrap_or_else(|_| \"true\".into());\n    assert!(Command::new(cc).status().unwrap().success());\n}\n";
    let root = workspace("compiler", "", &[("ws/demo/build.rs", script)]);
    let output = gate(&root);
    assert!(
        stdout(&output).contains("fail tests"),
        "{}",
        stdout(&output)
    );
}

const VENDORED_DEP: [(&str, &str); 1] = [("vendor/dep/src/lib.rs", LIB)];

fn vendored_dep(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let manifest = package("dep", "");
    let mut all = vec![("vendor/dep/Cargo.toml", manifest.as_str())];
    all.extend_from_slice(&VENDORED_DEP);
    all.extend_from_slice(files);
    workspace(name, DEP_WITH_BUILD_SCRIPT, &all)
}

#[test]
fn c_under_a_directory_named_target_fails() {
    let root = vendored_dep(
        "named-target",
        &[("vendor/dep/src/target/ffi.c", "planted\n")],
    );
    assert_floor_fails(&gate(&root), "ffi.c");
}

#[test]
fn a_symlink_to_a_c_file_fails() {
    let root = vendored_dep("file-link", &[("vendor/shared/real.c", "planted\n")]);
    let vendor = root.parent().unwrap().join("vendor");
    fs::create_dir_all(vendor.join("dep/native")).unwrap();
    symlink(
        vendor.join("shared/real.c"),
        vendor.join("dep/native/link.c"),
    )
    .unwrap();
    assert_floor_fails(&gate(&root), "link.c");
}

#[test]
fn a_symlink_to_a_directory_of_c_fails() {
    let root = vendored_dep("dir-link", &[("vendor/shared/real.c", "planted\n")]);
    let vendor = root.parent().unwrap().join("vendor");
    symlink(vendor.join("shared"), vendor.join("dep/native")).unwrap();
    assert_floor_fails(&gate(&root), "real.c");
}

#[test]
fn a_symlink_cycle_terminates() {
    let root = vendored_dep("cycle", &[]);
    let vendor = root.parent().unwrap().join("vendor");
    symlink(vendor.join("dep"), vendor.join("dep/again")).unwrap();
    let output = gate(&root);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn a_missing_lockfile_is_an_environment_error() {
    let root = workspace("no-lock", "", &[]);
    fs::remove_file(root.join("Cargo.lock")).unwrap();
    let output = gate(&root);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("cargo metadata"),
        "{}",
        stderr(&output)
    );
}
