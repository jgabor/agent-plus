//! Boots a machine: the host's side in [`boot`], and the guest's in [`init`].

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use guest::linux::{self, Linux};
use guest::protocol::{MAX_BODY, PORT, ToHost};
use guest::{Channel, Event, agent};
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::vsock::{Connection, Vsock};
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, boot as loader};

use crate::fetch::{self, Client};
use crate::store::Store;

const KVM: &str = "/dev/kvm";
const INIT_PATH: &str = "/init";
const MEMORY: usize = 256 << 20;
const DEVICE_ADDRESS: u64 = 0xd000_0000;
const DEVICE_SIZE: u64 = 0x1000;
const LINE: u32 = 5;
const COMMAND_LINE: &str = "console=ttyS0 reboot=t virtio_mmio.device=4K@0xd0000000:5";
const GUEST_CID: u64 = 3;
const RELEASE_FILE: &str = "/proc/sys/kernel/osrelease";
const HEADER_SIZE: usize = 4;

const TIMED_OUT_STATUS: u8 = 124;
const FAILED_STATUS: u8 = 125;

const ELF_MAGIC: &[u8] = b"\x7fELF";
const PT_INTERP: u32 = 3;
const PROGRAM_HEADER_MIN: usize = 56;

/// What a machine boots.
pub struct Guest<'a> {
    /// An uncompressed `vmlinux`.
    pub kernel: &'a [u8],
    /// A static program, which runs as `/init`.
    pub init: &'a [u8],
    /// More files for the guest's root filesystem.
    pub files: &'a [Entry<'a>],
    /// How long the guest may run, from entry to power-off.
    pub timeout: Duration,
}

/// Why a machine didn't boot and power off cleanly.
#[derive(Debug)]
pub enum Error {
    /// This `ap` can't run as the guest's init.
    DynamicallyLinked,
    /// `/dev/kvm` can't be opened.
    Kvm(io::Error),
    /// The guest was still running when the timeout passed.
    TimedOut,
    /// Anything else, described for the user.
    Failed(String),
}

impl Error {
    /// The status `ap` exits with.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::TimedOut => TIMED_OUT_STATUS,
            _ => FAILED_STATUS,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::DynamicallyLinked => write!(
                f,
                "this ap is dynamically linked, so it can't run as the guest's init; \
                 use a static build for x86_64-unknown-linux-musl"
            ),
            Error::Kvm(error) => write!(f, "can't open {KVM}: {error}"),
            Error::TimedOut => write!(f, "the guest didn't finish before the timeout"),
            Error::Failed(reason) => f.write_str(reason),
        }
    }
}

/// Whether a process with this PID and `argv[0]` is the guest's init.
pub fn is_init(pid: u32, program: Option<&OsStr>) -> bool {
    pid == 1 && program == Some(OsStr::new(INIT_PATH))
}

/// Whether this file is an ELF program the kernel can start with nothing else
/// in the guest. A dynamic program names its loader in a `PT_INTERP` header.
pub fn is_static(file: &[u8]) -> bool {
    interpreter_free(file).unwrap_or(false)
}

fn interpreter_free(file: &[u8]) -> Option<bool> {
    if !file.starts_with(ELF_MAGIC) {
        return None;
    }
    let offset = u64::from_le_bytes(file.get(32..40)?.try_into().ok()?) as usize;
    let size = u16::from_le_bytes(file.get(54..56)?.try_into().ok()?) as usize;
    let count = u16::from_le_bytes(file.get(56..58)?.try_into().ok()?) as usize;
    if size < PROGRAM_HEADER_MIN {
        return None;
    }
    for index in 0..count {
        let start = offset.checked_add(index.checked_mul(size)?)?;
        let header = file.get(start..start.checked_add(size)?)?;
        if u32::from_le_bytes(header.get(..4)?.try_into().ok()?) == PT_INTERP {
            return Some(false);
        }
    }
    Some(true)
}

/// Checks that this device can be opened for reading and writing, as a VM
/// needs.
pub fn check_kvm(device: &Path) -> Result<(), Error> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(drop)
        .map_err(Error::Kvm)
}

