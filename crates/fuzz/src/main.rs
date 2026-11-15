//! Feeds generated text to the code that reads untrusted input, and stops at
//! the first panic. It uses only the standard library, so it builds on stable
//! and adds no C to the tree. A run is reproduced by passing its seed again.

use std::env;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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

/// Network rules join this list when they exist.
const TARGETS: [Target; 4] = [
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
];

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
    let mut server = share::Server::new(vec![(OwnedFd::from(root), share::Mode::ReadOnly)])
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
/// own. Panics as it does, and also if anything outside the root, or
/// anything [`PROTECTED`] in it, changed, or if anything appeared beside
/// the root.
fn fuse_rw_input(input: &str) -> usize {
    fuse_rw_input_at(input, "root")
}

/// [`fuse_rw_input`] with the share's root at `served` in the tree, which
/// a test moves to stand in for a server that escaped it.
fn fuse_rw_input_at(input: &str, served: &str) -> usize {
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/fuzz-share-rw")
        .join(format!("{}-{run}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
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
    let mut server = share::Server::new(vec![(
        OwnedFd::from(File::open(base.join(served)).expect("the fuzz root")),
        share::Mode::ReadWrite,
    )])
    .expect("the share server");
    let answered = fuse_answers(&mut server, input);
    for name in PROTECTED {
        let expected = if name == PROTECTED[0] { CANARY } else { INSIDE };
        let found = fs::read(base.join(name)).ok();
        assert_eq!(
            found.as_deref(),
            Some(expected),
            "{name} changed: {input:?}"
        );
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
        names(&base),
        ["outside", "root"],
        "a file beside the root: {input:?}"
    );
    assert_eq!(
        names(&base.join("outside")),
        ["secret"],
        "a file outside: {input:?}"
    );
    let _ = fs::remove_dir_all(&base);
    answered
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
        let mut server =
            share::Server::new(vec![(OwnedFd::from(root), share::Mode::ReadOnly)]).unwrap();
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
    #[should_panic(expected = "outside/secret changed")]
    fn a_change_outside_an_rw_share_is_found() {
        fuse_rw_input_at(
            "1 1 outside\n1 2 secret\n14 3 w:1 w:0\n16 0 1 0 w:3 w:0 0 w:0 w:0 bad\n",
            ".",
        );
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
