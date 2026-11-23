//! Feeds generated text to the code that reads untrusted input, and stops at
//! the first panic. It uses only the standard library, so it builds on stable
//! and adds no C to the tree. A run is reproduced by passing its seed again.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vmm::GuestMemory;
use vmm::virtio::Transport;

const USAGE: &str = "usage: fuzz <seconds> [seed]";
/// Inputs between looks at the clock.
const BATCH: u64 = 1024;
const MAX_EDITS: usize = 8;
const SPECIAL: [char; 10] = ['"', '\\', '\n', '\r', '\t', ' ', '#', '\0', 'é', '🦀'];

struct Target {
    name: &'static str,
    check: fn(&str),
    corpus: &'static [&'static str],
    words: &'static [&'static str],
}

const TARGETS: [Target; 10] = [
    Target {
        name: "agentfile",
        check: |input| {
            let _ = agentfile::parse(input);
        },
        corpus: &[
            "",
            "# a comment\n\nFROM ubuntu:24.04\n",
            "SHARE ./out \"a b\" ro\n",
            "SHARE \"say \\\"hi\\\"\" C:\\work\n",
            "SYSTEM you are a helper\nPROMPT do the thing\nENV KEY=value\n",
        ],
        words: &[
            "FROM", "SHARE", "SYSTEM", "PROMPT", "ENV", "--", "--force", "\"\"", "\\\"", "\\\\",
            " rw",
        ],
    },
    Target {
        name: "device",
        check: |input| {
            device_input(input);
        },
        corpus: &[
            "",
            // A queue with one chain of two descriptors.
            concat!(
                "set 0x38 8 4\nset 0x80 0x1000 4\nset 0x90 0x2000 4\nset 0xa0 0x3000 4\n",
                "set 0x44 1 4\nset 0x70 4 4\n",
                "mem 0x1000 0x4000\nmem 0x1008 0x1000100000010\n",
                "mem 0x1010 0x5000\nmem 0x1018 0x200000040\n",
                "mem 0x2000 0x10000\nrun\n",
            ),
            // A descriptor that names itself.
            concat!(
                "set 0x38 8 4\nset 0x80 0x1000 4\nset 0x90 0x2000 4\nset 0xa0 0x3000 4\n",
                "set 0x44 1 4\nset 0x70 4 4\nmem 0x1000 0x4000\nmem 0x1008 0x100000010\n",
                "mem 0x2000 0x10000\nrun\n",
            ),
            "get 0 4\nget 0x100 1\nset 0x70 0 4\nset 0x50 0 4\nset 0x64 1 4\nget 0x34 4\n",
        ],
        words: &[
            "run\n",
            "mem ",
            "set ",
            "get ",
            " 4\n",
            " 1\n",
            "0x",
            "0xffffffffffffffff",
            "0xffffffff",
            "0xfff8",
            "0x10000",
            "0x10000000000000",
            "0x400000000",
            "set 0x70 0 4\n",
            "set 0x84 1 4\n",
            "set 0x30 1 4\n",
            "mem 0x2000 0x90000\n",
        ],
    },
    Target {
        name: "fuse",
        check: |input| {
            fuse_input(input);
        },
        corpus: &[
            "",
            // INIT, then a file looked up, opened, read, and released.
            concat!(
                "26 0 w:7 w:31 w:65536 w:4294967295\n1 1 hello.txt\n",
                "14 2 w:0 w:0\n15 0 1 0 w:4096\n18 0 1 0\n",
            ),
            // A directory listed.
            "26 0 w:7 w:31 w:0 w:0\n1 1 dir\n27 2 0\n28 0 1 0 w:4096\n1 2 nested.txt\n",
            // Symlinks read, and followed as directories.
            "1 1 escape\n5 2\n1 2 secret\n1 1 absolute\n5 3\n14 3 w:0 w:0\n",
            // Forgetting, and a write.
            "1 1 hello.txt\n2 2 1\n3 2 0 0\n42 0 w:1 w:0 2 1\n16 2 1 0 w:5 w:0\n",
        ],
        words: &[
            "\n",
            " ",
            "1 ",
            "2 ",
            "3 ",
            "w:0 ",
            "w:2 ",
            "w:65536 ",
            "0xffffffffffffffff ",
            "..",
            ".",
            "/",
            "hello.txt",
            "escape",
            "absolute",
            "dirlink",
            "dir",
            "secret",
            "14 ",
            "15 ",
            "27 ",
            "28 ",
            "1 1 ",
            "5 ",
        ],
    },
    Target {
        name: "fuse-rw",
        check: |input| {
            fuse_rw_input(input);
        },
        corpus: &[
            "",
            // A file made, written, truncated, renamed, linked, and removed.
            concat!(
                "26 0 w:7 w:31 w:65536 w:4294967295\n",
                "35 1 w:66 w:420 w:18 w:0 new.txt\n",
                "16 0 1 0 w:5 w:0 0 w:0 w:0 agent\n",
                "4 2 w:8 w:0 0 2 0 0 0 0 w:0 w:0 w:0 w:0 w:0 w:0 w:0 w:0\n",
                "9 1 w:493 w:18 made\n12 1 3 new.txt moved\n13 3 2 hard\n",
                "6 3 link ../hello.txt\n10 3 link\n10 3 hard\n",
            ),
            // What SPEC.md protects, written to, removed, and replaced.
            concat!(
                "1 1 Agentfile\n14 2 w:1 w:0\n10 1 Agentfile\n",
                "1 1 .git\n1 3 config\n14 4 w:513 w:0\n12 1 3 hello.txt config\n",
                "1 3 hooks\n35 5 w:66 w:493 w:0 w:0 post-merge\n11 1 .git\n",
            ),
            // Locks, and a rename that exchanges.
            concat!(
                "1 1 hello.txt\n14 2 w:2 w:0\n32 0 1 7 0 9223372036854775807 w:1 w:0 w:0 w:0\n",
                "31 0 1 8 0 9223372036854775807 w:1 w:0 w:0 w:0\n25 2 1 w:0 w:0 7\n",
                "45 1 1 w:2 w:0 hello.txt Agentfile\n",
            ),
        ],
        words: &[
            "\n",
            " ",
            "1 ",
            "2 ",
            "3 ",
            "w:0 ",
            "w:1 ",
            "w:2 ",
            "w:66 ",
            "w:493 ",
            "0xffffffffffffffff ",
            "..",
            "/",
            "hello.txt",
            "escape",
            "dirlink",
            "dir",
            "Agentfile",
            "AGENTFILE",
            ".git",
            ".GIT",
            "config",
            "hooks",
            "modules",
            "4 ",
            "6 ",
            "9 ",
            "10 ",
            "11 ",
            "12 ",
            "13 ",
            "16 ",
            "32 ",
            "35 ",
            "45 ",
        ],
    },
    Target {
        name: "git",
        check: |input| {
            git_input(input);
        },
        corpus: &[
            "",
            "78981922613b2afb6025042ff6bd878ac1994e85\n",
            "ref: refs/heads/main\n",
            concat!(
                "# pack-refs with: peeled fully-peeled sorted \n",
                "78981922613b2afb6025042ff6bd878ac1994e85 refs/heads/main\n",
                "78981922613b2afb6025042ff6bd878ac1994e85 refs/tags/v1\n",
                "^78981922613b2afb6025042ff6bd878ac1994e85\n",
            ),
            "blob 4\0four",
            "tree 0\0",
        ],
        words: &[
            "\n",
            " ",
            "\0",
            "\x0c",
            "ref:",
            "ref: ",
            "refs/heads/main",
            "refs/heads/fuzz",
            "refs/heads/",
            "refs/tags/v1",
            "78981922613b2afb6025042ff6bd878ac1994e85",
            "0000000000000000000000000000000000000000",
            "^",
            "# pack-refs with: ",
            "peeled ",
            "sorted ",
            "blob ",
            "commit ",
            "tag ",
            "0",
            "4",
            "99999999999999999999",
            "..",
            ".lock",
            "@{",
        ],
    },
    Target {
        name: "diff",
        check: |input| {
            diff_input(input);
        },
        corpus: &[
            "",
            "a\nb\nc\n\x01a\nc\n",
            "fn x() {\n    c\n}\n\n\x01fn x() {\n    c\n    d\n}\n\nend",
            "\tif y {\n\n}\n\x01\tif y {\n\n\n}\n",
        ],
        words: &[
            "\n",
            "\x01",
            "a\n",
            "b\n",
            "}\n",
            "fn x() {\n",
            "    c\n",
            "\tif y {\n",
            "   \n",
            "\x0b\n",
            "\x0c\n",
            "\0",
            "\r\n",
        ],
    },
    Target {
        name: "net",
        check: |input| {
            net_input(input);
        },
        corpus: &[
            "",
            concat!(
                "NETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\n",
                "NETWORK +https://codeberg.org/forgejo/forgejo\n",
                "> github.com 100.64.0.1 443 https /torvalds/linux\n",
                "> codeberg.org 100.64.0.2 443 https /forgejo/forgejo/%2e%2E/x\n",
                "> gist.github.com 100.64.0.3 22\n",
            ),
            concat!(
                "NETWORK +\nNETWORK -1.1.1.0/24\nNETWORK +1.1.1.1:443\n",
                "NETWORK -[2606:4700::]/32:443\nNETWORK +[::ffff:10.0.0.0]/104\n",
                "> - ::ffff:1.1.1.1 443\n> one.one.one.one 1.1.1.1 53\n",
                "> - 2606:4700::1 443 http /a?b=%2F\n",
            ),
        ],
        words: &[
            "\n",
            " ",
            "NETWORK ",
            "+",
            "-",
            "> ",
            "- ",
            "github.com",
            "api.github.com",
            "github.com.",
            "https://",
            "http://",
            "https ",
            "http ",
            "/",
            "/jgabor/agent-plus",
            "?",
            "#",
            "%",
            "%2F",
            "%2e",
            "%7E",
            ".",
            "..",
            ":22",
            ":443",
            " 443 ",
            " 80 ",
            "1.1.1.1",
            "10.0.0.0/8",
            "/0",
            "/32",
            "[::ffff:1.1.1.1]",
            "[2606:4700::]/32",
            "::ffff:1.1.1.1",
        ],
    },
    Target {
        name: "dns",
        check: |input| {
            dns_input(input);
        },
        corpus: &[
            "",
            // An `A` query for `kagi.com`, as a stub resolver sends it.
            "\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x04kagi\x03com\x00\x00\x01\x00\x01",
            // `AAAA` for `api.kagi.com`, with an EDNS record.
            concat!(
                "\x00\x07\x01\x00\x00\x01\x00\x00\x00\x00\x00\x01",
                "\x03api\x04kagi\x03com\x00\x00\x1c\x00\x01",
                "\x00\x00\x29\x04\x00\x00\x00\x00\x00\x00\x00",
            ),
            // `HTTPS` for a name no rule allows.
            "\x00\x08\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x41\x00\x01",
        ],
        words: &[
            "\x00",
            "\x01",
            "\x00\x01",
            "\x00\x1c",
            "\x00\x40",
            "\x00\x41",
            "\x00\x10",
            "\x01\x00",
            "\x04kagi\x03com\x00",
            "\x03api",
            "\x08kagi.com",
            "\x3f",
            "\x00\x00\x29",
            "\x7f",
            // A compression pointer to the question's name.
            "\u{c0}\x0c",
            "\u{ff}",
        ],
    },
    Target {
        name: "name-check",
        check: |input| {
            name_check_input(input);
        },
        corpus: &[
            "",
            // A TLS client hello for `host.ap.internal`, with only the
            // extensions a hello must have.
            HELLO,
            "GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\nGET /b HTTP/1.1\r\nhost: HOST.ap.internal.\r\n\r\n",
            concat!(
                "POST / HTTP/1.1\r\nHost: host.ap.internal\r\nTransfer-Encoding: chunked\r\n\r\n",
                "3;x\r\nabc\r\n0\r\nT: 1\r\n\r\n",
                "PUT / HTTP/1.1\r\nHost: host.ap.internal:80\r\nContent-Length: 3\r\n\r\nabc",
            ),
            "M-SEARCH * HTTP/1.1\r\nHost: host.ap.internal\r\nMan: x\r\n\r\n",
            "SSH-2.0-OpenSSH_9.9\r\n",
        ],
        words: &[
            "\x16\x03\x01",
            "\x00\x00",
            "\x10host.ap.internal",
            "\x0bexample.com",
            "GET ",
            "CONNECT ",
            " HTTP/1.1\r\n",
            " HTTP/2.0\r\n",
            "Host: host.ap.internal\r\n",
            "Host: example.com\r\n",
            "Content-Length: 5\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Upgrade: websocket\r\n",
            "\r\n",
            "\n",
            " \t",
            "0\r\n\r\n",
            "ff\r\n",
            "\u{7f}",
        ],
    },
    Target {
        name: "request-check",
        check: |input| {
            request_check_input(input);
        },
        corpus: &[
            "",
            "GET /allowed HTTP/1.1\r\nHost: host.ap.internal\r\n\r\nGET /allowed/b?c HTTP/1.1\r\nHost: host.ap.internal:443\r\n\r\n",
            concat!(
                "POST /allowed/a HTTP/1.1\r\nHost: host.ap.internal\r\nContent-Length: 5\r\n\r\nGET /",
                "PUT /allowed/%7Eb HTTP/1.1\r\nHost: host.ap.internal\r\nTransfer-Encoding: chunked\r\n\r\n",
                "3\r\nabc\r\n0\r\n\r\n",
            ),
        ],
        words: &[
            "GET ",
            "POST ",
            "/allowed",
            "/denied",
            "/allowed/../",
            "/./",
            "%2F",
            "%7E",
            "%",
            "?",
            "*",
            " HTTP/1.1\r\n",
            "Host: host.ap.internal\r\n",
            "Host: example.com\r\n",
            "Content-Length: 5\r\n",
            "Transfer-Encoding: chunked\r\n",
            "\r\n",
            "SSH-2.0-x\r\n",
            "\x16\x03\x01",
        ],
    },
];

