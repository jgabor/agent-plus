//! What the tests that build an agent share. Each test crate uses only
//! some of it.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use graviola::hashing::sha2::Sha256Context;

/// Puts a stand-in for the pinned `OS minimal` layer in the store at `root`,
/// so that a build finds it and downloads nothing. It holds only the frame
/// that records a layer's zstd level, so a build that reads the layer, as a
/// boot does, fails. T1 builds with the real layer.
pub fn seed_os(root: &Path) {
    let layers = root.join("zstd-layers");
    fs::create_dir_all(&layers).unwrap();
    // A zstd skippable frame of one byte, which holds level 3.
    let level_frame = [0x5a, 0x2a, 0x4d, 0x18, 1, 0, 0, 0, 3];
    fs::write(layers.join(ap::fetch::os().sha256), level_frame).unwrap();
}

/// The file the tests lock as their harness. A build only stores it, so any
/// bytes will do.
pub const HARNESS: &[u8] = b"#!/bin/sh\necho a harness\n";

/// The SHA-256 digest of `data`, in lowercase hex.
pub fn digest(data: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(data);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the download cache under `home` keeps the file with this digest.
pub fn cached(home: &Path, sha256: &str) -> PathBuf {
    home.join(".cache/ap").join(sha256)
}

/// Puts [`HARNESS`] in the download cache under `home`, and writes the
/// lockfile next to the Agentfile in `dir` that pins it, for the harness the
/// Agentfile names and the version it names, or `0.0.0` for a channel.
pub fn pin_harness(home: &Path, dir: &Path) {
    let sha256 = digest(HARNESS);
    let path = cached(home, &sha256);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, HARNESS).unwrap();
    let source = fs::read_to_string(dir.join("Agentfile")).unwrap_or_default();
    let agent = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("AGENT "))
        .unwrap_or("claude")
        .trim();
    let (harness, version) = agent.split_once(':').unwrap_or((agent, "latest"));
    let version = match version.starts_with(|c: char| c.is_ascii_digit()) {
        true => version,
        false => "0.0.0",
    };
    fs::write(
        dir.join("Agentfile.lock"),
        format!("HARNESS {harness} {version} {sha256}\n"),
    )
    .unwrap();
}
