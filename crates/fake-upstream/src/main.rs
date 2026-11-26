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
//! HTTPS answers every request with a 404, except a `POST` to a path that
//! ends in `/responses`, which it answers as a fake model on OpenAI's
//! Responses API: a call of Codex's code-mode `exec` tool while the request
//! holds no tool's output, and then a message. The tool runs [`COMMAND`], or
//! the text of `<directory>/command` when that file exists as the request
//! arrives. Each line it gets, and each request's target, is noted in
//! `<directory>/seen` as one line, `plain <line>`, `address <line>`, or
//! `tls <target>`, so a check can tell what reached it. Each HTTPS request, its head and body as they arrived, is
//! also kept whole in `<directory>/requests/<n>`, numbered from 1 in the
//! order each one finished arriving.

#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use ap::network::Authority;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The name the guest reaches your host by.
const HOST: &str = "host.ap.internal";
/// How long a connection may stay quiet before the upstream drops it.
const QUIET: Duration = Duration::from_secs(60);

/// The command the fake model's tool runs, unless `<directory>/command`
/// names another. It writes `tool-ran` in Codex's working directory.
const COMMAND: &str = "echo the tool ran > tool-ran";

/// Where each line that reached the upstream is noted.
type Seen = Arc<Mutex<File>>;

/// Where each HTTPS request is kept whole, how many came so far, and the
/// file that can name the command the fake model's tool runs.
struct Requests {
    directory: PathBuf,
    count: AtomicUsize,
    command: PathBuf,
}

impl Requests {
    fn keep(&self, request: &[u8]) -> io::Result<()> {
        let number = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        fs::write(self.directory.join(number.to_string()), request)
    }
}

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
    let requests = Arc::new(Requests {
        directory: directory.join("requests"),
        count: AtomicUsize::new(0),
        command: directory.join("command"),
    });
    fs::create_dir_all(&requests.directory)?;

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
    accept(tls, &seen, move |stream, seen| {
        https(stream, &config, seen, &requests)
    });
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

/// Answers each HTTPS request on the connection with a 404, and keeps it.
fn https(
    stream: TcpStream,
    config: &Arc<ServerConfig>,
    seen: &Seen,
    requests: &Requests,
) -> io::Result<()> {
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
        let model = line.starts_with("POST ") && target.ends_with("/responses");
        let mut request = line.into_bytes();
        let mut length = 0;
        let mut header = String::new();
        while stream.read_line(&mut header)? > 0 {
            request.extend_from_slice(header.as_bytes());
            if header == "\r\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().map_err(io::Error::other)?;
            }
            header.clear();
        }
        let start = request.len();
        (&mut stream).take(length).read_to_end(&mut request)?;
        requests.keep(&request)?;
        let answer = match model {
            true => {
                let command = match fs::read_to_string(&requests.command) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => COMMAND.to_owned(),
                    command => command?,
                };
                model_answer(&request[start..], &command)
            }
            false => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        };
        stream.get_mut().write_all(&answer)?;
    }
}

/// The fake model's answer to a request with this body, as a stream of
/// server-sent events: a call of Codex's code-mode `exec` tool that runs
/// `command`, until the body holds the output of a tool's call, and then a
/// message.
fn model_answer(body: &[u8], command: &str) -> Vec<u8> {
    let answered = body
        .windows(b"_call_output\"".len())
        .any(|window| window == b"_call_output\"");
    let item = match answered {
        false => format!(
            r#"{{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","namespace":"functions","name":"exec","status":"completed","input":{}}}"#,
            quoted(&format!(
                "text(JSON.stringify(await tools.exec_command({{cmd: {}}})));",
                quoted(command)
            ))
        ),
        true => r#"{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}"#.to_string(),
    };
    let usage = r#"{"input_tokens":1,"output_tokens":1,"total_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}"#;
    let events = [
        (
            "response.created",
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#.to_string(),
        ),
        (
            "response.output_item.done",
            format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{item}}}"#),
        ),
        (
            "response.completed",
            format!(
                r#"{{"type":"response.completed","response":{{"id":"resp_1","object":"response","status":"completed","output":[{item}],"usage":{usage}}}}}"#
            ),
        ),
    ];
    let mut body = String::new();
    for (event, data) in events {
        body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    let mut answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    answer.extend_from_slice(body.as_bytes());
    answer
}

/// `text` as a JSON string, which is also a JavaScript string.
fn quoted(text: &str) -> String {
    let mut quoted = String::from('"');
    for character in text.chars() {
        match character {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04x}", u32::from(character)));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
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