/// The TLS client hello in the name check's corpus.
const HELLO: &str = "\x16\x03\x01\x00\x57\x01\x00\x00\x53\x03\x03\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\x00\x00\x02\x13\x01\x01\x00\x00\x28\x00\x00\x00\x15\x00\x13\x00\x00\x10\x68\x6f\x73\x74\x2e\x61\x70\x2e\x69\x6e\x74\x65\x72\x6e\x61\x6c\x00\x0d\x00\x04\x00\x02\x04\x03\x00\x2b\x00\x03\x02\x03\x04";

const DEVICE_MEMORY: usize = 1 << 16;
const DEVICE_QUEUES: usize = 2;
const DEVICE_QUEUE_SIZE: u16 = 32;

fn number(word: Option<&str>) -> Option<u64> {
    let word = word?;
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// Plays a guest: each line pokes guest memory or a device register, and
/// `run` makes the device drain its queues. Text lets the generator's edits
/// reach deep into a valid setup. Returns how many chains the device accepted.
fn device_input(input: &str) -> usize {
    let memory = GuestMemory::new(DEVICE_MEMORY).expect("guest memory");
    let mut device = Transport::new(19, 1, vec![3; 8], DEVICE_QUEUES, DEVICE_QUEUE_SIZE);
    let mut accepted = 0;
    for line in input.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("mem") => {
                if let (Some(address), Some(value)) = (number(words.next()), number(words.next())) {
                    let _ = memory.write(address, &value.to_le_bytes());
                }
            }
            Some("set") => {
                let (offset, value, width) = (
                    number(words.next()),
                    number(words.next()),
                    number(words.next()),
                );
                if let (Some(offset), Some(value), Some(width)) = (offset, value, width) {
                    let bytes = value.to_le_bytes();
                    let _ = device.write(offset, &bytes[..(width as usize).min(8)]);
                }
            }
            Some("get") => {
                if let (Some(offset), Some(width)) = (number(words.next()), number(words.next())) {
                    let mut bytes = [0; 8];
                    device.read(offset, &mut bytes[..(width as usize).min(8)]);
                }
            }
            Some("run") => accepted += drain(&mut device, &memory),
            _ => {}
        }
    }
    accepted
}

/// Takes every chain from every queue, and panics if an accepted chain breaks
/// a promise the queue makes.
fn drain(device: &mut Transport, memory: &GuestMemory) -> usize {
    let mut accepted = 0;
    for index in 0..DEVICE_QUEUES {
        let Some(queue) = device.queue(index) else {
            continue;
        };
        while let Ok(Some(chain)) = queue.pop(memory) {
            assert!(chain.buffers.len() <= usize::from(DEVICE_QUEUE_SIZE));
            let mut total = 0;
            for buffer in &chain.buffers {
                total += u64::from(buffer.len);
                let mut bytes = vec![0; buffer.len as usize];
                memory
                    .read(buffer.address, &mut bytes)
                    .expect("an accepted buffer lies inside guest memory");
                if buffer.writable {
                    memory
                        .write(buffer.address, &bytes)
                        .expect("an accepted buffer lies inside guest memory");
                }
            }
            assert!(total <= u64::from(u32::MAX));
            let _ = queue.add_used(memory, chain.head, 0);
            accepted += 1;
        }
    }
    device.signal_used();
    accepted
}

