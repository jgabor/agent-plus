//! What a finished program's background job keeps alive in the agent.
//!
//! This file holds one test, because it counts the threads and descriptors of
//! the whole process, and another test would change both.

use guest::agent::{Split, serve};
use guest::protocol::{ToGuest, ToHost};
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Leaves a job that holds both pipes open until the file `$1` exists.
const SCRIPT: &str = "(while [ ! -e \"$1\" ]; do sleep 0.05; done) & echo started";
const COMMANDS: usize = 8;
/// How long a finished command's threads may take to exit.
const SETTLE: Duration = Duration::from_secs(5);

struct FakeChannel {
    from_host: Cursor<Vec<u8>>,
    to_host: Vec<u8>,
}

impl Read for FakeChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.from_host.read(buffer)
    }
}

impl Split for FakeChannel {}

impl Write for FakeChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.to_host.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Ends the jobs when dropped, even if the test fails first.
struct Stop(PathBuf);

impl Drop for Stop {
    fn drop(&mut self) {
        fs::write(&self.0, "").unwrap();
    }
}

/// Runs the script once, and leaves its job waiting for `stop`.
fn run_once(stop: &Path) {
    let mut wire = Vec::new();
    ToGuest::Run {
        argv: vec![
            b"sh".to_vec(),
            b"-c".to_vec(),
            SCRIPT.as_bytes().to_vec(),
            b"sh".to_vec(),
            stop.to_str().unwrap().as_bytes().to_vec(),
        ],
    }
    .write(&mut wire)
    .unwrap();
    let mut channel = FakeChannel {
        from_host: Cursor::new(wire),
        to_host: Vec::new(),
    };
    serve(&mut channel).unwrap();

    let mut stdout = Vec::new();
    let mut frames = channel.to_host.as_slice();
    while let Some(report) = ToHost::read(&mut frames).unwrap() {
        match report {
            ToHost::Stdout(bytes) => stdout.extend(bytes),
            ToHost::Exited(status) => assert_eq!(status, 0),
            other => panic!("unexpected report {other:?}"),
        }
    }
    assert_eq!(stdout, b"started\n");
}

fn entries(dir: &str) -> usize {
    fs::read_dir(dir).unwrap().count()
}

fn counts() -> (usize, usize) {
    (entries("/proc/self/task"), entries("/proc/self/fd"))
}

/// Threads and descriptors, once they are down to `wanted` or `SETTLE` runs
/// out. A finished command's threads may still be exiting.
fn settled(wanted: (usize, usize)) -> (usize, usize) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let now = counts();
        if (now.0 <= wanted.0 && now.1 <= wanted.1) || Instant::now() > deadline {
            return now;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn background_jobs_of_finished_commands_pin_no_threads_or_descriptors() {
    // Each run gets its own file, so a rerun can't strand the last one's jobs.
    let name = format!("pins-stop-{}", std::process::id());
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_file(&path);
    let stop = Stop(path);

    let before = counts();
    for _ in 0..COMMANDS {
        run_once(&stop.0);
    }
    let after = settled(before);
    assert_eq!(after, before, "(threads, descriptors)");
}
