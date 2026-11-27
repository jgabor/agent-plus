//! The words of a share's records, in the Agentfile's line syntax, which the
//! server writes and `ap` reads back. The `cow` module of the server says
//! what each record means.

/// Adds a space, then `bytes` quoted, with `\"`, `\\`, and `\xNN` for a
/// control character or a byte that isn't UTF-8.
pub fn quote(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b' ');
    out.push(b'"');
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.extend(b"\\\""),
                '\\' => out.extend(b"\\\\"),
                c if c.is_ascii_control() => out.extend(format!("\\x{:02x}", c as u8).bytes()),
                c => out.extend(c.encode_utf8(&mut [0; 4]).bytes()),
            }
        }
        for byte in chunk.invalid() {
            out.extend(format!("\\x{byte:02x}").bytes());
        }
    }
    out.push(b'"');
}

/// The words of a record: bare ones, split at a space, and quoted ones.
pub struct Words<'a> {
    rest: &'a [u8],
}

impl<'a> Words<'a> {
    pub fn new(line: &'a [u8]) -> Self {
        Self { rest: line }
    }

    pub fn bare(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() || self.rest.first() == Some(&b'"') {
            return None;
        }
        let end = (self.rest.iter())
            .position(|&byte| byte == b' ')
            .unwrap_or(self.rest.len());
        let word = &self.rest[..end];
        self.rest = self.rest.get(end + 1..).unwrap_or_default();
        (!word.is_empty()).then_some(word)
    }

    /// A quoted word, with `\"`, `\\`, and `\xNN` read back.
    pub fn quoted(&mut self) -> Option<Vec<u8>> {
        let mut bytes = self.rest.strip_prefix(b"\"")?.iter();
        let mut word = Vec::new();
        loop {
            match *bytes.next()? {
                b'"' => break,
                b'\\' => match *bytes.next()? {
                    b'x' => {
                        let high = (*bytes.next()? as char).to_digit(16)?;
                        let low = (*bytes.next()? as char).to_digit(16)?;
                        word.push((high * 16 + low) as u8);
                    }
                    byte @ (b'"' | b'\\') => word.push(byte),
                    _ => return None,
                },
                byte => word.push(byte),
            }
        }
        let rest = bytes.as_slice();
        self.rest = match rest.first() {
            None => rest,
            Some(b' ') => &rest[1..],
            Some(_) => return None,
        };
        Some(word)
    }

    /// Whether the record has no words left.
    pub fn end(&self) -> Option<()> {
        self.rest.is_empty().then_some(())
    }
}

/// A decimal number.
pub fn number(word: &[u8]) -> Option<u64> {
    std::str::from_utf8(word).ok()?.parse().ok()
}

/// A mode's permissions, in octal.
pub fn mode(word: &[u8]) -> Option<u32> {
    u32::from_str_radix(std::str::from_utf8(word).ok()?, 8)
        .ok()
        .filter(|mode| mode & !0o7777 == 0)
}

/// Whether `path` names something beneath a share's root: names joined by
/// `/`, none of them empty, `.`, or `..`.
pub fn valid_path(path: &[u8]) -> bool {
    !path.is_empty()
        && !path.contains(&0)
        && path
            .split(|&byte| byte == b'/')
            .all(|name| !matches!(name, b"" | b"." | b".."))
}
