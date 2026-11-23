//! Packs planted from raw entries, for the tests that read them.

use std::fs;
use std::path::Path;

use git::Id;

/// An entry's header: its type and size, as a pack writes them.
pub fn entry_header(kind: u8, size: usize) -> Vec<u8> {
    let mut bytes = vec![(kind << 4) | (size & 15) as u8];
    let mut size = size >> 4;
    while size > 0 {
        *bytes.last_mut().unwrap() |= 0x80;
        bytes.push((size & 0x7f) as u8);
        size >>= 7;
    }
    bytes
}

/// A pack entry of a whole object.
pub fn whole(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut entry = entry_header(kind, data.len());
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(data, 6));
    entry
}

/// A pack entry of a delta against the object named `base`.
pub fn ref_delta(base: &Id, delta: &[u8]) -> Vec<u8> {
    let mut entry = entry_header(7, delta.len());
    entry.extend(base.0);
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(delta, 6));
    entry
}

/// Writes a pack of `entries`, each with the name given, and its version 2
/// index. Neither is checked against its contents, as `git` doesn't check
/// them when it reads an object.
pub fn plant_pack(root: &Path, entries: &[(Id, Vec<u8>)]) {
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend((entries.len() as u32).to_be_bytes());
    let mut offsets = Vec::new();
    for (id, entry) in entries {
        offsets.push((*id, pack.len() as u32));
        pack.extend(entry);
    }
    let checksum = [0x5a; 20];
    pack.extend(checksum);
    offsets.sort();
    let mut idx = b"\xfftOc\0\0\0\x02".to_vec();
    for first in 0..=255u8 {
        let below = offsets.iter().filter(|(id, _)| id.0[0] <= first).count();
        idx.extend((below as u32).to_be_bytes());
    }
    for (id, _) in &offsets {
        idx.extend(id.0);
    }
    for _ in &offsets {
        idx.extend([0; 4]);
    }
    for (_, offset) in &offsets {
        idx.extend(offset.to_be_bytes());
    }
    idx.extend(checksum);
    idx.extend([0; 20]);
    let dir = root.join(".git/objects/pack");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("pack-planted.pack"), pack).unwrap();
    fs::write(dir.join("pack-planted.idx"), idx).unwrap();
}

/// A delta's header: the base's size, then the result's.
pub fn delta_header(base: usize, result: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for mut size in [base, result] {
        loop {
            let more = if size >= 0x80 { 0x80 } else { 0 };
            bytes.push((size & 0x7f) as u8 | more);
            size >>= 7;
            if more == 0 {
                break;
            }
        }
    }
    bytes
}