/// `ap boot`: the pinned kernel, with this program as its init. Prints the
/// guest's kernel release as soon as the init reports it.
pub fn boot_bare(timeout: Duration) -> Result<(), Error> {
    let init = fs::read("/proc/self/exe").map_err(|error| failed("can't read ap itself", error))?;
    if !is_static(&init) {
        return Err(Error::DynamicallyLinked);
    }
    check_kvm(Path::new(KVM))?;

    let store = Store::from_env().map_err(|error| failed("can't find the store", error))?;
    let path = fetch::fetch(&store, &Client::new(), &fetch::kernel())
        .map_err(|error| failed("can't get the kernel", error))?;
    let kernel = fs::read(path).map_err(|error| failed("can't read the kernel", error))?;

    let files = [Entry::Symlink {
        path: "usr/bin/ap",
        target: INIT_PATH,
    }];
    let guest = Guest {
        kernel: &kernel,
        init: &init,
        files: &files,
        timeout,
    };
    boot(&guest, |release| {
        // A closed stdout shouldn't take the VM down with a panic.
        let _ = writeln!(io::stdout(), "{release}");
    })
}

/// Boots `guest`, calls `on_ready` with the kernel release its init reports,
/// and returns once the machine has powered off.
///
/// The VM lives inside this call, on this thread. When it returns, for any
/// reason, the VM is gone.
pub fn boot(guest: &Guest, on_ready: impl FnMut(&str)) -> Result<(), Error> {
    let mut entries = vec![Entry::File {
        path: "init",
        mode: 0o755,
        contents: guest.init,
    }];
    entries.extend_from_slice(guest.files);
    let archive = initramfs::build(&entries)
        .map_err(|error| Error::Failed(format!("can't build the initramfs: {error:?}")))?;

    let vm_failed = |error: io::Error| failed("can't start the VM", error);
    let mut vm = Vm::new(GuestMemory::new(MEMORY).map_err(vm_failed)?).map_err(vm_failed)?;
    vm.add_interrupt_controllers().map_err(vm_failed)?;
    let bad_kernel = |error: loader::Error| Error::Failed(format!("bad kernel: {error:?}"));
    let entry = loader::load(vm.memory(), guest.kernel, COMMAND_LINE).map_err(bad_kernel)?;
    loader::load_initramfs(vm.memory(), &entry, &archive).map_err(bad_kernel)?;
    let mut vcpu = vm.create_vcpu().map_err(vm_failed)?;
    vm.expose_cpu_features(&vcpu).map_err(vm_failed)?;
    loader::enter(&vcpu, &entry).map_err(vm_failed)?;

    let mut machine = Machine {
        serial: Serial::new(),
        vsock: Vsock::new(GUEST_CID, &[PORT]),
        memory: vm.memory(),
        interrupt: vm.interrupt(LINE),
        connection: None,
        frame: Vec::new(),
        answered: false,
        ready: false,
        failure: None,
        on_ready,
    };
    let stop = vcpu
        .run_ports(guest.timeout, &mut machine)
        .map_err(vm_failed)?;

    match (stop, machine.failure, machine.ready) {
        (Stop::TimedOut, ..) => Err(Error::TimedOut),
        (_, Some(reason), _) => Err(Error::Failed(reason)),
        (Stop::Shutdown, None, true) => Ok(()),
        (Stop::Shutdown, None, false) => Err(Error::Failed(
            "the guest powered off before its init reported ready".to_owned(),
        )),
        (stop, None, _) => Err(Error::Failed(format!(
            "the guest stopped unexpectedly: {stop:?}"
        ))),
    }
}

fn failed(context: &str, error: io::Error) -> Error {
    Error::Failed(format!("{context}: {error}"))
}

