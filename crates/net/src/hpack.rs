//! HPACK, the header compression of HTTP/2, by RFC 7541. Every header block
//! the proxy decodes comes from the guest, so the decoder is strict: a block
//! that RFC 7541 doesn't allow is an error, never a guess. After an error,
//! the decoder refuses every block, because its dynamic table may no longer
//! agree with the encoder's.

use std::collections::VecDeque;

/// What an entry takes beyond its name and value, by section 4.1. A header
/// list's size counts it too, as SETTINGS_MAX_HEADER_LIST_SIZE does.
const OVERHEAD: usize = 32;
/// The most continuation bytes an integer may take: enough for 32 bits.
const CONTINUATIONS: u32 = 5;

/// A header field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    /// It came as a literal never indexed, which an intermediary must keep,
    /// by section 7.1.3.
    pub never_indexed: bool,
}

/// Why the decoder refused a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An index of 0, or past the static and dynamic tables.
    Index,
    /// A dynamic table size update past the limit.
    TableSize,
    /// A dynamic table size update after a header field.
    LateUpdate,
    /// The limit fell below the table's size, and the next block doesn't
    /// start with an update to the lowest limit set, or less.
    MissingUpdate,
    /// A Huffman string that holds EOS, or ends with more than 7 bits of
    /// padding, or with padding that isn't all ones.
    Huffman,
    /// An integer past 2^32 - 1, or one in more bytes than that needs.
    Overflow,
    /// The block ends inside a header field.
    Truncated,
    /// The headers are larger than the list limit.
    ListSize,
}

/// Decodes the header blocks of one direction of one connection.
pub struct Decoder {
    table: Table,
    /// The most the encoder may make the table, which the caller sets.
    limit: usize,
    /// The lowest limit set since the last block, where it fell below the
    /// table's size. The next block must start with an update to it or less.
    lowest: Option<usize>,
    /// The most a block's headers may take, counted as `OVERHEAD` and their
    /// names and values.
    list_limit: usize,
    failed: Option<Error>,
}

impl Decoder {
    /// A decoder whose table starts at `table_size`, which HTTP/2 sets to
    /// 4096 on every connection, and whose blocks' headers may take
    /// `list_limit`.
    pub fn new(table_size: usize, list_limit: usize) -> Decoder {
        Decoder {
            table: Table::new(table_size),
            limit: table_size,
            lowest: None,
            list_limit,
            failed: None,
        }
    }

    /// Sets the most the encoder may make the table, as HTTP/2 does once
    /// the peer acknowledges SETTINGS_HEADER_TABLE_SIZE.
    pub fn set_table_limit(&mut self, limit: usize) {
        self.limit = limit;
        if limit < self.table.max {
            self.lowest = Some(self.lowest.map_or(limit, |lowest| lowest.min(limit)));
        }
    }

    /// The headers of `block`, in order.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<Header>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let decoded = self.read(block);
        if let Err(error) = decoded {
            self.failed = Some(error);
        }
        decoded
    }

    /// The dynamic table's entries, newest first, as they are indexed.
    pub fn table(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (self.table.entries.iter()).map(|(name, value)| (&name[..], &value[..]))
    }

    /// The dynamic table's size, by section 4.1.
    pub fn table_size(&self) -> usize {
        self.table.size
    }

    fn read(&mut self, block: &[u8]) -> Result<Vec<Header>, Error> {
        let mut reader = Reader(block);
        let mut headers = Vec::new();
        let mut list_size = 0;
        let mut lowest = self.lowest.take();
        while let Some(&first) = reader.0.first() {
            if first & 0xe0 == 0x20 {
                // A dynamic table size update, by section 6.3.
                if !headers.is_empty() {
                    return Err(Error::LateUpdate);
                }
                let size = reader.integer(5)?;
                if size > self.limit {
                    return Err(Error::TableSize);
                }
                if lowest.take().is_some_and(|lowest| size > lowest) {
                    return Err(Error::MissingUpdate);
                }
                self.table.resize(size);
                continue;
            }
            if lowest.is_some() {
                return Err(Error::MissingUpdate);
            }
            let mut count = |name: &[u8], value: &[u8]| {
                list_size += name.len() + value.len() + OVERHEAD;
                match list_size > self.list_limit {
                    true => Err(Error::ListSize),
                    false => Ok(()),
                }
            };
            if first & 0x80 != 0 {
                // An indexed field, by section 6.1.
                let (name, value) = self.table.get(reader.integer(7)?)?;
                count(name, value)?;
                headers.push(Header {
                    name: name.to_vec(),
                    value: value.to_vec(),
                    never_indexed: false,
                });
                continue;
            }
            // A literal field, by section 6.2: with incremental indexing,
            // never indexed, or without indexing.
            let (prefix, indexing, never_indexed) = match first {
                0x40.. => (6, true, false),
                0x10.. => (4, false, true),
                _ => (4, false, false),
            };
            let name = match reader.integer(prefix)? {
                0 => reader.string()?,
                index => self.table.get(index)?.0.to_vec(),
            };
            let value = reader.string()?;
            count(&name, &value)?;
            if indexing {
                self.table.insert(name.clone(), value.clone());
            }
            headers.push(Header {
                name,
                value,
                never_indexed,
            });
        }
        if lowest.is_some() {
            return Err(Error::MissingUpdate);
        }
        Ok(headers)
    }
}

