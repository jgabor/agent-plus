//! The kernel and layer downloads, against a real TLS server on localhost.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use ap::fetch::{self, Client, Pin};
use ap::store::{self, Store};
use common::{Server, ok, redirect, trusting_client};

const BODY: &[u8] = b"a kernel, more or less";
// `printf 'a kernel, more or less' | sha256sum`
const BODY_SHA256: &str = "4a72eb507fca8c6741b689e8f002848b132c919f273b76244e44d715e23cd44a";
const OTHER_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

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

// spec(Build): A kernel, OS, or harness layer is downloaded once, however many agents use it.
#[test]
fn a_layer_is_downloaded_once_and_kept_by_its_digest() {
    let server = Server::start(|_| vec![("/minimal.erofs", ok(BODY))]);
    let root = empty_root("layer-once");
    let layers = ::store::Layers::new(&root);
    let url = server.url("/minimal.erofs");
    let pin = Pin {
        url: &url,
        sha256: BODY_SHA256,
    };

    assert!(fetch::layer(&layers, &trusting_client(), &pin, 3).unwrap());
    assert_eq!(server.requests(), 1);
    assert_eq!(layers.get(BODY_SHA256).unwrap(), BODY);
    assert_eq!(layers.level(BODY_SHA256).unwrap(), Some(3));
    assert_eq!(
        files(&root),
        [root.join("zstd-layers").join(BODY_SHA256)],
        "no temporary file may remain"
    );

    // Another agent, at any level, finds the layer in the store.
    for level in [3, 19] {
        assert!(!fetch::layer(&layers, &trusting_client(), &pin, level).unwrap());
    }
    assert_eq!(
        server.requests(),
        1,
        "a layer in the store must not be downloaded again"
    );
    assert_eq!(layers.level(BODY_SHA256).unwrap(), Some(3));
}

#[test]
fn a_layer_whose_digest_is_not_the_pin_is_refused_and_not_stored() {
    let server = Server::start(|_| vec![("/minimal.erofs", ok(BODY))]);
    let root = empty_root("layer-wrong-digest");
    let url = server.url("/minimal.erofs");
    let pin = Pin {
        url: &url,
        sha256: OTHER_SHA256,
    };
    let layers = ::store::Layers::new(&root);
    let error = fetch::layer(&layers, &trusting_client(), &pin, 3).unwrap_err();
    assert!(error.to_string().contains("digest"), "{error}");
    assert_eq!(files(&root), Vec::<PathBuf>::new());
}

#[test]
fn the_pin_names_the_os_layer_in_pin_toml() {
    let toml = include_str!("../../../guest/os/pin.toml");
    let pin = fetch::os();
    assert!(toml.contains(&format!("url = \"{}\"", pin.url)));
    assert!(toml.contains(&format!("sha256 = \"{}\"", pin.sha256)));
    assert!(pin.url.starts_with("https://"));
    assert_eq!(pin.sha256.len(), 64);
}
