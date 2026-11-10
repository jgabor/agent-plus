//! Writes a directory tree as an uncompressed EROFS image.
//!
//! The format follows the kernel's `Documentation/filesystems/erofs.rst` and
//! `fs/erofs/erofs_fs.h`. Every inode is a 32-byte compact inode with its
//! data in whole blocks of its own, and no xattrs. That limits a file or
//! directory to `u32::MAX` bytes, and an image to `u32::MAX` blocks of 4 KiB.
//! Anything larger is refused before a byte is written.
//!
//! The image is a function of the tree alone. Entries are sorted by name, and
//! every time, user id, and group id is zero. The tree has no inode numbers:
//! an inode's place in the image is its order in a depth-first walk.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};

/// A directory, with its entries by name.
pub struct Dir {
    /// Permission bits, up to `0o7777`.
    pub mode: u16,
    pub entries: BTreeMap<Vec<u8>, Node>,
}

pub enum Node {
    Dir(Dir),
    File(File),
    Symlink(Symlink),
}

/// A regular file. `content` must yield exactly `size` bytes.
pub struct File {
    /// Permission bits, up to `0o7777`.
    pub mode: u16,
    pub size: u64,
    pub content: Box<dyn Read>,
}

pub struct Symlink {
    /// Permission bits, up to `0o7777`.
    pub mode: u16,
    pub target: Vec<u8>,
}

