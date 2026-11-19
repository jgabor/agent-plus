//! Path normalization, checked against examples that RFC 3986 gives.

use net::normalize_path;

/// The path of the base URI in RFC 3986 section 5.4,
/// `http://a/b/c/d;p?q`.
const BASE: &str = "/b/c/d;p";

/// The examples of sections 5.4.1 and 5.4.2 whose reference is a path, and
/// the path of the URI they resolve to.
const RESOLVED: [(&str, &str); 28] = [
    ("g", "/b/c/g"),
    ("./g", "/b/c/g"),
    ("g/", "/b/c/g/"),
    ("/g", "/g"),
    (";x", "/b/c/;x"),
    ("g;x", "/b/c/g;x"),
    (".", "/b/c/"),
    ("./", "/b/c/"),
    ("..", "/b/"),
    ("../", "/b/"),
    ("../g", "/b/g"),
    ("../..", "/"),
    ("../../", "/"),
    ("../../g", "/g"),
    ("../../../g", "/g"),
    ("../../../../g", "/g"),
    ("/./g", "/g"),
    ("/../g", "/g"),
    ("g.", "/b/c/g."),
    (".g", "/b/c/.g"),
    ("g..", "/b/c/g.."),
    ("..g", "/b/c/..g"),
    ("./../g", "/b/g"),
    ("./g/.", "/b/c/g/"),
    ("g/./h", "/b/c/g/h"),
    ("g/../h", "/b/c/h"),
    ("g;x=1/./y", "/b/c/g;x=1/y"),
    ("g;x=1/../y", "/b/c/y"),
];

#[test]
fn dot_segments_go_as_section_5_4_resolves_them() {
    for (reference, resolved) in RESOLVED {
        // Section 5.2.3 merges a relative path with the base's directory.
        let merged = match reference.starts_with('/') {
            true => reference.to_string(),
            false => format!("{}{reference}", &BASE[..=BASE.rfind('/').unwrap()]),
        };
        assert_eq!(
            normalize_path(&merged).as_deref(),
            Some(resolved),
            "{reference}"
        );
    }
}

/// Section 6.2.2 normalizes `eXAMPLE://a/./b/../b/%63/%7bfoo%7d` to
/// `example://a/b/c/%7Bfoo%7D`.
#[test]
fn case_escapes_and_dots_go_as_section_6_2_2_normalizes_them() {
    assert_eq!(
        normalize_path("/./b/../b/%63/%7bfoo%7d").as_deref(),
        Some("/b/c/%7Bfoo%7D")
    );
}

/// Section 2.3 names the unreserved characters: an escape of one is the
/// character itself. Section 6.2.2.1 writes the hex digits of every other
/// escape in uppercase.
#[test]
fn unreserved_escapes_decode_and_others_go_uppercase() {
    let mut escaped = String::new();
    let mut plain = String::new();
    for byte in 0..=255u8 {
        escaped.push_str(&format!("/%{byte:02x}"));
        let unreserved = byte.is_ascii_alphanumeric() || b"-._~".contains(&byte);
        match unreserved {
            true => plain.push_str(&format!("/{}", byte as char)),
            false => plain.push_str(&format!("/%{byte:02X}")),
        }
    }
    // `%2E` alone is a dot segment, which goes.
    let plain = plain.replace("/./", "/");
    assert_eq!(normalize_path(&escaped).as_deref(), Some(plain.as_str()));
}

#[test]
fn a_broken_escape_has_no_normal_form() {
    for path in ["/%", "/%4", "/%zz", "/a%g1", "/%%41"] {
        assert_eq!(normalize_path(path), None, "{path}");
    }
}
