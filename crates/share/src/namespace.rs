//! The user namespace of the server of a run's `rw` shares, which lets it
//! change the mode or times of a file it can't read, as the host's user
//! can. The server changes a mode only through a file it opened, because an
//! `O_PATH` descriptor reaches past every Landlock rule. So it opens the
//! file past its mode instead, and Landlock still checks that open.
//!
//! The namespace maps only the user and the group of the process that
//! starts the server, so the one capability the server keeps in it,
//! `CAP_DAC_READ_SEARCH`, reads and searches past the modes of the user's
//! own files alone: the kernel grants it only on a file whose owner and
//! group are both mapped. The server takes it up only to open a file whose
//! mode or times it changes, in [`past_modes`].

use std::ffi::CStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const CAP_DAC_READ_SEARCH: u32 = 1 << 2;
/// `_LINUX_CAPABILITY_VERSION_3`, which reads two `struct
/// __user_cap_data_struct`s.
const VERSION_3: u32 = 0x2008_0522;
const SETGROUPS: &CStr = c"/proc/self/setgroups";
const UID_MAP: &CStr = c"/proc/self/uid_map";
const GID_MAP: &CStr = c"/proc/self/gid_map";

/// `struct __user_cap_header_struct`.
#[repr(C)]
struct Header {
    version: u32,
    pid: i32,
}

/// A header of its own for each call, which the kernel may write to.
fn header() -> Header {
    Header {
        version: VERSION_3,
        pid: 0,
    }
}

/// `struct __user_cap_data_struct`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn get() -> io::Result<[Data; 2]> {
    let mut header = header();
    let mut data = [Data::default(); 2];
    // SAFETY: the kernel reads the header and writes two `Data`s, which
    // outlive the call.
    match unsafe { libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr()) } {
        0 => Ok(data),
        _ => Err(io::Error::last_os_error()),
    }
}

fn set(data: &[Data; 2]) -> io::Result<()> {
    let mut header = header();
    // SAFETY: the kernel reads the header and two `Data`s, which outlive
    // the call.
    match unsafe { libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Starts `command`, the server of a run's `rw` shares, in a user
/// namespace of its own, holding `CAP_DAC_READ_SEARCH` there. Where the
/// host makes no such namespace for this user, it starts the server
/// without one, and the server changes the mode only of a file it can
/// open. A process running as root holds the capability already.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    // SAFETY: both calls take nothing, and can't fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if uid == 0 {
        return command.spawn();
    }
    let uid_line = format!("{uid} {uid} 1");
    let gid_line = format!("{gid} {gid} 1");
    let entering = Arc::new(AtomicBool::new(true));
    let enter = Arc::clone(&entering);
    // SAFETY: the closure runs in the forked child, and makes only system
    // calls, on memory made before the fork.
    unsafe {
        command.pre_exec(move || match enter.load(Ordering::Relaxed) {
            true => enter_namespace(uid_line.as_bytes(), gid_line.as_bytes()),
            false => Ok(()),
        });
    }
    match command.spawn() {
        // A namespace the host made, but with no ids or no capability,
        // can't be left, so the server starts again outside one.
        Err(_) => {
            entering.store(false, Ordering::Relaxed);
            command.spawn()
        }
        started => started,
    }
}

/// Enters a new user namespace that maps `uid_line` and `gid_line`, and
/// keeps `CAP_DAC_READ_SEARCH` there, as an ambient capability, so the
/// program this process runs next holds it too. A host that makes no
/// namespace leaves this process as it was.
fn enter_namespace(uid_line: &[u8], gid_line: &[u8]) -> io::Result<()> {
    // SAFETY: this call takes a flag.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Ok(());
    }
    write_proc(SETGROUPS, b"deny")?;
    write_proc(UID_MAP, uid_line)?;
    write_proc(GID_MAP, gid_line)?;
    let kept = Data {
        effective: CAP_DAC_READ_SEARCH,
        permitted: CAP_DAC_READ_SEARCH,
        inheritable: CAP_DAC_READ_SEARCH,
    };
    set(&[kept, Data::default()])?;
    let raise = libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong;
    let capability = libc::c_ulong::from(CAP_DAC_READ_SEARCH.trailing_zeros());
    // SAFETY: this call takes plain integers. The kernel refuses it unless
    // the last two are zero in all their bits.
    match unsafe { libc::prctl(libc::PR_CAP_AMBIENT, raise, capability, 0_u64, 0_u64) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn write_proc(path: &CStr, line: &[u8]) -> io::Result<()> {
    // SAFETY: `path` and `line` outlive the calls, and the descriptor is
    // closed once written.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, line.as_ptr().cast(), line.len());
        let error = io::Error::last_os_error();
        libc::close(fd);
        match usize::try_from(written) {
            Ok(written) if written == line.len() => Ok(()),
            Ok(_) => Err(io::Error::from_raw_os_error(libc::EIO)),
            Err(_) => Err(error),
        }
    }
}

/// Stops reading and searching past modes, which the server does only in
/// [`past_modes`], and lets no program it might start do so.
pub fn lower() -> io::Result<()> {
    let mut data = get()?;
    // A server outside the namespace holds nothing to lower, and may run
    // where it can't set its capabilities at all.
    if (data[0].effective | data[0].inheritable) & CAP_DAC_READ_SEARCH == 0 {
        return Ok(());
    }
    data[0].effective &= !CAP_DAC_READ_SEARCH;
    data[0].inheritable &= !CAP_DAC_READ_SEARCH;
    set(&data)
}

/// Runs `open` reading and searching past the modes of the user's own
/// files, if the server holds the capability to, or returns `None` if it
/// doesn't. Landlock still holds what `open` opens to the roots.
pub fn past_modes<T>(open: impl FnOnce() -> T) -> Option<T> {
    let mut data = get().ok()?;
    if data[0].permitted & CAP_DAC_READ_SEARCH == 0 {
        return None;
    }
    data[0].effective |= CAP_DAC_READ_SEARCH;
    set(&data).ok()?;
    let opened = open();
    data[0].effective &= !CAP_DAC_READ_SEARCH;
    // Lowering what was just raised can't fail. If it did, the server
    // would go on reading past modes, so it stops.
    if set(&data).is_err() {
        std::process::abort();
    }
    Some(opened)
}
