//! Feeds generated text to the code that reads untrusted input, and stops at
//! the first panic. It uses only the standard library, so it builds on stable
//! and adds no C to the tree. A run is reproduced by passing its seed again.

use std::env;
use std::panic;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Network rules and device input join this list when they exist.
const TARGETS: [Target; 1] = [Target {
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
        "FROM", "SHARE", "SYSTEM", "PROMPT", "ENV", "--", "--force", "\"\"", "\\\"", "\\\\", " rw",
    ],
}];

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
