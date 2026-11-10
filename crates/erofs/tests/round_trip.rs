//! Reads images back with a reader written from the kernel's
//! `Documentation/filesystems/erofs.rst` and `fs/erofs/erofs_fs.h`, and the
//! checks in `fs/erofs/super.c`, `inode.c`, and `dir.c`.

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read};

use erofs::{Dir, Error, File, Node, Symlink};

const SUPER_OFFSET: usize = 1024;
const MAGIC: u32 = 0xE0F5_E1E2;
const COMPAT_SB_CHKSUM: u32 = 0x1;
const NULL_ADDR: u32 = u32::MAX;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;
const FT_SYMLINK: u8 = 7;

/// What a tree holds, in a form both sides can compare.
#[derive(Debug, PartialEq, Eq)]
enum Tree {
    Dir(u16, BTreeMap<Vec<u8>, Tree>),
    File(u16, Vec<u8>),
    Symlink(u16, Vec<u8>),
}

fn to_node(tree: &Tree) -> Node {
    match tree {
        Tree::Dir(mode, entries) => Node::Dir(Dir {
            mode: *mode,
            entries: entries
                .iter()
                .map(|(name, t)| (name.clone(), to_node(t)))
                .collect(),
        }),
        Tree::File(mode, data) => Node::File(file(*mode, data.clone())),
        Tree::Symlink(mode, target) => Node::Symlink(Symlink {
            mode: *mode,
            target: target.clone(),
        }),
    }
}

fn file(mode: u16, data: Vec<u8>) -> File {
    File {
        mode,
        size: data.len() as u64,
        content: Box::new(Cursor::new(data)),
    }
}

fn image(tree: &Tree) -> Vec<u8> {
    let Node::Dir(root) = to_node(tree) else {
        panic!("the root is a directory");
    };
    let mut out = Vec::new();
    erofs::write(root, &mut out).expect("write the image");
    out
}

/// CRC-32C as Linux's `crc32c()` computes it: reflected, with the caller's
/// seed and no final inversion. Bitwise, so it shares no table with the
/// writer.
fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct Reader<'a> {
    img: &'a [u8],
    block: usize,
    meta: usize,
    seen: usize,
}

