//! Layers, each stored once as zstd frames under the SHA-256 of its
//! uncompressed bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graviola::hashing::sha2::Sha256Context;

/// The layers in a store. Creating one touches nothing.
pub struct Layers {
    dir: PathBuf,
}

impl Layers {
    /// The layers under the store root. The kernel store already keeps its
    /// plain downloads in `layers`, so these have a directory of their own.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Layers {
            dir: root.as_ref().join("zstd-layers"),
        }
    }

    /// Stores `data`, unless a layer with its digest is already there, and
    /// returns the digest. The layer appears under its final name complete
    /// and synced, or not at all.
    pub fn put(&self, data: &[u8]) -> io::Result<String> {
        let digest = sha256(data);
        let path = self.dir.join(&digest);
        if path.try_exists()? {
            // Another writer may have renamed it but not yet synced its name.
            File::open(&self.dir)?.sync_all()?;
            return Ok(digest);
        }
        create_dir_synced(&self.dir)?;
        let (partial, mut file) = self.create_partial(&digest)?;
        file.write_all(&crate::compress(data))?;
        file.sync_all()?;
        // Another writer may have finished first. Its file holds the same
        // bytes, so replacing it is harmless.
        fs::rename(&partial.0, &path)?;
        File::open(&self.dir)?.sync_all()?;
        Ok(digest)
    }

    /// The uncompressed bytes of the layer with this digest. A stored file
    /// whose bytes no longer hash to its name is an error.
    pub fn get(&self, digest: &str) -> io::Result<Vec<u8>> {
        let lowercase_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if digest.len() != 64 || !digest.bytes().all(lowercase_hex) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{digest:?} isn't a SHA-256 digest"),
            ));
        }
        let path = self.dir.join(digest);
        let frame = fs::read(&path)?;
        let data = crate::decompress(&frame).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("layer {digest} isn't a zstd frame: {error}"),
            )
        })?;
        let actual = sha256(&data);
        if actual != digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("layer {digest} holds bytes with digest {actual}"),
            ));
        }
        Ok(data)
    }

    /// Creates a file with a name no other writer holds. A killed writer can
    /// leave a name behind, and its process id can come round again.
    fn create_partial(&self, digest: &str) -> io::Result<(Partial, File)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!(".{digest}.{}.{id}", std::process::id());
            let path = self.dir.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Partial(path), file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Creates `dir` and any missing parents, and syncs each parent that gains
/// an entry, so that a layer written inside survives a power loss.
fn create_dir_synced(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.try_exists()? {
        return Ok(());
    }
    // A relative path's last parent is empty, and means the current directory.
    let parent = match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    create_dir_synced(parent)?;
    match fs::create_dir(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    File::open(parent)?.sync_all()
}

fn sha256(data: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(data);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// Removes an unfinished file, whichever way `put` returns.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        // After the rename there's nothing here to remove.
        let _ = fs::remove_file(&self.0);
    }
}
