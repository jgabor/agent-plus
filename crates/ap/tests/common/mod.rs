//! What tests that build an agent share.

use std::fs;
use std::path::Path;

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
