//! Boots a machine: the host's side in [`boot`], and the guest's in [`init`].

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use guest::linux::{self, Linux};
use guest::protocol::{MAX_BODY, PORT, ToGuest, ToHost};
use guest::{Channel, Event, agent};
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::vsock::{Connection, Vsock};
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, boot as loader};

use crate::fetch::{self, Client};
use crate::filter::Filter;
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

/// How many frames of output may wait for the user's terminal. Past this the
/// guest waits too, so a slow reader can't fill the host's memory.
const OUTPUT_WINDOW: usize = 16;

/// What the guest reports while it runs.
pub enum Report<'a> {
    /// The init is up, on a kernel with this release.
    Ready(&'a str),
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
}

/// What a machine boots.
pub struct Guest<'a> {
    /// An uncompressed `vmlinux`.
    pub kernel: &'a [u8],
    /// A static program, which runs as `/init`.
    pub init: &'a [u8],
    /// More files for the guest's root filesystem.
    pub files: &'a [Entry<'a>],
    /// A program to run once the init is ready. Without one, the machine
    /// powers off as soon as the init reports.
    pub command: Option<&'a [Vec<u8>]>,
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

/// `ap boot`: the pinned kernel, with this program as its init. With no
/// command it prints the guest's kernel release as soon as the init reports
/// it, and returns 0. With one it relays the command's output, filtered, and
/// returns its status.
pub fn boot_bare(timeout: Duration, command: &[Vec<u8>]) -> Result<u8, Error> {
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
        command: (!command.is_empty()).then_some(command),
        timeout,
    };
    let mut relay = Relay::start(Instant::now() + timeout);
    let result = boot(&guest, |report| match report {
        Report::Ready(release) if command.is_empty() => {
            relay.send(Piece::Release(release.to_owned()));
        }
        Report::Ready(_) => {}
        Report::Stdout(bytes) => relay.send(Piece::Stdout(bytes.to_vec())),
        Report::Stderr(bytes) => relay.send(Piece::Stderr(bytes.to_vec())),
    });
    relay.finish();
    result.map(|status| status.unwrap_or(0))
}

enum Piece {
    Release(String),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Hands output to a thread of its own, so that a terminal nobody reads can't
/// hold the VM past its timeout. Each piece is acknowledged once written, and
/// no wait goes past the deadline.
struct Relay {
    pieces: Sender<Piece>,
    written: Receiver<()>,
    unwritten: usize,
    deadline: Instant,
}

impl Relay {
    fn start(deadline: Instant) -> Self {
        let (pieces, received) = mpsc::channel();
        let (acknowledge, written) = mpsc::channel();
        thread::spawn(move || print(&received, &acknowledge));
        Relay {
            pieces,
            written,
            unwritten: 0,
            deadline,
        }
    }

    /// Sends a piece, waiting for room first. Past the deadline the piece is
    /// dropped, because the machine is about to be stopped.
    fn send(&mut self, piece: Piece) {
        while self.unwritten >= OUTPUT_WINDOW {
            if !self.wait() {
                return;
            }
        }
        self.unwritten += 1;
        let _ = self.pieces.send(piece);
    }

    /// Waits for everything sent to be written, up to the deadline.
    fn finish(&mut self) {
        while self.unwritten > 0 && self.wait() {}
    }

    fn wait(&mut self) -> bool {
        let left = self.deadline.saturating_duration_since(Instant::now());
        let written = self.written.recv_timeout(left).is_ok();
        if written {
            self.unwritten -= 1;
        }
        written
    }
}

/// Write errors are ignored: a reader that went away shouldn't change what
/// the guest's command does or how it ends.
fn print(pieces: &Receiver<Piece>, written: &Sender<()>) {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    for piece in pieces {
        match piece {
            Piece::Release(release) => {
                let _ = writeln!(io::stdout(), "{release}");
            }
            Piece::Stdout(bytes) => {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(&stdout_filter.push(&bytes));
                let _ = stdout.flush();
            }
            Piece::Stderr(bytes) => {
                let _ = io::stderr().write_all(&stderr_filter.push(&bytes));
            }
        }
        if written.send(()).is_err() {
            return;
        }
    }
}

/// Boots `guest`, calls `on_report` with what its init reports, and returns
/// once the machine has powered off, with the status of the guest's command
/// if it had one. `on_report` runs on the vCPU thread, so it must not block
/// past the timeout, or the timeout can't stop the guest.
///
/// The VM lives inside this call, on this thread. When it returns, for any
/// reason, the VM is gone.
pub fn boot(guest: &Guest, on_report: impl FnMut(Report)) -> Result<Option<u8>, Error> {
    let run_frame = guest.command.map(run_frame).transpose()?;
    let wants_status = run_frame.is_some();
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
        run_frame,
        finished: false,
        ready: false,
        status: None,
        failure: None,
        on_report,
    };
    let stop = vcpu
        .run_ports(guest.timeout, &mut machine)
        .map_err(vm_failed)?;

