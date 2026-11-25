//! HPACK, checked against oracles its author didn't write: Appendices A, B,
//! and C of RFC 7541, kept verbatim in `fixtures/hpack/rfc7541.txt`, and the
//! `hpack-test-case` corpus, from commit 8a1406e of
//! <https://github.com/http2jp/hpack-test-case>, under its MIT license.
//!
//! The corpus is kept in a smaller form than its JSON. Each encoder's
//! directory holds a `.bin` for each story it encoded: for each case, its
//! `header_table_size` as four big-endian bytes, `ffffffff` where it has
//! none, then its wire's length as four, then the wire. `raw-data` holds a
//! `.txt` for each story: each case's headers, one `name: value` to a line,
//! and a blank line after each case. Every encoder's headers are the same
//! as `raw-data`'s for that story.

use std::fs;
use std::path::{Path, PathBuf};

use net::hpack::{Decoder, Encoder, Error, Header};

const LIST_LIMIT: usize = 1 << 20;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hpack")
}

fn rfc() -> String {
    fs::read_to_string(fixtures().join("rfc7541.txt")).unwrap()
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.into(),
        value: value.into(),
        never_indexed: false,
    }
}

fn pairs(headers: &[Header]) -> Vec<(String, String)> {
    let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
    (headers.iter())
        .map(|header| (text(&header.name), text(&header.value)))
        .collect()
}

/// The names and values of `headers`.
fn fields(headers: &[Header]) -> Vec<(&[u8], &[u8])> {
    (headers.iter())
        .map(|header| (&header.name[..], &header.value[..]))
        .collect()
}

fn decoded(block: &[u8]) -> Result<Vec<Header>, Error> {
    Decoder::new(4096, LIST_LIMIT).decode(block)
}

// Appendix A.

#[test]
fn each_static_index_is_the_entry_appendix_a_lists() {
    let mut rows = 0;
    for line in rfc().lines() {
        let cells: Vec<&str> = line.trim().split('|').map(str::trim).collect();
        let [_, index, name, value, _] = cells[..] else {
            continue;
        };
        let Ok(index) = index.parse::<u8>() else {
            continue;
        };
        let headers = decoded(&[0x80 | index]).unwrap();
        assert_eq!(pairs(&headers), [(name.into(), value.into())], "{index}");
        rows += 1;
    }
    assert_eq!(rows, 61);
}

// Appendix B.

/// Each symbol's code and its length in bits, as Appendix B lists them,
/// in rows such as `'/' ( 47)  |011000     18  [ 6]`.
fn codes() -> Vec<(u32, u32)> {
    let mut codes = Vec::new();
    for line in rfc().lines() {
        // The symbol is the number in parentheses, where its character,
        // quoted before it, may be a parenthesis or a bar.
        let symbol = (line.match_indices('(')).find_map(|(at, _)| {
            let (number, rest) = line[at + 1..].split_once(')')?;
            Some((number.trim().parse::<usize>().ok()?, rest))
        });
        let Some((symbol, rest)) = symbol else {
            continue;
        };
        let Some((code, length)) = rest.rsplit_once('[') else {
            continue;
        };
        let words: Vec<&str> = code.split_whitespace().collect();
        let (&[bits, hex], Ok(length)) = (&words[..], length.trim_end_matches(']').trim().parse())
        else {
            continue;
        };
        assert!(bits.starts_with('|'), "{line}");
        assert_eq!(symbol, codes.len(), "{line}");
        codes.push((u32::from_str_radix(hex, 16).unwrap(), length));
    }
    assert_eq!(codes.len(), 257);
    codes
}

/// A literal field without indexing, named `x`, whose value is the bits
/// given, Huffman coded and padded with ones.
fn huffman_value(codes: &[(u32, u32)]) -> Vec<u8> {
    let mut bits: Vec<bool> = (codes.iter())
        .flat_map(|&(code, length)| (0..length).rev().map(move |at| code >> at & 1 == 1))
        .collect();
    while !bits.len().is_multiple_of(8) {
        bits.push(true);
    }
    let string: Vec<u8> = (bits.chunks(8))
        .map(|byte| byte.iter().fold(0, |byte, &bit| byte << 1 | u8::from(bit)))
        .collect();
    // The length, with a 7-bit prefix, by section 5.1.
    let mut block = vec![0x00, 0x01, b'x'];
    match string.len() {
        short @ ..127 => block.push(0x80 | short as u8),
        long => {
            let rest = long - 127;
            assert!((128..1 << 14).contains(&rest));
            block.extend([0xff, 0x80 | (rest & 0x7f) as u8, (rest >> 7) as u8]);
        }
    }
    block.extend(string);
    block
}

