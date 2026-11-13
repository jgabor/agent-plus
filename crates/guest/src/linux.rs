//! The real system calls.

use crate::protocol::Size;
use crate::{MOUNTS, Mount, System, root};
use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_short, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::{env, fs, io, ptr};

const WNOHANG: c_int = 1;
const ECHILD: i32 = 10;
/// A restart, which ends the VM. `RB_POWER_OFF` would only halt the CPU,
/// because the kernel has no ACPI, and the host would never find out.
const RB_AUTOBOOT: c_int = 0x0123_4567;
const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const HOST_CID: u32 = 2;
const POLLIN: c_short = 1;
const FIONREAD: c_ulong = 0x541B;
const FOREVER: c_int = -1;
const MS_RDONLY: c_ulong = 1;
const MS_MOVE: c_ulong = 8192;
const MS_NOSUID: c_ulong = 2;
const MS_NOEXEC: c_ulong = 8;
const O_RDWR: c_int = 2;
const O_NOCTTY: c_int = 0o400;
const O_CLOEXEC: c_int = 0o2000000;
const TIOCSCTTY: c_ulong = 0x540E;
const TIOCSWINSZ: c_ulong = 0x5414;
// _IOW('T', 0x31, int)
const TIOCSPTLCK: c_ulong = 0x4004_5431;
// _IO('T', 0x41)
const TIOCGPTPEER: c_ulong = 0x5441;
/// Where terminals live. The guest mounts them on first use.
const TERMINALS: &str = "/dev/pts";
const COMMAND_LINE: &str = "/proc/cmdline";
/// Where the init stacks the layers, in the initramfs. Once the stack is the
/// root, nothing in the guest can reach these.
const LAYERS: &str = "/layers";
const UPPER: &str = "/upper";
const STACK: &str = "/stack";

/// `struct sockaddr_vm`.
#[repr(C)]
struct VsockAddress {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    flags: u8,
    zero: [u8; 3],
}

/// `struct winsize`.
#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

/// `struct pollfd`.
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn setsid() -> c_int;
    fn sync();
    fn reboot(command: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const VsockAddress, len: u32) -> c_int;
}

/// The running Linux kernel.
pub struct Linux;

impl System for Linux {
    fn mount(&mut self, wanted: &Mount) -> io::Result<()> {
        // The initramfs holds only the init, so mount points may not exist.
        match fs::create_dir(path(wanted.target)) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        // SAFETY: every pointer comes from a `CStr` that outlives the call.
        checked(unsafe {
            mount(
                wanted.source.as_ptr(),
                wanted.target.as_ptr(),
                wanted.fstype.as_ptr(),
                wanted.flags,
                wanted.data.as_ptr().cast(),
            )
        })?;
        Ok(())
    }

    fn stack_layers(&mut self) -> io::Result<()> {
        let command_line = fs::read_to_string(COMMAND_LINE)?;
        let order = root::order(&command_line);
        if order.is_empty() {
            return Ok(());
        }
        let devices = root::devices(&order, &disks()?).map_err(|name| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no disk serves layer {name}"),
            )
        })?;
        let mut lower = Vec::new();
        for (index, device) in devices.iter().enumerate() {
            let target = format!("{LAYERS}/{index}");
            fs::create_dir_all(&target)?;
            mount_with(&format!("/dev/{device}"), &target, "erofs", MS_RDONLY, "")?;
            lower.push(target);
        }
        // The writable layer ends with the machine, so it lives in memory.
        fs::create_dir_all(UPPER)?;
        mount_with("tmpfs", UPPER, "tmpfs", 0, "mode=0755")?;
        let (upper, work) = (format!("{UPPER}/upper"), format!("{UPPER}/work"));
        fs::create_dir(&upper)?;
        fs::create_dir(&work)?;
        fs::create_dir_all(STACK)?;
        let options = root::overlay_options(&lower, &upper, &work);
        mount_with("overlay", STACK, "overlay", 0, &options)?;
        for mount in &MOUNTS {
            let target = path(mount.target);
            let moved = Path::new(STACK).join(target.strip_prefix("/").unwrap_or(target));
            match fs::create_dir(&moved) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
            mount_with(
                &target.to_string_lossy(),
                &moved.to_string_lossy(),
                "",
                MS_MOVE,
                "",
            )?;
        }
        // The initramfs is the kernel's first root, which can't be pivoted
        // away from, so the stack is moved over it instead.
        env::set_current_dir(STACK)?;
        mount_with(".", "/", "", MS_MOVE, "")?;
        unix_fs::chroot(".")?;
        env::set_current_dir("/")
    }

    fn reap(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: a null status pointer asks the kernel not to write one.
        match unsafe { waitpid(-1, ptr::null_mut(), WNOHANG) } {
            0 => Ok(None),
            -1 => {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ECHILD) {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
            pid => Ok(Some(pid)),
        }
    }

    fn power_off(&mut self) -> io::Result<()> {
        // SAFETY: neither call takes a pointer.
        unsafe {
            sync();
            reboot(RB_AUTOBOOT);
        }
        Err(io::Error::last_os_error())
    }
}

/// Reaps children until `pid` ends, and returns its status. Any other child
/// that ends first is reaped too, which is how orphans go while a program runs.
pub fn reap_until(pid: u32) -> io::Result<ExitStatus> {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `waitpid` writes one `int` through this pointer, which is
        // valid for the whole call.
        match unsafe { waitpid(-1, &mut status, 0) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            reaped if reaped as u32 == pid => return Ok(ExitStatus::from_raw(status)),
            _ => {}
        }
    }
}