/// Encodes the header blocks of one direction of one connection. It indexes
/// every field it can, and Huffman codes a string where that is shorter.
pub struct Encoder {
    table: Table,
    /// The most the encoder makes the table, whatever the limit.
    own: usize,
    /// The table's size as the decoder knows it, from the last block.
    signaled: usize,
    /// The lowest size the table took since the last block.
    lowest: usize,
}

impl Encoder {
    /// An encoder whose table starts at, and never grows past,
    /// `table_size`, which HTTP/2 sets to 4096 on every connection.
    pub fn new(table_size: usize) -> Encoder {
        Encoder {
            table: Table::new(table_size),
            own: table_size,
            signaled: table_size,
            lowest: table_size,
        }
    }

    /// Sets the most the decoder lets the table take, from the peer's
    /// SETTINGS_HEADER_TABLE_SIZE. The table takes that, up to the
    /// encoder's own size, and the next block says so.
    pub fn set_table_limit(&mut self, limit: usize) {
        let size = limit.min(self.own);
        self.lowest = self.lowest.min(size);
        self.table.resize(size);
    }

    /// Appends the block that encodes `headers` to `block`.
    pub fn encode(&mut self, headers: &[Header], block: &mut Vec<u8>) {
        // The lowest size first, so the decoder evicts what the encoder
        // did, then the size the table has now, by section 4.2.
        if self.lowest < self.signaled {
            integer(block, 0x20, 5, self.lowest);
        }
        if self.table.max != self.lowest.min(self.signaled) {
            integer(block, 0x20, 5, self.table.max);
        }
        (self.signaled, self.lowest) = (self.table.max, self.table.max);
        for header in headers {
            let (both, name) = self.table.find(&header.name, &header.value);
            match both {
                Some(index) if !header.never_indexed => {
                    integer(block, 0x80, 7, index);
                    continue;
                }
                _ if header.never_indexed => integer(block, 0x10, 4, name.unwrap_or(0)),
                _ => integer(block, 0x40, 6, name.unwrap_or(0)),
            }
            if name.is_none() {
                string(block, &header.name);
            }
            string(block, &header.value);
            if !header.never_indexed {
                self.table.insert(header.name.clone(), header.value.clone());
            }
        }
    }
}

/// The dynamic table, by section 2.3.2, with the static table before it.
struct Table {
    /// Newest first.
    entries: VecDeque<(Vec<u8>, Vec<u8>)>,
    size: usize,
    max: usize,
}

impl Table {
    fn new(max: usize) -> Table {
        Table {
            entries: VecDeque::new(),
            size: 0,
            max,
        }
    }

    /// The entry at `index` of the two tables together, by section 2.3.3.
    fn get(&self, index: usize) -> Result<(&[u8], &[u8]), Error> {
        match index {
            0 => Err(Error::Index),
            1..=61 => Ok(STATIC[index - 1]),
            _ => (self.entries.get(index - 62))
                .map(|(name, value)| (&name[..], &value[..]))
                .ok_or(Error::Index),
        }
    }

    /// The first index of an entry with this name and value, and the first
    /// of one with this name.
    fn find(&self, name: &[u8], value: &[u8]) -> (Option<usize>, Option<usize>) {
        let dynamic = self
            .entries
            .iter()
            .map(|(name, value)| (&name[..], &value[..]));
        let mut named = None;
        for (index, entry) in STATIC.into_iter().chain(dynamic).enumerate() {
            if entry.0 == name {
                if entry.1 == value {
                    return (Some(index + 1), Some(index + 1));
                }
                named.get_or_insert(index + 1);
            }
        }
        (None, named)
    }

    /// Adds an entry, evicting the oldest to make room, by section 4.4.
    fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let size = name.len() + value.len() + OVERHEAD;
        self.evict(self.max.saturating_sub(size));
        if size <= self.max {
            self.size += size;
            self.entries.push_front((name, value));
        }
    }

    fn resize(&mut self, max: usize) {
        self.max = max;
        self.evict(max);
    }

    /// Evicts the oldest entries until the table takes `size` or less.
    fn evict(&mut self, size: usize) {
        while self.size > size {
            let (name, value) = self.entries.pop_back().expect("a size has entries");
            self.size -= name.len() + value.len() + OVERHEAD;
        }
    }
}

