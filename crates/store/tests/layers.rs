//! The store keeps each layer once, as zstd frames, under the SHA-256 of its
//! uncompressed bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use store::Layers;

// `printf 'a layer, more or less' | sha256sum`
const DATA: &[u8] = b"a layer, more or less";
const DATA_SHA256: &str = "9829b1f290c032dec81bbccca3afe4287aa636485c0382bb1b877417681e3d03";

/// A fresh directory for one test, with `HOME` and the store root inside it.
fn empty_root(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("layers")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

fn store_root(dir: &Path) -> PathBuf {
    dir.join("home/.local/share/ap")
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn putting_a_layer_returns_the_sha256_of_its_bytes() {
    let dir = empty_root("digest");
    let layers = Layers::new(store_root(&dir));

    let digest = layers.put(DATA).unwrap();

    assert_eq!(digest, sha256_of(DATA));
    assert_eq!(digest.len(), 64);
}

#[test]
fn a_layer_lives_in_its_own_directory_under_the_store_root() {
    let dir = empty_root("directory");
    let root = store_root(&dir);
    let layers = Layers::new(&root);

    let digest = layers.put(DATA).unwrap();

    let stored = root.join("zstd-layers").join(&digest);
    let frame = fs::read(&stored).unwrap();
    assert_eq!(frame[..4], [0x28, 0xB5, 0x2F, 0xFD], "zstd frame magic");
    assert_eq!(store::decompress(&frame).unwrap(), DATA);
    // The kernel store keeps its downloads in `layers`, as plain files.
    assert!(!root.join("layers").exists());
}

#[test]
fn putting_the_same_bytes_again_stores_nothing_new() {
    let dir = empty_root("again");
    let root = store_root(&dir);
    let layers = Layers::new(&root);
    let digest = layers.put(DATA).unwrap();
    let stored = root.join("zstd-layers").join(&digest);
    let before = fs::metadata(&stored).unwrap();

    assert_eq!(layers.put(DATA).unwrap(), digest);

    let after = fs::metadata(&stored).unwrap();
    assert_eq!(files(&root.join("zstd-layers")), [digest]);
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    use std::os::unix::fs::MetadataExt;
    assert_eq!(after.ino(), before.ino(), "the file wasn't replaced");
}

#[test]
fn a_layer_reads_back_as_the_same_bytes() {
    let dir = empty_root("read");
    let layers = Layers::new(store_root(&dir));
    let data = sample();

    let digest = layers.put(&data).unwrap();

    assert_eq!(layers.get(&digest).unwrap(), data);
}

#[test]
fn an_empty_layer_reads_back() {
    let dir = empty_root("empty");
    let layers = Layers::new(store_root(&dir));

    let digest = layers.put(&[]).unwrap();

    assert_eq!(layers.get(&digest).unwrap(), Vec::<u8>::new());
}

#[test]
fn a_stored_file_that_no_longer_matches_its_digest_is_an_error() {
    let dir = empty_root("mismatch");
    let root = store_root(&dir);
    let layers = Layers::new(&root);
    let digest = layers.put(DATA).unwrap();
    // A valid frame, with other content, under the same name.
    fs::write(
        root.join("zstd-layers").join(&digest),
        store::compress(b"other bytes"),
    )
    .unwrap();

    let error = layers.get(&digest).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains(&digest), "{error}");
}

#[test]
fn a_stored_file_that_isnt_a_frame_is_an_error() {
    let dir = empty_root("garbage");
    let root = store_root(&dir);
    let layers = Layers::new(&root);
    let digest = layers.put(DATA).unwrap();
    fs::write(root.join("zstd-layers").join(&digest), b"not zstd").unwrap();

    assert!(layers.get(&digest).is_err());
}

#[test]
fn a_missing_layer_is_not_found() {
    let dir = empty_root("missing");
    let layers = Layers::new(store_root(&dir));

    let error = layers.get(DATA_SHA256).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn a_name_that_isnt_a_digest_is_refused() {
    let dir = empty_root("name");
    let layers = Layers::new(store_root(&dir));

    for name in [
        "",
        "../layers",
        &DATA_SHA256.to_uppercase(),
        &DATA_SHA256[1..],
    ] {
        let error = layers.get(name).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
    }
}

#[test]
fn an_unfinished_file_is_never_taken_for_a_layer() {
    let dir = empty_root("unfinished");
    let root = store_root(&dir);
    let layers = Layers::new(&root);
    let digest = sha256_of(DATA);
    let stored = root.join("zstd-layers");
    fs::create_dir_all(&stored).unwrap();
    // Every name this process could pick next, as a writer with the same
    // process id would have left them when it was killed.
    let stale: Vec<PathBuf> = (0..1000)
        .map(|id| stored.join(format!(".{digest}.{}.{id}", std::process::id())))
        .collect();
    for path in &stale {
        fs::write(path, &store::compress(DATA)[..8]).unwrap();
    }

    assert_eq!(
        layers.get(&digest).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(layers.put(DATA).unwrap(), digest);
    assert_eq!(layers.get(&digest).unwrap(), DATA);
    assert!(stale.iter().all(|path| path.exists()));
}

#[test]
fn two_writers_of_the_same_layer_both_succeed() {
    let dir = empty_root("concurrent");
    let root = store_root(&dir);
    let data = sample();
    let digest = sha256_of(&data);

    let writers: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let data = data.clone();
            thread::spawn(move || Layers::new(root).put(&data))
        })
        .collect();
    for writer in writers {
        assert_eq!(writer.join().unwrap().unwrap(), digest);
    }

    assert_eq!(
        files(&root.join("zstd-layers")),
        std::slice::from_ref(&digest)
    );
    assert_eq!(Layers::new(&root).get(&digest).unwrap(), data);
}

/// Run by `a_writer_killed_midway_leaves_no_partial_layer`, in a child process:
/// writes distinct layers until it's killed.
#[test]
#[ignore = "a helper that the kill test runs in a child process"]
fn write_layers_until_killed() {
    let root = std::env::var_os("LAYERS_TEST_ROOT").expect("LAYERS_TEST_ROOT");
    let layers = Layers::new(root);
    let base = sample();
    let mut i = 0u64;
    loop {
        let mut data = base.clone();
        data.extend_from_slice(&i.to_le_bytes());
        layers.put(&data).unwrap();
        i += 1;
    }
}

#[test]
fn a_writer_killed_midway_leaves_no_partial_layer() {
    let dir = empty_root("killed");
    let root = store_root(&dir);
    let stored = root.join("zstd-layers");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "write_layers_until_killed", "--ignored"])
        .args(["--test-threads", "1", "--quiet"])
        .env("HOME", dir.join("home"))
        .env("LAYERS_TEST_ROOT", &root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Kill it after it has finished a layer, while it's writing another.
    let midway = || {
        let names = fs::read_dir(&stored)
            .map(|_| files(&stored))
            .unwrap_or_default();
        names.iter().any(|name| name.starts_with('.'))
            && names.iter().any(|name| !name.starts_with('.'))
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !midway() {
        assert!(
            Instant::now() < deadline,
            "the writer was never caught midway"
        );
        assert!(child.try_wait().unwrap().is_none(), "the writer exited");
        thread::sleep(Duration::from_micros(200));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    let layers = Layers::new(&root);
    let mut found = 0;
    for name in files(&stored) {
        if name.starts_with('.') {
            continue;
        }
        let data = layers.get(&name).unwrap();
        assert_eq!(sha256_of(&data), name);
        found += 1;
    }
    assert!(found >= 1);
}

/// Bytes with enough repetition for the encoder to find matches.
fn sample() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..20_000u32 {
        data.extend_from_slice(format!("line {} of a layer\n", i % 97).as_bytes());
    }
    data
}

fn sha256_of(data: &[u8]) -> String {
    let output = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(data)?;
            child.wait_with_output()
        })
        .unwrap();
    String::from_utf8(output.stdout).unwrap()[..64].to_string()
}
