//! Orphans of a running program, reaped while it runs.
//!
//! This file holds one test. While a program runs, `serve` reaps any child of
//! the process, so a second test here could have its children taken.

use guest::System;
use guest::agent::{Split, serve};
use guest::linux::Linux;
use guest::protocol::{ToGuest, ToHost};
use std::ffi::{c_int, c_ulong};
use std::io::{self, Cursor, Read, Write};
use std::path::Path;

const PR_SET_CHILD_SUBREAPER: c_int = 36;

unsafe extern "C" {
    fn prctl(option: c_int, arg: c_ulong, ...) -> c_int;
}

/// Leaves two jobs that exit only once their parent has, so both are always
/// orphans. Then waits for them to go, for up to 5 seconds. Exits 5 if they
/// went while it ran, and 9 if either was left.
const SCRIPT: &str = r#"
pids=$(sh -c '
    for _ in 1 2; do
        (while kill -0 $$ 2>/dev/null; do sleep 0.01; done) >/dev/null 2>&1 &
        echo $!
    done
')
echo $pids
for pid in $pids; do
    n=0
    while [ -e /proc/$pid ] && [ $n -lt 500 ]; do sleep 0.01; n=$((n + 1)); done
    [ -e /proc/$pid ] && exit 9
done
exit 5
"#;

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

#[test]
fn orphans_of_a_running_program_are_reaped_and_its_status_still_arrives() {
    // PID 1 adopts orphans on its own. A test process has to ask.
    let result = unsafe { prctl(PR_SET_CHILD_SUBREAPER, 1) };
    assert_eq!(result, 0, "{}", io::Error::last_os_error());

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

    let mut reports = Vec::new();
    let mut frames = channel.to_host.as_slice();
    while let Some(report) = ToHost::read(&mut frames).unwrap() {
        reports.push(report);
    }
    let Some(ToHost::Exited(status)) = reports.pop() else {
        panic!("the last report was not the status: {reports:?}");
    };
    assert_eq!(status, 5);
    let stdout: Vec<u8> = reports
        .into_iter()
        .flat_map(|report| match report {
            ToHost::Stdout(bytes) => bytes,
            other => panic!("unexpected report {other:?}"),
        })
        .collect();
    let pids: Vec<i32> = String::from_utf8(stdout)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "zombie {pid}");
    }
    assert_eq!(Linux.reap().unwrap(), None);
}
