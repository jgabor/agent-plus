//! The local store: files on disk, named by the SHA-256 of their content.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graviola::hashing::sha2::Sha256Context;

/// The store's root for these values of `XDG_DATA_HOME` and `HOME`, or `None`
/// if neither gives an absolute path. The XDG spec treats an empty or relative
/// `XDG_DATA_HOME` as unset.
pub fn default_root(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    match absolute(xdg_data_home) {
        Some(data) => Some(data.join("ap")),
        None => Some(absolute(home)?.join(".local/share/ap")),
    }
}

fn absolute(value: Option<&OsStr>) -> Option<&Path> {
    value.map(Path::new).filter(|path| path.is_absolute())
}

/// The store on disk. Creating one touches nothing.
pub struct Store {
    layers: PathBuf,
}

impl Store {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Store {
            layers: root.as_ref().join("layers"),
        }
    }

    /// The store at the default location for this environment.
    pub fn from_env() -> io::Result<Self> {
        let xdg = std::env::var_os("XDG_DATA_HOME");
        let home = std::env::var_os("HOME");
        default_root(xdg.as_deref(), home.as_deref())
            .map(Store::new)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "HOME isn't an absolute path, so there's no place for the store",
                )
            })
    }

    /// Where the file with this digest lives, whether or not it's stored yet.
    pub fn path(&self, sha256: &str) -> io::Result<PathBuf> {
        let lowercase_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if sha256.len() != 64 || !sha256.bytes().all(lowercase_hex) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{sha256:?} isn't a SHA-256 digest"),
            ));
        }
        Ok(self.layers.join(sha256))
    }

    /// Stores `content` if it hashes to `sha256`. The file appears under its
    /// final name complete and synced, or not at all.
    pub fn put(&self, sha256: &str, mut content: impl Read) -> io::Result<PathBuf> {
        let path = self.path(sha256)?;
        fs::create_dir_all(&self.layers)?;

        // One name per writer, so that concurrent downloads can't mix.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let partial = Partial(
            self.layers
                .join(format!(".{sha256}.{}.{id}", std::process::id())),
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial.0)?;

        let mut hash = Sha256Context::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let read = content.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }

        let actual: String = hash.finish().iter().map(|b| format!("{b:02x}")).collect();
        if actual != sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the download has digest {actual}, not the pinned {sha256}"),
            ));
        }
        file.sync_all()?;
        fs::rename(&partial.0, &path)?;
        File::open(&self.layers)?.sync_all()?;
        Ok(path)
    }
}

/// Removes an unfinished file, whichever way `put` returns.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        // After the rename there's nothing here to remove.
        let _ = fs::remove_file(&self.0);
    }
}
