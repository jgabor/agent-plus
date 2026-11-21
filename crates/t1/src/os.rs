//! Fetches the pinned `OS minimal` layer for tests once, so that a test's
//! store can start with it, and only the tests about fetching it download it.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::OnceLock;

use store::Layers;

const PIN: &str = include_str!("../../../guest/os/pin.toml");
/// The level `ap build` keeps the OS layer at.
const LEVEL: i8 = 3;
/// Where a store keeps each layer, compressed and uncompressed.
const DIRECTORIES: [&str; 2] = ["zstd-layers", "plain-layers"];

/// Puts the pinned `OS minimal` layer in the store at `root`, compressed and
/// uncompressed, as a build that fetched it leaves it.
pub fn put(root: &Path) {
    let digest = crate::pinned(PIN, "sha256");
    let seed = seed();
    for directory in DIRECTORIES {
        let from = seed.join(directory).join(digest);
        let to = root.join(directory).join(digest);
        fs::create_dir_all(root.join(directory)).unwrap();
        // A store replaces or removes a layer's file, and never writes into
        // it, so its stores can share one. A copy works across filesystems.
        match fs::hard_link(&from, &to) {
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(&from, &to).unwrap();
            }
            linked => linked.unwrap(),
        }
    }
}

/// A store that holds only the pinned layer, checked against `pin.toml` once
/// in each process. Test binaries make and check it under a lock, and its
/// directory moves into place only when it's complete, so none can use part
/// of one, even after a killed one.
fn seed() -> &'static Path {
    static SEED: OnceLock<PathBuf> = OnceLock::new();
    SEED.get_or_init(|| {
        let digest = crate::pinned(PIN, "sha256");
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guest");
        let seed = directory.join(format!("os-{digest}"));
        fs::create_dir_all(&directory).expect("target/guest should be writable");
        let lock = File::create(directory.join(format!("os-{digest}.lock"))).unwrap();
        lock.lock().unwrap();
        if complete(&seed, digest) {
            return seed;
        }
        let _ = fs::remove_dir_all(&seed);
        let partial = directory.join(format!(".os-{digest}.{}", process::id()));
        let _ = fs::remove_dir_all(&partial);
        fs::create_dir_all(&partial).unwrap();
        let download = partial.join("download");
        let status = Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args([
                "--proto",
                "=https",
                "--retry",
                "3",
                "--connect-timeout",
                "30",
            ])
            // A download that stalls for 30 seconds fails, and is retried.
            .args(["--speed-limit", "1", "--speed-time", "30", "--output"])
            .arg(&download)
            .arg(crate::pinned(PIN, "url"))
            .status()
            .expect("curl should run");
        assert!(status.success(), "curl couldn't fetch the OS layer");
        assert!(
            crate::digest_matches(&download, digest),
            "the downloaded OS layer doesn't match pin.toml"
        );
        let layers = Layers::new(&partial);
        let data = fs::read(&download).unwrap();
        fs::remove_file(&download).unwrap();
        assert_eq!(layers.put(&data, LEVEL).unwrap(), digest);
        drop(layers.open(digest).unwrap());
        fs::rename(&partial, &seed).unwrap();
        seed
    })
}

/// Whether `seed` holds the layer with this digest, at the level `ap build`
/// keeps it at, and both its frame and its uncompressed copy hold bytes with
/// this digest.
fn complete(seed: &Path, digest: &str) -> bool {
    let layers = Layers::new(seed);
    let plain = seed.join("plain-layers").join(digest);
    layers.level(digest).ok().flatten() == Some(LEVEL)
        && layers.get(digest).is_ok()
        && plain.is_file()
        && crate::digest_matches(&plain, digest)
}
