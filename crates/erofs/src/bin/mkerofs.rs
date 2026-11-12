//! `mkerofs <dir> <image>` writes the tree under `dir` as an EROFS image.
//!
//! The image holds what the writer can: directories, regular files, and
//! symlinks, with their permission bits. Owners and times are zero. A hard
//! link becomes a copy. A device, FIFO, or socket is refused.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use erofs::{Dir, File, Node, Symlink};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [dir, image] = args.as_slice() else {
        eprintln!("usage: mkerofs <dir> <image>");
        return ExitCode::from(2);
    };
    match make(Path::new(dir), Path::new(image)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mkerofs: {error}");
            ExitCode::FAILURE
        }
    }
}

fn make(dir: &Path, image: &Path) -> Result<(), erofs::Error> {
    let root = read_dir(dir, b"")?;
    let mut out = BufWriter::new(fs::File::create(image).map_err(|e| at(image, e))?);
    erofs::write(root, &mut out)?;
    out.flush()?;
    Ok(())
}

fn read_dir(path: &Path, name: &[u8]) -> Result<Dir, erofs::Error> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(path).map_err(|e| at(path, e))? {
        let entry = entry.map_err(|e| at(path, e))?;
        let child = entry.path();
        let child_name = join(name, entry.file_name().as_bytes());
        let meta = fs::symlink_metadata(&child).map_err(|e| at(&child, e))?;
        let mode = (meta.permissions().mode() & 0o7777) as u16;
        let kind = meta.file_type();
        let node = if kind.is_dir() {
            Node::Dir(read_dir(&child, &child_name)?)
        } else if kind.is_file() {
            Node::File(File {
                mode,
                size: meta.len(),
                content: Box::new(Lazy::Path(child)),
            })
        } else if kind.is_symlink() {
            let target = fs::read_link(&child).map_err(|e| at(&child, e))?;
            Node::Symlink(Symlink {
                mode,
                target: target.into_os_string().into_vec(),
            })
        } else {
            return Err(erofs::Error::Invalid {
                path: child_name,
                reason: "a device, FIFO, or socket can't go in an image",
            });
        };
        entries.insert(entry.file_name().into_vec(), node);
    }
    let mode = fs::symlink_metadata(path).map_err(|e| at(path, e))?;
    Ok(Dir {
        mode: (mode.permissions().mode() & 0o7777) as u16,
        entries,
    })
}

/// A file opened when the writer first reads it, so that a tree of any size
/// holds one file open at a time.
enum Lazy {
    Path(PathBuf),
    Open(fs::File),
}

impl Read for Lazy {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Lazy::Path(path) = self {
            *self = Lazy::Open(fs::File::open(&*path).map_err(|e| at(path, e))?);
        }
        let Lazy::Open(file) = self else {
            unreachable!("opened above")
        };
        file.read(buf)
    }
}

fn join(path: &[u8], name: &[u8]) -> Vec<u8> {
    let mut out = path.to_vec();
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend(name);
    out
}

fn at(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}
