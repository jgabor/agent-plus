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
        .env("CARGO_HOME", cargo_home())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}

/// Keeps the git checkouts these fixtures make out of the owner's `~/.cargo`.
fn cargo_home() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("floors-cargo-home")
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
        .env("CARGO_HOME", cargo_home())
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
fn a_dependency_only_on_musl_still_fails() {
    let root = workspace(
        "musl-only",
        "\n[target.'cfg(target_env = \"musl\")'.dependencies]\ncc = { path = \"../../vendor/cc\" }\n",
        &[
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_build_dependency_only_on_macos_still_fails() {
    // Cargo matches a build dependency against the machine it runs on, so
    // this package is invisible to `cargo tree --target` on Linux.
    let root = workspace(
        "macos-build-only",
        "\n[target.'cfg(target_os = \"macos\")'.build-dependencies]\ncc = { path = \"../../vendor/cc\" }\n",
        &[
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_dev_dependency_that_builds_c_fails() {
    let root = workspace(
        "dev-only",
        "\n[dev-dependencies]\ncc = { path = \"../../vendor/cc\" }\n",
        &[
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    );
    assert_floor_fails(&gate(&root), "cc");
}

/// `demo` enables `features` on `mid`, whose `alloc` feature only refines the
/// optional package `cc`, through the weak syntax `cc?/x`.
fn weak_feature_workspace(name: &str, features: &str) -> PathBuf {
    let extra = format!(
        "\n[dependencies]\nmid = {{ path = \"../../vendor/mid\", features = [{features}] }}\n"
    );
    let mid = package(
        "mid",
        "\n[dependencies]\ncc = { path = \"../cc\", optional = true }\n\n[features]\nalloc = [\"cc?/x\"]\n",
    );
    let cc = package("cc", "\n[features]\nx = []\n");
    workspace(
        name,
        &extra,
        &[
            ("vendor/mid/Cargo.toml", &mid),
            ("vendor/mid/src/lib.rs", LIB),
            ("vendor/cc/Cargo.toml", &cc),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    )
}

#[test]
fn a_package_reachable_only_through_a_weak_feature_passes() {
    let output = gate(&weak_feature_workspace("weak", "\"alloc\""));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("pass floors"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_weak_feature_on_a_package_something_enables_fails() {
    let root = weak_feature_workspace("weak-enabled", "\"alloc\", \"cc\"");
    assert_floor_fails(&gate(&root), "cc");
}

/// `demo` depends on `mid`, and `gate` is the platform condition on a package
/// `win` that turns on the feature of `mid` that pulls in `cc`.
/// `demo` reaches `mid` through `edge`, and `win` turns on the feature of
/// `mid` that pulls in `cc`, but only where `gate` holds.
fn feature_from_platform_workspace(name: &str, edge: &str, gate: &str) -> PathBuf {
    let extra = format!(
        "\n[{edge}]\nmid = {{ path = \"../../vendor/mid\" }}\n\n[target.'{gate}'.{edge}]\nwin = {{ path = \"../../vendor/win\" }}\n"
    );
    let mid = package(
        "mid",
        "\n[dependencies]\ncc = { path = \"../cc\", optional = true }\n\n[features]\nnative = [\"dep:cc\"]\n",
    );
    let win = package(
        "win",
        "\n[dependencies]\nmid = { path = \"../mid\", features = [\"native\"] }\n",
    );
    workspace(
        name,
        &extra,
        &[
            ("vendor/mid/Cargo.toml", &mid),
            ("vendor/mid/src/lib.rs", LIB),
            ("vendor/win/Cargo.toml", &win),
            ("vendor/win/src/lib.rs", LIB),
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    )
}

#[track_caller]
fn assert_floors_pass(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    assert!(stdout(output).contains("pass floors"), "{}", stdout(output));
}

#[test]
fn a_feature_enabled_only_on_an_unsupported_platform_passes() {
    let root = feature_from_platform_workspace("feature-windows", "dependencies", "cfg(windows)");
    assert_floors_pass(&gate(&root));
}

#[test]
fn a_feature_enabled_only_on_macos_still_fails() {
    let root = feature_from_platform_workspace(
        "feature-macos",
        "dependencies",
        "cfg(target_os = \"macos\")",
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_host_side_feature_enabled_only_on_an_unsupported_platform_passes() {
    let root = feature_from_platform_workspace(
        "host-feature-windows",
        "build-dependencies",
        "cfg(windows)",
    );
    assert_floors_pass(&gate(&root));
}

#[test]
fn a_host_side_feature_enabled_only_on_macos_still_fails() {
    let root = feature_from_platform_workspace(
        "host-feature-macos",
        "build-dependencies",
        "cfg(target_os = \"macos\")",
    );
    assert_floor_fails(&gate(&root), "cc");
}

const MACOS: &str = "'cfg(target_os = \"macos\")'";

/// `demo` reaches `tool` through `edge`, and `tool` depends on `cc` only on
/// `platform`, a macOS one. Cargo resolves `tool` for the machine it runs on,
/// so on Linux `cc` is invisible to `cargo tree --target`.
fn host_side_workspace(name: &str, edge: &str, platform: &str, tool_extra: &str) -> PathBuf {
    let extra = format!("\n[{edge}]\ntool = {{ path = \"../../vendor/tool\" }}\n");
    let tool = package(
        "tool",
        &format!("{tool_extra}\n[target.{platform}.dependencies]\ncc = {{ path = \"../cc\" }}\n"),
    );
    workspace(
        name,
        &extra,
        &[
            ("vendor/tool/Cargo.toml", &tool),
            ("vendor/tool/src/lib.rs", ""),
            ("vendor/cc/Cargo.toml", &package("cc", "")),
            ("vendor/cc/src/lib.rs", LIB),
        ],
    )
}

#[test]
fn a_macos_only_dependency_of_a_build_dependency_still_fails() {
    let root = host_side_workspace("host-build", "build-dependencies", MACOS, "");
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_macos_only_dependency_of_a_proc_macro_still_fails() {
    let root = host_side_workspace(
        "host-proc-macro",
        "dependencies",
        MACOS,
        "\n[lib]\nproc-macro = true\n",
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_dependency_of_a_build_dependency_named_by_a_macos_triple_still_fails() {
    let root = host_side_workspace(
        "host-build-triple",
        "build-dependencies",
        "aarch64-apple-darwin",
        "",
    );
    assert_floor_fails(&gate(&root), "cc");
}

#[test]
fn a_dependency_of_a_proc_macro_named_by_a_macos_triple_still_fails() {
    let root = host_side_workspace(
        "host-proc-macro-triple",
        "dependencies",
        "aarch64-apple-darwin",
        "\n[lib]\nproc-macro = true\n",
    );
    assert_floor_fails(&gate(&root), "cc");
}

/// A git repository holding `dep 0.1.0` with a C file in it, under the same
/// name and version as the clean `dep` in `vendor/`.
fn dep_with_c_in_git(name: &str) -> PathBuf {
    let repo = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("floors-git")
        .join(name);
    if repo.exists() {
        fs::remove_dir_all(&repo).unwrap();
    }
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        package("dep", "\n[features]\nx = []\n"),
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), LIB).unwrap();
    fs::write(repo.join("src/ffi.c"), "planted\n").unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet", "--initial-branch", "main"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "--message", "fixture"]);
    repo
}

/// `demo` builds the clean `dep`. It also enables `features` on `mid`, whose
/// `alloc` feature only refines the git `dep` through weak syntax, and whose
/// `real` feature builds it.
fn same_name_workspace(name: &str, features: &str) -> PathBuf {
    let repo = dep_with_c_in_git(name);
    let extra = format!(
        "\n[dependencies]\ndep = {{ path = \"../../vendor/dep\" }}\nmid = {{ path = \"../../vendor/mid\", features = [{features}] }}\n"
    );
    let mid = package(
        "mid",
        &format!(
            "\n[dependencies]\nother = {{ package = \"dep\", git = \"file://{}\", branch = \"main\", optional = true }}\n\n[features]\nalloc = [\"other?/x\"]\nreal = [\"dep:other\"]\n",
            repo.display()
        ),
    );
    workspace(
        name,
        &extra,
        &[
            ("vendor/dep/Cargo.toml", &package("dep", "")),
            ("vendor/dep/src/lib.rs", LIB),
            ("vendor/mid/Cargo.toml", &mid),
            ("vendor/mid/src/lib.rs", LIB),
        ],
    )
}

#[test]
fn an_inactive_package_sharing_a_built_name_and_version_passes() {
    let output = gate(&same_name_workspace("same-name", "\"alloc\""));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("pass floors"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_built_package_sharing_a_name_and_version_still_fails() {
    let root = same_name_workspace("same-name-built", "\"alloc\", \"real\"");
    assert_floor_fails(&gate(&root), "ffi.c");
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
