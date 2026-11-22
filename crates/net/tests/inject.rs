//! A hidden secret's value in a request head, by **Hidden** under SECRET in
//! SPEC.md. The proxy's own tests drive it over TLS. These check each
//! header on its own.

use net::{NameCheck, Swap, inject};

const NAME: &str = "api.example.com";
const PLACEHOLDER: &[u8] = b"0123456789abcdef0123456789abcdef";
const VALUE: &[u8] = b"sk-real-value";
const HEADERS: &[&str] = &["Authorization", "x-api-key"];

fn injected(head: &str) -> String {
    let swaps = [Swap {
        placeholder: PLACEHOLDER,
        value: VALUE,
    }];
    let mut out = Vec::new();
    inject(head.as_bytes(), NAME, HEADERS, &swaps, &mut out);
    String::from_utf8(out).unwrap()
}

fn placeholder() -> &'static str {
    std::str::from_utf8(PLACEHOLDER).unwrap()
}

// spec(SECRET): in request headers on the adapter's allowlist (for example, `Authorization` and `x-api-key`), never in query strings or bodies
#[test]
fn only_the_headers_on_the_allowlist_get_the_value() {
    let p = placeholder();
    let head = format!(
        "GET /?key={p} HTTP/1.1\r\nHost: {NAME}\r\nauthorization: Bearer {p}\r\n\
         X-API-KEY: {p}\r\nX-Other: {p}\r\n\r\n"
    );
    assert_eq!(
        injected(&head),
        format!(
            "GET /?key={p} HTTP/1.1\r\nHost: {NAME}\r\nauthorization: Bearer sk-real-value\r\n\
             X-API-KEY: sk-real-value\r\nX-Other: {p}\r\n\r\n"
        )
    );
}

// spec(SECRET): In an `Authorization: Basic` header, which git sends over HTTPS, the proxy decodes the credentials, replaces the placeholder in them, and encodes them again.
#[test]
fn basic_credentials_are_decoded_replaced_and_encoded_again() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    let p = placeholder();
    let sent = STANDARD.encode(format!("x-access-token:{p}"));
    let real = STANDARD.encode("x-access-token:sk-real-value");
    let head = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: basic  {sent}\r\n\r\n");
    assert_eq!(
        injected(&head),
        format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: basic  {real}\r\n\r\n")
    );
    // Credentials that don't decode are read as any other value.
    let broken = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: Basic {p}!\r\n\r\n");
    assert_eq!(
        injected(&broken),
        format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: Basic sk-real-value!\r\n\r\n")
    );
}

// spec(SECRET): On any other connection, the placeholder passes through unchanged.
#[test]
fn a_head_whose_host_names_another_host_passes_unchanged() {
    let p = placeholder();
    for host in ["other.example.com", "api.example.com.evil", ""] {
        let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {p}\r\n\r\n");
        assert_eq!(injected(&head), head, "{host}");
    }
    // Two `Host` headers name no one host.
    let two = format!(
        "GET / HTTP/1.1\r\nHost: {NAME}\r\nHost: other.example.com\r\nx-api-key: {p}\r\n\r\n"
    );
    assert_eq!(injected(&two), two);
    // A port, a final dot, and another case still name the host.
    let named = format!("GET / HTTP/1.1\r\nHost: API.example.com.:443\r\nx-api-key: {p}\r\n\r\n");
    assert!(injected(&named).contains("x-api-key: sk-real-value\r\n"));
}

// spec(SECRET): A value that holds a line break replaces the placeholder only in `Basic` credentials, where it is encoded, so the proxy never puts a line break in a header.
#[test]
fn a_value_with_a_line_break_is_never_put_in_a_header() {
    let p = placeholder();
    let head = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nx-api-key: {p}\r\n\r\n");
    let swaps = [Swap {
        placeholder: PLACEHOLDER,
        value: b"one\r\nX-Injected: two",
    }];
    let mut out = Vec::new();
    inject(head.as_bytes(), NAME, HEADERS, &swaps, &mut out);
    assert_eq!(out, head.as_bytes());
    // In `Basic` credentials, which are encoded, it is.
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    let sent = STANDARD.encode(format!("git:{p}"));
    let basic = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: Basic {sent}\r\n\r\n");
    let mut out = Vec::new();
    inject(basic.as_bytes(), NAME, HEADERS, &swaps, &mut out);
    let real = STANDARD.encode("git:one\r\nX-Injected: two");
    let expected = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\nAuthorization: Basic {real}\r\n\r\n");
    assert_eq!(String::from_utf8(out).unwrap(), expected);
}

#[test]
fn a_head_that_would_grow_too_long_passes_unchanged() {
    let p = placeholder();
    let keys: String = (0..64).map(|_| format!("x-api-key: {p}\r\n")).collect();
    let head = format!("GET / HTTP/1.1\r\nHost: {NAME}\r\n{keys}\r\n");
    let value = vec![b'v'; net::LONGEST / 32];
    let swaps = [Swap {
        placeholder: PLACEHOLDER,
        value: &value,
    }];
    let mut out = Vec::new();
    inject(head.as_bytes(), NAME, HEADERS, &swaps, &mut out);
    assert_eq!(out, head.as_bytes());
}

#[test]
fn the_injecting_check_rewrites_each_head_and_passes_bodies_unread() {
    let p = placeholder();
    let first = format!(
        "POST /a HTTP/1.1\r\nHost: {NAME}\r\nx-api-key: {p}\r\nContent-Length: 32\r\n\r\n{p}"
    );
    let second = format!("GET /b HTTP/1.1\r\nHost: other.example.com\r\nx-api-key: {p}\r\n\r\n");
    let input = [first.as_bytes(), second.as_bytes()].concat();
    let swaps = [Swap {
        placeholder: PLACEHOLDER,
        value: VALUE,
    }];
    // Fed a byte at a time, so a head is rewritten only once it is whole.
    let mut check = NameCheck::injecting(NAME);
    let mut out = Vec::new();
    for byte in input.chunks(1) {
        check
            .feed_rewriting(byte, &mut out, &mut |_| true, &mut |head, out| {
                inject(head, NAME, HEADERS, &swaps, out)
            })
            .unwrap();
    }
    let expected = format!(
        "POST /a HTTP/1.1\r\nHost: {NAME}\r\nx-api-key: sk-real-value\r\nContent-Length: 32\r\n\r\n{p}{second}"
    );
    assert_eq!(String::from_utf8(out).unwrap(), expected);
    // The check for a host with URL rules still refuses another host.
    let mut strict = NameCheck::requests(NAME);
    assert!(strict.feed(second.as_bytes(), &mut Vec::new()).is_err());
}
