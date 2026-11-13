//! Children that run at once, each waited for on a thread of its own, as the
//! harness and a command beside it are.
//!
//! This file holds one test. While a child that `spawn` started runs, the
//! reaper collects any child of the process, so a second test here could
//! have its children taken.

use guest::linux::spawn;
use std::process::Command;

#[test]
fn each_child_gets_its_own_status_whichever_ends_first() {
    let (_, slow) = spawn(Command::new("sh").args(["-c", "sleep 0.5; exit 3"])).unwrap();
    let (_, fast) = spawn(Command::new("sh").args(["-c", "exit 5"])).unwrap();
    // The fast child ends while the reaper may be waiting for the slow one.
    assert_eq!(fast.recv().unwrap().unwrap().code(), Some(5));
    let (_, again) = spawn(Command::new("sh").args(["-c", "exit 7"])).unwrap();
    assert_eq!(again.recv().unwrap().unwrap().code(), Some(7));
    assert_eq!(slow.recv().unwrap().unwrap().code(), Some(3));
}
