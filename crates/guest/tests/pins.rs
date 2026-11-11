//! What a finished program's background job keeps alive in the agent.
//!
//! This file holds one test, because it counts the threads and descriptors of
//! the whole process, and another test would change both.

use guest::agent::serve;
use guest::protocol::{ToGuest, ToHost};
use std::ffi::c_int;
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::time::{Duration, Instant};

const SIGKILL: c_int = 9;

unsafe extern "C" {
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

/// Leaves a job that holds both pipes open, and says its PID.
const SCRIPT: &str = "sleep 30 & echo $!";
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

impl Write for FakeChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.to_host.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs the script once, and returns the PID of the job it left.
fn run_once() -> c_int {
    let mut wire = Vec::new();
    ToGuest::Run {
        argv: [b"sh".as_slice(), b"-c", SCRIPT.as_bytes()]
            .map(<[u8]>::to_vec)
            .to_vec(),
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
    String::from_utf8(stdout).unwrap().trim().parse().unwrap()
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
    let before = counts();
    let jobs: Vec<c_int> = (0..COMMANDS).map(|_| run_once()).collect();
    let after = settled(before);
    let live = jobs
        .iter()
        .filter(|&&pid| fs::exists(format!("/proc/{pid}")).unwrap())
        .count();
    for pid in jobs {
        // SAFETY: this call takes no pointers. Each PID is a job this test
        // started, and still holds unless the job already ended.
        unsafe { kill(pid, SIGKILL) };
    }

    assert_eq!(live, COMMANDS, "every job outlived its command");
    assert_eq!(after, before, "(threads, descriptors)");
}
