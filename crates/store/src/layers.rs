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
            // Another writer may have renamed it, or made a directory above
            // it, and been killed before syncing.
            sync_up_from(&self.dir)?;
            return Ok(digest);
        }
        fs::create_dir_all(&self.dir)?;
        let (partial, mut file) = create_partial(&self.dir, &digest)?;
        file.write_all(&crate::compress(data))?;
        file.sync_all()?;
        // Another writer may have finished first. Its file holds the same
        // bytes, so replacing it is harmless.
        fs::rename(&partial.0, &path)?;
        sync_up_from(&self.dir)?;
        Ok(digest)
    }

    /// The uncompressed bytes of the layer with this digest. A stored file
    /// whose bytes no longer hash to its name is an error.
    pub fn get(&self, digest: &str) -> io::Result<Vec<u8>> {
        if !is_digest(digest) {
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

    /// Syncs the directory that holds the layers, and every directory above
    /// it, so that every layer in it survives a power loss. Another writer
    /// may have made the directory or renamed a layer into it, and been killed
    /// before syncing.
    pub(crate) fn sync(&self) -> io::Result<()> {
        sync_up_from(&self.dir)
    }

    /// The file that holds the layer with this digest, which may not exist.
    pub(crate) fn path(&self, digest: &str) -> PathBuf {
        self.dir.join(digest)
    }
}

/// Whether `text` is a SHA-256 digest in lowercase hex.
pub(crate) fn is_digest(text: &str) -> bool {
    let lowercase_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    text.len() == 64 && text.bytes().all(lowercase_hex)
}

/// Creates a file in `dir` for `stem`, with a name no other writer holds and
/// that starts with a dot. A killed writer can leave a name behind, and its
/// process id can come round again.
pub(crate) fn create_partial(dir: &Path, stem: &str) -> io::Result<(Partial, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{stem}.{}.{id}", std::process::id());
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((Partial(path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Syncs `dir` and every directory above it, so that an entry's name and
/// each directory on its path survive a power loss. A directory that existed
/// may have been made by a writer that was killed before syncing it. A
/// directory above `dir` that can be passed through but not read is skipped.
pub(crate) fn sync_up_from(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()?;
    for dir in dir.ancestors().skip(1) {
        // A relative path's last ancestor is empty, and means the current
        // directory.
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        match File::open(dir) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn sha256(data: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(data);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// Removes an unfinished file, whichever way its writer returns.
pub(crate) struct Partial(pub(crate) PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        // After the rename there's nothing here to remove.
        let _ = fs::remove_file(&self.0);
    }
}