/// The serial port, the vsock device, and the host's end of the channel. It
/// all runs on the vCPU thread, during the guest's exits.
struct Machine<'vm, F> {
    serial: Serial,
    vsock: Vsock,
    memory: &'vm GuestMemory,
    interrupt: Interrupt<'vm>,
    connection: Option<Connection>,
    frame: Vec<u8>,
    answered: bool,
    ready: bool,
    failure: Option<String>,
    on_ready: F,
}

impl<F: FnMut(&str)> Machine<'_, F> {
    /// Reads the guest's first frame, and hangs up, which powers it off.
    fn run_host(&mut self) {
        if self.connection.is_none() {
            self.connection = self.vsock.accept();
        }
        let Some(connection) = self.connection else {
            return;
        };
        if self.answered {
            return;
        }
        let mut buffer = [0; 4096];
        while !frame_is_complete(&self.frame) {
            let len = self.vsock.recv(connection, &mut buffer);
            if len == 0 {
                return;
            }
            self.frame.extend_from_slice(&buffer[..len]);
        }
        self.answered = true;
        match release_in(&self.frame) {
            Ok(release) => {
                self.ready = true;
                (self.on_ready)(&release);
            }
            Err(reason) => self.failure = Some(reason),
        }
        self.vsock.close(connection);
    }
}

/// Whether `bytes` hold a whole frame, or a length that can't be one. The
/// guest chooses the length, so nothing waits for more than a frame can hold.
fn frame_is_complete(bytes: &[u8]) -> bool {
    let Some(header) = bytes.first_chunk::<HEADER_SIZE>() else {
        return false;
    };
    let length = u32::from_le_bytes(*header) as usize;
    length > MAX_BODY || bytes.len() >= HEADER_SIZE + length
}

/// The release in a guest's `Ready` frame. It reaches the user's terminal, so
/// it must hold only printable characters.
fn release_in(mut frame: &[u8]) -> Result<String, String> {
    match ToHost::read(&mut frame) {
        Ok(Some(ToHost::Ready(release)))
            if !release.is_empty() && release.iter().all(u8::is_ascii_graphic) =>
        {
            Ok(String::from_utf8_lossy(&release).into_owned())
        }
        Ok(Some(ToHost::Ready(_))) => Err("the guest reported an invalid release".to_owned()),
        _ => Err("the guest's init didn't report ready".to_owned()),
    }
}

impl<F: FnMut(&str)> Ports for Machine<'_, F> {
    fn write(&mut self, port: u16, data: &[u8]) {
        self.serial.write(port, data);
    }

    fn read(&mut self, port: u16, data: &mut [u8]) -> bool {
        self.serial.read(port, data)
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) {
        let Some(offset) = device_offset(address) else {
            return;
        };
        let mut interrupt = self.vsock.write(self.memory, offset, data);
        self.run_host();
        interrupt |= self.vsock.service(self.memory);
        if interrupt {
            // A failed pulse leaves the guest waiting, and the timeout ends it.
            let _ = self.interrupt.pulse();
        }
    }

    fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        match device_offset(address) {
            Some(offset) => self.vsock.read(offset, data),
            None => data.fill(0xff),
        }
    }
}

fn device_offset(address: u64) -> Option<u64> {
    address
        .checked_sub(DEVICE_ADDRESS)
        .filter(|offset| *offset < DEVICE_SIZE)
}

/// Runs the guest's init until the machine powers off. Returns only if the
/// power-off itself fails.
pub fn init() -> io::Result<()> {
    guest::run(&mut Linux, &mut Host)
}

/// The init's one job so far: report ready, then serve the host until it
/// hangs up.
struct Host;

impl Channel for Host {
    fn next_event(&mut self) -> io::Result<Event> {
        let mut channel = linux::connect_to_host(PORT)?;
        let release = fs::read(RELEASE_FILE)?;
        ToHost::Ready(release.trim_ascii().to_vec())
            .write(&mut channel)
            .map_err(protocol_failed)?;
        agent::serve(&mut channel).map_err(protocol_failed)?;
        Ok(Event::PowerOff)
    }
}

fn protocol_failed(error: guest::protocol::ProtocolError) -> io::Error {
    io::Error::other(format!("{error:?}"))
}