/// Asks a resolver that allows `kagi.com` and its subdomains, from a pool
/// of two addresses, the input as one query, twice, and panics if an answer
/// isn't a reply to it, or the second differs. Each character is the byte
/// of its code point's low eight bits, so that the input can hold any byte.
/// Returns whether it answered.
fn dns_input(input: &str) -> bool {
    let at = [std::net::IpAddr::from([10, 0, 2, 2])];
    let pool = netstack::Pool::new([198, 18, 0, 0].into(), 30);
    let mut resolver = netstack::Resolver::new(&at, pool, |name| {
        name == "kagi.com" || name.ends_with(".kagi.com")
    });
    let query: Vec<u8> = input.chars().map(|c| c as u32 as u8).collect();
    let query = &query[..];
    let Some(answer) = resolver.answer(query) else {
        return false;
    };
    // The same id, a response, and the one question.
    assert_eq!(answer.get(..2), query.get(..2), "{answer:?}");
    assert_eq!(answer[2] & 0x80, 0x80, "{answer:?}");
    assert_eq!(answer.get(4..6), Some(&[0, 1][..]), "{answer:?}");
    assert_eq!(resolver.answer(query), Some(answer));
    true
}

/// Feeds `input` to the name check of a connection attributed to
/// `host.ap.internal`, whole and in pieces, and panics if the two disagree,
/// or if what passes isn't the start of `input`. Returns whether it all
/// passed.
fn name_check_input(input: &str) -> bool {
    let bytes: Vec<u8> = input.chars().map(|c| c as u32 as u8).collect();
    let checked = |piece: usize| {
        let mut check = net::NameCheck::new("host.ap.internal");
        let mut passed = Vec::new();
        for chunk in bytes.chunks(piece) {
            if check.feed(chunk, &mut passed).is_err() {
                return (passed, false);
            }
        }
        (passed, !check.holding())
    };
    let whole = checked(bytes.len().max(1));
    for piece in [1, 1 + bytes.len() % 13] {
        let pieces = checked(piece);
        assert_eq!(whole.1, pieces.1, "in pieces of {piece}");
        if whole.1 {
            assert_eq!(whole.0, pieces.0, "in pieces of {piece}");
        }
    }
    assert!(bytes.starts_with(&whole.0), "{:?}", whole.0);
    // httparse, as a server would, reads the first request on its own. If
    // it finds one whose `Host` doesn't agree, none of that head passed.
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    if let Ok(httparse::Status::Complete(head)) = request.parse(&bytes) {
        let hosts: Vec<_> = (request.headers.iter())
            .filter(|header| header.name.eq_ignore_ascii_case("host"))
            .collect();
        if !matches!(hosts[..], [host] if names_host(host.value)) {
            assert!(
                whole.0.len() < head,
                "a head for another host passed: {:?}",
                whole.0
            );
        }
    }
    // hyper, as a server would, reads every request that passed.
    same_requests(&whole.0, &checked_requests(&whole.0));
    whole.1 && whole.0 == bytes
}

/// Whether a `Host` header's value names `host.ap.internal`.
fn names_host(value: &[u8]) -> bool {
    let value = value.trim_ascii().to_ascii_lowercase();
    let colon = value.iter().rposition(|&byte| byte == b':');
    let (name, port) = value.split_at(colon.unwrap_or(value.len()));
    let port_ok = port.iter().skip(1).all(u8::is_ascii_digit);
    port_ok && name.strip_suffix(b".").unwrap_or(name) == b"host.ap.internal"
}

/// A request that a reader took from a stream: the offset just after its
/// last byte, its method, and its `Host` values, escaped.
#[derive(Debug, PartialEq)]
struct Framed {
    end: usize,
    method: String,
    hosts: Vec<String>,
}

/// The requests that the name check of a connection to `host.ap.internal`
/// reads from `passed`, fed to it one byte at a time. A request ends where
/// the check holds nothing.
fn checked_requests(passed: &[u8]) -> Vec<Framed> {
    let mut check = net::NameCheck::requests("host.ap.internal");
    let mut requests = Vec::new();
    let mut start = 0;
    for end in 1..=passed.len() {
        if check.feed(&passed[end - 1..end], &mut Vec::new()).is_err() {
            break;
        }
        if check.holding() {
            continue;
        }
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let mut head = httparse::Request::new(&mut headers);
        let _ = head.parse(&passed[start..end]);
        requests.push(Framed {
            end,
            method: head.method.unwrap_or_default().to_owned(),
            hosts: (head.headers.iter())
                .filter(|header| header.name.eq_ignore_ascii_case("host"))
                .map(|header| header.value.escape_ascii().to_string())
                .collect(),
        });
        start = end;
    }
    requests
}

/// Panics unless hyper's HTTP/1.1 server, reading `passed` one byte at a
/// time, reads the same requests as `ours`, up to where it stops reading,
/// and each names `host.ap.internal`.
fn same_requests(passed: &[u8], ours: &[Framed]) {
    let (theirs, stop, refused) = served_requests(passed);
    let read = (ours.iter())
        .take_while(|request| stop.is_none_or(|stop| request.end < stop))
        .count();
    let named = (theirs.iter())
        .all(|request| matches!(&request.hosts[..], [host] if names_host(host.as_bytes())));
    assert!(
        ours.starts_with(&theirs) && theirs.len() >= read && named && refused.is_none(),
        "{}: crates/net {ours:?}, hyper {theirs:?}, which stopped at {stop:?} and refused a body at {refused:?}",
        passed.escape_ascii(),
    );
}

/// The requests that hyper's HTTP/1.1 server reads from `passed`, fed to it
/// one byte at a time, the offset where it stopped reading, if it did, and
/// the offset where it refused a body, if it did. Only a body's framing can
/// make it refuse a body, since the stream never ends.
/// A request ends where hyper has read all of its body.
fn served_requests(passed: &[u8]) -> (Vec<Framed>, Option<usize>, Option<usize>) {
    use hyper::body::{Body, Bytes, Frame, Incoming};
    use std::cell::{Cell, RefCell};
    use std::convert::Infallible;
    use std::pin::{Pin, pin};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    /// The stream hyper reads, and how far it got.
    struct Stream<'a> {
        passed: &'a [u8],
        fed: Rc<Cell<usize>>,
        read: Rc<Cell<usize>>,
    }
    impl hyper::rt::Read for Stream<'_> {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<std::io::Result<()>> {
            let (read, fed) = (self.read.get(), self.fed.get());
            if read == fed {
                return Poll::Pending;
            }
            let end = fed.min(read + buf.remaining());
            buf.put_slice(&self.passed[read..end]);
            self.read.set(end);
            Poll::Ready(Ok(()))
        }
    }
    impl hyper::rt::Write for Stream<'_> {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    /// An empty response body.
    struct Nothing;
    impl Body for Nothing {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            Poll::Ready(None)
        }
        fn is_end_stream(&self) -> bool {
            true
        }
    }

    let fed = Rc::new(Cell::new(0));
    let read = Rc::new(Cell::new(0));
    let served = Rc::new(RefCell::new(Vec::new()));
    let refused = Rc::new(Cell::new(None));
    let service = {
        let (fed, served, refused) = (fed.clone(), served.clone(), refused.clone());
        hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
            let (fed, served, refused) = (fed.clone(), served.clone(), refused.clone());
            async move {
                let method = request.method().as_str().to_owned();
                let hosts = (request.headers().get_all(hyper::header::HOST).iter())
                    .map(|value| value.as_bytes().escape_ascii().to_string())
                    .collect();
                let mut body = request.into_body();
                loop {
                    match std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                        None => {
                            let end = fed.get();
                            served.borrow_mut().push(Framed { end, method, hosts });
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) => {
                            refused.set(Some(fed.get()));
                            break;
                        }
                    }
                }
                Ok::<_, Infallible>(hyper::Response::new(Nothing))
            }
        })
    };
    let stream = Stream {
        passed,
        fed: fed.clone(),
        read: read.clone(),
    };
    // As many headers as the name check reads, so that hyper refuses fewer
    // of the heads that the check passes.
    let mut connection = pin!(
        hyper::server::conn::http1::Builder::new()
            .max_headers(128)
            .serve_connection(stream, service)
    );
    let mut cx = Context::from_waker(Waker::noop());
    for end in 0..=passed.len() {
        fed.set(end);
        // Hyper may stop to let other tasks run, so it has read all it can
        // only once it makes no progress for a few polls in a row.
        let mut idle = 0;
        while idle < 4 {
            let before = (read.get(), served.borrow().len());
            if connection.as_mut().poll(&mut cx).is_ready() {
                return (served.take(), Some(end), refused.get());
            }
            idle = match (read.get(), served.borrow().len()) == before {
                true => idle + 1,
                false => 0,
            };
        }
    }
    (served.take(), None, refused.get())
}