#[test]
fn each_huffman_code_decodes_to_the_symbol_appendix_b_lists() {
    let codes = codes();
    for (symbol, &code) in codes[..256].iter().enumerate() {
        let headers = decoded(&huffman_value(&[code])).unwrap();
        assert_eq!(headers[0].value, [symbol as u8], "{symbol}");
    }
    let headers = decoded(&huffman_value(&codes[..256])).unwrap();
    assert_eq!(headers[0].value, (0..=255).collect::<Vec<u8>>());
}

#[test]
fn a_huffman_string_holding_eos_is_refused() {
    let codes = codes();
    let block = huffman_value(&[codes[b'a' as usize], codes[256]]);
    assert_eq!(decoded(&block), Err(Error::Huffman));
}

#[test]
fn huffman_padding_longer_than_seven_bits_is_refused() {
    // `a` is 00011, then eleven ones: a prefix of EOS that is too long.
    assert_eq!(
        decoded(&[0x00, 0x01, b'x', 0x82, 0x1f, 0xff]),
        Err(Error::Huffman)
    );
    // Eight ones alone.
    assert_eq!(
        decoded(&[0x00, 0x01, b'x', 0x81, 0xff]),
        Err(Error::Huffman)
    );
    // `a` padded with three ones decodes.
    let headers = decoded(&[0x00, 0x01, b'x', 0x81, 0x1f]).unwrap();
    assert_eq!(headers[0].value, b"a");
}

#[test]
fn huffman_padding_that_isnt_all_ones_is_refused() {
    // `a` is 00011, then 110.
    assert_eq!(
        decoded(&[0x00, 0x01, b'x', 0x81, 0x1e]),
        Err(Error::Huffman)
    );
    // `a` is 00011, then 000.
    assert_eq!(
        decoded(&[0x00, 0x01, b'x', 0x81, 0x18]),
        Err(Error::Huffman)
    );
}

// Appendix C.

/// One example of Appendix C: its section, its block, its header list,
/// how many fields it codes as never indexed, and the dynamic table after
/// it, with the table's size.
#[derive(Debug, Default)]
struct Example {
    section: String,
    block: Vec<u8>,
    encode: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    never_indexed: usize,
    table: Vec<(String, String)>,
    table_size: usize,
}

#[derive(PartialEq)]
enum Part {
    None,
    Encode,
    Hex,
    Decoding,
    Table,
    Headers,
}

fn field(text: &str) -> (String, String) {
    let (name, value) =
        (text.split_once(": ")).unwrap_or_else(|| (text.strip_suffix(':').unwrap(), ""));
    (name.into(), value.into())
}

fn examples() -> Vec<Example> {
    let text = rfc();
    let start = text.find("\nC.2.  ").unwrap();
    let mut examples: Vec<Example> = Vec::new();
    let mut part = Part::None;
    for line in text[start..].lines() {
        let indented = line
            .strip_prefix("   ")
            .filter(|rest| !rest.starts_with(' '));
        if line.starts_with("C.") {
            part = Part::None;
        }
        let bytes = line.as_bytes();
        if line.starts_with("C.") && bytes.len() > 6 && bytes[3] == b'.' && bytes[5] == b'.' {
            examples.push(Example {
                section: line.split_whitespace().next().unwrap().into(),
                ..Example::default()
            });
            part = Part::None;
            continue;
        }
        let Some(example) = examples.last_mut() else {
            continue;
        };
        match line.trim() {
            "Header list to encode:" => part = Part::Encode,
            "Hex dump of encoded data:" => part = Part::Hex,
            "Decoding process:" => part = Part::Decoding,
            "Dynamic Table (after decoding):" => part = Part::Table,
            "Dynamic table (after decoding): empty." => part = Part::None,
            "Decoded header list:" => part = Part::Headers,
            trimmed => match part {
                Part::Encode => {
                    if let Some(rest) = indented {
                        example.encode.push(field(rest));
                    }
                }
                Part::Hex => {
                    if let Some((hex, _)) = indented.and_then(|rest| rest.split_once(" | ")) {
                        for pair in hex.split_whitespace() {
                            for at in (0..pair.len()).step_by(2) {
                                let byte = u8::from_str_radix(&pair[at..at + 2], 16).unwrap();
                                example.block.push(byte);
                            }
                        }
                    }
                }
                Part::Decoding => {
                    if trimmed.ends_with("== Literal never indexed ==") {
                        example.never_indexed += 1;
                    }
                }
                Part::Table => {
                    if let Some(size) = trimmed.strip_prefix("Table size:") {
                        example.table_size = size.trim().parse().unwrap();
                        part = Part::None;
                    } else if let Some((_, entry)) = trimmed.split_once(") ") {
                        example.table.push(field(entry));
                    } else if line.starts_with("                ") {
                        // An entry the page wrapped.
                        let (_, value) = example.table.last_mut().unwrap();
                        value.push(' ');
                        value.push_str(trimmed);
                    }
                }
                Part::Headers => {
                    if let Some(rest) = indented {
                        example.headers.push(field(rest));
                    }
                }
                Part::None => {}
            },
        }
    }
    examples
}

