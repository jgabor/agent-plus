//! Layers, each stored once as zstd frames under the SHA-256 of its
//! uncompressed bytes.
//!
//! A layer's file is its zstd frame, then a skippable frame that holds the
//! level it was compressed at. Any zstd decoder skips that frame.
//!
//! A run reads a layer from an uncompressed copy, so that the host keeps only
//! the blocks its guest reads, in its page cache, rather than the whole layer
//! in the run's memory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graviola::hashing::sha2::Sha256Context;

/// The layers in a store. Creating one touches nothing.
pub struct Layers {
    dir: PathBuf,
    /// The uncompressed copies that runs read.
    plain: PathBuf,
}

impl Layers {
    /// The layers under the store root. The kernel store already keeps its
    /// plain downloads in `layers`, so these have a directory of their own.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Layers {
            dir: root.as_ref().join("zstd-layers"),
            plain: root.as_ref().join("plain-layers"),
        }
    }

    /// Stores `data` at a zstd level, unless a layer with its digest is
    /// already there at that level, and returns the digest. A layer at
    /// another level is compressed again. The layer appears under its final
    /// name complete and synced, or not at all.
    pub fn put(&self, data: &[u8], level: i8) -> io::Result<String> {
        let digest = sha256(data);
        let path = self.dir.join(&digest);
        if stored_level(&path)? == Some(level) {
            // Another writer may have renamed it, or made a directory above
            // it, and been killed before syncing.
            sync_up_from(&self.dir)?;
            return Ok(digest);
        }
        let frame = crate::compress(data, level);
        // The encoder and the decoder are different crates. A frame that
        // doesn't read back never reaches the store.
        if crate::decompress(&frame)? != data {
            return Err(io::Error::other(format!(
                "layer {digest} doesn't read back after compressing it at level {level}"
            )));
        }
        fs::create_dir_all(&self.dir)?;
        let (partial, mut file) = create_partial(&self.dir, &digest)?;
        file.write_all(&frame)?;
        file.write_all(&level_frame(level))?;
        file.sync_all()?;
        // Another writer may have finished first. Its file holds the same
        // bytes, so replacing it is harmless.
        fs::rename(&partial.0, &path)?;
        sync_up_from(&self.dir)?;
        Ok(digest)
    }

    /// The level that the layer with this digest is kept at, or `None` if
    /// there is no such layer, or it was kept without its level.
    pub fn level(&self, digest: &str) -> io::Result<Option<i8>> {
        if !is_digest(digest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{digest:?} isn't a SHA-256 digest"),
            ));
        }
        stored_level(&self.dir.join(digest))
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

    /// The layer with this digest, as an open file of its uncompressed bytes.
    /// The first open makes that copy from the layer's frame, and checks it
    /// against the digest. Later opens read the copy as it is.
    pub fn open(&self, digest: &str) -> io::Result<File> {
        if !is_digest(digest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{digest:?} isn't a SHA-256 digest"),
            ));
        }
        let path = self.plain.join(digest);
        match File::open(&path) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let frame = BufReader::new(File::open(self.dir.join(digest))?);
        fs::create_dir_all(&self.plain)?;
        let (partial, mut file) = create_partial(&self.plain, digest)?;
        let mut hash = Sha256Context::new();
        let mut writing_failed = false;
        let decoded = crate::decompress_to(frame, |piece| {
            hash.update(piece);
            file.write_all(piece).inspect_err(|_| writing_failed = true)
        });
        match decoded {
            Err(error) if !writing_failed => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("layer {digest} isn't a zstd frame: {error}"),
                ));
            }
            decoded => decoded?,
        }
        let actual = hex(&hash.finish());
        if actual != digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("layer {digest} holds bytes with digest {actual}"),
            ));
        }
        file.sync_all()?;
        // Another run may have finished first. Its copy holds the same
        // bytes, so replacing it is harmless.
        fs::rename(&partial.0, &path)?;
        let copy = File::open(&path)?;
        // Freeing removes the layer, then its copy. A layer freed while this
        // made the copy is gone by now, so the copy goes too, and this run
        // keeps reading what it opened.
        if !self.path(digest).try_exists()? {
            self.remove_copy(digest)?;
            return Ok(copy);
        }
        sync_up_from(&self.plain)?;
        Ok(copy)
    }

    /// Removes the uncompressed copy of the layer with this digest, if there
    /// is one. A run that has it open keeps reading it.
    pub(crate) fn remove_copy(&self, digest: &str) -> io::Result<()> {
        match fs::remove_file(self.plain.join(digest)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
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

/// The magic number of the skippable frame that records a layer's level.
const LEVEL_MAGIC: [u8; 4] = 0x184D_2A5A_u32.to_le_bytes();

/// A skippable frame that holds `level`.
fn level_frame(level: i8) -> [u8; 9] {
    let mut frame = [0; 9];
    frame[..4].copy_from_slice(&LEVEL_MAGIC);
    frame[4..8].copy_from_slice(&1u32.to_le_bytes());
    frame[8] = level as u8;
    frame
}

/// The level that the layer at `path` was compressed at, or `None` if there
/// is no layer there, or it doesn't end in a level frame.
fn stored_level(path: &Path) -> io::Result<Option<i8>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut tail = [0; 9];
    if file.metadata()?.len() < tail.len() as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(tail.len() as i64)))?;
    file.read_exact(&mut tail)?;
    let level = tail[8] as i8;
    Ok((tail == level_frame(level)).then_some(level))
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

pub(crate) fn sha256(data: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(data);
    hex(&hash.finish())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Removes an unfinished file, whichever way its writer returns.
pub(crate) struct Partial(pub(crate) PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        // After the rename there's nothing here to remove.
        let _ = fs::remove_file(&self.0);
    }
}
