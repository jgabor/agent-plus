//! The real system calls.

use crate::protocol::Size;
use crate::{MOUNTS, Mount, System, net, root};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_short, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{env, fs, io, mem, ptr, thread};

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
const MS_NODEV: c_ulong = 4;
const MS_REMOUNT: c_ulong = 32;
const MS_BIND: c_ulong = 4096;
const MNT_DETACH: c_int = 2;
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
/// The options of the tmpfs that holds the writable layer and `/tmp`.
const WRITABLE: &str = "mode=0755,size=256m";
const TMP: &str = "/tmp";

const AF_INET: c_int = 2;
const AF_INET6: c_int = 10;
const SOCK_DGRAM: c_int = 2;
const SOCK_CLOEXEC: c_int = 0o2000000;
const SIOCADDRT: c_ulong = 0x890b;
const SIOCGIFFLAGS: c_ulong = 0x8913;
const SIOCSIFFLAGS: c_ulong = 0x8914;
const SIOCSIFADDR: c_ulong = 0x8916;
const SIOCSIFNETMASK: c_ulong = 0x891c;
const SIOCGIFINDEX: c_ulong = 0x8933;
const IFF_UP: c_short = 1;
const RTF_UP: u16 = 1;
const RTF_GATEWAY: u16 = 2;
const RESOLVER: &str = "/etc/resolv.conf";
/// How long the link may take to come up, and how often the init looks.
const LINK_WAIT: Duration = Duration::from_secs(5);
const LINK_POLL: Duration = Duration::from_millis(1);

/// `struct ifreq`, with its union as bytes.
#[repr(C)]
struct InterfaceRequest {
    name: [u8; 16],
    data: [u8; 24],
}

/// `struct rtentry`, with each `struct sockaddr` as bytes.
#[repr(C)]
struct RouteEntry {
    pad1: c_ulong,
    destination: [u8; 16],
    gateway: [u8; 16],
    mask: [u8; 16],
    flags: u16,
    pad2: c_short,
    pad3: c_ulong,
    pad4: *mut c_void,
    metric: c_short,
    device: *const c_char,
    mtu: c_ulong,
    window: c_ulong,
    irtt: u16,
}

/// `struct in6_ifreq`.
#[repr(C)]
struct Ipv6Request {
    address: [u8; 16],
    prefix: u32,
    index: c_int,
}

