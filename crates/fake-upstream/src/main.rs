//! A stand-in for a server on your host, which journey step 10 reaches from
//! the guest. It runs on the host, next to `ap`, until it is killed.
//!
//! `fake-upstream <directory>` listens on three ports, writes its
//! certificate authority to `<directory>/ca.pem`, and then prints one line
//! that says where it listens:
//!
//! ```text
//! plain <port> tls <port> address <address> <port>
//! ```
//!
//! - `plain` is plain TCP on your host's loopback.
//! - `tls` is HTTPS on your host's loopback, with a certificate for
//!   `host.ap.internal` that the authority in `ca.pem` signed. A program
//!   whose `SSL_CERT_FILE` names that file trusts it.
//! - `address` is plain TCP on your host's IPv4 address on its route out,
//!   which an IP rule can allow, as it can't allow a loopback address.
//!
//! Plain TCP answers a connection's first line with `upstream:<line>`.
//! HTTPS answers every request with a 404. Each line it gets, and each
//! request's target, is noted in `<directory>/seen` as one line, `plain
//! <line>`, `address <line>`, or `tls <target>`, so a check can tell what
//! reached it.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use ap::network::Authority;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The name the guest reaches your host by.
const HOST: &str = "host.ap.internal";
/// How long a connection may stay quiet before the upstream drops it.
const QUIET: Duration = Duration::from_secs(60);

/// Where each line that reached the upstream is noted.
type Seen = Arc<Mutex<File>>;

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    let (Some(directory), None) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: fake-upstream <directory>");
        return ExitCode::from(2);
    };
    match serve(Path::new(&directory)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake-upstream: {error}");
            ExitCode::FAILURE
        }
    }
}

fn serve(directory: &Path) -> io::Result<()> {
    let seen: Seen = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("seen"))?,
    ));
    let now = SystemTime::now();
    let authority = Authority::generate(now)?;
    let leaf = authority.issue(HOST, now)?;
    let provider = Arc::new(rustls_graviola::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![leaf.certificate], leaf.key)
        .map_err(io::Error::other)?;
    fs::write(directory.join("ca.pem"), authority.certificate_pem())?;

    let plain = TcpListener::bind("127.0.0.1:0")?;
    let tls = TcpListener::bind("127.0.0.1:0")?;
    let address = TcpListener::bind((outward_address()?, 0))?;
    let at = |listener: &TcpListener| listener.local_addr().map(|a| a.port());
    let outward = address.local_addr()?;
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "plain {} tls {} address {} {}",
        at(&plain)?,
        at(&tls)?,
        outward.ip(),
        outward.port()
    )?;
    stdout.flush()?;

    let config = Arc::new(config);
    accept(tls, &seen, move |stream, seen| https(stream, &config, seen));
    accept(address, &seen, |stream, seen| echo(stream, "address", seen));
    let seen = Arc::clone(&seen);
    for stream in plain.incoming() {
        let seen = Arc::clone(&seen);
        thread::spawn(move || echo(stream?, "plain", &seen));
    }
    Ok(())
}

/// Serves each connection to `listener` with `serve`, on a thread of its
/// own.
fn accept<F>(listener: TcpListener, seen: &Seen, serve: F)
where
    F: Fn(TcpStream, &Seen) -> io::Result<()> + Send + Sync + 'static,
{
    let seen = Arc::clone(seen);
    let serve = Arc::new(serve);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (seen, serve) = (Arc::clone(&seen), Arc::clone(&serve));
            thread::spawn(move || serve(stream, &seen));
        }
    });
}

/// Notes `line` as one line of `seen`.
fn note(seen: &Seen, line: &str) {
    let mut file = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = file.write_all(format!("{line}\n").as_bytes());
}

/// Answers the connection's first line, then reads it to its end.
fn echo(stream: TcpStream, label: &str, seen: &Seen) -> io::Result<()> {
    stream.set_read_timeout(Some(QUIET))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim_end();
    note(seen, &format!("{label} {line}"));
    (&stream).write_all(format!("upstream:{line}\n").as_bytes())?;
    io::copy(&mut reader, &mut io::sink())?;
    Ok(())
}

/// Answers each HTTPS request on the connection with a 404.
fn https(stream: TcpStream, config: &Arc<ServerConfig>, seen: &Seen) -> io::Result<()> {
    stream.set_read_timeout(Some(QUIET))?;
    let server = ServerConnection::new(Arc::clone(config)).map_err(io::Error::other)?;
    let mut stream = BufReader::new(StreamOwned::new(server, stream));
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let target = line.split(' ').nth(1).unwrap_or_default();
        note(seen, &format!("tls {target}"));
        let mut length = 0;
        let mut header = String::new();
        while stream.read_line(&mut header)? > 0 && header != "\r\n" {
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().map_err(io::Error::other)?;
            }
            header.clear();
        }
        io::copy(&mut (&mut stream).take(length), &mut io::sink())?;
        let answer = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        stream.get_mut().write_all(answer)?;
    }
}

/// This host's IPv4 address on its route out, which isn't a loopback
/// address. Connecting a UDP socket sends nothing.
fn outward_address() -> io::Result<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("192.0.2.1:9")?;
    match socket.local_addr()?.ip() {
        address @ IpAddr::V4(_) => Ok(address),
        address => Err(io::Error::other(format!(
            "the route out has no IPv4 address, only {address}"
        ))),
    }
}
