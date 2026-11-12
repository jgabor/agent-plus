//! The guest's init: the work of PID 1 inside the microVM.

pub mod agent;
pub mod linux;
pub mod protocol;
pub mod root;

use std::ffi::{CStr, c_ulong};
use std::io;

const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;

/// One filesystem the guest needs before anything else can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mount {
    pub source: &'static CStr,
    pub target: &'static CStr,
    pub fstype: &'static CStr,
    pub flags: c_ulong,
    pub data: &'static CStr,
}

/// What the guest mounts, in order.
pub const MOUNTS: [Mount; 4] = [
    Mount {
        source: c"proc",
        target: c"/proc",
        fstype: c"proc",
        flags: MS_NOSUID | MS_NODEV | MS_NOEXEC,
        data: c"",
    },
    Mount {
        source: c"sysfs",
        target: c"/sys",
        fstype: c"sysfs",
        flags: MS_NOSUID | MS_NODEV | MS_NOEXEC,
        data: c"",
    },
    Mount {
        source: c"devtmpfs",
        target: c"/dev",
        fstype: c"devtmpfs",
        flags: MS_NOSUID,
        data: c"mode=0755",
    },
    Mount {
        source: c"tmpfs",
        target: c"/tmp",
        fstype: c"tmpfs",
        flags: MS_NOSUID | MS_NODEV,
        data: c"mode=1777",
    },
];

/// What wakes the init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// At least one child changed state. Signals coalesce, so there may be
    /// more than one to reap.
    ChildExited,
    /// The host asked the machine to power off.
    PowerOff,
}

/// The init's connection to the host.
pub trait Channel {
    /// Blocks until something happens.
    fn next_event(&mut self) -> io::Result<Event>;
}

/// The system calls the init makes.
pub trait System {
    fn mount(&mut self, mount: &Mount) -> io::Result<()>;
    /// Makes the agent's layers the root, if the host attached any, and moves
    /// the filesystems in [`MOUNTS`] into it.
    fn stack_layers(&mut self) -> io::Result<()>;
    /// Collects one exited child and returns its PID, or `None` if no child
    /// has exited.
    fn reap(&mut self) -> io::Result<Option<i32>>;
    /// Does not return on success.
    fn power_off(&mut self) -> io::Result<()>;
}

/// Runs the init until the machine powers off.
///
/// Every failure powers off too, because PID 1 exiting would panic the
/// kernel. Returns only if the power-off itself returns.
pub fn run(system: &mut impl System, channel: &mut impl Channel) -> io::Result<()> {
    let result = serve(system, channel);
    system.power_off()?;
    result
}

fn serve(system: &mut impl System, channel: &mut impl Channel) -> io::Result<()> {
    for mount in &MOUNTS {
        system.mount(mount)?;
    }
    system.stack_layers()?;
    loop {
        match channel.next_event()? {
            Event::ChildExited => while system.reap()?.is_some() {},
            Event::PowerOff => return Ok(()),
        }
    }
}
