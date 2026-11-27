//! A git server on loopback, over HTTPS, for the tests that fetch a remote
//! source. The host's `git http-backend` serves git's smart HTTP protocol for
//! the repositories under a directory, so what `ap` fetches is what a real
//! server sends. It counts requests, so a check can tell that a build made
//! none.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rustls::{ServerConnection, StreamOwned};

/// A server whose thread ends with the test process.
pub struct GitServer {
    pub port: u16,
    requests: Arc<AtomicUsize>,
}

impl GitServer {
    /// Serves the repositories under `root`. A `private` server answers
    /// every request with a 401, as a host does for a private repository
    /// when a client sends no credentials.
    pub fn start(root: &Path, private: bool) -> GitServer {
        let config = super::tls_config();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let root = root.to_path_buf();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let config = config.clone();
                let root = root.clone();
                let counted = counted.clone();
                thread::spawn(move || {
                    let connection = ServerConnection::new(config).unwrap();
                    let mut tls = StreamOwned::new(connection, stream);
                    let _ = serve(&mut tls, &root, private, &counted);
                });
            }
        });
        GitServer { port, requests }
    }

    /// The URL of the repository at `name` under the root.
    pub fn url(&self, name: &str) -> String {
        format!("https://localhost:{}/{name}", self.port)
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Writes the certificate the server presents into `dir`, and returns the
/// file, for `GIT_SSL_CAINFO`.
pub fn certificate(dir: &Path) -> PathBuf {
    let file = dir.join("git-server.pem");
    std::fs::write(&file, super::CERT).unwrap();
    file
}

/// Answers each request on one connection, until the client closes it.
fn serve(
    stream: &mut (impl Read + Write),
    root: &Path,
    private: bool,
    requests: &AtomicUsize,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end().to_string();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let header = |name: &str| {
            headers
                .iter()
                .find(|(found, _)| found == name)
                .map(|(_, value)| value.clone())
        };
        let mut body = Vec::new();
        if header("transfer-encoding").is_some_and(|value| value.contains("chunked")) {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size)?;
                let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
                let mut chunk = vec![0; size + 2];
                reader.read_exact(&mut chunk)?;
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        } else if let Some(length) = header("content-length") {
            let mut chunk = vec![0; length.parse().unwrap_or(0)];
            reader.read_exact(&mut chunk)?;
            body = chunk;
        }
        requests.fetch_add(1, Ordering::SeqCst);

        let reply = if private {
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
              Content-Length: 0\r\n\r\n"
                .to_vec()
        } else {
            backend(root, &method, &target, &header, &body)?
        };
        let stream = reader.get_mut();
        stream.write_all(&reply)?;
        stream.flush()?;
    }
}

/// What `git http-backend` answers, as an HTTP/1.1 response.
fn backend(
    root: &Path,
    method: &str,
    target: &str,
    header: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
) -> io::Result<Vec<u8>> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", root.parent().unwrap_or(root))
        .current_dir(root)
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, variable) in [
        ("content-type", "CONTENT_TYPE"),
        ("content-encoding", "HTTP_CONTENT_ENCODING"),
        ("git-protocol", "GIT_PROTOCOL"),
    ] {
        if let Some(value) = header(name) {
            command.env(variable, value);
        }
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let body = body.to_vec();
    let writer = thread::spawn(move || stdin.write_all(&body));
    let output = child.wait_with_output()?;
    let _ = writer.join();
    let out = output.stdout;
    let split = out
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| (at, at + 4))
        .or_else(|| {
            out.windows(2)
                .position(|window| window == b"\n\n")
                .map(|at| (at, at + 2))
        })
        .unwrap_or((out.len(), out.len()));
    let head = String::from_utf8_lossy(&out[..split.0]);
    let content = &out[split.1..];
    let mut status = "200 OK".to_string();
    let mut reply = String::new();
    for line in head.lines() {
        match line.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("status") => {
                status = value.trim().to_string();
            }
            Some(_) => {
                reply.push_str(line.trim_end());
                reply.push_str("\r\n");
            }
            None => {}
        }
    }
    let mut response = format!(
        "HTTP/1.1 {status}\r\n{reply}Content-Length: {}\r\n\r\n",
        content.len()
    )
    .into_bytes();
    response.extend_from_slice(content);
    Ok(response)
}
