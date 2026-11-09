//! Real orphans, reaped by the real system call.
//!
//! This file holds one test. `reap` collects any child of the process, so a
//! second test here could have its children taken.

use guest::linux::Linux;
use guest::{Channel, Event, Mount, System, run};
use std::ffi::{c_int, c_ulong, c_void};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

const PR_SET_CHILD_SUBREAPER: c_int = 36;
const P_PID: c_int = 1;
const WEXITED: c_int = 4;
const WNOWAIT: c_int = 0x0100_0000;

unsafe extern "C" {
    fn prctl(option: c_int, arg: c_ulong, ...) -> c_int;
    fn waitid(idtype: c_int, id: u32, info: *mut c_void, options: c_int) -> c_int;
}

/// Leaves two grandchildren whose parent has exited, and returns their PIDs.
///
/// They hold until the parent is gone. A shell reaps a background job that
/// exits while the shell runs, and that one would never be orphaned.
fn orphans() -> Vec<i32> {
    let script = "exec 3<&0; for _ in 1 2; do cat <&3 >/dev/null & echo $!; done";
    let mut shell = Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let release = shell.stdin.take().unwrap();
    let mut pids = String::new();
    shell
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut pids)
        .unwrap();
    assert!(shell.wait().unwrap().success());
    drop(release);
    pids.lines().map(|line| line.parse().unwrap()).collect()
}

/// Blocks until `pid` has exited, and leaves it for `reap` to collect.
fn wait_without_reaping(pid: i32) {
    let mut info = [0u64; 16];
    let result = unsafe {
        waitid(
            P_PID,
            pid as u32,
            info.as_mut_ptr().cast(),
            WEXITED | WNOWAIT,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

/// Real reaping, with the calls a test can't make faked.
struct RealReap(Vec<i32>);

impl System for RealReap {
    fn mount(&mut self, _mount: &Mount) -> std::io::Result<()> {
        Ok(())
    }

    fn reap(&mut self) -> std::io::Result<Option<i32>> {
        let pid = Linux.reap()?;
        self.0.extend(pid);
        Ok(pid)
    }

    fn power_off(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct OrphansExit {
    pids: Vec<i32>,
    told: bool,
}

impl Channel for OrphansExit {
    fn next_event(&mut self) -> std::io::Result<Event> {
        if self.told {
            return Ok(Event::PowerOff);
        }
        self.told = true;
        self.pids.iter().for_each(|&pid| wait_without_reaping(pid));
        Ok(Event::ChildExited)
    }
}

#[test]
fn orphaned_children_are_reaped_and_leave_no_zombie() {
    // PID 1 adopts orphans on its own. A test process has to ask.
    let result = unsafe { prctl(PR_SET_CHILD_SUBREAPER, 1) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());

    let pids = orphans();
    assert_eq!(pids.len(), 2);
    let mut system = RealReap(Vec::new());
    let mut channel = OrphansExit {
        pids: pids.clone(),
        told: false,
    };

    run(&mut system, &mut channel).unwrap();

    let mut reaped = system.0;
    reaped.sort_unstable();
    let mut expected = pids.clone();
    expected.sort_unstable();
    assert_eq!(reaped, expected);
    for pid in pids {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "zombie {pid}");
    }
    assert_eq!(Linux.reap().unwrap(), None);
}