/// The table size a group of examples starts from: C.5 and C.6 set
/// SETTINGS_HEADER_TABLE_SIZE to 256.
fn table_limit(section: &str) -> usize {
    match &section[..3] {
        "C.5" | "C.6" => 256,
        _ => 4096,
    }
}

/// Whether an example starts a new connection: C.2's are independent, and
/// C.3 to C.6 are each one connection.
fn starts_connection(section: &str) -> bool {
    section.starts_with("C.2") || section.ends_with(".1.")
}

#[test]
fn appendix_c_examples_decode_to_their_headers_and_tables() {
    let examples = examples();
    assert_eq!(examples.len(), 16);
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    for example in &examples {
        let section = &example.section;
        if starts_connection(section) {
            decoder = Decoder::new(table_limit(section), LIST_LIMIT);
        }
        assert!(!example.block.is_empty(), "{section}");
        assert!(!example.headers.is_empty(), "{section}");
        assert_eq!(example.encode, example.headers, "{section}");
        let headers = decoder.decode(&example.block).unwrap();
        assert_eq!(pairs(&headers), example.headers, "{section}");
        let never = headers.iter().filter(|header| header.never_indexed).count();
        assert_eq!(never, example.never_indexed, "{section}");
        let table: Vec<_> = (decoder.table())
            .map(|(name, value)| {
                (String::from_utf8(name.to_vec()).unwrap(), {
                    String::from_utf8(value.to_vec()).unwrap()
                })
            })
            .collect();
        assert_eq!(table, example.table, "{section}");
        assert_eq!(decoder.table_size(), example.table_size, "{section}");
    }
}

#[test]
fn appendix_c_header_lists_encode_to_what_decodes_back() {
    let (mut encoder, mut decoder) = (Encoder::new(4096), Decoder::new(4096, LIST_LIMIT));
    for example in &examples() {
        let section = &example.section;
        if starts_connection(section) {
            encoder = Encoder::new(table_limit(section));
            decoder = Decoder::new(table_limit(section), LIST_LIMIT);
        }
        let headers: Vec<_> = (example.encode.iter())
            .map(|(name, value)| Header {
                never_indexed: section == "C.2.3.",
                ..header(name, value)
            })
            .collect();
        let mut block = Vec::new();
        encoder.encode(&headers, &mut block);
        assert_eq!(decoder.decode(&block).unwrap(), headers, "{section}");
    }
}

// The hpack-test-case corpus.

/// Each case of a story: its headers.
fn story_headers(story: &str) -> Vec<Vec<Header>> {
    let path = fixtures()
        .join("hpack-test-case/raw-data")
        .join(format!("{story}.txt"));
    let text = fs::read_to_string(path).unwrap();
    let text = text.strip_suffix('\n').unwrap();
    (text.split("\n\n"))
        .map(|case| {
            (case.lines().map(field))
                .map(|(name, value)| header(&name, &value))
                .collect()
        })
        .collect()
}

/// Each case of an encoder's story: its table size, if it has one, and its
/// wire.
fn story_wires(path: &Path) -> Vec<(Option<usize>, Vec<u8>)> {
    let bytes = fs::read(path).unwrap();
    let mut rest = &bytes[..];
    let mut cases = Vec::new();
    while !rest.is_empty() {
        let number = |at: usize| u32::from_be_bytes(rest[at..at + 4].try_into().unwrap());
        let (size, length) = (number(0), number(4) as usize);
        let size = (size != u32::MAX).then_some(size as usize);
        cases.push((size, rest[8..8 + length].to_vec()));
        rest = &rest[8 + length..];
    }
    cases
}

fn stories(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = (fs::read_dir(directory).unwrap())
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
}