/// `struct in6_rtmsg`.
#[repr(C)]
struct Ipv6Route {
    destination: [u8; 16],
    source: [u8; 16],
    gateway: [u8; 16],
    kind: u32,
    destination_len: u16,
    source_len: u16,
    metric: u32,
    info: c_ulong,
    flags: u32,
    index: c_int,
}

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
    fn umount2(target: *const c_char, flags: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn setsid() -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
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
        // `/tmp` lives beside it, so the two share one limit, and what the
        // agent writes can take only half the machine's memory.
        fs::create_dir_all(UPPER)?;
        mount_with("tmpfs", UPPER, "tmpfs", 0, WRITABLE)?;
        let (upper, work) = (format!("{UPPER}/upper"), format!("{UPPER}/work"));
        fs::create_dir(&upper)?;
        fs::create_dir(&work)?;
        let tmp = format!("{UPPER}/tmp");
        fs::create_dir(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o1777))?;
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
            if target == Path::new(TMP) {
                let unmounted = CString::new(TMP).map_err(io::Error::other)?;
                // SAFETY: the path is a `CString` that outlives the call.
                checked(unsafe { umount2(unmounted.as_ptr(), 0) })?;
                let moved = moved.to_string_lossy();
                mount_with(&tmp, &moved, "", MS_BIND, "")?;
                // A bind keeps none of the flags of the mount it comes from.
                mount_with("", &moved, "", MS_REMOUNT | MS_BIND | mount.flags, "")?;
                continue;
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

    fn set_up_link(&mut self) -> io::Result<()> {
        if !Path::new("/sys/class/net").join(net::INTERFACE).exists() {
            return Ok(());
        }
        // The guest is alone on its link, so no address can be taken, and
        // IPv6 needn't wait to find out.
        let dad = format!("/proc/sys/net/ipv6/conf/{}/accept_dad", net::INTERFACE);
        fs::write(dad, "0")?;
        let ipv4 = datagram_socket(AF_INET)?;
        let mut request = interface_request();
        ioctl_on(&ipv4, SIOCGIFFLAGS, &mut request)?;
        let mut flags = [request.data[0], request.data[1]];
        flags = (c_short::from_ne_bytes(flags) | IFF_UP).to_ne_bytes();
        request.data[..2].copy_from_slice(&flags);
        ioctl_on(&ipv4, SIOCSIFFLAGS, &mut request)?;

        let mut request = interface_request();
        request.data[..16].copy_from_slice(&ipv4_address(net::IPV4));
        ioctl_on(&ipv4, SIOCSIFADDR, &mut request)?;
        request.data[..16].copy_from_slice(&ipv4_address(net::ipv4_netmask()));
        ioctl_on(&ipv4, SIOCSIFNETMASK, &mut request)?;
        let device = CString::new(net::INTERFACE).map_err(io::Error::other)?;
        let mut route = RouteEntry {
            pad1: 0,
            destination: ipv4_address(Ipv4Addr::UNSPECIFIED),
            gateway: ipv4_address(net::IPV4_GATEWAY),
            mask: ipv4_address(Ipv4Addr::UNSPECIFIED),
            flags: RTF_UP | RTF_GATEWAY,
            pad2: 0,
            pad3: 0,
            pad4: ptr::null_mut(),
            metric: 0,
            device: device.as_ptr(),
            mtu: 0,
            window: 0,
            irtt: 0,
        };
        ioctl_on(&ipv4, SIOCADDRT, &mut route)?;

        let ipv6 = datagram_socket(AF_INET6)?;
        let mut request = interface_request();
        ioctl_on(&ipv6, SIOCGIFINDEX, &mut request)?;
        let index = c_int::from_ne_bytes(request.data[..4].try_into().unwrap_or_default());
        let mut address = Ipv6Request {
            address: net::IPV6.octets(),
            prefix: u32::from(net::IPV6_PREFIX),
            index,
        };
        ioctl_on(&ipv6, SIOCSIFADDR, &mut address)?;
        let mut route = Ipv6Route {
            destination: Ipv6Addr::UNSPECIFIED.octets(),
            source: Ipv6Addr::UNSPECIFIED.octets(),
            gateway: net::IPV6_GATEWAY.octets(),
            kind: 0,
            destination_len: 0,
            source_len: 0,
            metric: 0,
            info: 0,
            flags: u32::from(RTF_UP | RTF_GATEWAY),
            index,
        };
        ioctl_on(&ipv6, SIOCADDRT, &mut route)?;
        wait_for_ipv6()?;

        // A layer's file may be a link to a resolver the guest doesn't run.
        let _ = fs::remove_file(RESOLVER);
        fs::create_dir_all("/etc")?;
        fs::write(RESOLVER, net::resolv_conf())
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

/// The children [`spawn`] started that haven't ended, each with where its
/// status goes, and whether a thread is reaping.
struct Children {
    waiting: BTreeMap<u32, SyncSender<io::Result<ExitStatus>>>,
    reaping: bool,
}

static CHILDREN: Mutex<Children> = Mutex::new(Children {
    waiting: BTreeMap::new(),
    reaping: false,
});

fn children() -> MutexGuard<'static, Children> {
    CHILDREN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts `command`, and returns it with where its status arrives.
///
/// While a child started here runs, one thread reaps every child of the
/// process, and hands each of these its own status. Any other child it
/// reaps is an orphan, which is how orphans go while a program runs.
pub fn spawn(command: &mut Command) -> io::Result<(Child, Receiver<io::Result<ExitStatus>>)> {
    // Held from before the fork until the child is listed, so the reaper
    // can't take the child's status before it knows where to send it.
    let mut children = children();
    let child = command.spawn()?;
    let (sender, status) = sync_channel(1);
    children.waiting.insert(child.id(), sender);
    if !children.reaping {
        children.reaping = true;
        thread::spawn(reap);
    }
    Ok((child, status))
}

/// Reaps children until none that [`spawn`] started is left. If the process
/// has no child left at all, each that is still listed gets the error.
fn reap() {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `waitpid` writes one `int` through this pointer, which is
        // valid for the whole call.
        let reaped = unsafe { waitpid(-1, &mut status, 0) };
        let error = (reaped == -1).then(io::Error::last_os_error);
        let mut children = children();
        match error {
            None => {
                if let Some(sender) = children.waiting.remove(&(reaped as u32)) {
                    let _ = sender.send(Ok(ExitStatus::from_raw(status)));
                }
            }
            Some(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Some(error) => {
                let code = error.raw_os_error().unwrap_or(ECHILD);
                for (_, sender) in mem::take(&mut children.waiting) {
                    let _ = sender.send(Err(io::Error::from_raw_os_error(code)));
                }
            }
        }
        if children.waiting.is_empty() {
            children.reaping = false;
            return;
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

/// Whether `fd` can be read without blocking, which includes having closed,
/// within `milliseconds`.
pub fn readable_within(fd: BorrowedFd, milliseconds: c_int) -> io::Result<bool> {
    let mut fds = [waiting_on(fd)];
    loop {
        // SAFETY: the pointer and count describe `fds`, which outlives the call.
        let polled = unsafe { poll(fds.as_mut_ptr(), 1, milliseconds) };
        match checked(polled) {
            Ok(ready) => return Ok(ready != 0),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
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
/// Mounts the share whose device has `tag` at `target`, which it creates.
/// Its files keep their modes, but the guest makes no device of a device
/// file, and runs no program as its owner.
pub fn mount_share(tag: &str, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let target = target
        .to_str()
        .ok_or_else(|| io::Error::other("a share's guest path isn't text"))?;
    mount_with(tag, target, "virtiofs", MS_NOSUID | MS_NODEV, "")
}

/// Mounts the one file `name` of the share whose device has `tag` at
/// `target`, which it creates if it is missing, as [`mount_share`] mounts a
/// directory. The share's root is mounted only on the way, so nothing in
/// the guest can reach it.
pub fn mount_file_share(tag: &str, name: &OsStr, target: &Path) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if !target.exists() {
        File::create(target)?;
    }
    let text = |path: &Path| {
        (path.to_str().map(str::to_owned))
            .ok_or_else(|| io::Error::other("a share's guest path isn't text"))
    };
    // SPEC.md reserves `/run`, so no share is mounted there.
    let stage = format!("/run/{tag}");
    let file = text(&Path::new(&stage).join(name))?;
    let target = text(target)?;
    fs::create_dir_all(&stage)?;
    mount_with(tag, &stage, "virtiofs", MS_NOSUID | MS_NODEV, "")?;
    let bound = mount_with(&file, &target, "", MS_BIND, "").and_then(|()| {
        // A bind keeps none of the flags of the mount it comes from.
        let flags = MS_REMOUNT | MS_BIND | MS_NOSUID | MS_NODEV;
        mount_with("", &target, "", flags, "")
    });
    let staged = CString::new(stage.as_str()).map_err(io::Error::other)?;
    // SAFETY: the path is a `CString` that outlives the call.
    checked(unsafe { umount2(staged.as_ptr(), MNT_DETACH) })?;
    fs::remove_dir(&stage)?;
    bound
}

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

/// Sends the process `pid` `SIGTERM`.
pub fn terminate(pid: u32) -> io::Result<()> {
    const SIGTERM: c_int = 15;
    let pid = c_int::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `kill` takes no pointer.
    checked(unsafe { kill(pid, SIGTERM) })?;
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

/// Waits until the link's IPv6 address can be used. The kernel finishes
/// bringing the link up on a worker of its own, and until then the address
/// is tentative, and nothing can send from it.
fn wait_for_ipv6() -> io::Result<()> {
    let started = Instant::now();
    while net::tentative(&fs::read_to_string("/proc/net/if_inet6")?) != Some(false) {
        if started.elapsed() > LINK_WAIT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the link's IPv6 address never became usable",
            ));
        }
        thread::sleep(LINK_POLL);
    }
    Ok(())
}

/// A socket of `family` to configure the link through.
fn datagram_socket(family: c_int) -> io::Result<File> {
    // SAFETY: socket takes no pointers.
    let fd = checked(unsafe { socket(family, SOCK_DGRAM | SOCK_CLOEXEC, 0) })?;
    // SAFETY: the kernel just returned this descriptor, and nothing else owns
    // it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// A `struct ifreq` that names the link's interface.
fn interface_request() -> InterfaceRequest {
    let mut request = InterfaceRequest {
        name: [0; 16],
        data: [0; 24],
    };
    request.name[..net::INTERFACE.len()].copy_from_slice(net::INTERFACE.as_bytes());
    request
}

/// A `struct sockaddr_in` for `address`, with no port.
fn ipv4_address(address: Ipv4Addr) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    bytes[4..8].copy_from_slice(&address.octets());
    bytes
}

/// An `ioctl` on `socket` whose argument is a `T` that it may read and
/// write.
fn ioctl_on<T>(socket: &File, request: c_ulong, argument: &mut T) -> io::Result<()> {
    // SAFETY: each request this is given takes a pointer to the structure
    // that `T` declares, which lives across the call. Any pointer inside it
    // is null or points to memory the caller keeps alive.
    checked(unsafe { ioctl(socket.as_raw_fd(), request, ptr::from_mut(argument)) })?;
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
