//! The kernel download, against a real TLS server on localhost.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ap::fetch::{self, Client, Pin};
use ap::store::{self, Store};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

// A self-signed certificate for `localhost`, valid until 2126. It's trusted
// only by the clients these tests build, so its key protects nothing.
const CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBkjCCATigAwIBAgIUKo7VEB3v57sU3eXd/W6G4jkwzgcwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTEwOTE0NDYwNloYDzIxMjYxMDE2
MTQ0NjA2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAATLNXdJ+wCT3FFz+TbeyJz0PfkxNEKCiqGEuHaCoco3ZuRptVGExCa4
JrgarkFAnFDBVnliryVLZIZmJyvgP+kKo2YwZDAdBgNVHQ4EFgQUJGEA0tybv+W0
+Ax4bN+yZx27vs4wHwYDVR0jBBgwFoAUJGEA0tybv+W0+Ax4bN+yZx27vs4wFAYD
VR0RBA0wC4IJbG9jYWxob3N0MAwGA1UdEwEB/wQCMAAwCgYIKoZIzj0EAwIDSAAw
RQIgChZzpHJEJfBlwLJjLMDLVaM6+T5/tyvUKuTEchZ8veICIQCpQZUh7CGuu45X
UUYQOfUpWKiQ8qAUv2Yto+dEVabYcA==
-----END CERTIFICATE-----
";
const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgB9RUgEImB2I1Guvk
eXVzHPDywIrYfllulLAfhVrw5tmhRANCAATLNXdJ+wCT3FFz+TbeyJz0PfkxNEKC
iqGEuHaCoco3ZuRptVGExCa4JrgarkFAnFDBVnliryVLZIZmJyvgP+kK
-----END PRIVATE KEY-----
";

