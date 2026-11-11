//! `COMPRESSION`, from SPEC.md: the zstd level that `ap build` compresses an
//! Agentfile's layers at. Which levels parse is `agentfile`'s, and what a
//! level does to a frame is `store`'s.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use store::{Agents, Layers};

/// A test's own directory, with a home directory and a project in it.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("compression")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    /// Makes the project `notes`, with `data` to add and this Agentfile.
    fn project(&self, agentfile: &str) -> PathBuf {
        let dir = self.root.join("notes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data"), data()).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        dir
    }

    /// Runs `ap build` in `dir` with only `HOME` in its environment, approves
    /// the grants it asks for, and fails the test unless it exits 0.
    fn build(&self, dir: &Path) -> String {
        let source = fs::read_to_string(dir.join("Agentfile")).unwrap();
        let instructions = agentfile::parse(&source).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let output: Output = Command::new(env!("CARGO_BIN_EXE_ap"))
            .arg("build")
            .arg(format!("--approve={}", approved.display()))
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn store(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    /// Each layer of the agent `notes`: its digest, its uncompressed bytes,
    /// and the file the store keeps it in.
    fn layers(&self) -> Vec<(String, Vec<u8>, Vec<u8>)> {
        let agent = Agents::new(self.store()).get("notes").unwrap().unwrap();
        let layers = Layers::new(self.store());
        agent
            .layers
            .iter()
            .map(|layer| {
                let data = layers.get(&layer.digest).unwrap();
                let path = self.store().join("zstd-layers").join(&layer.digest);
                (layer.digest.clone(), data, fs::read(path).unwrap())
            })
            .collect()
    }

    /// Fails the test unless every layer is kept as a frame at `level`.
    fn assert_level(&self, level: i8) {
        let layers = self.layers();
        assert!(!layers.is_empty());
        for (digest, data, file) in layers {
            assert!(
                file.starts_with(&store::compress(&data, level)),
                "layer {digest} isn't at level {level}"
            );
        }
    }
}

/// What the project adds: enough text that each level compresses it
/// differently.
fn data() -> Vec<u8> {
    let mut data = Vec::new();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    while data.len() < 1 << 18 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(format!("line {} of the notes\n", state % 4093).as_bytes());
    }
    data
}

// spec(COMPRESSION): `COMPRESSION` sets the zstd level for the layers this Agentfile builds.
#[test]
fn compression_sets_the_level_of_the_layers_a_build_makes() {
    let scratch = Scratch::new("sets");
    for level in [19, -3] {
        let dir = scratch.project(&format!("AGENT claude\nCOMPRESSION {level}\nADD ./data\n"));
        scratch.build(&dir);
        scratch.assert_level(level);
    }
}

// spec(COMPRESSION): The default is `3`.
#[test]
fn a_build_without_compression_uses_level_3() {
    let scratch = Scratch::new("default");
    let dir = scratch.project("AGENT claude\nADD ./data\n");

    scratch.build(&dir);

    scratch.assert_level(3);
}

// spec(COMPRESSION): Layers are addressed by their uncompressed content, so changing the level recompresses layers without changing what is inside them.
#[test]
fn changing_the_level_recompresses_layers_under_the_same_digests() {
    let scratch = Scratch::new("change");
    let dir = scratch.project("AGENT claude\nADD ./data\n");
    scratch.build(&dir);
    let before = scratch.layers();

    fs::write(
        dir.join("Agentfile"),
        "AGENT claude\nCOMPRESSION 19\nADD ./data\n",
    )
    .unwrap();
    let out = scratch.build(&dir);

    assert!(out.contains("built ADD ./data\n"), "{out}");
    scratch.assert_level(19);
    let after = scratch.layers();
    let digests = |layers: &[(String, Vec<u8>, Vec<u8>)]| -> Vec<String> {
        layers.iter().map(|layer| layer.0.clone()).collect()
    };
    assert_eq!(digests(&after), digests(&before));
    // The added data compresses differently at the new level.
    assert_ne!(after[0].2, before[0].2);
    let files = fs::read_dir(scratch.store().join("zstd-layers")).unwrap();
    assert_eq!(files.count(), before.len());
}
