//! Boots a machine: the host's side in [`boot`], and the guest's in [`init`].

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use guest::linux::{self, Linux};
use guest::protocol::{MAX_BODY, PORT, Start, ToGuest, ToHost};
use guest::{Channel, Event, agent, root};
use vmm::block::Block;
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::vsock::{Connection, Vsock};
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, Waker, boot as loader};

use crate::fetch::{self, Client};
use crate::filter::Filter;
use crate::store::Store;

const KVM: &str = "/dev/kvm";
const INIT_PATH: &str = "/init";
const MEMORY: usize = 256 << 20;
const DEVICE_ADDRESS: u64 = 0xd000_0000;
const DEVICE_SIZE: u64 = 0x1000;
const LINE: u32 = 5;
/// The vsock device has the first slot, and each layer's disk one after it,
/// with the next interrupt line. The guest has no MP table, so its interrupts
/// are the legacy PIC's, which end at 15.
const LAST_LINE: u32 = 15;
const MAX_LAYERS: usize = (LAST_LINE - LINE) as usize;
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
/// How many bytes of input may wait for the guest. Past this the sender
/// waits too.
const INPUT_WINDOW: usize = 64 << 10;

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
    /// An agent's layers, bottom first. With any, the guest's root is these
    /// layers stacked under a writable layer that ends with the machine.
    pub layers: Vec<Layer>,
    /// What to ask of the init once it is ready. Without a request, the
    /// machine powers off as soon as the init reports.
    pub request: Option<ToGuest>,
    /// How long the guest may run, from entry to power-off.
    pub timeout: Duration,
    /// Whether this process confines itself before the guest's first
    /// instruction. It stays confined for the rest of its life, so only a
    /// process that has done all its host work may ask.
    pub confine: bool,
    /// What another thread sends the guest's harness while it runs.
    pub input: Option<&'a Input>,
}

/// What a harness in a terminal gets while it runs: keys and resizes, sent
/// from a thread other than the vCPU's. They reach the guest after the
/// request that started the harness.
pub struct Input {
    /// Frames for the guest, not yet handed to the vsock device.
    queued: Mutex<Vec<u8>>,
    room: Condvar,
    waker: Waker,
    /// The descriptor the sender reads its input from. A confined process
    /// may read it, and no other.
    source: RawFd,
}

impl Input {
    /// Input that the sender reads from `source`.
    pub fn new(source: RawFd) -> Input {
        Input {
            queued: Mutex::new(Vec::new()),
            room: Condvar::new(),
            waker: Waker::default(),
            source,
        }
    }

    /// Queues `message` for the guest, and waits while too much is queued
    /// already, so the guest's pace holds back the sender.
    pub fn send(&self, message: &ToGuest) -> Result<(), Error> {
        let mut frame = Vec::new();
        message
            .write(&mut frame)
            .map_err(|error| Error::Failed(format!("can't send input: {error:?}")))?;
        let mut queued = self.queued();
        while queued.len() >= INPUT_WINDOW {
            queued = self.room.wait(queued).unwrap_or_else(PoisonError::into_inner);
        }
        queued.extend_from_slice(&frame);
        drop(queued);
        self.waker.wake();
        Ok(())
    }