/// Feeds `input`, as the guest's requests on a decrypted connection to
/// `host.ap.internal`, to the request check, whole and in pieces, under a
/// rule that allows paths under `/allowed`. Panics if the two disagree, if
/// what passes isn't the start of `input`, or if any of the head of a first
/// request that the rules deny passed. Returns whether it all passed.
fn request_check_input(input: &str) -> bool {
    let bytes: Vec<u8> = input.chars().map(|c| c as u32 as u8).collect();
    let instructions =
        agentfile::parse("AGENT claude\nNETWORK -\nNETWORK +https://host.ap.internal/allowed\n")
            .unwrap();
    let network = agentfile::Network::read(&instructions).unwrap();
    let allows = |target: &str| {
        let connection = net::Connection {
            name: Some("host.ap.internal"),
            address: std::net::Ipv4Addr::LOCALHOST.into(),
            port: 443,
            request: Some(net::Request {
                scheme: agentfile::Scheme::Https,
                target,
            }),
        };
        net::proxied(&network, &[], &connection).allows()
    };
    let checked = |piece: usize| {
        let mut check = net::NameCheck::requests("host.ap.internal");
        let mut passed = Vec::new();
        for chunk in bytes.chunks(piece) {
            if (check.feed_deciding(chunk, &mut passed, &mut |target| allows(target))).is_err() {
                return (passed, false);
            }
        }
        (passed, !check.holding())
    };
    let whole = checked(bytes.len().max(1));
    for piece in [1, 1 + bytes.len() % 13] {
        let pieces = checked(piece);
        assert_eq!(whole.1, pieces.1, "in pieces of {piece}");
        if whole.1 {
            assert_eq!(whole.0, pieces.0, "in pieces of {piece}");
        }
    }
    assert!(bytes.starts_with(&whole.0), "{:?}", whole.0);
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    if let Ok(httparse::Status::Complete(head)) = request.parse(&bytes)
        && !request.path.is_some_and(allows)
    {
        assert!(
            whole.0.len() < head,
            "a head the rules deny passed: {:?}",
            whole.0
        );
    }
    whole.1 && whole.0 == bytes
}

/// Decides each `> <name> <address> <port> [<scheme> <target>]` line, with
/// `-` for no name, under the `NETWORK` lines of the input, and panics if a
/// decision breaks a promise of the evaluation. Returns how many it made.
fn net_input(input: &str) -> usize {
    let (connections, lines): (Vec<&str>, Vec<&str>) =
        input.lines().partition(|line| line.starts_with("> "));
    let Ok(instructions) = agentfile::parse(&format!("AGENT claude\n{}", lines.join("\n"))) else {
        return 0;
    };
    let Ok(network) = agentfile::Network::read(&instructions) else {
        return 0;
    };
    let mut reversed = network.rules.clone();
    reversed.reverse();
    let mut decided = 0;
    for line in connections {
        let words: Vec<&str> = line[2..].split(' ').collect();
        let (Some(address), Some(port)) = (
            words.get(1).and_then(|word| word.parse().ok()),
            words.get(2).and_then(|word| word.parse().ok()),
        ) else {
            continue;
        };
        let request = match (words.get(3), words.get(4)) {
            (Some(&"https"), Some(target)) => Some((agentfile::Scheme::Https, *target)),
            (Some(&"http"), Some(target)) => Some((agentfile::Scheme::Http, *target)),
            _ => None,
        };
        let connection = net::Connection {
            name: words.first().filter(|name| **name != "-").copied(),
            address,
            port,
            request: request.map(|(scheme, target)| net::Request { scheme, target }),
        };
        if let Some((_, target)) = request {
            let path = target.split(['?', '#']).next().unwrap_or_default();
            same_path(path, net::normalize_path(path).as_deref());
            if let Some(normal) = net::normalize_path(target) {
                assert_eq!(net::normalize_path(&normal).as_ref(), Some(&normal));
            }
        }
        let decision = net::decide(&network.rules, network.default, &connection);
        match decision.by {
            net::By::Rule(rule) => assert_eq!(rule.sign, decision.sign),
            net::By::Default => assert_eq!(decision.sign, network.default),
            net::By::Path => assert_eq!(decision.sign, agentfile::Sign::Deny),
        }
        // Line order never matters, though of equally specific rules that
        // agree, either can be the one named.
        let other = net::decide(&reversed, network.default, &connection);
        assert_eq!(other.sign, decision.sign);
        match (decision.by, other.by) {
            (net::By::Rule(one), net::By::Rule(two)) => assert_eq!(one.sign, two.sign),
            (one, two) => assert_eq!(one, two),
        }
        decided += 1;
    }
    decided
}

/// A path normalizes as iri-string normalizes the path of
/// `http://h<path>`, and one that only a broken escape keeps from being a
/// URI's path has no normal form. Only an empty path or one that starts with
/// `/` is the whole path of such a URI.
fn same_path(path: &str, ours: Option<&str>) {
    if !path.is_empty() && !path.starts_with('/') {
        return;
    }
    let uri = format!("http://h{path}");
    let theirs = iri_string::types::UriStr::new(&uri)
        .ok()
        .map(|uri| uri.normalize().to_string()["http://h".len()..].to_string());
    // RFC 3986 section 3.3 allows these, `/`, and escapes in a path.
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/%".contains(&b);
    if theirs.is_some() || path.bytes().all(allowed) {
        assert!(
            ours == theirs.as_deref(),
            "{path:?}: crates/net {ours:?}, iri-string {theirs:?}"
        );
    }
}

/// The blob `a\n`, which every ref in the git target's corpus names.
const GIT_BLOB: &str = "78981922613b2afb6025042ff6bd878ac1994e85";

/// Two texts for the diff target, each in a file of this process's own, so
/// `git` reads them.
fn diff_files() -> &'static (PathBuf, PathBuf) {
    static FILES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FILES.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fuzz-diff")
            .join(std::process::id().to_string());
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("the fuzz directory");
        (root.join("old"), root.join("new"))
    })
}

/// Diffs the text before the first `\x01` with the text after it, as `ap
/// diff` and as `git diff --no-index` do, and panics where their hunks
/// differ. Returns how many bytes of hunks there were.
fn diff_input(input: &str) -> usize {
    let (old, new) = input.split_once('\x01').unwrap_or((input, ""));
    let (a, b) = diff_files();
    fs::write(a, old).expect("the fuzz directory");
    fs::write(b, new).expect("the fuzz directory");
    let root = a.parent().expect("the fuzz directory");
    let out = git(root)
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "old",
            "new",
        ])
        .output()
        .expect("git");
    let text = out.stdout;
    // `ap diff` prints one line for a binary file, as git does, and has
    // no hunks to compare.
    let binary = |text: &str| text.as_bytes()[..text.len().min(8000)].contains(&0);
    if binary(old) || binary(new) {
        return 0;
    }
    let theirs = match text.windows(3).position(|window| window == b"\n@@") {
        Some(at) => text[at + 1..].to_vec(),
        None => Vec::new(),
    };
    same_hunks(
        input,
        &ap::unified::hunks(old.as_bytes(), new.as_bytes()),
        &theirs,
    );
    theirs.len()
}

fn same_hunks(input: &str, ours: &[u8], theirs: &[u8]) {
    assert!(
        ours == theirs,
        "ap diff's hunks differ from git's for {input:?}:\n{}\ngit's:\n{}",
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(theirs)
    );
}

/// The name of the loose object the git target plants.
const GIT_PLANTED: &str = "fe00000000000000000000000000000000000000";

/// A repository holding one blob and `refs/heads/main`, for this process
/// alone, since each input rewrites it.
fn git_repo() -> &'static Path {
    static REPO: OnceLock<PathBuf> = OnceLock::new();
    REPO.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fuzz-git")
            .join(std::process::id().to_string());
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("the fuzz repository");
        let init = git(&root).args(["init", "-q", "-b", "main"]).status();
        assert!(init.expect("git").success(), "git init");
        fs::write(root.join("a"), "a\n").expect("the fuzz repository");
        let hash = git(&root).args(["hash-object", "-w", "a"]).output();
        assert_eq!(
            hash.expect("git").stdout,
            format!("{GIT_BLOB}\n").as_bytes()
        );
        fs::write(root.join(".git/refs/heads/main"), format!("{GIT_BLOB}\n"))
            .expect("the fuzz repository");
        root
    })
}

/// `git` in `root`, reading no configuration but the repository's own.
fn git(root: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    git
}

/// What `git` prints, or `None` if it fails.
fn git_says(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = git(root).args(args).output().expect("git");
    out.status.success().then_some(out.stdout)
}