impl Reader<'_> {
    fn read(img: &[u8]) -> Tree {
        let sb = &img[SUPER_OFFSET..];
        assert_eq!(u32_at(sb, 0), MAGIC, "magic");
        let blkszbits = sb[12];
        assert!((9..=12).contains(&blkszbits), "blkszbits {blkszbits}");
        let block = 1usize << blkszbits;
        let compat = u32_at(sb, 8);
        assert_eq!(compat & COMPAT_SB_CHKSUM, COMPAT_SB_CHKSUM, "checksum on");
        // super.c: crc32c(0x5045B54A) over the rest of block 0 after
        // .checksum.
        let len = if block > SUPER_OFFSET { block - SUPER_OFFSET } else { block };
        assert_eq!(
            crc32c(0x5045_B54A, &img[SUPER_OFFSET + 8..SUPER_OFFSET + len]),
            u32_at(sb, 4),
            "superblock checksum"
        );
        assert_eq!(sb[13], 0, "sb_extslots");
        assert_eq!(u32_at(sb, 80), 0, "feature_incompat");
        assert_eq!(sb[90], 0, "dirblkbits");
        assert_eq!(u64_at(sb, 24), 0, "epoch");
        assert_eq!(u32_at(sb, 32), 0, "fixed_nsec");
        let blocks = u32_at(sb, 36) as usize;
        assert_eq!(blocks * block, img.len(), "blocks count");
        let inos = u64_at(sb, 16) as usize;
        let root = u64::from(u16_at(sb, 14));
        let mut reader = Reader {
            img,
            block,
            meta: u32_at(sb, 40) as usize * block,
            seen: 0,
        };
        let tree = reader.inode(root, root);
        assert_eq!(reader.seen, inos, "inos");
        tree
    }

    fn inode(&mut self, nid: u64, parent: u64) -> Tree {
        self.seen += 1;
        let at = self.meta + nid as usize * 32;
        let i = &self.img[at..at + 32];
        let format = u16_at(i, 0);
        assert_eq!(format & 1, 0, "compact inode");
        assert_eq!((format >> 1) & 7, 0, "flat plain");
        assert_eq!(format >> 4, 0, "no other format bits");
        assert_eq!(u16_at(i, 2), 0, "no xattrs");
        let mode = u16_at(i, 4);
        let nlink = u16_at(i, 6);
        let size = u32_at(i, 8) as usize;
        assert_eq!(u32_at(i, 12), 0, "mtime");
        let start = u32_at(i, 16);
        assert_eq!(u16_at(i, 24), 0, "uid");
        assert_eq!(u16_at(i, 26), 0, "gid");
        let data = if size == 0 {
            assert_eq!(start, NULL_ADDR, "an empty inode has no blocks");
            &[][..]
        } else {
            let from = start as usize * self.block;
            assert!(from >= self.meta, "data after the superblock");
            &self.img[from..from + size]
        };
        let perm = mode & !S_IFMT;
        match mode & S_IFMT {
            S_IFREG => {
                assert_eq!(nlink, 1, "file nlink");
                Tree::File(perm, data.to_vec())
            }
            S_IFLNK => {
                assert_eq!(nlink, 1, "symlink nlink");
                assert!(!data.is_empty() && !data.contains(&0), "link target");
                Tree::Symlink(perm, data.to_vec())
            }
            S_IFDIR => {
                let entries = self.dir(nid, parent, data);
                let subdirs = entries
                    .values()
                    .filter(|t| matches!(t, Tree::Dir(..)))
                    .count();
                assert_eq!(usize::from(nlink), 2 + subdirs, "dir nlink");
                Tree::Dir(perm, entries)
            }
            other => panic!("unexpected file type {other:o}"),
        }
    }

    /// Follows `erofs_readdir` and `erofs_fill_dentries`.
    fn dir(&mut self, nid: u64, parent: u64, data: &[u8]) -> BTreeMap<Vec<u8>, Tree> {
        let mut found = Vec::new();
        for (index, blk) in data.chunks(self.block).enumerate() {
            let maxsize = blk.len();
            let nameoff0 = usize::from(u16_at(blk, 8));
            assert!(
                nameoff0 != 0 && nameoff0 < self.block && nameoff0 % 12 == 0,
                "nameoff0 {nameoff0} in block {index}"
            );
            let count = nameoff0 / 12;
            for k in 0..count {
                let de = &blk[k * 12..k * 12 + 12];
                let nameoff = usize::from(u16_at(de, 8));
                let len = if k + 1 < count {
                    usize::from(u16_at(blk, (k + 1) * 12 + 8)) - nameoff
                } else {
                    assert!(nameoff < maxsize, "last name in the block");
                    blk[nameoff..maxsize]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(maxsize - nameoff)
                };
                assert!((1..=255).contains(&len), "name length {len}");
                assert!(nameoff + len <= maxsize, "name inside the block");
                let name = blk[nameoff..nameoff + len].to_vec();
                found.push((name, u64_at(de, 0), de[10]));
            }
        }
        let names: Vec<_> = found.iter().map(|(n, ..)| n.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "entries strictly sorted by name");
        let mut entries = BTreeMap::new();
        for (name, child, ftype) in found {
            match name.as_slice() {
                b"." => {
                    assert_eq!((child, ftype), (nid, FT_DIR), "dot");
                }
                b".." => {
                    assert_eq!((child, ftype), (parent, FT_DIR), "dot dot");
                }
                _ => {
                    assert!(!name.contains(&b'/') && !name.contains(&0));
                    let tree = self.inode(child, nid);
                    let want = match tree {
                        Tree::Dir(..) => FT_DIR,
                        Tree::File(..) => FT_REG_FILE,
                        Tree::Symlink(..) => FT_SYMLINK,
                    };
                    assert_eq!(ftype, want, "file type of {name:?}");
                    entries.insert(name, tree);
                }
            }
        }
        assert_eq!(
            entries.len() + 2,
            names.len(),
            "a directory holds . and .."
        );
        entries
    }
}

/// xorshift64, so the tests need no dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    fn name(&mut self) -> Vec<u8> {
        let len = match self.below(4) {
            0 => 255,
            1 => 1 + self.below(3) as usize,
            _ => 1 + self.below(40) as usize,
        };
        loop {
            let name: Vec<u8> = (0..len)
                .map(|_| match self.below(3) {
                    // Bytes that sort before and after '.'.
                    0 => b"!-.+_ ~"[self.below(7) as usize],
                    1 => b'a' + self.below(26) as u8,
                    _ => loop {
                        let b = self.next() as u8;
                        if b != 0 && b != b'/' {
                            break b;
                        }
                    },
                })
                .collect();
            if name != b"." && name != b".." {
                return name;
            }
        }
    }

    fn mode(&mut self) -> u16 {
        self.below(0o10000) as u16
    }

    fn tree(&mut self, depth: u32) -> Tree {
        let mut entries = BTreeMap::new();
        let count = match self.below(8) {
            0 => 0,
            1 => 100 + self.below(200),
            _ => self.below(8),
        };
        for _ in 0..count {
            let node = match self.below(if depth < 3 { 4 } else { 3 }) {
                0 => {
                    let len = 1 + self.below(4095) as usize;
                    Tree::Symlink(self.mode(), self.name_bytes(len))
                }
                3 => self.tree(depth + 1),
                _ => {
                    let size = match self.below(6) {
                        0 => 0,
                        1 => 4096,
                        2 => 8192,
                        3 => 4095,
                        _ => self.below(20_000) as usize,
                    };
                    Tree::File(self.mode(), self.bytes(size))
                }
            };
            entries.insert(self.name(), node);
        }
        Tree::Dir(self.mode(), entries)
    }

    fn name_bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| 1 + self.below(255) as u8).collect()
    }
}