#[test]
fn every_story_of_hpack_test_case_decodes_to_its_headers() {
    let root = fixtures().join("hpack-test-case");
    let mut decoded = 0;
    for encoder in stories(&root) {
        let name = encoder.file_name().unwrap().to_str().unwrap();
        if name == "raw-data" || name == "LICENSE" {
            continue;
        }
        for path in stories(&encoder) {
            let story = path.file_stem().unwrap().to_str().unwrap();
            let expected = story_headers(story);
            let wires = story_wires(&path);
            assert_eq!(wires.len(), expected.len(), "{path:?}");
            let mut decoder = Decoder::new(4096, LIST_LIMIT);
            for (seqno, ((size, wire), headers)) in wires.iter().zip(&expected).enumerate() {
                if let Some(size) = size {
                    decoder.set_table_limit(*size);
                }
                let got = decoder.decode(wire).unwrap_or_else(|error| {
                    panic!("{path:?} case {seqno}: {error:?}");
                });
                assert_eq!(fields(&got), fields(headers), "{path:?} case {seqno}");
            }
            decoded += 1;
        }
    }
    assert_eq!(decoded, 446);
}

#[test]
fn every_story_of_hpack_test_case_encodes_to_what_decodes_back() {
    let limits = [4096, 256, 0, 4096, 1365, 16384];
    for path in stories(&fixtures().join("hpack-test-case/raw-data")) {
        let story = path.file_stem().unwrap().to_str().unwrap();
        let (mut encoder, mut decoder) = (Encoder::new(4096), Decoder::new(4096, LIST_LIMIT));
        for (seqno, headers) in story_headers(story).iter().enumerate() {
            // The limit changes as SETTINGS would change it.
            let limit = limits[seqno % limits.len()];
            encoder.set_table_limit(limit);
            decoder.set_table_limit(limit);
            let mut block = Vec::new();
            encoder.encode(headers, &mut block);
            let got = decoder.decode(&block);
            assert_eq!(got.as_ref(), Ok(headers), "{story} case {seqno}");
            assert!(decoder.table_size() <= limit.min(4096));
        }
    }
}

// What the decoder refuses.

#[test]
fn an_index_past_the_tables_is_refused() {
    assert_eq!(decoded(&[0x80]), Err(Error::Index));
    assert_eq!(decoded(&[0x80 | 62]), Err(Error::Index));
    // A literal field whose name is indexed past the tables.
    assert_eq!(decoded(&[0x40 | 62, 0x01, b'v']), Err(Error::Index));
    // One entry in the dynamic table, then the index after it.
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.decode(&[0x40, 0x01, b'n', 0x01, b'v']).unwrap();
    assert_eq!(
        pairs(&decoder.decode(&[0x80 | 62]).unwrap()),
        [("n".into(), "v".into())]
    );
    assert_eq!(decoder.decode(&[0x80 | 63]), Err(Error::Index));
}

#[test]
fn a_table_size_update_past_the_limit_is_refused() {
    // 4096, then 4097.
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    assert_eq!(decoder.decode(&[0x3f, 0xe1, 0x1f]), Ok(vec![]));
    assert_eq!(decoded(&[0x3f, 0xe2, 0x1f]), Err(Error::TableSize));
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.set_table_limit(100);
    assert_eq!(decoder.decode(&[0x3f, 0x46]), Err(Error::TableSize));
}

#[test]
fn a_table_size_update_after_a_field_is_refused() {
    assert_eq!(decoded(&[0x20, 0x82]).map(|headers| headers.len()), Ok(1));
    assert_eq!(decoded(&[0x82, 0x20]), Err(Error::LateUpdate));
}

#[test]
fn a_block_after_the_limit_fell_must_start_with_an_update_under_it() {
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.set_table_limit(100);
    assert_eq!(decoder.decode(&[0x82]), Err(Error::MissingUpdate));
    // The limit fell to 0 and rose again, so the update must be to 0 first.
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.set_table_limit(0);
    decoder.set_table_limit(4096);
    assert_eq!(
        decoder.decode(&[0x3f, 0xe1, 0x1f, 0x82]),
        Err(Error::MissingUpdate)
    );
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.set_table_limit(0);
    decoder.set_table_limit(4096);
    assert_eq!(
        decoder
            .decode(&[0x20, 0x3f, 0xe1, 0x1f, 0x82])
            .map(|h| h.len()),
        Ok(1)
    );
    // A rise needs no update.
    let mut decoder = Decoder::new(256, LIST_LIMIT);
    decoder.set_table_limit(4096);
    assert_eq!(decoder.decode(&[0x82]).map(|h| h.len()), Ok(1));
}

