//! The init against a fake host channel and fake system calls.

use guest::{Channel, Event, MOUNTS, Mount, System, run};
use std::collections::VecDeque;
use std::io;

#[derive(Default)]
struct FakeSystem {
    log: Vec<String>,
    exited: VecDeque<i32>,
    mount_fails: bool,
    reap_fails: bool,
}

impl System for FakeSystem {
    fn mount(&mut self, mount: &Mount) -> io::Result<()> {
        if self.mount_fails {
            return Err(io::Error::from_raw_os_error(1));
        }
        self.log
            .push(format!("mount {}", mount.target.to_str().unwrap()));
        Ok(())
    }

    fn reap(&mut self) -> io::Result<Option<i32>> {
        if self.reap_fails {
            return Err(io::Error::from_raw_os_error(10));
        }
        let pid = self.exited.pop_front();
        if let Some(pid) = pid {
            self.log.push(format!("reap {pid}"));
        }
        Ok(pid)
    }

    fn power_off(&mut self) -> io::Result<()> {
        self.log.push("power off".to_string());
        Ok(())
    }
}

struct FakeChannel {
    events: VecDeque<io::Result<Event>>,
    reads: usize,
}

impl FakeChannel {
    fn new(events: Vec<io::Result<Event>>) -> FakeChannel {
        FakeChannel {
            events: events.into(),
            reads: 0,
        }
    }
}

impl Channel for FakeChannel {
    fn next_event(&mut self) -> io::Result<Event> {
        self.reads += 1;
        self.events.pop_front().expect("the init read past the end")
    }
}

const MOUNTED: [&str; 4] = ["mount /proc", "mount /sys", "mount /dev", "mount /tmp"];

#[test]
fn the_four_filesystems_are_mounted_before_the_host_is_heard() {
    let mut system = FakeSystem::default();
    let mut channel = FakeChannel::new(vec![Ok(Event::PowerOff)]);

    run(&mut system, &mut channel).unwrap();

    assert_eq!(system.log[..4], MOUNTED);
}

#[test]
fn each_filesystem_has_its_kernel_type() {
    let types: Vec<_> = MOUNTS.iter().map(|m| m.fstype.to_str().unwrap()).collect();
    assert_eq!(types, ["proc", "sysfs", "devtmpfs", "tmpfs"]);
}

#[test]
fn a_power_off_request_powers_off_and_stops_listening() {
    let mut system = FakeSystem::default();
    let mut channel = FakeChannel::new(vec![Ok(Event::PowerOff), Ok(Event::ChildExited)]);

    run(&mut system, &mut channel).unwrap();

    assert_eq!(system.log.last().unwrap(), "power off");
    assert_eq!(channel.reads, 1);
}

#[test]
fn one_wake_reaps_every_exited_child() {
    let mut system = FakeSystem {
        exited: [10, 11, 12].into(),
        ..FakeSystem::default()
    };
    let mut channel = FakeChannel::new(vec![Ok(Event::ChildExited), Ok(Event::PowerOff)]);

    run(&mut system, &mut channel).unwrap();

    assert_eq!(
        system.log[4..],
        ["reap 10", "reap 11", "reap 12", "power off"]
    );
}

#[test]
fn a_failed_mount_powers_off_without_hearing_the_host() {
    let mut system = FakeSystem {
        mount_fails: true,
        ..FakeSystem::default()
    };
    let mut channel = FakeChannel::new(vec![]);

    let error = run(&mut system, &mut channel).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(1));
    assert_eq!(system.log, ["power off"]);
    assert_eq!(channel.reads, 0);
}

#[test]
fn a_lost_channel_powers_off() {
    let mut system = FakeSystem::default();
    let mut channel = FakeChannel::new(vec![Err(io::ErrorKind::BrokenPipe.into())]);

    let error = run(&mut system, &mut channel).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(system.log.last().unwrap(), "power off");
}

#[test]
fn a_failed_reap_powers_off() {
    let mut system = FakeSystem {
        reap_fails: true,
        ..FakeSystem::default()
    };
    let mut channel = FakeChannel::new(vec![Ok(Event::ChildExited)]);

    let error = run(&mut system, &mut channel).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(10));
    assert_eq!(system.log.last().unwrap(), "power off");
}
