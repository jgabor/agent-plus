//! The guest agent against a fake channel, running real programs.

use guest::agent::{self, Split};
use guest::protocol::{MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tests run on threads of one process, and a running `serve` reaps any child
/// of it, so they take turns.
fn serve(channel: &mut impl Split) -> Result<(), ProtocolError> {
    static TURN: Mutex<()> = Mutex::new(());
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    agent::serve(channel, None)
}

/// Fails the test instead of the machine when a program floods the channel.
const MAX_RECORDED: usize = 16 * 1024 * 1024;

/// The host's end: what it sent, and what the guest said back.
struct FakeChannel {
    from_host: Cursor<Vec<u8>>,
    to_host: Vec<u8>,
}

impl FakeChannel {
    fn sending(requests: &[ToGuest]) -> FakeChannel {
        let mut wire = Vec::new();
        for request in requests {
            request.write(&mut wire).unwrap();
        }
        FakeChannel::with_bytes(wire)
    }

    fn with_bytes(from_host: Vec<u8>) -> FakeChannel {
        FakeChannel {
            from_host: Cursor::new(from_host),
            to_host: Vec::new(),
        }
    }

    fn reports(&self) -> Vec<ToHost> {
        let mut wire = Cursor::new(&self.to_host);
        let mut reports = Vec::new();
        while let Some(report) = ToHost::read(&mut wire).unwrap() {
            reports.push(report);
        }
        reports
    }
}

impl Read for FakeChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.from_host.read(buffer)
    }
}

impl Split for FakeChannel {}