    match (stop, machine.failure, machine.ready) {
        (Stop::TimedOut, ..) => Err(Error::TimedOut),
        (_, Some(reason), _) => Err(Error::Failed(reason)),
        (Stop::Shutdown, None, true) if wants_status && machine.status.is_none() => Err(
            Error::Failed("the guest powered off before its command finished".to_owned()),
        ),
        (Stop::Shutdown, None, true) => Ok(machine.status),
        (Stop::Shutdown, None, false) => Err(Error::Failed(
            "the guest powered off before its init reported ready".to_owned(),
        )),
        (stop, None, _) => Err(Error::Failed(format!(
            "the guest stopped unexpectedly: {stop:?}"
        ))),
    }
}

fn run_frame(argv: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    let mut frame = Vec::new();
    ToGuest::Run {
        argv: argv.to_vec(),
    }
    .write(&mut frame)
    .map_err(|error| Error::Failed(format!("can't send the command: {error:?}")))?;
    Ok(frame)
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
    /// The request to send once the init is ready.
    run_frame: Option<Vec<u8>>,
    finished: bool,
    ready: bool,
    status: Option<u8>,
    failure: Option<String>,
    on_report: F,
}

impl<F: FnMut(Report)> Machine<'_, F> {
    /// Takes what the guest has sent. Hanging up powers it off.
    fn run_host(&mut self) {
        if self.connection.is_none() {
            self.connection = self.vsock.accept();
        }
        let Some(connection) = self.connection else {
            return;
        };
        let mut buffer = [0; 4096];
        while !self.finished {
            let len = self.vsock.recv(connection, &mut buffer);
            if len == 0 {
                return;
            }
            self.frame.extend_from_slice(&buffer[..len]);
            while !self.finished && frame_is_complete(&self.frame) {
                self.handle_frame(connection);
            }
        }
    }

    fn handle_frame(&mut self, connection: Connection) {
        let mut rest = self.frame.as_slice();
        let message = ToHost::read(&mut rest);
        let used = self.frame.len() - rest.len();
        let running = self.ready && self.run_frame.is_none();
        match message {
            Ok(Some(ToHost::Ready(release))) if !self.ready => {
                self.handle_ready(connection, &release)
            }
            Ok(Some(ToHost::Stdout(bytes))) if running => {
                (self.on_report)(Report::Stdout(&bytes));
            }
            Ok(Some(ToHost::Stderr(bytes))) if running => {
                (self.on_report)(Report::Stderr(&bytes));
            }
            Ok(Some(ToHost::Exited(status))) if running => {
                self.status = Some(status);
                self.hang_up(connection);
            }
            _ => self.give_up(
                connection,
                "the guest's init didn't report ready".to_owned(),
            ),
        }
        self.frame.drain(..used);
    }

    fn handle_ready(&mut self, connection: Connection, release: &[u8]) {
        let Some(release) = printable(release) else {
            self.give_up(
                connection,
                "the guest reported an invalid release".to_owned(),
            );
            return;
        };
        self.ready = true;
        (self.on_report)(Report::Ready(&release));
        match self.run_frame.take() {
            Some(frame) if self.vsock.send(connection, &frame) == frame.len() => {}
            Some(_) => self.give_up(connection, "can't send the command to the guest".to_owned()),
            None => self.hang_up(connection),
        }
    }

    fn give_up(&mut self, connection: Connection, reason: String) {
        self.failure = Some(reason);
        self.hang_up(connection);
    }

    fn hang_up(&mut self, connection: Connection) {
        self.finished = true;
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
fn printable(release: &[u8]) -> Option<String> {
    (!release.is_empty() && release.iter().all(u8::is_ascii_graphic))
        .then(|| String::from_utf8_lossy(release).into_owned())
}

impl<F: FnMut(Report)> Ports for Machine<'_, F> {
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
