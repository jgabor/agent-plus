//! What the tests that build an agent share. Each test crate uses only
//! some of it.

#![allow(dead_code)]

use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ap::fetch::Client;
use graviola::hashing::sha2::Sha256Context;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

/// Puts a stand-in for the pinned `OS minimal` layer in the store at `root`,
/// so that a build finds it and downloads nothing. It holds only the frame
/// that records a layer's zstd level, so a build that reads the layer, as a
/// boot does, fails. T1 builds with the real layer.
pub fn seed_os(root: &Path) {
    let layers = root.join("zstd-layers");
    fs::create_dir_all(&layers).unwrap();
    // A zstd skippable frame of one byte, which holds level 3.
    let level_frame = [0x5a, 0x2a, 0x4d, 0x18, 1, 0, 0, 0, 3];
    fs::write(layers.join(ap::fetch::os().sha256), level_frame).unwrap();
}

/// The file the tests lock as their harness. A build only stores it, so any
/// bytes will do.
pub const HARNESS: &[u8] = b"#!/bin/sh\necho a harness\n";

/// The SHA-256 digest of `data`, in lowercase hex.
pub fn digest(data: &[u8]) -> String {
    let mut hash = Sha256Context::new();
    hash.update(data);
    hash.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the download cache under `home` keeps the file with this digest.
pub fn cached(home: &Path, sha256: &str) -> PathBuf {
    home.join(".cache/ap").join(sha256)
}

/// The file Codex's release ships [`HARNESS`] in: one zstd frame.
pub fn codex_file() -> Vec<u8> {
    store::compress(HARNESS, 3)
}

/// Puts the file for [`HARNESS`] in the download cache under `home`, and
/// writes the lockfile next to the Agentfile in `dir` that pins it, for the
/// harness the Agentfile names and the version it names, or `0.0.0` for a
/// channel. Claude Code's file is [`HARNESS`], and Codex's is
/// [`codex_file`].
pub fn pin_harness(home: &Path, dir: &Path) {
    let source = fs::read_to_string(dir.join("Agentfile")).unwrap_or_default();
    let agent = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("AGENT "))
        .unwrap_or("claude")
        .trim();
    let (harness, version) = agent.split_once(':').unwrap_or((agent, "latest"));
    let file = match harness {
        "codex" => codex_file(),
        _ => HARNESS.to_vec(),
    };
    let sha256 = digest(&file);
    let path = cached(home, &sha256);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, file).unwrap();
    let version = match version.starts_with(|c: char| c.is_ascii_digit()) {
        true => version,
        false => "0.0.0",
    };
    fs::write(
        dir.join("Agentfile.lock"),
        format!("HARNESS {harness} {version} {sha256}\n"),
    )
    .unwrap();
}

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

pub fn ok(body: &[u8]) -> Vec<u8> {
    let mut reply =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

pub fn redirect(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

/// An HTTPS server that answers each path with fixed bytes, and counts
/// requests. Its thread ends with the test process.
pub struct Server {
    pub port: u16,
    pub requests: Arc<AtomicUsize>,
    pub paths: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// `routes` receives the server's own origin, for writing redirects.
    pub fn start(routes: impl FnOnce(&str) -> Vec<(&'static str, Vec<u8>)>) -> Self {
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

    pub fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.port)
    }

    pub fn requests(&self) -> usize {
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

pub fn trusting_client() -> Client {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap())
        .unwrap();
    Client::with_roots(roots)
}
