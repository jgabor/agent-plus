//! Feeds generated text to the code that reads untrusted input, and stops at
//! the first panic. It uses only the standard library, so it builds on stable
//! and adds no C to the tree. A run is reproduced by passing its seed again.

use std::env;
use std::panic;
use std::process::ExitCode;
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
const TARGETS: [Target; 2] = [
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
                "set 0x44 1 4\n",
                "mem 0x1000 0x4000\nmem 0x1008 0x1000100000010\n",
                "mem 0x1010 0x5000\nmem 0x1018 0x200000040\n",
                "mem 0x2000 0x10000\nrun\n",
            ),
            // A descriptor that names itself.
            concat!(
                "set 0x38 8 4\nset 0x80 0x1000 4\nset 0x90 0x2000 4\nset 0xa0 0x3000 4\n",
                "set 0x44 1 4\nmem 0x1000 0x4000\nmem 0x1008 0x100000010\n",
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
        let queue = device.queue(index).expect("the queue exists");
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
