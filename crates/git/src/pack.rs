//! Reads objects from a pack and its version 2 index, as git's `packfile.c`
//! does, resolving both kinds of delta.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::stream::{CHUNK, Stream, Why};
use crate::{Error, Id, Kind, Object, corrupt};

/// The start of a version 2 index: a magic number, then the version.
const INDEX_MAGIC: &[u8] = b"\xfftOc\0\0\0\x02";

/// Where the ids start in an index, after the magic and 256 counts.
const IDS: usize = 8 + 256 * 4;

/// The two checksums that end an index: the pack's, then its own.
const TRAILER: usize = 40;

/// The length of a pack's header: `PACK`, a version, and a count.
const PACK_HEADER: u64 = 12;

/// The shortest delta git applies, as `DELTA_SIZE_MIN`.
const MIN_DELTA: usize = 4;

/// A pack, with its index read into memory.
pub(crate) struct Pack {
    pub(crate) index: Vec<u8>,
    count: usize,
    file: File,
    /// Where the pack's checksum starts, after its last entry.
    end: u64,
    name: String,
}

/// An entry in a pack: an object whole, or a delta against another entry.
enum Entry {
    Whole(Kind),
    Delta { base: u64 },
}

impl Pack {
    /// The pack beside the index at `idx`, or `None` if there is no pack.
    pub(crate) fn open(idx: &Path, limit: usize) -> Result<Option<Pack>, Error> {
        let name = idx.display().to_string();
        let Some((file, len)) = crate::open(&idx.with_extension("pack"))? else {
            return Ok(None);
        };
        let Some(index) = crate::read(idx, limit)? else {
            return Ok(None);
        };
        let bad = |why: &str| corrupt(format!("{name} {why}"));
        if index.len() < IDS + TRAILER || !index.starts_with(INDEX_MAGIC) {
            return Err(bad("isn't a version 2 pack index"));
        }
        let mut below = 0;
        for n in 0..256 {
            let count = be32(&index, 8 + n * 4) as usize;
            if count < below {
                return Err(bad("has counts that go down"));
            }
            below = count;
        }
        let count = below;
        // Each object has an id, a checksum, and an offset, and as in git's
        // `check_packed_git_idx`, all but one may have a 64-bit offset too.
        let least = count
            .checked_mul(28)
            .and_then(|tables| tables.checked_add(IDS + TRAILER))
            .ok_or_else(|| bad("is too large"))?;
        let most = least + count.saturating_sub(1) * 8;
        if index.len() < least || index.len() > most || (index.len() - least) % 8 != 0 {
            return Err(bad("has the wrong length"));
        }
        let pack_bad =
            |why: &str| corrupt(format!("{} {why}", idx.with_extension("pack").display()));
        if len < PACK_HEADER + 20 {
            return Err(pack_bad("is too short"));
        }
        let mut header = [0; PACK_HEADER as usize];
        file.read_exact_at(&mut header, 0)?;
        let version = be32(&header, 4);
        if &header[..4] != b"PACK" || !(version == 2 || version == 3) {
            return Err(pack_bad("isn't a pack"));
        }
        if be32(&header, 8) as usize != count {
            return Err(pack_bad(
                "holds a different number of objects than its index",
            ));
        }
        // As git does, the pack's checksum must be the one its index names.
        // Neither is computed.
        let mut checksum = [0; 20];
        file.read_exact_at(&mut checksum, len - 20)?;
        if checksum[..] != index[index.len() - TRAILER..][..20] {
            return Err(pack_bad("doesn't match its index"));
        }
        Ok(Some(Pack {
            index,
            count,
            file,
            end: len - 20,
            name,
        }))
    }

    fn bad(&self, why: impl std::fmt::Display) -> Error {
        corrupt(format!("{} {why}", self.name))
    }

