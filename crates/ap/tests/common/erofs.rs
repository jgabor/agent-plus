//! A small EROFS reader, for reading back the layers `ap` writes. The
//! `erofs` crate's own tests check the format against the kernel's
//! documentation.

use std::collections::BTreeMap;

/// What the image holds at one path.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Dir(u16),
    File(u16, Vec<u8>),
    Symlink(Vec<u8>),
}

/// Every path in the image, the root as `/`, and the user and group id
/// of each.
pub fn read_image(image: &[u8]) -> (BTreeMap<String, Entry>, BTreeMap<String, (u16, u16)>) {
    let sb = &image[1024..];
    assert_eq!(u32_at(sb, 0), 0xE0F5_E1E2, "magic");
    let reader = Reader {
        image,
        block: 1 << sb[12],
        meta: u32_at(sb, 40) as usize * (1 << sb[12]),
    };
    let mut found = BTreeMap::new();
    let mut owners = BTreeMap::new();
    let root = u64::from(u16_at(sb, 14));
    reader.inode(root, "/".to_string(), &mut found, &mut owners);
    (found, owners)
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
    image: &'a [u8],
    block: usize,
    meta: usize,
}

impl Reader<'_> {
    fn inode(
        &self,
        nid: u64,
        path: String,
        found: &mut BTreeMap<String, Entry>,
        owners: &mut BTreeMap<String, (u16, u16)>,
    ) {
        let at = self.meta + nid as usize * 32;
        let inode = &self.image[at..at + 32];
        owners.insert(path.clone(), (u16_at(inode, 24), u16_at(inode, 26)));
        let mode = u16_at(inode, 4);
        let size = u32_at(inode, 8) as usize;
        let start = u32_at(inode, 16) as usize * self.block;
        let data = if size == 0 {
            &[][..]
        } else {
            &self.image[start..start + size]
        };
        let perm = mode & 0o7777;
        match mode & 0o170000 {
            0o100000 => {
                found.insert(path, Entry::File(perm, data.to_vec()));
            }
            0o120000 => {
                found.insert(path, Entry::Symlink(data.to_vec()));
            }
            0o040000 => {
                let prefix = if path == "/" {
                    String::new()
                } else {
                    path.clone()
                };
                found.insert(path, Entry::Dir(perm));
                for (name, child) in self.dir(data) {
                    self.inode(child, format!("{prefix}/{name}"), found, owners);
                }
            }
            other => panic!("unexpected file type {other:o}"),
        }
    }

    /// The entries of a directory, without `.` and `..`.
    fn dir(&self, data: &[u8]) -> Vec<(String, u64)> {
        let mut entries = Vec::new();
        for block in data.chunks(self.block) {
            let count = usize::from(u16_at(block, 8)) / 12;
            for k in 0..count {
                let start = usize::from(u16_at(block, k * 12 + 8));
                let end = if k + 1 < count {
                    usize::from(u16_at(block, (k + 1) * 12 + 8))
                } else {
                    block[start..]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(block.len(), |p| start + p)
                };
                let name = String::from_utf8(block[start..end].to_vec()).unwrap();
                if name != "." && name != ".." {
                    entries.push((name, u64_at(block, k * 12)));
                }
            }
        }
        entries
    }
}

/// Writes what the image holds at `below` and under it into the directory
/// `at`, as the guest would see it, apart from owners and times.
pub fn unpack(image: &[u8], below: &str, at: &std::path::Path) {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (found, _) = read_image(image);
    for (path, entry) in &found {
        let Some(rest) = path.strip_prefix(below) else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            continue;
        }
        let to = at.join(rest.trim_start_matches('/'));
        match entry {
            Entry::Dir(_) => std::fs::create_dir_all(&to).unwrap(),
            Entry::File(mode, data) => {
                std::fs::write(&to, data).unwrap();
                let mode = u32::from(*mode);
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            Entry::Symlink(target) => {
                symlink(std::str::from_utf8(target).unwrap(), &to).unwrap();
            }
        }
    }
}