#[test]
fn an_integer_that_overflows_is_refused() {
    // An index.
    assert_eq!(
        decoded(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]),
        Err(Error::Overflow)
    );
    // A string's length.
    let block = [0x00, 0x7f, 0xff, 0xff, 0xff, 0xff, 0x7f];
    assert_eq!(decoded(&block), Err(Error::Overflow));
    // More continuation bytes than 32 bits need, even though they add zero.
    let block = [0x00, 0x01, b'x', 0x7f, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
    assert_eq!(decoded(&block), Err(Error::Overflow));
}

#[test]
fn a_block_that_ends_inside_a_field_is_refused() {
    for block in [
        &[0x40][..],
        &[0x40, 0x05, b'a'],
        &[0x40, 0x01, b'a'],
        &[0x40, 0x01, b'a', 0x02, b'b'],
        &[0xff],
        &[0x3f],
        &[0x00, 0x7f, 0x80],
        &[0x10, 0x01],
    ] {
        assert_eq!(decoded(block), Err(Error::Truncated), "{block:x?}");
    }
}

#[test]
fn a_decoder_that_refused_a_block_refuses_every_block_after_it() {
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    assert_eq!(decoder.decode(&[0x80]), Err(Error::Index));
    assert_eq!(decoder.decode(&[0x82]), Err(Error::Index));
}

// The limits the caller sets.

#[test]
fn the_dynamic_table_stays_within_the_limit() {
    let mut decoder = Decoder::new(4096, LIST_LIMIT);
    decoder.set_table_limit(100);
    // An update to 100, then two entries of 34 and one of 35.
    let mut block = vec![0x3f, 0x45];
    for value in [&b"a"[..], b"b", b"cc"] {
        block.extend([0x40, 0x01, b'n', value.len() as u8]);
        block.extend(value);
    }
    decoder.decode(&block).unwrap();
    assert_eq!(decoder.table_size(), 69);
    let table: Vec<_> = decoder.table().collect();
    assert_eq!(table, [(&b"n"[..], &b"cc"[..]), (b"n", b"b")]);
    // An entry larger than the table empties it.
    let mut block = vec![0x40, 0x01, b'n', 70];
    block.extend([b'v'; 70]);
    decoder.decode(&block).unwrap();
    assert_eq!(decoder.table_size(), 0);
    assert_eq!(decoder.table().count(), 0);
}

#[test]
fn headers_past_the_list_limit_are_refused() {
    // `:method: GET` is 7 + 3 + 32.
    assert_eq!(
        Decoder::new(4096, 126).decode(&[0x82; 3]).map(|h| h.len()),
        Ok(3)
    );
    assert_eq!(
        Decoder::new(4096, 125).decode(&[0x82; 3]),
        Err(Error::ListSize)
    );
    // A big entry in the table, indexed many times in a short block.
    let mut decoder = Decoder::new(4096, 64 * 1024);
    let mut block = vec![0x40, 0x01, b'n', 0x7f, 0xa1, 0x1e];
    block.extend([b'v'; 4000]);
    decoder.decode(&block).unwrap();
    assert_eq!(decoder.decode(&[0x80 | 62; 17]), Err(Error::ListSize));
}

// What the encoder writes.

#[test]
fn a_never_indexed_field_is_encoded_as_never_indexed() {
    let mut encoder = Encoder::new(4096);
    let secret = Header {
        never_indexed: true,
        ..header("authorization", "secret")
    };
    for _ in 0..2 {
        let mut block = Vec::new();
        encoder.encode(std::slice::from_ref(&secret), &mut block);
        // Never indexed, with the static name 23.
        assert_eq!(block[0], 0x1f);
        assert_eq!(block[1], 23 - 15);
        assert_eq!(decoded(&block), Ok(vec![secret.clone()]));
    }
}

#[test]
fn the_encoder_signals_the_lowest_limit_and_then_the_last() {
    let mut encoder = Encoder::new(4096);
    encoder.set_table_limit(0);
    encoder.set_table_limit(300);
    let mut block = Vec::new();
    encoder.encode(&[header(":method", "GET")], &mut block);
    // An update to 0, one to 300, then index 2.
    assert_eq!(block, [0x20, 0x3f, 0x8d, 0x02, 0x82]);
    // A limit above the encoder's own size grows the table only to that.
    encoder.set_table_limit(1 << 20);
    let mut block = Vec::new();
    encoder.encode(&[header(":method", "GET")], &mut block);
    assert_eq!(block, [0x3f, 0xe1, 0x1f, 0x82]);
    encoder.set_table_limit(8192);
    let mut block = Vec::new();
    encoder.encode(&[header(":method", "GET")], &mut block);
    assert_eq!(block, [0x82]);
}