/// Reads the input as `crates/git` and as `git` do, and panics where they
/// differ. The input is a loose ref, then `packed-refs`, then the
/// uncompressed bytes of a loose object. Returns how many of the three git
/// accepted.
fn git_input(input: &str) -> usize {
    let root = git_repo();
    let dir = root.join(".git");
    let repo = git::Repository::at(&dir);
    let mut accepted = 0;

    let fuzz = dir.join("refs/heads/fuzz");
    fs::write(&fuzz, input).expect("the fuzz repository");
    let theirs = git_says(root, &["rev-parse", "--verify", "-q", "refs/heads/fuzz"]);
    let ours = repo.resolve("refs/heads/fuzz");
    accepted += usize::from(theirs.is_some());
    same_ref(input, ours, theirs);
    fs::remove_file(&fuzz).expect("the fuzz repository");

    fs::write(dir.join("packed-refs"), input).expect("the fuzz repository");
    // With a pattern, git seeks past records it never parses, so it lists
    // every ref, and the crate's are those under `refs/`.
    let theirs = git_says(root, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .map(|listed| under_refs(&listed));
    let ours = repo.refs().map(|refs| {
        let mut listed = String::new();
        for (name, _) in refs {
            let id = repo.resolve(&name).ok().flatten();
            let id = id.map_or("?".to_string(), |id| id.to_string());
            listed.push_str(&format!("{name} {id}\n"));
        }
        listed.into_bytes()
    });
    accepted += usize::from(theirs.is_some());
    same_list(input, ours, theirs);
    fs::remove_file(dir.join("packed-refs")).expect("the fuzz repository");

    let object = dir.join("objects/fe").join(&GIT_PLANTED[2..]);
    fs::create_dir_all(object.parent().unwrap()).expect("the fuzz repository");
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(input.as_bytes(), 1);
    fs::write(&object, compressed).expect("the fuzz repository");
    let id = git::Id::from_hex(GIT_PLANTED).unwrap();
    let ours = repo.loose_object(&id);
    let kind = git_says(root, &["cat-file", "-t", GIT_PLANTED]);
    accepted += usize::from(kind.is_some());
    // `git cat-file` reads a size that disagrees with the data leniently,
    // where the crate follows `git fsck`, so only what the crate accepts must
    // match exactly.
    match (ours, kind) {
        (Ok(Some(object)), Some(kind)) => {
            assert_eq!(format!("{}\n", object.kind).as_bytes(), kind, "{input:?}");
            let kind = object.kind.to_string();
            let data = git_says(root, &["cat-file", &kind, GIT_PLANTED]);
            assert_eq!(Some(&object.data), data.as_ref(), "{input:?}");
            written_input(&repo, &object, input);
        }
        (Ok(Some(_)), None) => panic!("crates/git reads an object git refuses: {input:?}"),
        (Err(git::Error::Corrupt(_)), _) => {}
        (other, _) => panic!("{input:?}: {other:?}"),
    }
    fs::remove_file(&object).expect("the fuzz repository");
    packed_input(root, input);
    accepted
}

/// Writes `object` as `crates/git` does, and panics unless it reads back the
/// same under the name its bytes hash to, from the crate and from `git`.
fn written_input(repo: &git::Repository, object: &git::Object, input: &str) {
    let root = git_repo();
    let raw = [
        format!("{} {}\0", object.kind, object.data.len()).as_bytes(),
        &object.data,
    ]
    .concat();
    let mut child = Command::new("sha1sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sha1sum");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&raw)
        .expect("sha1sum");
    let out = child.wait_with_output().expect("sha1sum");
    let hex = String::from_utf8(out.stdout).expect("sha1sum")[..40].to_string();
    let path = root.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
    let existed = path.exists();
    let id = repo.write(object).expect("crates/git writes the object");
    assert_eq!(id.to_string(), hex, "{input:?}");
    let ours = repo.loose_object(&id).expect("crates/git reads it back");
    assert_eq!(ours.as_ref(), Some(object), "{input:?}");
    let kind = object.kind.to_string();
    let theirs = git_says(root, &["cat-file", &kind, &hex]);
    assert_eq!(theirs.as_ref(), Some(&object.data), "{input:?}");
    if !existed {
        fs::remove_file(&path).expect("the fuzz repository");
    }
}

/// Writes the input and two blobs like it, which git can store as deltas
/// against it, packs them with each kind of delta, and reads each blob from
/// the pack as `crates/git` and as `git` do.
fn packed_input(root: &Path, input: &str) {
    let reversed: String = input.chars().rev().collect();
    let blobs = [
        input.to_string(),
        format!("{input}{input}"),
        format!("{input}\n{reversed}"),
    ];
    let mut paths = Vec::new();
    for (n, blob) in blobs.iter().enumerate() {
        let path = root.join(format!("blob{n}"));
        fs::write(&path, blob).expect("the fuzz repository");
        paths.push(path.to_str().expect("a UTF-8 path").to_string());
    }
    let pack = root.join(".git/objects/pack");
    for delta in ["--delta-base-offset", "--no-delta-base-offset"] {
        let mut args = vec!["hash-object", "-w", "--"];
        args.extend(paths.iter().map(String::as_str));
        let ids = git_says(root, &args).expect("git hash-object");
        let prefix = pack.join("pack");
        let prefix = prefix.to_str().expect("a UTF-8 path");
        let packed = git_fed(root, &["pack-objects", "-q", delta, prefix], &ids);
        assert!(packed.is_some(), "git pack-objects");
        // Git must read the pack too, not the loose copies.
        for hex in ids.split(|&b| b == b'\n').filter(|hex| !hex.is_empty()) {
            let hex = std::str::from_utf8(hex).expect("an id");
            let loose = root.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
            let _ = fs::remove_file(loose);
        }
        let theirs = git_fed(root, &["cat-file", "--batch"], &ids).expect("git cat-file");
        let repo = git::Repository::at(root.join(".git"));
        let mut ours = Vec::new();
        let mut objects = Vec::new();
        for hex in ids.split(|&b| b == b'\n').filter(|hex| !hex.is_empty()) {
            let id = git::Id::from_hex(std::str::from_utf8(hex).expect("an id")).expect("an id");
            match repo.packed_object(&id) {
                Ok(Some(object)) => {
                    let size = object.data.len();
                    ours.extend(format!("{id} {} {size}\n", object.kind).into_bytes());
                    ours.extend(&object.data);
                    ours.push(b'\n');
                    objects.push(object);
                }
                other => {
                    panic!("crates/git reads {id} from a {delta} pack as {other:?}: {input:?}")
                }
            }
        }
        let show = String::from_utf8_lossy;
        assert_eq!(show(&ours), show(&theirs), "crates/git, {delta}: {input:?}");
        for entry in fs::read_dir(&pack).expect("the fuzz repository") {
            fs::remove_file(entry.expect("the fuzz repository").path())
                .expect("the fuzz repository");
        }
        // A new `Repository`, which finds no pack, writes each object loose.
        let repo = git::Repository::at(root.join(".git"));
        for object in &objects {
            written_input(&repo, object, input);
        }
    }
}

/// What `git` prints when fed `input`, or `None` if it fails.
fn git_fed(root: &Path, args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = git(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("a pipe");
    stdin.write_all(input).expect("git's stdin");
    drop(stdin);
    let out = child.wait_with_output().expect("git");
    out.status.success().then_some(out.stdout)
}

/// The name in a line of a `git for-each-ref` listing.
fn name_of(line: &[u8]) -> &[u8] {
    let space = line.iter().rposition(|&b| b == b' ');
    &line[..space.unwrap_or(line.len())]
}

/// The lines of a `git for-each-ref` listing that name refs under `refs/`.
fn under_refs(listed: &[u8]) -> Vec<u8> {
    let lines = listed.split_inclusive(|&b| b == b'\n');
    lines
        .filter(|line| line.starts_with(b"refs/"))
        .flatten()
        .copied()
        .collect()
}

/// A ref resolves to what git resolves it to, and git's refusal is ours.
fn same_ref(input: &str, ours: Result<Option<git::Id>, git::Error>, theirs: Option<Vec<u8>>) {
    match (ours, theirs) {
        (Ok(Some(id)), Some(theirs)) => {
            assert_eq!(format!("{id}\n").as_bytes(), theirs, "{input:?}");
        }
        (Ok(None) | Err(git::Error::Corrupt(_)), None) => {}
        (ours, theirs) => panic!("{input:?}: crates/git {ours:?}, git {theirs:?}"),
    }
}

/// Refs list as `git for-each-ref` lists them, and git's refusal is ours.
fn same_list(input: &str, ours: Result<Vec<u8>, git::Error>, theirs: Option<Vec<u8>>) {
    match (ours, theirs) {
        (Ok(ours), Some(theirs)) => {
            // Git writes `packed-refs` sorted, with each name once. Given a
            // file that isn't, git lists it in file order, and the crate
            // lists each name once in order, so only the verdicts compare.
            let names: Vec<&[u8]> = theirs.split(|&b| b == b'\n').map(name_of).collect();
            if names
                .windows(2)
                .all(|pair| pair[0] < pair[1] || pair[1].is_empty())
            {
                let show = String::from_utf8_lossy;
                assert_eq!(show(&ours), show(&theirs), "{input:?}");
            }
        }
        (Err(git::Error::Corrupt(_)), None) => {}
        (ours, theirs) => panic!("{input:?}: crates/git {ours:?}, git {theirs:?}"),
    }
}

const CANARY: &[u8] = b"canary: a host file outside the share";
const INSIDE: &[u8] = b"inside the share";

/// A share's root, beside a host file that no answer may hold. Every run
/// makes the same tree, so runs at once can share it.
fn fuse_tree() -> &'static Path {
    static TREE: OnceLock<PathBuf> = OnceLock::new();
    TREE.get_or_init(|| {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fuzz-share");
        let root = base.join("root");
        fs::create_dir_all(root.join("dir")).expect("the fuzz tree");
        fs::create_dir_all(base.join("outside")).expect("the fuzz tree");
        let secret = base.join("outside/secret");
        let files = [
            (secret.clone(), CANARY),
            (root.join("hello.txt"), INSIDE),
            (root.join("dir/nested.txt"), INSIDE),
        ];
        for (path, contents) in files {
            if fs::read(&path).ok().as_deref() != Some(contents) {
                fs::write(&path, contents).expect("the fuzz tree");
            }
        }
        let links = [
            (PathBuf::from("../outside/secret"), "escape"),
            (secret, "absolute"),
            (PathBuf::from("../outside"), "dirlink"),
        ];
        for (target, name) in links {
            match symlink(target, root.join(name)) {
                Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                    panic!("the fuzz tree: {error}")
                }
                _ => {}
            }
        }
        base
    })
}

