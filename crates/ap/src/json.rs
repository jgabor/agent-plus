//! A JSON reader for what `ap` reads from a harness's publisher, such as a
//! GitHub release. It reads RFC 8259 JSON into a [`Value`], and keeps a
//! number as its text, because nothing `ap` reads needs its value.

/// A JSON value. An object keeps its members in order, and a repeated name
/// keeps its first value.
#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// How deeply arrays and objects may nest. GitHub's releases nest three
/// deep.
const MAX_DEPTH: usize = 64;

impl Value {
    /// The value of the member `name`, if this is an object that has one.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(values) => Some(values),
            _ => None,
        }
    }
}

/// Reads `text` as one JSON value, or `None` if it isn't one.
pub fn parse(text: &str) -> Option<Value> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value(0)?;
    reader.space();
    (reader.at == reader.bytes.len()).then_some(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        self.space();
        (self.peek() == Some(byte)).then(|| self.at += 1)
    }

    fn word(&mut self, word: &str, value: Value) -> Option<Value> {
        let end = self.at + word.len();
        (self.bytes.get(self.at..end) == Some(word.as_bytes())).then(|| {
            self.at = end;
            value
        })
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        self.space();
        match self.peek()? {
            b'n' => self.word("null", Value::Null),
            b't' => self.word("true", Value::Bool(true)),
            b'f' => self.word("false", Value::Bool(false)),
            b'"' => self.string().map(Value::String),
            b'[' if depth < MAX_DEPTH => {
                self.at += 1;
                let mut values = Vec::new();
                if self.eat(b']').is_some() {
                    return Some(Value::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    if self.eat(b']').is_some() {
                        return Some(Value::Array(values));
                    }
                    self.eat(b',')?;
                }
            }
            b'{' if depth < MAX_DEPTH => {
                self.at += 1;
                let mut members: Vec<(String, Value)> = Vec::new();
                if self.eat(b'}').is_some() {
                    return Some(Value::Object(members));
                }
                loop {
                    self.space();
                    let name = self.string()?;
                    self.eat(b':')?;
                    let value = self.value(depth + 1)?;
                    if !members.iter().any(|(key, _)| *key == name) {
                        members.push((name, value));
                    }
                    if self.eat(b'}').is_some() {
                        return Some(Value::Object(members));
                    }
                    self.eat(b',')?;
                }
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.at;
        let digits = |reader: &mut Self| {
            let from = reader.at;
            while matches!(reader.peek(), Some(b'0'..=b'9')) {
                reader.at += 1;
            }
            (reader.at > from).then_some(())
        };
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek()? {
            b'0' => self.at += 1,
            _ => digits(self)?,
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            digits(self)?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            digits(self)?;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
        Some(Value::Number(text.to_string()))
    }

    /// A string, from its opening quote.
    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.at += 1;
        let mut text = Vec::new();
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(text).ok(),
                b'\\' => {
                    let escaped = self.peek()?;
                    self.at += 1;
                    let c = match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return None,
                    };
                    text.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0..=0x1f => return None,
                _ => text.push(byte),
            }
        }
    }

    /// The character of a `\u` escape, after the `u`, with the low half of a
    /// surrogate pair if it starts one.
    fn unicode(&mut self) -> Option<char> {
        let high = self.hex4()?;
        if !(0xd800..0xdc00).contains(&high) {
            return char::from_u32(high);
        }
        if self.bytes.get(self.at..self.at + 2)? != b"\\u" {
            return None;
        }
        self.at += 2;
        let low = self.hex4()?;
        if !(0xdc00..0xe000).contains(&low) {
            return None;
        }
        char::from_u32(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00))
    }

    fn hex4(&mut self) -> Option<u32> {
        let hex = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.at += 4;
        u32::from_str_radix(hex, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_release() {
        let text = r#"{
  "tag_name": "rust-v0.169.0",
  "prerelease": false,
  "id": 12345,
  "body": "Fixes \"quotes\", \\ and \u00e9 \ud83d\ude00\n",
  "assets": [
    { "name": "a.zst", "size": 1.5e3, "digest": null },
    { "name": "codex.zst", "digest": "sha256:00ff" }
  ]
}"#;
        let release = parse(text).unwrap();
        assert_eq!(
            release.get("tag_name").unwrap().as_str(),
            Some("rust-v0.169.0")
        );
        assert_eq!(release.get("prerelease"), Some(&Value::Bool(false)));
        assert_eq!(release.get("id"), Some(&Value::Number("12345".into())));
        assert_eq!(
            release.get("body").unwrap().as_str(),
            Some("Fixes \"quotes\", \\ and \u{e9} \u{1f600}\n")
        );
        let assets = release.get("assets").unwrap().as_array().unwrap();
        assert_eq!(assets[0].get("digest"), Some(&Value::Null));
        assert_eq!(
            assets[1].get("digest").unwrap().as_str(),
            Some("sha256:00ff")
        );
    }

    #[test]
    fn a_repeated_name_keeps_its_first_value() {
        let value = parse(r#"{"a": "one", "a": "two"}"#).unwrap();
        assert_eq!(value.get("a").unwrap().as_str(), Some("one"));
    }

    #[test]
    fn refuses_what_is_not_json() {
        for text in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "{\"a\":1,}",
            "\"open",
            "\"a\u{1}\"",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\ud800\\u0041\"",
            "01",
            "1.",
            "-",
            "tru",
            "{} {}",
            "{'a': 1}",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn refuses_nesting_past_the_limit() {
        let deep = |depth: usize| "[".repeat(depth) + &"]".repeat(depth);
        assert!(parse(&deep(MAX_DEPTH)).is_some());
        assert_eq!(parse(&deep(MAX_DEPTH + 1)), None);
    }
}
