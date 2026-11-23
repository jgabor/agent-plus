//! The name check, by **Name checks** under NETWORK in SPEC.md. The TLS
//! client hellos come from rustls' client, so the hello reader is checked
//! against a writer its author didn't write.

use std::sync::Arc;

use agentfile::{Network, parse};
use net::{Connection, NameCheck, Refused, proxied};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

const NAME: &str = "host.ap.internal";

/// The client hello that rustls' client sends to `server`.
fn hello(server: &str) -> Vec<u8> {
    let provider = Arc::new(rustls_graviola::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    let server = ServerName::try_from(server.to_owned()).unwrap();
    let mut client = ClientConnection::new(Arc::new(config), server).unwrap();
    let mut bytes = Vec::new();
    while client.wants_write() {
        client.write_tls(&mut bytes).unwrap();
    }
    bytes
}

/// What passes of `input`, fed in pieces of `piece` bytes, and whether the
/// check refused it.
fn fed(input: &[u8], piece: usize) -> (Vec<u8>, Result<(), Refused>, bool) {
    let mut check = NameCheck::new(NAME);
    let mut out = Vec::new();
    for chunk in input.chunks(piece) {
        if let Err(refused) = check.feed(chunk, &mut out) {
            return (out, Err(refused), check.holding());
        }
    }
    let holding = check.holding();
    (out, Ok(()), holding)
}

/// Whether all of `input` passes, fed whole and a byte at a time.
#[track_caller]
fn passes(input: &[u8]) {
    for piece in [input.len().max(1), 1, 7] {
        let (out, result, holding) = fed(input, piece);
        assert_eq!(result, Ok(()), "in pieces of {piece}");
        assert!(!holding, "in pieces of {piece}");
        assert_eq!(out, input, "in pieces of {piece}");
    }
}

/// Whether `input` is refused after only `passed` of it passed, fed whole
/// and a byte at a time.
#[track_caller]
fn refused_after(input: &[u8], passed: &[u8]) {
    for piece in [input.len().max(1), 1, 7] {
        let (out, result, _) = fed(input, piece);
        assert_eq!(result, Err(Refused), "in pieces of {piece}");
        assert!(passed.starts_with(&out), "in pieces of {piece}: {out:?}");
        let whole = piece >= input.len();
        if whole {
            assert_eq!(out, passed);
        }
    }
}

#[track_caller]
fn refused(input: &[u8]) {
    refused_after(input, b"");
}

// spec(NETWORK): **Name checks.** When a host or URL rule decides a connection, the attributed name must agree with whatever the protocol shows without decrypting: the TLS server name (SNI), or the `Host` header of plain HTTP.
#[test]
fn a_hello_for_the_name_passes_and_one_for_another_is_refused() {
    passes(&hello(NAME));
    passes(&hello("HOST.ap.internal"));
    passes(&hello("host.ap.internal."));
    refused(&hello("example.com"));
    refused(&hello("ap.internal"));
    refused(&hello("evil.host.ap.internal"));
}

#[test]
fn nothing_passes_after_a_refusal() {
    let mut check = NameCheck::new(NAME);
    let mut out = Vec::new();
    assert_eq!(check.feed(&hello("example.com"), &mut out), Err(Refused));
    assert_eq!(check.feed(b"more", &mut out), Err(Refused));
    assert!(out.is_empty());
}

#[test]
fn what_follows_an_agreeing_hello_passes_unread() {
    let mut input = hello(NAME);
    input.extend_from_slice(b"\x17\x03\x03\x00\x02hi GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    passes(&input);
}

#[test]
fn a_hello_with_no_server_name_passes() {
    passes(&hello("127.0.0.1"));
}

#[test]
fn a_hello_cut_off_passes_nothing_and_holds() {
    let whole = hello(NAME);
    for end in [1, 5, whole.len() / 2, whole.len() - 1] {
        let (out, result, holding) = fed(&whole[..end], 3);
        assert_eq!(result, Ok(()));
        assert!(out.is_empty());
        assert!(holding, "cut at {end}");
    }
}

#[test]
fn a_malformed_hello_is_refused() {
    refused(b"\x16\x03\x01\x00\x05hello");
    // A record of another type, where a handshake should be.
    let mut wrong = hello(NAME);
    wrong[5] = 0x02;
    refused(&wrong);
}

#[test]
fn a_hello_that_never_ends_is_refused() {
    let mut input = b"\x16\x03\x01\x40\x00\x01\xff\xff\xff".to_vec();
    input.resize(70_000, 0);
    let (out, result, _) = fed(&input, 4096);
    assert_eq!(result, Err(Refused));
    assert!(out.is_empty());
}

#[test]
fn requests_whose_host_agrees_pass() {
    passes(b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    passes(b"GET /a?b HTTP/1.1\r\nhost: HOST.ap.internal.:8080\r\nAccept: */*\r\n\r\n");
    passes(b"OPTIONS * HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    passes(b"GET / HTTP/1.0\r\nHost:host.ap.internal  \r\n\r\n");
    // An empty line before a request.
    passes(b"\r\nGET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
}

#[test]
fn a_request_whose_host_disagrees_is_refused() {
    refused(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused(b"GET / HTTP/1.1\r\nHost: host.ap.internal.example.com\r\n\r\n");
    refused(b"\r\n\r\nGET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused(b"GET / HTTP/1.1\r\nHost: [::1]\r\n\r\n");
    refused(b"GET / HTTP/1.1\r\nHost: host.ap.internal:http\r\n\r\n");
}

#[test]
fn every_request_on_a_connection_is_checked() {
    let first = b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";
    let mut input = first.to_vec();
    input.extend_from_slice(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused_after(&input, first);

    // A body passes as a body, whatever it looks like.
    let body = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    let mut first = format!(
        "POST / HTTP/1.1\r\nHost: host.ap.internal\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    first.extend_from_slice(body);
    passes(&first);
    let mut input = first.clone();
    input.extend_from_slice(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused_after(&input, &first);
}

#[test]
fn a_chunked_body_is_read_to_its_end() {
    let first = concat!(
        "POST / HTTP/1.1\r\nHost: host.ap.internal\r\nTransfer-Encoding: chunked\r\n\r\n",
        "5;ext=1\r\nHost:\r\n",
        "19\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n",
        "0\r\nTrailer: x\r\n\r\n",
    );
    passes(first.as_bytes());
    let input = format!("{first}GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused_after(input.as_bytes(), first.as_bytes());
    let next = format!("{first}GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    passes(next.as_bytes());
}

// spec(NETWORK): The proxy refuses plain HTTP that it can't check request by request: a request with no `Host` header or more than one, a `CONNECT` request or one whose target names a host, an `Upgrade` request, and a body whose length a server could read another way.
#[test]
fn a_request_the_server_could_read_another_way_is_refused() {
    let head = "POST / HTTP/1.1\r\nHost: host.ap.internal\r\n";
    for headers in [
        "Content-Length: 1\r\nContent-Length: 1\r\n",
        "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: gzip, chunked\r\n",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: +1\r\n",
        "Content-Length: \r\n",
        "Content-Length: 99999999999999999999999\r\n",
        "X-Folded: a\r\n b\r\n",
        "Host: host.ap.internal\r\n",
    ] {
        refused(format!("{head}{headers}\r\nx").as_bytes());
    }
    // A server may end a trailer line only at CRLF, as hyper does.
    let bad_chunks = [
        "zz\r\n",
        "1\r\nab\r\n",
        "1\r\na\n0\r\n\r\n",
        "0\r\n\n",
        "0\r\nT: 1\n\r\n",
        "0\r\nT: 1\r\n\n",
        "0;\n\r\n\r\n",
        "\r\n\r\n",
        ";x\r\n\r\n",
        " 0\r\n\r\n",
    ];
    for chunks in bad_chunks {
        let input = format!("{head}Transfer-Encoding: chunked\r\n\r\n{chunks}");
        let (_, result, _) = fed(input.as_bytes(), input.len());
        assert_eq!(result, Err(Refused), "{chunks:?}");
    }
}

#[test]
fn a_request_with_no_host_of_its_own_is_refused() {
    refused(b"GET / HTTP/1.1\r\n\r\n");
    refused(b"GET / HTTP/1.0\r\n\r\n");
    refused(b"GET http://example.com/ HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    refused(b"CONNECT example.com:443 HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    refused(b"CONNECT / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    // HTTP/2 with prior knowledge, whose names are in frames.
    refused(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    refused(b"GET / HTTP/1.1 \r\nHost: host.ap.internal\r\n\r\n");
}

#[test]
fn any_method_is_checked() {
    passes(b"M-SEARCH * HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    passes(b"x.Y_1 / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n");
    refused(b"M-SEARCH * HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused(b"get / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused(b"PROPFIND! / HTTP/1.1\r\nHost: example.com\r\n\r\n");
}

#[test]
fn a_request_line_a_lenient_server_reads_is_refused() {
    for line in [
        "GET\t/ HTTP/1.1",
        "GET /\tHTTP/1.1",
        "GET  / HTTP/1.1",
        "GET / http/1.1",
        " GET / HTTP/1.1",
        "\tGET / HTTP/1.1",
        "GET /\x0bHTTP/1.1",
        "GET\x0b/ HTTP/1.1",
        "\x0bGET / HTTP/1.1",
    ] {
        for host in ["example.com", NAME] {
            refused(format!("{line}\r\nHost: {host}\r\n\r\n").as_bytes());
        }
    }
    // After an agreeing request too.
    let first = b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";
    let mut input = first.to_vec();
    input.extend_from_slice(b"GET\t/ HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused_after(&input, first);
}

#[test]
fn an_upgrade_is_refused() {
    let first = b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";
    let mut input = first.to_vec();
    input.extend_from_slice(b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\nUpgrade: x\r\n\r\n");
    input.extend_from_slice(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
    refused_after(&input, first);
}

#[test]
fn a_request_cut_off_holds() {
    for input in [
        &b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n"[..],
        b"GET / HTT",
        b"POST / HTTP/1.1\r\nHost: host.ap.internal\r\nContent-Length: 4\r\n\r\nab",
    ] {
        let (_, result, holding) = fed(input, input.len());
        assert_eq!(result, Ok(()));
        assert!(holding, "{input:?}");
    }
}

#[test]
fn traffic_that_is_neither_tls_nor_http_passes() {
    passes(b"SSH-2.0-OpenSSH_9.9\r\n\x00\x00\x01\x00");
    passes(b"EHLO example.com\r\nMAIL FROM:<a@example.com>\r\n");
    passes(b"ping\n");
    passes(b"\x00\x01\x02");
}

#[test]
fn a_first_line_that_never_ends_is_refused() {
    let mut input = b"GET /".to_vec();
    input.resize(70_000, b'a');
    let (out, result, _) = fed(&input, 4096);
    assert_eq!(result, Err(Refused));
    assert!(out.is_empty());
}

/// Whether the proxy would check a connection to `name` at `address` on
/// port 443 under `lines`, with `api.anthropic.com` as a harness host.
fn checked(lines: &str, name: Option<&str>, address: &str) -> bool {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    let network = Network::read(&instructions).unwrap();
    let connection = Connection {
        name,
        address: address.parse().unwrap(),
        port: 443,
        request: None,
    };
    let decided = proxied(&network, &["api.anthropic.com"], &connection);
    assert!(decided.allows());
    decided.by_name()
}

// spec(NETWORK): **What the name check reads.** A harness host that no rule decides is checked too.
// spec(NETWORK): A connection that no name rule decides skips this check, so under `NETWORK +`, clients that resolve names on their own (DNS over HTTPS, hard-coded addresses) keep working.
#[test]
fn only_a_connection_a_name_decides_is_checked() {
    assert!(checked(
        "NETWORK -\nNETWORK +kagi.com\n",
        Some("kagi.com"),
        "1.2.3.4"
    ));
    assert!(checked("NETWORK -\n", Some("api.anthropic.com"), "1.2.3.4"));
    assert!(checked(
        "NETWORK +\nNETWORK +kagi.com\n",
        Some("kagi.com"),
        "1.2.3.4"
    ));
    assert!(!checked("NETWORK +\n", Some("kagi.com"), "1.2.3.4"));
    assert!(!checked("NETWORK +\n", None, "1.2.3.4"));
    assert!(!checked(
        "NETWORK -\nNETWORK +1.2.3.0/24\n",
        Some("kagi.com"),
        "1.2.3.4"
    ));
    assert!(!checked(
        "NETWORK -\nNETWORK +1.2.3.0/24\n",
        None,
        "1.2.3.4"
    ));
}
