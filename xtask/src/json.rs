//! A small JSON reader, for the output of `cargo metadata` and `gh`.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
    /// `null`, a boolean, or a number. Nothing reads their values yet.
    Other,
}

const MAX_DEPTH: usize = 128;

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }
}

pub fn parse(text: &str) -> Result<Json, String> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value(0)?;
    reader.space();
    if reader.at != reader.bytes.len() {
        return Err(reader.error("trailing data"));
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.at)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.space();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.at += 1;
        let mut fields = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Ok(Json::Obj(fields));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return Err(self.error("expected `:`"));
            }
            fields.push((key, self.value(depth + 1)?));
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(Json::Obj(fields));
            }
            return Err(self.error("expected `,` or `}`"));
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                return Ok(Json::Arr(items));
            }
            return Err(self.error("expected `,` or `]`"));
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.at + 4;
        let digits = self
            .bytes
            .get(self.at..end)
            .and_then(|d| std::str::from_utf8(d).ok())
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("bad \\u escape"))?;
        let code = u32::from_str_radix(digits, 16).map_err(|e| e.to_string())?;
        self.at = end;
        Ok(code)
    }

    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        let byte = self
            .peek()
            .ok_or_else(|| self.error("unterminated escape"))?;
        self.at += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let mut code = self.hex4()?;
                if (0xd800..0xdc00).contains(&code) {
                    if !(self.eat(b'\\') && self.eat(b'u')) {
                        return Err(self.error("unpaired surrogate"));
                    }
                    let low = self.hex4()?;
                    if !(0xdc00..0xe000).contains(&low) {
                        return Err(self.error("unpaired surrogate"));
                    }
                    code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                }
                out.push(char::from_u32(code).ok_or_else(|| self.error("bad code point"))?);
            }
            _ => return Err(self.error("unknown escape")),
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while !matches!(self.peek(), None | Some(b'"' | b'\\' | 0..=0x1f)) {
                self.at += 1;
            }
            let chunk = std::str::from_utf8(&self.bytes[start..self.at])
                .map_err(|_| self.error("invalid UTF-8"))?;
            out.push_str(chunk);
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error("control character in string")),
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    fn digits(&mut self) -> bool {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at > start
    }

    fn number(&mut self) -> Result<Json, String> {
        self.eat(b'-');
        let leading_zero = self.peek() == Some(b'0');
        let start = self.at;
        if !self.digits() {
            return Err(self.error("expected a digit"));
        }
        if leading_zero && self.at - start > 1 {
            return Err(self.error("leading zero"));
        }
        if self.eat(b'.') && !self.digits() {
            return Err(self.error("expected a digit"));
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            if !self.digits() {
                return Err(self.error("expected a digit"));
            }
        }
        Ok(Json::Other)
    }

    fn literal(&mut self, word: &str) -> Result<Json, String> {
        let end = self.at + word.len();
        if self.bytes.get(self.at..end) == Some(word.as_bytes()) {
            self.at = end;
            Ok(Json::Other)
        } else {
            Err(self.error("unexpected word"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_values() {
        let v = parse(r#" {"a": [1, -2.5e3, true, false, null], "b": {"c": "d"}} "#).unwrap();
        assert_eq!(v.get("a").unwrap().as_arr().unwrap().len(), 5);
        let inner = v.get("b").unwrap().get("c").unwrap();
        assert_eq!(inner.as_str(), Some("d"));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn reads_empty_containers() {
        assert_eq!(parse("{}").unwrap(), Json::Obj(vec![]));
        assert_eq!(parse("[ ]").unwrap(), Json::Arr(vec![]));
    }

    #[test]
    fn reads_escapes() {
        let v = parse(r#""a\"\\\/\b\f\n\r\t\u0041\ud83d\ude00""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"\\/\u{8}\u{c}\n\r\tA\u{1f600}"));
    }

    #[test]
    fn keeps_multibyte_text() {
        assert_eq!(parse("\"héllo\"").unwrap().as_str(), Some("héllo"));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "{\"a\":1,}",
            "tru",
            "nul",
            "01",
            "1.",
            "1e",
            "-",
            "\"unterminated",
            "\"bad \\q\"",
            "\"\\u12\"",
            "\"\\ud83d\"",
            "\"\\ud83d\\u0041\"",
            "\"\n\"",
            "1 2",
            "{a:1}",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn limits_nesting() {
        let deep = "[".repeat(100_000);
        assert!(parse(&deep).unwrap_err().contains("nested too deeply"));
    }
}