/// What is left of a block.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, Error> {
        let (&byte, rest) = self.0.split_first().ok_or(Error::Truncated)?;
        self.0 = rest;
        Ok(byte)
    }

    /// An integer in the low `prefix` bits of this byte and the bytes
    /// after it, by section 5.1.
    fn integer(&mut self, prefix: u32) -> Result<usize, Error> {
        let mask = (1 << prefix) - 1;
        let mut value = u64::from(self.byte()? & mask);
        if value < u64::from(mask) {
            return Ok(value as usize);
        }
        for continuation in 0..CONTINUATIONS {
            let byte = self.byte()?;
            value += u64::from(byte & 0x7f) << (7 * continuation);
            if byte & 0x80 == 0 {
                let value = u32::try_from(value).map_err(|_| Error::Overflow)?;
                return Ok(value as usize);
            }
        }
        Err(Error::Overflow)
    }

    /// A string, by section 5.2.
    fn string(&mut self) -> Result<Vec<u8>, Error> {
        let huffman = self.0.first().is_some_and(|first| first & 0x80 != 0);
        let length = self.integer(7)?;
        if length > self.0.len() {
            return Err(Error::Truncated);
        }
        let (string, rest) = self.0.split_at(length);
        self.0 = rest;
        match huffman {
            true => huffman_decode(string),
            false => Ok(string.to_vec()),
        }
    }
}

/// Appends `value` in the low `prefix` bits of a byte that starts with
/// `first`, and the bytes after it, by section 5.1.
fn integer(block: &mut Vec<u8>, first: u8, prefix: u32, value: usize) {
    let mask = (1 << prefix) - 1;
    if value < mask {
        block.push(first | value as u8);
        return;
    }
    block.push(first | mask as u8);
    let mut rest = value - mask;
    while rest >= 0x80 {
        block.push(0x80 | (rest & 0x7f) as u8);
        rest >>= 7;
    }
    block.push(rest as u8);
}

/// Appends `value` as a string, Huffman coded where that is shorter.
fn string(block: &mut Vec<u8>, value: &[u8]) {
    let bits: usize = value
        .iter()
        .map(|&byte| usize::from(CODES[byte as usize].1))
        .sum();
    let length = bits.div_ceil(8);
    if length >= value.len() {
        integer(block, 0x00, 7, value.len());
        block.extend_from_slice(value);
        return;
    }
    integer(block, 0x80, 7, length);
    let (mut held, mut count) = (0u64, 0);
    for &byte in value {
        let (code, length) = CODES[byte as usize];
        held = held << length | u64::from(code);
        count += length;
        while count >= 8 {
            count -= 8;
            block.push((held >> count) as u8);
        }
        held &= (1 << count) - 1;
    }
    if count > 0 {
        // Padded with the start of EOS, which is all ones.
        block.push((held << (8 - count) | (0xff >> count)) as u8);
    }
}

/// Decodes a Huffman coded string, by section 5.2.
fn huffman_decode(string: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoded = Vec::with_capacity(string.len() * 8 / 5);
    let (mut code, mut length) = (0u32, 0);
    for &byte in string {
        for bit in (0..8).rev() {
            code = code << 1 | u32::from(byte >> bit & 1);
            length += 1;
            // The code is canonical, so the codes of each length are
            // consecutive, and the code is complete, so a symbol ends
            // within 30 bits.
            let offset = code.wrapping_sub(CANONICAL.first[length]);
            if offset < CANONICAL.count[length] {
                let symbol = CANONICAL.symbols[CANONICAL.start[length] + offset as usize];
                if symbol == EOS {
                    return Err(Error::Huffman);
                }
                decoded.push(symbol as u8);
                (code, length) = (0, 0);
            }
        }
    }
    if length > 7 || code != (1 << length) - 1 {
        return Err(Error::Huffman);
    }
    Ok(decoded)
}

const EOS: u16 = 256;
const LONGEST: usize = 30;

/// The Huffman code by length: for each length, the first code of that
/// length, how many codes it has, and where its symbols start in `symbols`,
/// which holds the symbols in the order of their codes.
struct Canonical {
    first: [u32; LONGEST + 1],
    count: [u32; LONGEST + 1],
    start: [usize; LONGEST + 1],
    symbols: [u16; 257],
}

const CANONICAL: Canonical = canonical();

/// Sorts the codes by length, and fails to compile unless the codes of
/// each length are consecutive, in the order of their symbols, and the
/// code is complete.
const fn canonical() -> Canonical {
    let mut canonical = Canonical {
        first: [0; LONGEST + 1],
        count: [0; LONGEST + 1],
        start: [0; LONGEST + 1],
        symbols: [0; 257],
    };
    let (mut sorted, mut kraft) = (0, 0u64);
    let mut length = 1;
    while length <= LONGEST {
        canonical.start[length] = sorted;
        let mut symbol = 0;
        while symbol < CODES.len() {
            let (code, bits) = CODES[symbol];
            if bits as usize == length {
                let count = canonical.count[length];
                if count == 0 {
                    canonical.first[length] = code;
                }
                assert!(code == canonical.first[length] + count, "not consecutive");
                canonical.symbols[sorted] = symbol as u16;
                canonical.count[length] = count + 1;
                sorted += 1;
                kraft += 1 << (LONGEST - length);
            }
            symbol += 1;
        }
        length += 1;
    }
    assert!(
        sorted == CODES.len() && kraft == 1 << LONGEST,
        "not complete"
    );
    canonical
}

/// The static table, by Appendix A.
const STATIC: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// Each symbol's Huffman code and its length in bits, by Appendix B, with
/// EOS last.
const CODES: [(u32, u8); 257] = [
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
    (0x3fffffff, 30),
];