/// Waits until `pipe` can be read without blocking, which includes having
/// closed, or until `ended` closes. Returns whether `ended` has closed.
pub fn program_has_ended(pipe: BorrowedFd, ended: BorrowedFd) -> io::Result<bool> {
    let mut fds = [waiting_on(pipe), waiting_on(ended)];
    poll_retrying(&mut fds)?;
    Ok(fds[1].revents != 0)
}

/// How many bytes `pipe` holds, which can be more than its nominal size.
pub fn bytes_waiting(pipe: BorrowedFd) -> io::Result<usize> {
    let mut count: c_int = 0;
    // SAFETY: `FIONREAD` writes one `int` through this pointer, which is valid
    // for the whole call.
    checked(unsafe { ioctl(pipe.as_raw_fd(), FIONREAD, &mut count as *mut c_int) })?;
    Ok(count as usize)
}

fn waiting_on(fd: BorrowedFd) -> PollFd {
    PollFd {
        fd: fd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    }
}

fn poll_retrying(fds: &mut [PollFd]) -> io::Result<()> {
    loop {
        // SAFETY: the pointer and count describe `fds`, which outlives the call.
        let polled = unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, FOREVER) };
        let Err(error) = checked(polled) else {
            return Ok(());
        };
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Connects to `port` on the host over vsock.
pub fn connect_to_host(port: u32) -> io::Result<File> {
    // SAFETY: this call takes no pointers.
    let fd = checked(unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) })?;
    // SAFETY: `socket` returned a new descriptor that nothing else owns.
    let stream = unsafe { File::from_raw_fd(fd) };
    let address = VsockAddress {
        family: AF_VSOCK as u16,
        reserved: 0,
        port,
        cid: HOST_CID,
        flags: 0,
        zero: [0; 3],
    };
    let length = size_of::<VsockAddress>() as u32;
    // SAFETY: `address` outlives the call, and `length` is its size.
    checked(unsafe { connect(fd, &address, length) })?;
    Ok(stream)
}

/// Opens a new terminal of this size. Returns its controlling side, which
/// stands in for the screen and the keyboard, and the side a program runs
/// in.
pub fn open_terminal(size: Size) -> io::Result<(File, File)> {
    if !Path::new(TERMINALS).join("ptmx").exists() {
        fs::create_dir_all(TERMINALS)?;
        mount_with(
            "devpts",
            TERMINALS,
            "devpts",
            MS_NOSUID | MS_NOEXEC,
            "mode=0620,ptmxmode=0666",
        )?;
    }
    let controller = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open("/dev/ptmx")?;
    let unlocked: c_int = 0;
    // SAFETY: `TIOCSPTLCK` reads one `int` through this pointer, which is
    // valid for the whole call.
    checked(unsafe {
        ioctl(
            controller.as_raw_fd(),
            TIOCSPTLCK,
            &unlocked as *const c_int,
        )
    })?;
    resize(controller.as_fd(), size)?;
    // SAFETY: `TIOCGPTPEER` takes open flags, not a pointer.
    let fd = checked(unsafe {
        ioctl(
            controller.as_raw_fd(),
            TIOCGPTPEER,
            (O_RDWR | O_NOCTTY | O_CLOEXEC) as c_ulong,
        )
    })?;
    // SAFETY: the call returned a new descriptor that nothing else owns.
    let terminal = unsafe { File::from_raw_fd(fd) };
    Ok((controller, terminal))
}

/// Sets the size of the terminal that `terminal` is either side of. The
/// program in it gets `SIGWINCH`.
pub fn resize(terminal: BorrowedFd, size: Size) -> io::Result<()> {
    let window = WindowSize {
        rows: size.rows,
        columns: size.columns,
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one `struct winsize` through this pointer,
    // which is valid for the whole call.
    checked(unsafe {
        ioctl(
            terminal.as_raw_fd(),
            TIOCSWINSZ,
            &window as *const WindowSize,
        )
    })?;
    Ok(())
}

/// Starts a session whose controlling terminal is standard input, as a
/// login does. It only makes system calls, so it can run between fork and
/// exec too.
pub fn take_terminal() -> io::Result<()> {
    // SAFETY: neither call takes a pointer.
    unsafe {
        checked(setsid())?;
        checked(ioctl(0, TIOCSCTTY, 0 as c_ulong))?;
    }
    Ok(())
}

/// Every virtio-blk disk, with its serial.
fn disks() -> io::Result<Vec<(String, String)>> {
    let mut disks = Vec::new();
    for entry in fs::read_dir("/sys/block")? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with("vd") {
            continue;
        }
        let serial = fs::read_to_string(format!("/sys/block/{name}/serial"))?;
        disks.push((name, serial.trim().to_owned()));
    }
    Ok(disks)
}

/// `mount(2)` with arguments made at run time. An empty type or data passes
/// none.
fn mount_with(
    source: &str,
    target: &str,
    fstype: &str,
    flags: c_ulong,
    data: &str,
) -> io::Result<()> {
    let text = |value: &str| CString::new(value).map_err(io::Error::other);
    let (source, target) = (text(source)?, text(target)?);
    let (fstype, data) = (text(fstype)?, text(data)?);
    let or_null = |value: &CString| match value.is_empty() {
        true => ptr::null(),
        false => value.as_ptr(),
    };
    // SAFETY: every pointer is null or comes from a `CString` that outlives
    // the call.
    checked(unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            or_null(&fstype),
            flags,
            or_null(&data).cast(),
        )
    })?;
    Ok(())
}

/// A call's result, or its error when it returns -1, as these calls do.
fn checked(result: c_int) -> io::Result<c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn path(target: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(target.to_bytes()))
}