/// Plays a guest that sends the share server FUSE requests: each line is an
/// opcode, a node, and a body, whose words are a `u64` each, a `u32` after
/// `w:`, or a name ending in a NUL. Panics if an answer is malformed, holds
/// the host file outside the root, or the share changed. Returns how many
/// requests got an answer without an error.
fn fuse_input(input: &str) -> usize {
    let root = File::open(fuse_tree().join("root")).expect("the fuzz root");
    let mut server = share::Server::new(vec![share::Root::new(
        OwnedFd::from(root),
        share::Mode::ReadOnly,
    )])
    .expect("the share server");
    fuse_input_with(&mut server, input)
}

/// The host files an `rw` share's guest must never change: the file
/// outside the root, and what SPEC.md protects inside it.
const PROTECTED: [&str; 4] = [
    "outside/secret",
    "root/Agentfile",
    "root/.git/config",
    "root/.git/hooks/pre-commit",
];

/// Plays a guest of an `rw` share, as [`fuse_input`] does, in a tree of its
/// own that it removes afterwards. Panics as it does, and also if anything
/// outside the root, or anything [`PROTECTED`] in it, changed, or if
/// anything appeared beside the root.
fn fuse_rw_input(input: &str) -> usize {
    fuse_rw_input_at(input, "root", &fuse_rw_base())
}

/// A path for a new tree of [`fuse_rw_input`]'s, unique to this run.
fn fuse_rw_base() -> PathBuf {
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/fuzz-share-rw")
        .join(format!("{}-{run}", std::process::id()))
}

/// [`fuse_rw_input`] in a tree at `base`, with the share's root at
/// `served` in it, which a test moves to stand in for a server that
/// escaped it.
fn fuse_rw_input_at(input: &str, served: &str, base: &Path) -> usize {
    let _ = fs::remove_dir_all(base);
    let _tree = Tree(base);
    let root = base.join("root");
    fs::create_dir_all(root.join("dir")).expect("the fuzz tree");
    fs::create_dir_all(root.join(".git/hooks")).expect("the fuzz tree");
    fs::create_dir_all(base.join("outside")).expect("the fuzz tree");
    fs::write(base.join("outside/secret"), CANARY).expect("the fuzz tree");
    for name in &PROTECTED[1..] {
        fs::write(base.join(name), INSIDE).expect("the fuzz tree");
    }
    fs::write(root.join("hello.txt"), INSIDE).expect("the fuzz tree");
    symlink("../outside/secret", root.join("escape")).expect("the fuzz tree");
    symlink("../outside", root.join("dirlink")).expect("the fuzz tree");
    let mut server = share::Server::new(vec![share::Root::new(
        OwnedFd::from(File::open(base.join(served)).expect("the fuzz root")),
        share::Mode::ReadWrite,
    )])
    .expect("the share server");
    // The directories whose mode the guest must not change: the tree, the
    // one outside the root, and the read-only hooks.
    let kept = ["", "outside", "root/.git/hooks"].map(|name| base.join(name));
    let mode = |path: &PathBuf| fs::symlink_metadata(path).map(|meta| meta.mode()).ok();
    let modes = kept.each_ref().map(mode);
    let answered = fuse_answers(&mut server, input);
    // The guest may change the mode of any other directory, so the check
    // reaches past it.
    reach(base, &kept);
    for name in PROTECTED {
        let expected = if name == PROTECTED[0] { CANARY } else { INSIDE };
        let found = fs::read(base.join(name)).ok();
        assert_eq!(
            found.as_deref(),
            Some(expected),
            "{name} changed: {input:?}"
        );
    }
    for (path, before) in kept.iter().zip(modes) {
        let name = path.strip_prefix(base).expect("the fuzz tree").display();
        assert_eq!(mode(path), before, "{name}'s mode changed: {input:?}");
    }
    let names = |dir: &Path| -> Vec<_> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .expect("the fuzz tree")
            .map(|entry| entry.expect("the fuzz tree").file_name())
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        names(base),
        ["outside", "root"],
        "a file beside the root: {input:?}"
    );
    assert_eq!(
        names(&base.join("outside")),
        ["secret"],
        "a file outside: {input:?}"
    );
    answered
}

/// A tree of [`fuse_rw_input`]'s, which it removes when it drops, even
/// after a panic, whatever modes the guest set in it.
struct Tree<'a>(&'a Path);

impl Drop for Tree<'_> {
    fn drop(&mut self) {
        reach(self.0, &[]);
        let _ = fs::remove_dir_all(self.0);
    }
}

/// Lets the owner into `directory` and every directory beneath it, except
/// those `kept`, so a mode the guest may set hides no file from the check.
/// It follows no symlink. It doesn't panic, so a [`Tree`] can use it while
/// a panic unwinds, and a directory it can't open shows in the check.
fn reach(directory: &Path, kept: &[PathBuf]) {
    if !kept.iter().any(|path| path == directory) {
        let reachable = fs::Permissions::from_mode(0o700);
        let _ = fs::set_permissions(directory, reachable);
    }
    // A kept directory the guest closed hides its files, and the check
    // finds them changed.
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            reach(&entry.path(), kept);
        }
    }
}

fn fuse_input_with(server: &mut share::Server, input: &str) -> usize {
    let answered = fuse_answers(server, input);
    let hello = fs::read(fuse_tree().join("root/hello.txt")).expect("the fuzz root");
    assert_eq!(hello, INSIDE, "the share changed");
    answered
}