const BODY: &[u8] = b"a kernel, more or less";
// `printf 'a kernel, more or less' | sha256sum`
const BODY_SHA256: &str = "4a72eb507fca8c6741b689e8f002848b132c919f273b76244e44d715e23cd44a";
const OTHER_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn ok(body: &[u8]) -> Vec<u8> {
    let mut reply =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

fn redirect(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

/// An HTTPS server that answers each path with fixed bytes, and counts
/// requests. Its thread ends with the test process.
struct Server {
    port: u16,
    requests: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// `routes` receives the server's own origin, for writing redirects.
    fn start(routes: impl FnOnce(&str) -> Vec<(&'static str, Vec<u8>)>) -> Self {
        let certs = vec![CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap()];
        let key = PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap();
        let config = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(rustls_graviola::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = routes(&format!("https://localhost:{port}"));
        let requests = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let server = Server {
            port,
            requests: requests.clone(),
            paths: paths.clone(),
        };
        thread::spawn(move || {
            for stream in listener.incoming() {
                let connection = ServerConnection::new(config.clone()).unwrap();
                let mut tls = StreamOwned::new(connection, stream.unwrap());
                let Ok(head) = read_head(&mut tls) else {
                    continue;
                };
                let path = head.split(' ').nth(1).unwrap().to_owned();
                requests.fetch_add(1, Ordering::SeqCst);
                paths.lock().unwrap().push(path.clone());
                let reply = routes.iter().find(|(route, _)| *route == path);
                let reply = reply.map_or(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".as_slice(),
                    |(_, r)| r,
                );
                let _ = tls.write_all(reply);
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        server
    }

    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.port)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn read_head(stream: &mut impl Read) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn trusting_client() -> Client {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap())
        .unwrap();
    Client::with_roots(roots)
}

/// A new, empty store root for one test.
fn empty_root(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("store")
        .join(test);
    let _ = fs::remove_dir_all(&root);
    root
}

/// Every file under `dir`, at any depth.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

#[test]
fn the_kernel_is_fetched_once_and_a_second_call_makes_no_request() {
    let server = Server::start(|_| vec![("/vmlinux", ok(BODY))]);
    let root = empty_root("once");
    let store = Store::new(&root);
    let url = server.url("/vmlinux");
    let pin = Pin {
        url: &url,
        sha256: BODY_SHA256,
    };

    let first = fetch::fetch(&store, &trusting_client(), &pin).unwrap();
    assert_eq!(fs::read(&first).unwrap(), BODY);
    assert_eq!(server.requests(), 1);
    assert!(first.starts_with(&root));
    assert_eq!(first.file_name().unwrap(), BODY_SHA256);
    assert_eq!(
        files(&root),
        std::slice::from_ref(&first),
        "no temporary file may remain"
    );

    let second = fetch::fetch(&store, &trusting_client(), &pin).unwrap();
    assert_eq!(second, first);
    assert_eq!(
        server.requests(),
        1,
        "the second call must not reach the network"
    );
}

#[test]
fn one_redirect_is_followed_with_its_query() {
    let server = Server::start(|origin| {
        vec![
            ("/release", redirect(&format!("{origin}/asset?sig=abc"))),
            ("/asset?sig=abc", ok(BODY)),
        ]
    });
    let root = empty_root("redirect");
    let url = server.url("/release");
    let pin = Pin {
        url: &url,
        sha256: BODY_SHA256,
    };

    let path = fetch::fetch(&Store::new(&root), &trusting_client(), &pin).unwrap();
    assert_eq!(fs::read(path).unwrap(), BODY);
    assert_eq!(
        *server.paths.lock().unwrap(),
        ["/release", "/asset?sig=abc"]
    );
}

/// Asserts that fetching `path` fails, leaves the store empty, and says `why`.
fn fails_and_stores_nothing(server: &Server, test: &str, path: &str, sha256: &str, why: &str) {
    let root = empty_root(test);
    let url = server.url(path);
    let pin = Pin { url: &url, sha256 };
    let error = fetch::fetch(&Store::new(&root), &trusting_client(), &pin).unwrap_err();
    assert!(error.to_string().contains(why), "{error}");
    assert_eq!(files(&root), Vec::<PathBuf>::new());
}

#[test]
fn a_second_redirect_is_refused() {
    let server = Server::start(|origin| {
        vec![
            ("/one", redirect(&format!("{origin}/two"))),
            ("/two", redirect(&format!("{origin}/three"))),
            ("/three", ok(BODY)),
        ]
    });
    fails_and_stores_nothing(&server, "two-redirects", "/one", BODY_SHA256, "redirect");
    assert_eq!(server.requests(), 2);
}

#[test]
fn a_redirect_to_plain_http_is_refused() {
    let server = Server::start(|_| vec![("/one", redirect("http://localhost:1/two"))]);
    fails_and_stores_nothing(&server, "downgrade", "/one", BODY_SHA256, "https");
}

#[test]
fn a_wrong_digest_leaves_nothing_in_the_store() {
    let server = Server::start(|_| vec![("/vmlinux", ok(BODY))]);
    fails_and_stores_nothing(&server, "wrong-digest", "/vmlinux", OTHER_SHA256, "digest");
}

#[test]
fn a_partial_download_leaves_nothing_in_the_store() {
    let mut short = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        BODY.len() + 1
    )
    .into_bytes();
    short.extend_from_slice(BODY);
    let server = Server::start(|_| vec![("/vmlinux", short)]);
    fails_and_stores_nothing(&server, "partial", "/vmlinux", BODY_SHA256, "ended early");
}

#[test]
fn an_error_status_is_refused() {
    let server = Server::start(|_| Vec::new());
    fails_and_stores_nothing(&server, "status", "/missing", BODY_SHA256, "404");
}

#[test]
fn a_response_with_no_length_is_refused() {
    let server = Server::start(|_| vec![("/vmlinux", b"HTTP/1.1 200 OK\r\n\r\nbody".to_vec())]);
    fails_and_stores_nothing(
        &server,
        "no-length",
        "/vmlinux",
        BODY_SHA256,
        "Content-Length",
    );
}

#[test]
fn an_oversized_response_is_refused_before_its_body() {
    let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n".to_vec();
    let server = Server::start(|_| vec![("/vmlinux", reply)]);
    fails_and_stores_nothing(&server, "oversized", "/vmlinux", BODY_SHA256, "too large");
}

#[test]
fn a_server_outside_the_web_roots_is_refused() {
    let server = Server::start(|_| vec![("/vmlinux", ok(BODY))]);
    let root = empty_root("untrusted");
    let url = server.url("/vmlinux");
    let pin = Pin {
        url: &url,
        sha256: BODY_SHA256,
    };
    assert!(fetch::fetch(&Store::new(&root), &Client::new(), &pin).is_err());
    assert_eq!(server.requests(), 0);
    assert_eq!(files(&root), Vec::<PathBuf>::new());
}

#[test]
fn a_plain_http_pin_is_refused() {
    let root = empty_root("http-pin");
    let pin = Pin {
        url: "http://localhost:1/vmlinux",
        sha256: BODY_SHA256,
    };
    let error = fetch::fetch(&Store::new(&root), &trusting_client(), &pin).unwrap_err();
    assert!(error.to_string().contains("https"), "{error}");
}

#[test]
fn a_malformed_digest_never_becomes_a_path() {
    let server = Server::start(|_| vec![("/vmlinux", ok(BODY))]);
    fails_and_stores_nothing(&server, "bad-digest", "/vmlinux", "../../escape", "digest");
    assert_eq!(server.requests(), 0);
}

#[test]
fn the_pin_names_the_kernel_in_pin_toml() {
    let toml = include_str!("../../../guest/kernel/pin.toml");
    let pin = fetch::kernel();
    assert!(toml.contains(&format!("url = \"{}\"", pin.url)));
    assert!(toml.contains(&format!("sha256 = \"{}\"", pin.sha256)));
    assert!(pin.url.starts_with("https://"));
    assert_eq!(pin.sha256.len(), 64);
}

#[test]
fn the_store_is_under_xdg_data_home_when_it_is_absolute() {
    let root = store::default_root(Some(OsStr::new("/data")), Some(OsStr::new("/home/u")));
    assert_eq!(root.unwrap(), Path::new("/data/ap"));
}

#[test]
fn the_store_falls_back_to_local_share() {
    let home = Some(OsStr::new("/home/u"));
    let expected = Path::new("/home/u/.local/share/ap");
    assert_eq!(store::default_root(None, home).unwrap(), expected);
    // The XDG spec says to treat empty and relative values as unset.
    assert_eq!(
        store::default_root(Some(OsStr::new("")), home).unwrap(),
        expected
    );
    assert_eq!(
        store::default_root(Some(OsStr::new("data")), home).unwrap(),
        expected
    );
}

#[test]
fn the_store_needs_an_absolute_home_without_xdg() {
    assert_eq!(store::default_root(None, None), None);
    assert_eq!(store::default_root(None, Some(OsStr::new(""))), None);
    assert_eq!(store::default_root(None, Some(OsStr::new("home"))), None);
}

#[test]
fn a_temporary_file_left_by_a_crash_is_neither_fatal_nor_deleted() {
    let root = empty_root("stale");
    let store = Store::new(&root);
    let layers = root.join("layers");
    fs::create_dir_all(&layers).unwrap();
    // Every name this process could pick next, as a crashed process with
    // the same id would have left them.
    let stale: Vec<PathBuf> = (0..1000)
        .map(|id| layers.join(format!(".{BODY_SHA256}.{}.{id}", std::process::id())))
        .collect();
    for path in &stale {
        fs::write(path, "left behind").unwrap();
    }

    let path = store.put(BODY_SHA256, BODY).unwrap();

    assert_eq!(fs::read(path).unwrap(), BODY);
    assert!(stale.iter().all(|path| path.exists()));
}