    /// The offset of the object named `id`, or `None` if it isn't here.
    pub(crate) fn find(&self, id: &Id) -> Result<Option<u64>, Error> {
        let first = usize::from(id.0[0]);
        let start = match first {
            0 => 0,
            _ => be32(&self.index, 8 + (first - 1) * 4) as usize,
        };
        let stop = be32(&self.index, 8 + first * 4) as usize;
        let ids = &self.index[IDS..IDS + self.count * 20];
        let (mut low, mut high) = (start, stop);
        while low < high {
            let mid = low + (high - low) / 2;
            match ids[mid * 20..mid * 20 + 20].cmp(&id.0) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return self.offset(mid).map(Some),
            }
        }
        Ok(None)
    }

    /// The offset of the `n`th object in the index.
    fn offset(&self, n: usize) -> Result<u64, Error> {
        let small = be32(&self.index, IDS + self.count * 24 + n * 4);
        if small & 0x8000_0000 == 0 {
            return Ok(u64::from(small));
        }
        let at = (small & 0x7fff_ffff) as usize * 8 + IDS + self.count * 28;
        let large = self.index.len() - TRAILER;
        if at + 8 > large {
            return Err(self.bad("has a 64-bit offset past its table"));
        }
        let high = u64::from(be32(&self.index, at));
        Ok(high << 32 | u64::from(be32(&self.index, at + 4)))
    }

    /// The object whose entry starts at `offset`, with its deltas applied.
    /// An entry or a result larger than `limit` is refused before it is
    /// inflated or applied. The deltas are inflated and applied one at a
    /// time, from the base, so a long chain holds only one of them.
    pub(crate) fn read(&self, mut offset: u64, limit: usize) -> Result<Object, Error> {
        let mut deltas = Vec::new();
        let (kind, size, data) = loop {
            // An offset delta's base comes before it, but a delta by id can
            // name any entry, so a longer chain than there are entries loops.
            if deltas.len() > self.count {
                return Err(self.bad("has a delta chain that loops"));
            }
            let (entry, size, data) = self.entry(offset)?;
            if size > limit {
                return Err(self.bad(format!("has an entry at {offset} larger than the limit")));
            }
            match entry {
                Entry::Whole(kind) => break (kind, size, data),
                Entry::Delta { base } => {
                    deltas.push((size, data));
                    offset = base;
                }
            }
        };
        let mut data = self.inflate(data, size)?;
        while let Some((size, at)) = deltas.pop() {
            let delta = self.inflate(at, size)?;
            data = self.apply(&data, &delta, limit)?;
        }
        Ok(Object { kind, data })
    }

    /// The type of the object whose entry starts at `offset`: its own, or
    /// that of the base its chain of deltas ends in. It inflates nothing.
    pub(crate) fn kind(&self, mut offset: u64) -> Result<Kind, Error> {
        for _ in 0..=self.count {
            match self.entry(offset)?.0 {
                Entry::Whole(kind) => return Ok(kind),
                Entry::Delta { base } => offset = base,
            }
        }
        Err(self.bad("has a delta chain that loops"))
    }

    /// The entry at `offset`, its size, and where its zlib data starts.
    fn entry(&self, offset: u64) -> Result<(Entry, usize, u64), Error> {
        if offset < PACK_HEADER || offset >= self.end {
            return Err(self.bad(format!("has no entry at {offset}")));
        }
        // A header is at most 10 bytes for the size, and 20 for a base.
        let mut head = [0; 32];
        let len = (self.end - offset).min(head.len() as u64) as usize;
        let head = &mut head[..len];
        self.file.read_exact_at(head, offset)?;
        let short = || self.bad(format!("has a short entry at {offset}"));
        let mut bytes = head.iter().copied();
        let mut byte = bytes.next().ok_or_else(short)?;
        let kind = (byte >> 4) & 7;
        let mut size = u64::from(byte & 15);
        let mut shift = 4;
        while byte & 0x80 != 0 {
            // As git's `unpack_object_header_buffer`, which stops before a
            // group that might not fit.
            if shift > 64 - 7 {
                return Err(self.bad(format!("has too large an entry at {offset}")));
            }
            byte = bytes.next().ok_or_else(short)?;
            size |= u64::from(byte & 0x7f) << shift;
            shift += 7;
        }
        let size = usize::try_from(size).map_err(|_| self.bad("has too large an entry"))?;
        let entry = match kind {
            1 => Entry::Whole(Kind::Commit),
            2 => Entry::Whole(Kind::Tree),
            3 => Entry::Whole(Kind::Blob),
            4 => Entry::Whole(Kind::Tag),
            6 => {
                // As git's `get_delta_base`: big-endian, with one added for
                // each byte after the first, so each length has one encoding.
                let mut byte = bytes.next().ok_or_else(short)?;
                let mut back = u64::from(byte & 0x7f);
                while byte & 0x80 != 0 {
                    byte = bytes.next().ok_or_else(short)?;
                    back = back
                        .checked_add(1)
                        .filter(|back| back >> 57 == 0)
                        .ok_or_else(|| self.bad(format!("has a bad base at {offset}")))?;
                    back = back << 7 | u64::from(byte & 0x7f);
                }
                if back == 0 || back >= offset {
                    return Err(self.bad(format!("has a bad base at {offset}")));
                }
                Entry::Delta {
                    base: offset - back,
                }
            }
            7 => {
                let mut id = [0; 20];
                for byte in &mut id {
                    *byte = bytes.next().ok_or_else(short)?;
                }
                // As git does, the base must be in the same pack.
                let base = self.find(&Id(id))?.ok_or_else(|| {
                    self.bad(format!("has a delta whose base {} isn't in it", Id(id)))
                })?;
                Entry::Delta { base }
            }
            _ => return Err(self.bad(format!("has an entry of type {kind} at {offset}"))),
        };
        let used = len - bytes.len();
        Ok((entry, size, offset + used as u64))
    }

    /// Inflates the zlib stream at `at`, which must hold `size` bytes.
    fn inflate(&self, at: u64, size: usize) -> Result<Vec<u8>, Error> {
        // The size bounds the output, but isn't trusted with an allocation.
        let mut out = Vec::with_capacity(size.min(1 << 20));
        // A small object needs small buffers. Zlib adds a few bytes to data
        // it can't compress, and one byte more than the size is enough to
        // find an entry that is too long.
        let small = (size as u64).saturating_add(64).min(CHUNK) as usize;
        let mut stream = Stream::new(&self.file, at, self.end, small);
        let mut output = vec![0; small];
        loop {
            let (written, ended) = stream.next(&mut output).map_err(|why| match why {
                Why::Io(error) => Error::Io(error),
                Why::Short => self.bad("ends inside an entry"),
                Why::NotZlib => self.bad("has an entry that isn't zlib"),
            })?;
            // Checked before the output grows, so it never holds more.
            if written > size - out.len() {
                return Err(self.bad("has an entry longer than its header says"));
            }
            out.extend_from_slice(&output[..written]);
            if ended && out.len() == size {
                return Ok(out);
            }
            if ended {
                return Err(self.bad("has an entry shorter than its header says"));
            }
        }
    }

    /// Applies `delta` to `base`, as git's `patch_delta` does.
    /// A result larger than `limit` is refused before anything is copied.
    fn apply(&self, base: &[u8], delta: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
        let bad = || self.bad("has a bad delta");
        if delta.len() < MIN_DELTA {
            return Err(bad());
        }
        let mut bytes = delta.iter().copied();
        let from = delta_size(&mut bytes).ok_or_else(bad)?;
        if from != base.len() as u64 {
            return Err(bad());
        }
        let size = delta_size(&mut bytes).ok_or_else(bad)?;
        let size = usize::try_from(size).map_err(|_| bad())?;
        if size > limit {
            return Err(self.bad("has a delta whose result is larger than the limit"));
        }
        let mut out = Vec::with_capacity(size.min(1 << 24));
        while let Some(op) = bytes.next() {
            let piece = if op & 0x80 != 0 {
                let mut start = 0;
                for n in 0..4 {
                    if op & (1 << n) != 0 {
                        start |= usize::from(bytes.next().ok_or_else(bad)?) << (8 * n);
                    }
                }
                let mut len = 0;
                for n in 0..3 {
                    if op & (0x10 << n) != 0 {
                        len |= usize::from(bytes.next().ok_or_else(bad)?) << (8 * n);
                    }
                }
                if len == 0 {
                    len = 0x10000;
                }
                base.get(start..start + len).ok_or_else(bad)?
            } else if op != 0 {
                let at = delta.len() - bytes.len();
                let piece = delta.get(at..at + usize::from(op)).ok_or_else(bad)?;
                bytes.nth(piece.len() - 1);
                piece
            } else {
                return Err(bad());
            };
            if piece.len() > size - out.len() {
                return Err(bad());
            }
            out.extend_from_slice(piece);
        }
        if out.len() != size {
            return Err(bad());
        }
        Ok(out)
    }
}

/// A size in a delta's header, as git's `get_delta_hdr_size` reads it: it
/// stops at the end of the delta, even in the middle of a size.
fn delta_size(bytes: &mut impl Iterator<Item = u8>) -> Option<u64> {
    let mut size = 0;
    let mut shift: u32 = 0;
    for byte in bytes {
        let part = u64::from(byte & 0x7f);
        if part != 0 && (shift >= 64 || (part << shift) >> shift != part) {
            return None;
        }
        if shift < 64 {
            size |= part << shift;
        }
        shift = shift.saturating_add(7);
        if byte & 0x80 == 0 {
            break;
        }
    }
    Some(size)
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_size_of_many_empty_groups_is_zero() {
        // More groups than a 32-bit count of shifts can hold.
        let mut bytes = std::iter::repeat_n(0x80, 310_000_000).chain([0]);
        assert_eq!(delta_size(&mut bytes), Some(0));
    }
}