#[test]
fn seeded_random_trees_round_trip() {
    for seed in 1..=40u64 {
        let tree = Rng(seed * 0x9E37_79B9_7F4A_7C15).tree(0);
        assert_eq!(Reader::read(&image(&tree)), tree, "seed {seed}");
    }
}

#[test]
fn the_same_tree_gives_the_same_bytes() {
    for seed in 1..=10u64 {
        let tree = Rng(seed * 0xD1B5_4A32_D192_ED03).tree(0);
        assert_eq!(image(&tree), image(&tree), "seed {seed}");
    }
}

#[test]
fn edge_cases_round_trip() {
    let mut big = BTreeMap::new();
    for n in 0..400u32 {
        let name = format!("{n:0>40}").into_bytes();
        big.insert(name, Tree::File(0o644, n.to_le_bytes().to_vec()));
    }
    let entries = BTreeMap::from([
        (b"empty".to_vec(), Tree::File(0o600, Vec::new())),
        (b"one-block".to_vec(), Tree::File(0o644, vec![7; 4096])),
        (b"two-blocks".to_vec(), Tree::File(0o755, vec![9; 8192])),
        (b"link".to_vec(), Tree::Symlink(0o777, b"one-block".to_vec())),
        (b"long-link".to_vec(), Tree::Symlink(0o777, vec![b'x'; 4095])),
        (b"big".to_vec(), Tree::Dir(0o700, big)),
        (b"empty-dir".to_vec(), Tree::Dir(0o1777, BTreeMap::new())),
        (vec![b'n'; 255], Tree::File(0o4755, b"setuid".to_vec())),
    ]);
    let tree = Tree::Dir(0o755, entries);
    let img = image(&tree);
    assert_eq!(Reader::read(&img), tree);
}

#[test]
fn an_empty_root_round_trips() {
    let tree = Tree::Dir(0o755, BTreeMap::new());
    assert_eq!(Reader::read(&image(&tree)), tree);
}

#[test]
fn the_checksum_seed_is_the_magic_and_a_zero_checksum() {
    // super.c skips .magic and .checksum, and starts from their CRC.
    let mut head = MAGIC.to_le_bytes().to_vec();
    head.extend([0; 4]);
    assert_eq!(crc32c(!0, &head), 0x5045_B54A);
}

fn root_with(name: &[u8], node: Node) -> Dir {
    Dir {
        mode: 0o755,
        entries: BTreeMap::from([(name.to_vec(), node)]),
    }
}

fn refused(root: Dir) -> Error {
    let mut out = Vec::new();
    let err = erofs::write(root, &mut out).expect_err("the writer refuses");
    assert!(out.is_empty(), "nothing is written before a refusal");
    err
}

#[test]
fn a_file_too_large_for_compact_inodes_is_refused() {
    let size = u64::from(u32::MAX) + 1;
    let root = root_with(
        b"huge",
        Node::File(File {
            mode: 0o644,
            size,
            content: Box::new(io::repeat(0).take(size)),
        }),
    );
    let err = refused(root);
    assert!(matches!(err, Error::TooLarge { .. }), "{err}");
    assert_eq!(err.to_string(), "huge: 4294967296 bytes is more than the image format holds");
}

#[test]
fn an_image_with_more_blocks_than_the_format_counts_is_refused() {
    let size = u64::from(u32::MAX);
    let entries = (0..4097u32)
        .map(|n| {
            let content: Box<dyn Read> = Box::new(io::repeat(0).take(size));
            let file = File {
                mode: 0o644,
                size,
                content,
            };
            (n.to_string().into_bytes(), Node::File(file))
        })
        .collect();
    let err = refused(Dir {
        mode: 0o755,
        entries,
    });
    assert!(matches!(err, Error::ImageTooLarge { .. }), "{err}");
}

#[test]
fn bad_names_modes_and_targets_are_refused() {
    let f = || Node::File(file(0o644, Vec::new()));
    for name in [&b""[..], b".", b"..", b"a/b", b"a\0b", &[b'n'; 256]] {
        refused(root_with(name, f()));
    }
    refused(root_with(b"f", Node::File(file(0o10644, Vec::new()))));
    refused(Dir {
        mode: 0o40755,
        entries: BTreeMap::new(),
    });
    for target in [&b""[..], b"a\0b", &[b'x'; 4096]] {
        let link = Symlink {
            mode: 0o777,
            target: target.to_vec(),
        };
        refused(root_with(b"l", Node::Symlink(link)));
    }
}

#[test]
fn content_of_the_wrong_length_is_an_error() {
    for (size, data) in [(5, vec![1; 4]), (5, vec![1; 6])] {
        let root = root_with(
            b"f",
            Node::File(File {
                mode: 0o644,
                size,
                content: Box::new(Cursor::new(data)),
            }),
        );
        let mut out = Vec::new();
        let err = erofs::write(root, &mut out).expect_err("wrong length");
        assert!(matches!(err, Error::Content { .. }), "{err}");
    }
}
