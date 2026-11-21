//! The request check, on a connection to a host with URL rules: what the
//! proxy reads of each HTTP request there, by **Decryption** under NETWORK
//! in SPEC.md.

use std::net::IpAddr;

use agentfile::{Network, Scheme, parse};
use net::{Connection, NameCheck, Refused, Request, could_request, proxied, url_rules};

const NAME: &str = "host.ap.internal";

fn network(lines: &str) -> Network {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    Network::read(&instructions).unwrap()
}

/// What passes of `input`, fed in pieces of `piece` bytes, to a request
/// check that allows a target under `/allowed`, and the targets it was
/// asked about.
fn fed(input: &[u8], piece: usize) -> (Vec<u8>, Result<(), Refused>, Vec<String>) {
    let mut check = NameCheck::requests(NAME);
    let mut out = Vec::new();
    let mut asked = Vec::new();
    let mut allows = |target: &str| {
        asked.push(target.to_owned());
        target.starts_with("/allowed")
    };
    for chunk in input.chunks(piece) {
        if let Err(refused) = check.feed_deciding(chunk, &mut out, &mut allows) {
            return (out, Err(refused), asked);
        }
    }
    (out, Ok(()), asked)
}

const ALLOWED: &[u8] = b"GET /allowed/a HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";
const POSTED: &[u8] =
    b"POST /allowed/b HTTP/1.1\r\nHost: host.ap.internal:8443\r\nContent-Length: 5\r\n\r\nGET /";
const DENIED: &[u8] = b"GET /denied HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";

#[test]
fn each_request_passes_only_once_its_target_is_allowed() {
    for piece in [usize::MAX, 1, 7] {
        let input = [ALLOWED, POSTED, DENIED].concat();
        let (out, result, asked) = fed(&input, piece.min(input.len()));
        assert_eq!(result, Err(Refused), "in pieces of {piece}");
        // The body of the second request, which looks like a request, is
        // a body, and no head of the denied request passed.
        assert_eq!(out, [ALLOWED, POSTED].concat(), "in pieces of {piece}");
        assert_eq!(asked, ["/allowed/a", "/allowed/b", "/denied"]);
    }
}

#[test]
fn anything_that_isn_t_http_is_refused() {
    let tls = [0x16, 0x03, 0x01, 0x00, 0x05, 0x01, 0, 0, 1, 0];
    for input in [
        &b"SSH-2.0-OpenSSH_9.9\r\n"[..],
        &tls,
        b"\0\0\0\0",
        b"GET /allowed HTTP/1.1\r\nHost: example.com\r\n\r\n",
        b"GET http://host.ap.internal/allowed HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n",
    ] {
        let (out, result, _) = fed(input, input.len());
        assert_eq!(result, Err(Refused), "{}", input.escape_ascii());
        assert_eq!(out, b"");
    }
}

#[test]
fn a_host_has_url_rules_only_for_its_exact_name() {
    let rules = network("NETWORK -\nNETWORK +https://Host.ap.internal/allowed\n");
    assert!(url_rules(&rules, NAME));
    assert!(url_rules(&rules, "HOST.ap.internal."));
    assert!(!url_rules(&rules, "sub.host.ap.internal"));
    assert!(!url_rules(&rules, "ap.internal"));
    assert!(!url_rules(&network("NETWORK +host.ap.internal\n"), NAME));
}

fn connection<'a>(name: &'a str, address: &str) -> Connection<'a> {
    Connection {
        name: Some(name),
        address: address.parse::<IpAddr>().unwrap(),
        port: 443,
        request: None,
    }
}

#[test]
fn a_connection_to_a_host_with_url_rules_opens_when_some_request_could_be_allowed() {
    let opens = |lines: &str, name: &str, address: &str| {
        could_request(&network(lines), &[], &connection(name, address))
    };
    let only = "NETWORK -\nNETWORK +https://api.example.com/v1\n";
    assert!(opens(only, "api.example.com", "192.0.2.7"));
    // An IP rule denies the address, so every request would be denied.
    let fenced = format!("{only}NETWORK -192.0.2.0/24\n");
    assert!(!opens(&fenced, "api.example.com", "192.0.2.7"));
    // Every path is denied.
    assert!(!opens(
        "NETWORK -\nNETWORK -https://api.example.com/v1\n",
        "api.example.com",
        "192.0.2.7"
    ));
    // Under an open default, paths a rule doesn't deny are allowed.
    assert!(opens(
        "NETWORK +\nNETWORK -https://api.example.com/v1\n",
        "api.example.com",
        "192.0.2.7"
    ));
    // Your host's loopback is reached only by its name.
    let yours = "NETWORK -\nNETWORK +https://host.ap.internal/allowed\n";
    assert!(opens(yours, NAME, "127.0.0.1"));
    assert!(!opens(
        "NETWORK +\nNETWORK -https://api.example.com/v1\n",
        "api.example.com",
        "127.0.0.1"
    ));
}

#[test]
fn a_request_on_a_host_with_url_rules_is_decided_by_its_path() {
    let network = network("NETWORK -\nNETWORK +https://host.ap.internal/allowed\n");
    let decided = |scheme, target| {
        let connection = Connection {
            request: Some(Request { scheme, target }),
            ..connection(NAME, "127.0.0.1")
        };
        proxied(&network, &[], &connection).allows()
    };
    assert!(decided(Scheme::Https, "/allowed/x?y"));
    assert!(!decided(Scheme::Https, "/denied"));
    assert!(!decided(Scheme::Https, "/allowed/../denied"));
    assert!(!decided(Scheme::Http, "/allowed/x"));
}