impl Write for FakeChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.to_host.len() + bytes.len() > MAX_RECORDED {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.to_host.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn request(argv: &[&str]) -> ToGuest {
    ToGuest::Run {
        argv: argv.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
    }
}

fn run(argv: &[&str]) -> Vec<ToHost> {
    let mut channel = FakeChannel::sending(&[request(argv)]);
    serve(&mut channel).unwrap();
    channel.reports()
}

fn shell(script: &str) -> Vec<ToHost> {
    run(&["sh", "-c", script])
}

fn stdout(reports: &[ToHost]) -> Vec<u8> {
    let chunks = reports.iter().filter_map(|report| match report {
        ToHost::Stdout(bytes) => Some(bytes.as_slice()),
        _ => None,
    });
    chunks.flatten().copied().collect()
}

fn stderr(reports: &[ToHost]) -> Vec<u8> {
    let chunks = reports.iter().filter_map(|report| match report {
        ToHost::Stderr(bytes) => Some(bytes.as_slice()),
        _ => None,
    });
    chunks.flatten().copied().collect()
}

/// The status, which must be the last report and the only one of its kind.
fn status(reports: &[ToHost]) -> u8 {
    let exits = reports
        .iter()
        .filter(|report| matches!(report, ToHost::Exited(_)))
        .count();
    assert_eq!(exits, 1, "{reports:?}");
    match reports.last() {
        Some(ToHost::Exited(status)) => *status,
        other => panic!("the last report was {other:?}"),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn stdout_and_stderr_stay_separate() {
    let reports = shell("printf out; printf err >&2; printf more; printf again >&2");
    assert_eq!(stdout(&reports), b"outmore");
    assert_eq!(stderr(&reports), b"erragain");
    assert_eq!(status(&reports), 0);
}

#[test]
fn a_program_that_succeeds_reports_0() {
    assert_eq!(status(&shell("exit 0")), 0);
}

#[test]
fn a_program_that_fails_reports_its_status() {
    assert_eq!(status(&shell("exit 2")), 2);
    assert_eq!(status(&shell("exit 255")), 255);
}

#[test]
fn a_file_that_cannot_be_executed_reports_126() {
    let file = scratch("not-executable");
    fs::write(&file, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(run(&[file.to_str().unwrap()]), [ToHost::Exited(126)]);
}

#[test]
fn a_directory_reports_126() {
    let dir = scratch("a-directory");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(run(&[dir.to_str().unwrap()]), [ToHost::Exited(126)]);
}

#[test]
fn a_missing_program_reports_127() {
    assert_eq!(run(&["/no/such/program"]), [ToHost::Exited(127)]);
    assert_eq!(run(&["no-such-program-on-path"]), [ToHost::Exited(127)]);
    assert_eq!(run(&[""]), [ToHost::Exited(127)]);
}

#[test]
fn a_signal_reports_128_plus_its_number() {
    assert_eq!(status(&shell("kill -TERM $$")), 128 + 15);
    assert_eq!(status(&shell("kill -KILL $$")), 128 + 9);
}

#[test]
fn arguments_arrive_as_exact_bytes() {
    let mut channel = FakeChannel::sending(&[ToGuest::Run {
        argv: vec![
            b"sh".to_vec(),
            b"-c".to_vec(),
            b"printf %s \"$1\"".to_vec(),
            b"name".to_vec(),
            b"a \xff b".to_vec(),
        ],
    }]);
    serve(&mut channel).unwrap();
    assert_eq!(stdout(&channel.reports()), b"a \xff b");
}

#[test]
fn large_output_arrives_whole_in_chunks_that_fit() {
    let reports = shell("head -c 1000000 /dev/zero; head -c 1000000 /dev/zero >&2");
    assert_eq!(stdout(&reports).len(), 1_000_000);
    assert_eq!(stderr(&reports).len(), 1_000_000);
    assert_eq!(status(&reports), 0);
    for report in &reports {
        if let ToHost::Stdout(bytes) | ToHost::Stderr(bytes) = report {
            assert!(bytes.len() <= MAX_OUTPUT);
            assert!(!bytes.is_empty());
        }
    }
}

#[test]
fn the_guest_gives_the_program_no_input() {
    let reports = shell("cat; echo done");
    assert_eq!(stdout(&reports), b"done\n");
}

/// A background job can hold the pipes open for as long as it likes, and the
/// program may be quiet for a while before it ends.
#[test]
fn a_background_job_does_not_hold_back_the_status() {
    for script in [
        "printf out; sleep 30 & exit 3",
        "printf out; sleep 30 & sleep 0.2; exit 3",
    ] {
        let started = Instant::now();
        let reports = shell(script);
        assert!(started.elapsed() < Duration::from_secs(15), "{script}");
        assert_eq!(stdout(&reports), b"out", "{script}");
        assert_eq!(status(&reports), 3, "{script}");
    }
}

/// Neither a flood nor a trickle from a background job may delay the status.
#[test]
fn a_background_job_that_keeps_writing_does_not_hold_back_the_status() {
    for script in [
        "yes &\nexit 7",
        "while echo x; do sleep 0.01; done &\nexit 7",
    ] {
        let started = Instant::now();
        let reports = shell(script);
        assert!(started.elapsed() < Duration::from_secs(15), "{script}");
        assert_eq!(status(&reports), 7, "{script}");
    }
}

/// A host that reads slowly must not cost a finished program its output.
#[test]
fn a_slow_host_still_gets_all_of_a_finished_programs_output() {
    struct SlowHost(FakeChannel);

    impl Read for SlowHost {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer)
        }
    }

    impl Split for SlowHost {}

    impl Write for SlowHost {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(30));
            self.0.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let script = "head -c 60000 /dev/zero; head -c 60000 /dev/zero >&2";
    let mut channel = SlowHost(FakeChannel::sending(&[request(&["sh", "-c", script])]));
    serve(&mut channel).unwrap();
    let reports = channel.0.reports();
    assert_eq!(stdout(&reports).len(), 60_000);
    assert_eq!(stderr(&reports).len(), 60_000);
    assert_eq!(status(&reports), 0);
}

#[test]
fn requests_run_one_after_another() {
    let mut channel = FakeChannel::sending(&[request(&["sh", "-c", "exit 4"]), request(&["true"])]);
    serve(&mut channel).unwrap();
    assert_eq!(channel.reports(), [ToHost::Exited(4), ToHost::Exited(0)]);
}

#[test]
fn a_host_that_hangs_up_ends_the_agent_cleanly() {
    let mut channel = FakeChannel::sending(&[]);
    serve(&mut channel).unwrap();
    assert!(channel.reports().is_empty());
}

#[test]
fn an_unknown_frame_is_refused_and_runs_nothing_after_it() {
    let marker = scratch("after-unknown");
    let _ = fs::remove_file(&marker);
    let mut wire = vec![1, 0, 0, 0, 0x7F];
    request(&["touch", marker.to_str().unwrap()])
        .write(&mut wire)
        .unwrap();
    let mut channel = FakeChannel::with_bytes(wire);
    assert!(matches!(
        serve(&mut channel),
        Err(ProtocolError::UnknownTag(0x7F))
    ));
    assert!(channel.reports().is_empty());
    assert!(!marker.exists());
}

#[test]
fn an_oversized_frame_is_refused() {
    let mut channel = FakeChannel::with_bytes(u32::MAX.to_le_bytes().to_vec());
    assert!(matches!(
        serve(&mut channel),
        Err(ProtocolError::TooLarge(_))
    ));
    assert!(channel.reports().is_empty());
}

/// Writes fail from the first byte, as they do once the host has gone.
struct DeadHost(Cursor<Vec<u8>>);

impl Read for DeadHost {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Split for DeadHost {}

impl Write for DeadHost {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_host_that_stops_listening_is_an_error() {
    let mut wire = Vec::new();
    request(&["sh", "-c", "exit 0"]).write(&mut wire).unwrap();
    let mut channel = DeadHost(Cursor::new(wire));
    assert!(matches!(serve(&mut channel), Err(ProtocolError::Io(_))));
}