    fn queued(&self) -> MutexGuard<'_, Vec<u8>> {
        self.queued.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One of an agent's layers, served to the guest as a read-only disk.
pub struct Layer {
    /// The SHA-256 of the layer's content, which names it to the guest.
    pub digest: String,
    /// The layer's EROFS image.
    pub disk: Vec<u8>,
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
    let request = (!command.is_empty()).then(|| ToGuest::Run {
        argv: command.to_vec(),
    });
    let print_release = request.is_none();
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    // Write errors are ignored: a reader that went away shouldn't change what
    // the guest's command does or how it ends.
    let print = move |report: Report| match report {
        Report::Ready(release) if print_release => {
            let _ = writeln!(io::stdout(), "{release}");
        }
        Report::Ready(_) => {}
        Report::Stdout(bytes) => {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(&stdout_filter.push(bytes));
            let _ = stdout.flush();
        }
        Report::Stderr(bytes) => {
            let _ = io::stderr().write_all(&stderr_filter.push(bytes));
        }
    };
    relayed(timeout, Vec::new(), request, None, print)
}

/// A run's machine: the agent's layers, with its harness started as `start`
/// says, and sent `input` while it runs. It hands the harness's output to
/// `output`, unfiltered, on a thread of its own, and returns the harness's
/// status.
pub fn boot_agent(
    timeout: Duration,
    layers: Vec<Layer>,
    start: Start,
    input: Option<&Input>,
    output: impl FnMut(Report) + Send + 'static,
) -> Result<u8, Error> {
    relayed(timeout, layers, Some(ToGuest::Start(start)), input, output)
}

/// Boots the pinned kernel with this program as its init, and relays what
/// the guest reports to `output`.
fn relayed(
    timeout: Duration,
    layers: Vec<Layer>,
    request: Option<ToGuest>,
    input: Option<&Input>,
    output: impl FnMut(Report) + Send + 'static,
) -> Result<u8, Error> {
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
        layers,
        request,
        timeout,
        confine: true,
        input,
    };
    let mut relay = Relay::start(Instant::now() + timeout, output);
    let result = boot(guest, |report| match report {
        Report::Ready(release) => relay.send(Piece::Release(release.to_owned())),
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

/// Hands output to a thread of its own, so that a reader that doesn't read
/// can't hold the VM past its timeout. Each piece is acknowledged once
/// written, and no wait goes past the deadline.
struct Relay {
    pieces: Sender<Piece>,
    written: Receiver<()>,
    unwritten: usize,
    deadline: Instant,
}

impl Relay {
    fn start(deadline: Instant, output: impl FnMut(Report) + Send + 'static) -> Self {
        let (pieces, received) = mpsc::channel();
        let (acknowledge, written) = mpsc::channel();
        thread::spawn(move || forward(&received, &acknowledge, output));
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

fn forward(pieces: &Receiver<Piece>, written: &Sender<()>, mut output: impl FnMut(Report)) {
    for piece in pieces {
        match &piece {
            Piece::Release(release) => output(Report::Ready(release)),
            Piece::Stdout(bytes) => output(Report::Stdout(bytes)),
            Piece::Stderr(bytes) => output(Report::Stderr(bytes)),
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
pub fn boot(guest: Guest, on_report: impl FnMut(Report)) -> Result<Option<u8>, Error> {
    let command_line = command_line(&guest.layers)?;
    let run_frame = guest.request.as_ref().map(request_frame).transpose()?;
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
    let vm = Vm::new(GuestMemory::new(MEMORY).map_err(vm_failed)?).map_err(vm_failed)?;
    vm.add_interrupt_controllers().map_err(vm_failed)?;
    let bad_kernel = |error: loader::Error| Error::Failed(format!("bad kernel: {error:?}"));
    let entry = loader::load(vm.memory(), guest.kernel, &command_line).map_err(bad_kernel)?;
    loader::load_initramfs(vm.memory(), &entry, &archive).map_err(bad_kernel)?;
    let mut vcpu = vm.create_vcpu().map_err(vm_failed)?;
    vm.expose_cpu_features(&vcpu).map_err(vm_failed)?;
    loader::enter(&vcpu, &entry).map_err(vm_failed)?;

    let disks = (guest.layers.into_iter().zip(LINE + 1..))
        .map(|(layer, line)| {
            let name = root::name(&layer.digest).as_bytes().to_vec();
            (Block::new(layer.disk, &name), vm.interrupt(line))
        })
        .collect();
    let mut machine = Machine {
        serial: Serial::new(),
        vsock: Vsock::new(GUEST_CID, &[PORT]),
        disks,
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
        input: guest.input,
    };
    if guest.confine {
        let readable = guest.input.map(|input| input.source);
        vmm::confine::confine(readable).map_err(|error| failed("can't confine ap", error))?;
    }
    let waker = Waker::default();
    let waker = guest.input.map_or(&waker, |input| &input.waker);
    let stop = vcpu
        .run_waking(guest.timeout, &mut machine, waker)
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

/// The kernel command line: a slot for each layer's disk after the vsock
/// device's, and the order to stack the layers in.
fn command_line(layers: &[Layer]) -> Result<String, Error> {
    if layers.len() > MAX_LAYERS {
        return Err(Error::Failed(format!(
            "an agent can have at most {MAX_LAYERS} layers for now, and this one has {}",
            layers.len()
        )));
    }
    let mut line = COMMAND_LINE.to_owned();
    if layers.is_empty() {
        return Ok(line);
    }
    let mut names = Vec::new();
    for (slot, layer) in (1..).zip(layers) {
        let name = root::name(&layer.digest);
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(Error::Failed(format!(
                "a layer has a bad digest: {:?}",
                layer.digest
            )));
        }
        let address = DEVICE_ADDRESS + slot * DEVICE_SIZE;
        let irq = LINE as u64 + slot;
        line.push_str(&format!(" virtio_mmio.device=4K@{address:#x}:{irq}"));
        names.push(name);
    }
    line.push(' ');
    line.push_str(&root::parameter(&names));
    Ok(line)
}

fn request_frame(request: &ToGuest) -> Result<Vec<u8>, Error> {
    let mut frame = Vec::new();
    request
        .write(&mut frame)
        .map_err(|error| Error::Failed(format!("can't send the command: {error:?}")))?;
    Ok(frame)
}

fn failed(context: &str, error: io::Error) -> Error {
    Error::Failed(format!("{context}: {error}"))
}

/// The serial port, the vsock device, the layers' disks, and the host's end of
/// the channel. It all runs on the vCPU thread, during the guest's exits.
struct Machine<'vm, F> {
    serial: Serial,
    vsock: Vsock,
    /// Each layer's disk, bottom first, and its interrupt.
    disks: Vec<(Block, Interrupt<'vm>)>,
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
    input: Option<&'vm Input>,
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

    /// Hands queued input to the vsock device, once the harness has been
    /// asked to start, as much as fits.
    fn pass_input(&mut self) {
        let (Some(input), Some(connection)) = (self.input, self.connection) else {
            return;
        };
        if !self.ready || self.run_frame.is_some() || self.finished {
            return;
        }
        let mut queued = input.queued();
        if queued.is_empty() {
            return;
        }
        let taken = self.vsock.send(connection, &queued);
        queued.drain(..taken);
        input.room.notify_all();
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
            Ok(Some(ToHost::Failed(reason))) if running => {
                self.give_up(connection, shown(&reason));
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

/// A reason the guest gave, for the user's terminal. Control characters
/// become `?`, so the guest can't send the terminal an escape sequence.
fn shown(reason: &[u8]) -> String {
    String::from_utf8_lossy(reason)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
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
        let Some((slot, offset)) = device_offset(address) else {
            return;
        };
        if let Some((disk, interrupt)) = slot.checked_sub(1).and_then(|at| self.disks.get_mut(at)) {
            if disk.write(self.memory, offset, data) {
                // A failed pulse leaves the guest waiting, and the timeout ends it.
                let _ = interrupt.pulse();
            }
            return;
        }
        if slot != 0 {
            return;
        }
        let mut interrupt = self.vsock.write(self.memory, offset, data);
        self.run_host();
        self.pass_input();
        interrupt |= self.vsock.service(self.memory);
        if interrupt {
            // A failed pulse leaves the guest waiting, and the timeout ends it.
            let _ = self.interrupt.pulse();
        }
    }

    fn woken(&mut self) {
        self.pass_input();
        if self.vsock.service(self.memory) {
            // A failed pulse leaves the guest waiting, and the timeout ends it.
            let _ = self.interrupt.pulse();
        }
    }

    fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        match device_offset(address) {
            Some((0, offset)) => self.vsock.read(offset, data),
            Some((slot, offset)) if slot <= self.disks.len() => {
                self.disks[slot - 1].0.read(offset, data);
            }
            _ => data.fill(0xff),
        }
    }
}

/// Which device slot an address falls in, and where in it.
fn device_offset(address: u64) -> Option<(usize, u64)> {
    let offset = address.checked_sub(DEVICE_ADDRESS)?;
    let slot = usize::try_from(offset / DEVICE_SIZE).ok()?;
    (slot <= MAX_LAYERS).then_some((slot, offset % DEVICE_SIZE))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(digest: &str) -> Layer {
        Layer {
            digest: digest.to_owned(),
            disk: Vec::new(),
        }
    }

    #[test]
    fn a_machine_without_layers_keeps_its_command_line() {
        assert_eq!(command_line(&[]).unwrap(), COMMAND_LINE);
    }

    #[test]
    fn each_layer_gets_the_next_slot_and_line_and_keeps_its_place() {
        let top = "b".repeat(64);
        let line = command_line(&[layer("a1"), layer(&top)]).unwrap();
        assert_eq!(
            line,
            format!(
                "{COMMAND_LINE} virtio_mmio.device=4K@0xd0001000:6 \
                 virtio_mmio.device=4K@0xd0002000:7 ap.layers=a1,{}",
                &top[..root::NAME_LEN]
            )
        );
        assert_eq!(root::order(&line), ["a1", &top[..root::NAME_LEN]]);
    }

    #[test]
    fn the_last_line_is_the_pic_s_last() {
        let most: Vec<Layer> = (0..MAX_LAYERS).map(|_| layer("ab")).collect();
        assert!(
            command_line(&most)
                .unwrap()
                .ends_with(":15 ap.layers=ab,ab,ab,ab,ab,ab,ab,ab,ab,ab")
        );
        let too_many: Vec<Layer> = (0..=MAX_LAYERS).map(|_| layer("ab")).collect();
        let error = command_line(&too_many).unwrap_err().to_string();
        assert!(error.contains("at most 10 layers"), "{error}");
    }

    #[test]
    fn a_digest_that_could_break_the_command_line_is_refused() {
        for digest in ["", "a b", "a,b", "a=b"] {
            assert!(command_line(&[layer(digest)]).is_err(), "{digest:?}");
        }
    }

    #[test]
    fn an_address_names_its_slot_and_offset() {
        assert_eq!(device_offset(DEVICE_ADDRESS), Some((0, 0)));
        assert_eq!(device_offset(DEVICE_ADDRESS + 0x1070), Some((1, 0x70)));
        assert_eq!(
            device_offset(DEVICE_ADDRESS + MAX_LAYERS as u64 * DEVICE_SIZE + 0xfff),
            Some((MAX_LAYERS, 0xfff))
        );
        assert_eq!(
            device_offset(DEVICE_ADDRESS + (MAX_LAYERS as u64 + 1) * DEVICE_SIZE),
            None
        );
        assert_eq!(device_offset(DEVICE_ADDRESS - 1), None);
    }
}