/// Sends each request in `input`, and checks each answer.
fn fuse_answers(server: &mut share::Server, input: &str) -> usize {
    let mut answered = 0;
    for (unique, line) in (1u64..).zip(input.lines()) {
        let mut words = line.split(' ');
        let (Some(opcode), Some(node)) = (number(words.next()), number(words.next())) else {
            continue;
        };
        let mut body = Vec::new();
        for word in words {
            match (word.strip_prefix("w:"), number(Some(word))) {
                (Some(value), _) => match number(Some(value)) {
                    Some(value) => body.extend((value as u32).to_le_bytes()),
                    None => body.extend(word.as_bytes()),
                },
                (None, Some(value)) => body.extend(value.to_le_bytes()),
                (None, None) => {
                    body.extend(word.as_bytes());
                    body.push(0);
                }
            }
        }
        let mut request = Vec::new();
        request.extend(((40 + body.len()) as u32).to_le_bytes());
        request.extend((opcode as u32).to_le_bytes());
        request.extend(unique.to_le_bytes());
        request.extend(node.to_le_bytes());
        request.extend([0; 16]);
        request.extend(body);
        let answer = server.answer(0, &request);
        if answer.is_empty() {
            continue;
        }
        assert_eq!(answer.len(), u32_at(&answer, 0) as usize, "{line:?}");
        assert_eq!(answer[8..16], unique.to_le_bytes(), "{line:?}");
        assert!(
            !answer.windows(CANARY.len()).any(|window| window == CANARY),
            "{line:?} reached outside the share"
        );
        if u32_at(&answer, 4) == 0 {
            answered += 1;
        }
    }
    answered
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

struct Random(u64);

impl Random {
    fn new(seed: u64) -> Random {
        Random((seed ^ 0x9E37_79B9_7F4A_7C15).max(1))
    }

    /// One step of xorshift64.
    fn below(&mut self, limit: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % limit as u64) as usize
    }

    /// A position in `text` that is safe to cut at.
    fn boundary(&mut self, text: &str) -> usize {
        let mut at = self.below(text.len() + 1);
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn range(&mut self, text: &str) -> (usize, usize) {
        let (a, b) = (self.boundary(text), self.boundary(text));
        (a.min(b), a.max(b))
    }
}

fn generate(target: &Target, random: &mut Random) -> String {
    let mut text = target.corpus[random.below(target.corpus.len())].to_string();
    for _ in 0..=random.below(MAX_EDITS) {
        match random.below(5) {
            0 => {
                let word = target.words[random.below(target.words.len())];
                text.insert_str(random.boundary(&text), word);
            }
            1 => text.insert(random.boundary(&text), SPECIAL[random.below(SPECIAL.len())]),
            2 => {
                let (from, to) = random.range(&text);
                text.replace_range(from..to, "");
            }
            3 => {
                let (from, to) = random.range(&text);
                let copy = text[from..to].to_string();
                text.insert_str(to, &copy);
            }
            _ => text.truncate(random.boundary(&text)),
        }
    }
    text
}

/// Runs inputs until `deadline`. Returns how many ran, or the input that
/// panicked.
fn search(target: &Target, seed: u64, deadline: Instant) -> Result<u64, String> {
    let mut random = Random::new(seed);
    let mut count = 0;
    while count % BATCH != 0 || Instant::now() < deadline {
        let input = generate(target, &mut random);
        if panic::catch_unwind(|| (target.check)(&input)).is_err() {
            return Err(input);
        }
        count += 1;
    }
    Ok(count)
}

fn parse(arguments: &[String]) -> Option<(u64, u64)> {
    let (seconds, seed) = match arguments {
        [seconds] => (seconds, None),
        [seconds, seed] => (seconds, Some(seed.parse().ok()?)),
        _ => return None,
    };
    let clock = || {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH);
        elapsed.map_or(1, |since| since.as_nanos() as u64)
    };
    Some((seconds.parse().ok()?, seed.unwrap_or_else(clock)))
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let Some((seconds, seed)) = parse(&arguments) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    println!("fuzz: seed {seed}");
    if run_all(&TARGETS, seconds, seed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Splits the time between the targets. False if any of them panicked.
fn run_all(targets: &[Target], seconds: u64, seed: u64) -> bool {
    let share = Duration::from_secs(seconds) / targets.len() as u32;
    for target in targets {
        match search(target, seed, Instant::now() + share) {
            Ok(count) => println!("fuzz: {} ran {count} inputs", target.name),
            Err(input) => {
                eprintln!(
                    "fuzz: {} panicked with seed {seed} on {input:?}",
                    target.name
                );
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const CLEAN: Target = Target {
        name: "clean",
        check: |_| {},
        corpus: &["a b", "é🦀"],
        words: &["#", "\""],
    };
    const PLANTED: Target = Target {
        name: "planted",
        check: |input| assert!(!input.contains("#\"")),
        ..CLEAN
    };

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(50)
    }

    #[test]
    fn a_planted_panic_is_found_and_its_input_returned() {
        let later = Instant::now() + Duration::from_secs(30);
        let input = search(&PLANTED, 7, later).unwrap_err();
        assert!(input.contains("#\""), "{input:?}");
    }

    #[test]
    fn the_same_seed_gives_the_same_inputs() {
        let run = |seed| {
            let mut random = Random::new(seed);
            (0..200)
                .map(|_| generate(&CLEAN, &mut random))
                .collect::<Vec<String>>()
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5), run(6));
    }

    #[test]
    fn a_zero_state_doesnt_stick() {
        let mut random = Random::new(0x9E37_79B9_7F4A_7C15);
        assert_ne!(random.below(1 << 30), random.below(1 << 30));
    }

    #[test]
    fn a_clean_target_runs_until_the_deadline() {
        let started = Instant::now();
        let count = search(&CLEAN, 1, soon()).unwrap();
        assert!(count >= BATCH, "{count}");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_passed_deadline_runs_nothing() {
        assert_eq!(search(&PLANTED, 7, Instant::now()), Ok(0));
    }

    #[test]
    fn inputs_vary() {
        let mut random = Random::new(3);
        let inputs: Vec<String> = (0..1000).map(|_| generate(&CLEAN, &mut random)).collect();
        let unique: std::collections::HashSet<&String> = inputs.iter().collect();
        assert!(unique.len() > 200, "{}", unique.len());
    }

    #[test]
    fn each_kind_of_edit_happens() {
        let target = Target {
            corpus: &["abcdefgh"],
            words: &["WORD"],
            ..CLEAN
        };
        let mut random = Random::new(3);
        let inputs: Vec<String> = (0..4000).map(|_| generate(&target, &mut random)).collect();
        let found = |test: &dyn Fn(&String) -> bool| inputs.iter().any(test);
        assert!(found(
            &|input| input.contains('W') && !input.starts_with('W')
        ));
        assert!(found(&|input| SPECIAL.iter().any(|c| input.contains(*c))));
        // Only a deletion takes out the middle and keeps both ends.
        assert!(found(&|input| input == "abgh"));
        // Only a copy repeats text with nothing new added.
        assert!(found(&|input| input == "abcabcdefgh"));
        assert!(found(&|input| input == "abc"));
    }

    #[test]
    fn the_device_corpus_reaches_the_queue() {
        let corpus = TARGETS[1].corpus;
        assert_eq!(device_input(corpus[1]), 1);
        assert_eq!(device_input(corpus[2]), 0);
    }

    #[test]
    fn the_fuse_corpus_reaches_the_files() {
        let corpus = TARGETS[2].corpus;
        assert_eq!(fuse_input(corpus[0]), 0);
        // INIT, the lookup, the open, the read, and the release.
        assert_eq!(fuse_input(corpus[1]), 5);
        assert_eq!(fuse_input(corpus[2]), 5);
        // Two lookups and their links, and nothing through a symlink.
        assert_eq!(fuse_input(corpus[3]), 4);
        // The lookup, and nothing for a node once it is forgotten.
        assert_eq!(fuse_input(corpus[4]), 1);
    }

    #[test]
    #[should_panic(expected = "reached outside the share")]
    fn an_answer_holding_the_outside_file_is_found() {
        // A root that holds the host file stands in for a server that
        // escaped it.
        let outside = fuse_tree().join("outside");
        let root = File::open(outside).unwrap();
        let mut server = share::Server::new(vec![share::Root::new(
            OwnedFd::from(root),
            share::Mode::ReadOnly,
        )])
        .unwrap();
        fuse_input_with(&mut server, "1 1 secret\n14 2 w:0 w:0\n15 0 1 0 w:4096\n");
    }

    #[test]
    fn the_fuse_rw_corpus_changes_files_but_nothing_protected() {
        let corpus = TARGETS[3].corpus;
        assert_eq!(fuse_rw_input(corpus[0]), 0);
        // INIT, the create, the write, the truncate, and the rest.
        assert_eq!(fuse_rw_input(corpus[1]), 10);
        // The four lookups alone: every change is refused.
        assert_eq!(fuse_rw_input(corpus[2]), 4);
        // The lookup, the open, both locks, and the flush.
        assert_eq!(fuse_rw_input(corpus[3]), 5);
    }

    #[test]
    fn a_root_the_guest_locked_itself_out_of_still_holds_what_is_protected() {
        // SETATTR of the root's mode to 0: SPEC.md lets the guest of an
        // `rw` share do it, and nothing protected changes.
        let mode = "4 1 w:1 w:0 0 0 0 0 0 0 w:0 w:0 w:0 w:0 w:0 w:0 w:0 w:0\n";
        assert_eq!(fuse_rw_input(mode), 1);
    }

    /// The body of a SETATTR of a node's mode to 0.
    const LOCK: &str = "w:1 w:0 0 0 0 0 0 0 w:0 w:0 w:0 w:0 w:0 w:0 w:0 w:0";

    #[test]
    fn a_tree_whose_modes_the_guest_took_away_is_removed() {
        let input = format!("1 1 hello.txt\n4 2 {LOCK}\n4 1 {LOCK}\n");
        let base = fuse_rw_base();
        assert_eq!(fuse_rw_input_at(&input, "root", &base), 3);
        assert!(!base.exists(), "the tree stayed");
    }

    #[test]
    fn a_tree_a_failing_input_locked_is_removed() {
        // A root at the tree stands in for a server that escaped it, so the
        // guest locks a file, the share's root, and the tree, and fails.
        let input = format!("1 1 root\n1 2 hello.txt\n4 3 {LOCK}\n4 2 {LOCK}\n4 1 {LOCK}\n");
        let base = fuse_rw_base();
        let run = panic::catch_unwind(|| fuse_rw_input_at(&input, ".", &base));
        let failure = run.expect_err("the input passed");
        let message = failure.downcast_ref::<String>().expect("a message");
        assert!(message.contains("outside/secret changed"), "{message}");
        assert!(!base.exists(), "the tree stayed");
    }

    #[test]
    #[should_panic(expected = "outside/secret changed")]
    fn a_change_outside_an_rw_share_is_found() {
        fuse_rw_input_at(
            "1 1 outside\n1 2 secret\n14 3 w:1 w:0\n16 0 1 0 w:3 w:0 0 w:0 w:0 bad\n",
            ".",
            &fuse_rw_base(),
        );
    }

    #[test]
    #[should_panic(expected = "outside's mode changed")]
    fn a_mode_change_outside_an_rw_share_is_found() {
        fuse_rw_input_at(
            "1 1 outside\n4 2 w:1 w:0 0 0 0 0 0 0 w:0 w:0 w:0 w:511 w:0 w:0 w:0 w:0\n",
            ".",
            &fuse_rw_base(),
        );
    }

    #[test]
    fn the_git_corpus_reads_as_git_reads_it() {
        let corpus = TARGETS[4].corpus;
        // An empty loose ref and object are refused, and an empty
        // packed-refs holds no refs.
        assert_eq!(git_input(corpus[0]), 1);
        // The loose ref, and as packed-refs it holds a bad line.
        assert_eq!(git_input(corpus[1]), 1);
        assert_eq!(git_input(corpus[2]), 1);
        // Only as packed-refs.
        assert_eq!(git_input(corpus[3]), 1);
        // Only as an object.
        assert_eq!(git_input(corpus[4]), 1);
        assert_eq!(git_input(corpus[5]), 1);
    }

    #[test]
    #[should_panic(expected = "crates/git")]
    fn a_ref_git_reads_differently_is_found() {
        let id = git::Id::from_hex(GIT_BLOB).unwrap();
        same_ref("planted", Ok(Some(id)), None);
    }

    #[test]
    fn the_diff_corpus_diffs_as_git_diffs() {
        let corpus = TARGETS[5].corpus;
        assert_eq!(diff_input(corpus[0]), 0);
        for input in &corpus[1..] {
            assert!(diff_input(input) > 0, "{input:?}");
        }
    }

    #[test]
    #[should_panic(expected = "differ from git's")]
    fn a_hunk_git_prints_differently_is_found() {
        same_hunks("planted", b"@@ -1 +1 @@\n-a\n+b\n", b"");
    }

    #[test]
    fn the_net_corpus_is_decided() {
        let corpus = TARGETS.iter().find(|t| t.name == "net").unwrap().corpus;
        assert_eq!(net_input(corpus[0]), 0);
        assert_eq!(net_input(corpus[1]), 3);
        assert_eq!(net_input(corpus[2]), 3);
    }

    #[test]
    fn paths_normalize_as_iri_string_normalizes_them() {
        for path in [
            "",
            "/",
            "/a/./b/../c",
            "/./b/../b/%63/%7bfoo%7d",
            "/a/.%2E/%2e%2e/b",
            "//x/../y",
            "/a//b/..",
            "/%41%2f%e2%82%ac",
            "/a b",
            "/é",
            "/%",
            "/%zz",
        ] {
            same_path(path, net::normalize_path(path).as_deref());
        }
    }

    #[test]
    #[should_panic(expected = "iri-string")]
    fn a_path_normalized_differently_is_found() {
        same_path("/a/../b", Some("/a/b"));
    }

    #[test]
    #[should_panic(expected = "iri-string")]
    fn a_broken_escape_with_a_normal_form_is_found() {
        same_path("/a%zz", Some("/a%zz"));
    }

    #[test]
    fn the_name_check_corpus_passes() {
        let target = TARGETS.iter().find(|t| t.name == "name-check").unwrap();
        for input in target.corpus {
            assert!(name_check_input(input), "{input:?}");
        }
        assert!(!name_check_input(&HELLO.replace("host.ap", "evil.ap")));
        assert!(!name_check_input(
            "GET / HTTP/1.1\r\nHost: example.com\r\n\r\n"
        ));
    }

    #[test]
    fn requests_end_where_hyper_ends_them() {
        let host = "Host: host.ap.internal\r\n";
        for (input, served_count) in [
            format!("POST / HTTP/1.1\r\n{host}Transfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET / HTTP/1.1\r\n{host}\r\n"),
            format!("POST / HTTP/1.1\r\n{host}Content-Length: 3\r\n\r\nabcGET / HTTP/1.1\r\n{host}\r\n"),
            format!("POST / HTTP/1.1\r\n{host}Content-Length: 0\r\n\r\nGET / HTTP/1.1\r\n{host}\r\n"),
            format!(
                "POST / HTTP/1.1\r\n{host}Transfer-Encoding: chunked\r\n\r\n3;a=b\r\nabc\r\n10\r\n{}\r\n0\r\nT: 1\r\n\r\nGET / HTTP/1.1\r\n{host}\r\n",
                "x".repeat(16),
            ),
            format!("\r\nGET / HTTP/1.1\r\n{host}\r\nGET /b HTTP/1.1\r\n{host}Connection: close\r\n\r\nGET / HTTP/1.1\r\n{host}\r\n"),
            format!("PUT / HTTP/1.0\r\n{host}Content-Length: 2\r\n\r\nab"),
        ]
        .into_iter()
        .zip([2, 2, 2, 2, 2, 1])
        {
            assert!(name_check_input(&input), "{input:?}");
            let served = served_requests(input.as_bytes()).0;
            assert_eq!(served.len(), served_count, "{input:?}");
        }
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_request_that_ends_elsewhere_is_found() {
        let passed = b"POST / HTTP/1.1\r\nHost: host.ap.internal\r\nContent-Length: 3\r\n\r\nabc";
        let mut ours = checked_requests(passed);
        assert_eq!(ours.len(), 1);
        ours[0].end -= 1;
        same_requests(passed, &ours);
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_request_that_names_another_host_is_found() {
        let passed = b"GET / HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n";
        let mut ours = checked_requests(passed);
        assert_eq!(ours.len(), 1);
        ours[0].hosts = vec!["g".to_owned()];
        same_requests(passed, &ours);
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_request_hyper_has_not_finished_is_found() {
        let passed = b"POST / HTTP/1.1\r\nHost: host.ap.internal\r\nContent-Length: 3\r\n\r\nab";
        let mut ours = checked_requests(passed);
        assert_eq!(ours, []);
        let hosts = vec!["host.ap.internal".to_owned()];
        let method = "POST".to_owned();
        ours.push(Framed {
            end: passed.len(),
            method,
            hosts,
        });
        same_requests(passed, &ours);
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_request_for_another_host_that_both_read_is_found() {
        let passed = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let hosts = vec!["example.com".to_owned()];
        let method = "GET".to_owned();
        same_requests(
            passed,
            &[Framed {
                end: passed.len(),
                method,
                hosts,
            }],
        );
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_body_hyper_refuses_is_found() {
        let passed = b"POST / HTTP/1.1\r\nHost: host.ap.internal\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        same_requests(passed, &[]);
    }

    #[test]
    #[should_panic(expected = "hyper")]
    fn a_request_hyper_reads_and_the_check_misses_is_found() {
        same_requests(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n", &[]);
    }

    #[test]
    fn the_request_check_corpus_passes() {
        let target = TARGETS.iter().find(|t| t.name == "request-check").unwrap();
        for input in target.corpus {
            assert!(request_check_input(input), "{input:?}");
        }
        for denied in [
            "GET /denied HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n",
            "GET /allowed/../denied HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n",
            "GET /allowed HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "SSH-2.0-OpenSSH_9.9\r\n",
        ] {
            assert!(!request_check_input(denied), "{denied:?}");
        }
    }

    #[test]
    fn the_dns_corpus_gets_answers() {
        let corpus = TARGETS[7].corpus;
        assert!(!dns_input(corpus[0]));
        for input in &corpus[1..] {
            assert!(dns_input(input), "{input:?}");
        }
    }

    #[test]
    fn device_input_ignores_lines_it_cant_read() {
        assert_eq!(device_input("mem x\nset 1\nget\nwhat\nmem 0x 1\n\nrun"), 0);
    }

    static RAN: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn a_clean_run_tries_inputs_and_passes() {
        let counting = Target {
            check: |_| {
                RAN.fetch_add(1, Ordering::Relaxed);
            },
            ..CLEAN
        };
        assert!(run_all(&[counting], 1, 1));
        assert!(RAN.load(Ordering::Relaxed) >= BATCH);
    }

    #[test]
    fn a_panic_fails_the_run() {
        assert!(!run_all(&[PLANTED], 30, 7));
    }

    #[test]
    fn usage_takes_seconds_and_maybe_a_seed() {
        let of = |words: &[&str]| parse(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>());
        assert_eq!(of(&["10", "4"]), Some((10, 4)));
        assert_eq!(of(&["10"]).map(|(seconds, _)| seconds), Some(10));
        assert_eq!(of(&[]), None);
        assert_eq!(of(&["ten"]), None);
        assert_eq!(of(&["10", "x"]), None);
        assert_eq!(of(&["1", "2", "3"]), None);
    }
}