/// Why an image wasn't written. Only `Content` and `Io` can happen after
/// writing began, and then the output must be thrown away.
#[derive(Debug)]
pub enum Error {
    /// A file or directory is larger than a compact inode can describe.
    TooLarge {
        path: Vec<u8>,
        size: u64,
    },
    /// The image would need more blocks than the superblock can count.
    ImageTooLarge {
        blocks: u64,
    },
    /// A name, mode, or link target the format can't hold.
    Invalid {
        path: Vec<u8>,
        reason: &'static str,
    },
    /// A file's content didn't yield its declared size.
    Content {
        path: Vec<u8>,
        size: u64,
    },
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge { path, size } => write!(
                f,
                "{}: {size} bytes is more than the image format holds",
                show(path)
            ),
            Error::ImageTooLarge { blocks } => write!(
                f,
                "the image needs {blocks} blocks, more than the format counts"
            ),
            Error::Invalid { path, reason } => write!(f, "{}: {reason}", show(path)),
            Error::Content { path, size } => {
                write!(f, "{}: the content isn't {size} bytes long", show(path))
            }
            Error::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

fn show(path: &[u8]) -> String {
    if path.is_empty() {
        ".".to_owned()
    } else {
        String::from_utf8_lossy(path).into_owned()
    }
}

const BLKSZBITS: u8 = 12;
const BLOCK: usize = 1 << BLKSZBITS;
const SUPER_OFFSET: usize = 1024;
const MAGIC: u32 = 0xE0F5_E1E2;
const COMPAT_SB_CHKSUM: u32 = 0x1;
/// `crc32c(~0)` over the magic and a zero checksum, which `super.c` skips.
const CHECKSUM_SEED: u32 = 0x5045_B54A;
const INODE_SIZE: usize = 32;
const DIRENT_SIZE: usize = 12;
const NAME_LEN: usize = 255;
/// `PATH_MAX` less its NUL.
const TARGET_LEN: usize = 4095;
const NULL_ADDR: u32 = u32::MAX;
/// The metadata area starts after the superblock's block, so `nid` 0, the
/// root, is the first inode in block 1.
const META_BLKADDR: u32 = 1;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;
const FT_SYMLINK: u8 = 7;

enum Data {
    Bytes(Vec<u8>),
    Content(Box<dyn Read>, Vec<u8>),
}

struct Inode {
    mode: u16,
    nlink: u16,
    size: u32,
    start: u32,
    data: Data,
}

/// Writes `root` and everything under it to `out` as an EROFS image.
pub fn write(root: Dir, out: &mut dyn Write) -> Result<(), Error> {
    let mut inodes = Vec::new();
    plan_dir(root, Vec::new(), 0, &mut inodes)?;

    let meta_blocks = (inodes.len() * INODE_SIZE).div_ceil(BLOCK) as u64;
    let mut next = u64::from(META_BLKADDR) + meta_blocks;
    for inode in &mut inodes {
        inode.start = NULL_ADDR;
        if inode.size > 0 {
            // `next` stays below `NULL_ADDR`, or the check after the loop
            // refuses the image.
            inode.start = next as u32;
            next += u64::from(inode.size).div_ceil(BLOCK as u64);
        }
    }
    if next > u64::from(u32::MAX) {
        return Err(Error::ImageTooLarge { blocks: next });
    }

    out.write_all(&superblock(inodes.len() as u64, next as u32))?;
    let mut meta = vec![0; meta_blocks as usize * BLOCK];
    for (index, (slot, inode)) in meta.chunks_mut(INODE_SIZE).zip(&inodes).enumerate() {
        put16(slot, 4, inode.mode);
        put16(slot, 6, inode.nlink);
        put32(slot, 8, inode.size);
        put32(slot, 16, inode.start);
        // Only for 32-bit stat. The kernel names an inode by its nid.
        put32(slot, 20, (index as u32).wrapping_add(1));
    }
    out.write_all(&meta)?;

    for inode in inodes {
        let size = u64::from(inode.size);
        match inode.data {
            Data::Bytes(bytes) => out.write_all(&bytes)?,
            Data::Content(mut content, path) => {
                let copied = io::copy(&mut (&mut content).take(size), out)?;
                if copied != size || content.read(&mut [0])? != 0 {
                    return Err(Error::Content { path, size });
                }
            }
        }
        let tail = inode.size as usize % BLOCK;
        if tail != 0 {
            out.write_all(&[0; BLOCK][tail..])?;
        }
    }
    Ok(())
}

/// Adds `dir` and everything under it to `inodes` in depth-first order, and
/// returns its index, which is also its nid.
fn plan_dir(
    dir: Dir,
    path: Vec<u8>,
    parent: usize,
    inodes: &mut Vec<Inode>,
) -> Result<usize, Error> {
    check_mode(dir.mode, &path)?;
    let me = inodes.len();
    inodes.push(Inode {
        mode: 0,
        nlink: 0,
        size: 0,
        start: 0,
        data: Data::Bytes(Vec::new()),
    });
    let mut dirents = vec![
        (b".".to_vec(), me, FT_DIR),
        (b"..".to_vec(), parent, FT_DIR),
    ];
    let mut subdirs = 0usize;
    for (name, node) in dir.entries {
        let child = join(&path, &name);
        check_name(&name, &child)?;
        let entry = match node {
            Node::Dir(d) => {
                subdirs += 1;
                (plan_dir(d, child, me, inodes)?, FT_DIR)
            }
            Node::File(f) => {
                check_mode(f.mode, &child)?;
                let size = u32::try_from(f.size).map_err(|_| Error::TooLarge {
                    path: child.clone(),
                    size: f.size,
                })?;
                inodes.push(Inode {
                    mode: S_IFREG | f.mode,
                    nlink: 1,
                    size,
                    start: 0,
                    data: Data::Content(f.content, child),
                });
                (inodes.len() - 1, FT_REG_FILE)
            }
            Node::Symlink(s) => {
                check_mode(s.mode, &child)?;
                let reason = if s.target.is_empty() {
                    Some("a link target can't be empty")
                } else if s.target.contains(&0) {
                    Some("a link target can't hold a NUL byte")
                } else if s.target.len() > TARGET_LEN {
                    Some("a link target can't be longer than 4095 bytes")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(Error::Invalid {
                        path: child,
                        reason,
                    });
                }
                inodes.push(Inode {
                    mode: S_IFLNK | s.mode,
                    nlink: 1,
                    size: s.target.len() as u32,
                    start: 0,
                    data: Data::Bytes(s.target),
                });
                (inodes.len() - 1, FT_SYMLINK)
            }
        };
        dirents.push((name, entry.0, entry.1));
    }
    // `.` and `..` sort among the names, as the kernel's lookup expects.
    dirents.sort_by(|a, b| a.0.cmp(&b.0));
    let bytes = dir_blocks(&dirents);
    let size = u32::try_from(bytes.len()).map_err(|_| Error::TooLarge {
        path: path.clone(),
        size: bytes.len() as u64,
    })?;
    let nlink = u16::try_from(2 + subdirs).map_err(|_| Error::Invalid {
        path,
        reason: "a directory can't hold more than 65533 directories",
    })?;
    inodes[me] = Inode {
        mode: S_IFDIR | dir.mode,
        nlink,
        size,
        start: 0,
        data: Data::Bytes(bytes),
    };
    Ok(me)
}

/// Lays out sorted entries in directory blocks: each block holds its dirents,
/// then their names. A name ends at the next one's offset, or for the last
/// entry in a block, at a NUL or the end of the directory. The last block is
/// cut where its last name ends.
fn dir_blocks(dirents: &[(Vec<u8>, usize, u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut first = 0;
    while first < dirents.len() {
        let mut used = 0;
        let mut end = first;
        while end < dirents.len() && used + DIRENT_SIZE + dirents[end].0.len() <= BLOCK {
            used += DIRENT_SIZE + dirents[end].0.len();
            end += 1;
        }
        let block = &dirents[first..end];
        let start = out.len();
        let mut nameoff = DIRENT_SIZE * block.len();
        for (name, nid, file_type) in block {
            out.extend((*nid as u64).to_le_bytes());
            out.extend((nameoff as u16).to_le_bytes());
            out.extend([*file_type, 0]);
            nameoff += name.len();
        }
        for (name, ..) in block {
            out.extend(name);
        }
        if end < dirents.len() {
            out.resize(start + BLOCK, 0);
        }
        first = end;
    }
    out
}

fn superblock(inos: u64, blocks: u32) -> [u8; BLOCK] {
    let mut block = [0; BLOCK];
    let sb = &mut block[SUPER_OFFSET..];
    put32(sb, 0, MAGIC);
    put32(sb, 8, COMPAT_SB_CHKSUM);
    sb[12] = BLKSZBITS;
    // The root's nid, at 14, is 0.
    sb[16..24].copy_from_slice(&inos.to_le_bytes());
    put32(sb, 36, blocks);
    put32(sb, 40, META_BLKADDR);
    let checksum = crc32c(CHECKSUM_SEED, &sb[8..]);
    put32(sb, 4, checksum);
    block
}

fn check_name(name: &[u8], path: &[u8]) -> Result<(), Error> {
    let reason = if name.is_empty() {
        "a name can't be empty"
    } else if name == b"." || name == b".." {
        "a name can't be . or .."
    } else if name.contains(&b'/') || name.contains(&0) {
        "a name can't hold / or a NUL byte"
    } else if name.len() > NAME_LEN {
        "a name can't be longer than 255 bytes"
    } else {
        return Ok(());
    };
    Err(Error::Invalid {
        path: path.to_vec(),
        reason,
    })
}

fn check_mode(mode: u16, path: &[u8]) -> Result<(), Error> {
    if mode & !0o7777 == 0 {
        return Ok(());
    }
    Err(Error::Invalid {
        path: path.to_vec(),
        reason: "a mode holds only permission bits, up to 0o7777",
    })
}

fn join(path: &[u8], name: &[u8]) -> Vec<u8> {
    let mut out = path.to_vec();
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend(name);
    out
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// CRC-32C as Linux's `crc32c()` computes it: with the caller's seed and no
/// final inversion.
fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = CRC_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};
