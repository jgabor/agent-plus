//! Boots a machine: the host's side in [`boot`], and the guest's in [`init`].

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{OwnedFd, RawFd};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use guest::linux::{self, Linux};
use guest::protocol::{EXEC_PORT, KEYS_PORT, MAX_BODY, PORT, Start, ToGuest, ToHost};
use guest::{Channel, Event, agent, net, root, share};
use vmm::block::{Block, Disk};
use vmm::fs::{Fs, Relay as ShareRelay};
use vmm::initramfs::{self, Entry};
use vmm::net::Net;
use vmm::serial::Serial;
use vmm::vsock::{Connection, Vsock};
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, Waker, boot as loader};

use crate::fetch::{self, Client};
use crate::filter::Filter;
use crate::store::Store;

const KVM: &str = "/dev/kvm";
const INIT_PATH: &str = "/init";
/// The memory of every VM, including `ap boot`'s. SPEC.md's Run section
/// gives each run's VM 512 MiB. Claude Code 2.1.323 at its prompt left 76
/// MiB of 256 MiB available.
const MEMORY: usize = 512 << 20;
const DEVICE_ADDRESS: u64 = 0xd000_0000;
const DEVICE_SIZE: u64 = 0x1000;
const LINE: u32 = 5;
/// The vsock device has the first slot, each layer's disk one after it, each
/// share's device after those, and the network device last, if there is
/// one, each with the next interrupt line. The
/// guest has no MP table, so its interrupts are the legacy PIC's, which end
/// at 15.
const LAST_LINE: u32 = 15;
const MAX_DEVICES: usize = (LAST_LINE - LINE) as usize;
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
/// How many bytes of keys and resizes may wait for the guest. Past this the
/// sender waits too.
const INPUT_WINDOW: usize = 64 << 10;
/// How many messages of a command beside the harness may be on their way to
/// `ap exec`. Past this, only that command waits.
pub const EXEC_WINDOW: usize = 8;

/// What the guest reports while it runs.
pub enum Report<'a> {
    /// The init is up, on a kernel with this release.
    Ready(&'a str),
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
    /// What the guest reports about the command beside the harness that has
    /// this id.
    Exec(u32, &'a ToHost),
    /// The guest took one more message for the command with this id.
    ExecTook(u32),
}

/// What a machine boots.
pub struct Guest<'a> {
    /// An uncompressed `vmlinux`. One the machine owns is freed once it is
    /// in guest memory.
    pub kernel: Cow<'a, [u8]>,
    /// A static program, which runs as `/init`, freed as the kernel is.
    pub init: Cow<'a, [u8]>,
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

/// The shares a machine gets, served by a share server this socket
/// reaches. The guest finds each one by the tag that [`share::tag`] gives
/// its place.
pub struct Shares {
    /// The VMM's end of the share server's socket.
    pub socket: OwnedFd,
    /// How many shares the server serves.
    pub count: usize,
}

/// What a run gets while it runs, sent from a thread other than the vCPU's.
/// Keys and resizes for a harness in a terminal go on a connection of their
/// own. A stop reaches the guest after the request that started the harness.
/// The commands beside the harness come with their input.
pub struct Input {
    /// A stop for the init, not yet handed to the vsock device.
    stops: Mutex<Vec<u8>>,
    /// Keys and resizes, not yet handed to the vsock device.
    keys: Mutex<Vec<u8>>,
    room: Condvar,
    waker: Arc<Waker>,
    /// The descriptors the sender reads its input from. A confined process
    /// may read these, and no others.
    sources: [RawFd; 2],
    /// The commands beside the harness, by the id `ap run` gave each.
    execs: Mutex<BTreeMap<u32, Exec>>,
}

/// A command beside the harness, until it ends.
#[derive(Default)]
struct Exec {
    /// Frames for the guest, the request first, not yet handed to the vsock
    /// device. The sender holds back what it sends, so this stays small.
    frames: VecDeque<Vec<u8>>,
    /// How many messages of its output are on their way to `ap exec`.
    unread: usize,
    /// `ap exec` is gone.
    hung_up: bool,
}

impl Input {
    /// Input that the sender reads from `sources`.
    pub fn new(sources: [RawFd; 2]) -> Input {
        Input {
            stops: Mutex::new(Vec::new()),
            keys: Mutex::new(Vec::new()),
            room: Condvar::new(),
            waker: Arc::default(),
            sources,
            execs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Queues `message` for the command beside the harness that has this id.
    /// An [`ToGuest::Exec`] starts that command, and nothing else reaches one
    /// that hasn't started or has ended.
    pub fn send_exec(&self, id: u32, message: &ToGuest) -> Result<(), Error> {
        let mut frame = Vec::new();
        message
            .write(&mut frame)
            .map_err(|error| Error::Failed(format!("can't send to the command: {error:?}")))?;
        let mut execs = self.execs();
        match (message, execs.get_mut(&id)) {
            (ToGuest::Exec(_), None) => {
                let mut exec = Exec::default();
                exec.frames.push_back(frame);
                execs.insert(id, exec);
            }
            (ToGuest::Exec(_), Some(_)) | (_, None) => return Ok(()),
            (_, Some(exec)) => exec.frames.push_back(frame),
        }
        drop(execs);
        self.waker.wake();
        Ok(())
    }

    /// Says that `ap exec` has written one more message of the command with
    /// this id.
    pub fn exec_taken(&self, id: u32) {
        if let Some(exec) = self.execs().get_mut(&id) {
            exec.unread = exec.unread.saturating_sub(1);
        }
        self.waker.wake();
    }

    /// Says that the `ap exec` of the command with this id is gone, so the
    /// command's connection is reset.
    pub fn exec_hung_up(&self, id: u32) {
        if let Some(exec) = self.execs().get_mut(&id) {
            exec.hung_up = true;
        }
        self.waker.wake();
    }

    fn execs(&self) -> MutexGuard<'_, BTreeMap<u32, Exec>> {
        self.execs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a key or a resize for the harness's terminal, and waits while
    /// too much is queued already, so the harness's pace holds back the
    /// sender.
    pub fn send(&self, message: &ToGuest) -> Result<(), Error> {
        let mut frame = Vec::new();
        message
            .write(&mut frame)
            .map_err(|error| Error::Failed(format!("can't send input: {error:?}")))?;
        let mut queued = lock(&self.keys);
        while queued.len() >= INPUT_WINDOW {
            queued = self
                .room
                .wait(queued)
                .unwrap_or_else(PoisonError::into_inner);
        }
        queued.extend_from_slice(&frame);
        drop(queued);
        self.waker.wake();
        Ok(())
    }

    /// Queues a request to send the harness `SIGTERM`. It waits for no
    /// key, so it reaches a harness that has stopped reading them.
    pub fn terminate(&self) -> Result<(), Error> {
        let mut stops = lock(&self.stops);
        ToGuest::Terminate
            .write(&mut *stops)
            .map_err(|error| Error::Failed(format!("can't send the stop: {error:?}")))?;
        drop(stops);
        self.waker.wake();
        Ok(())
    }
}

fn lock(queue: &Mutex<Vec<u8>>) -> MutexGuard<'_, Vec<u8>> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One of an agent's layers, served to the guest as a read-only disk.
pub struct Layer {
    /// The SHA-256 of the layer's content, which names it to the guest.
    pub digest: String,
    /// The layer's EROFS image, which the guest reads as it needs it.
    pub disk: Box<dyn Disk>,
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
    let request = bare_request(command);
    let print = bare_output(request.is_none(), io::stdout(), io::stderr());
    let devices = Devices {
        layers: Vec::new(),
        shares: None,
        link: None,
    };
    relayed(timeout, devices, request, None, print)
}

/// What `ap boot` asks of the init: to run `command`, if there is one.
fn bare_request(command: &[Vec<u8>]) -> Option<ToGuest> {
    (!command.is_empty()).then(|| ToGuest::Run {
        argv: command.to_vec(),
    })
}

/// Writes what the guest reports for `ap boot`: the kernel release, if
/// `print_release`, and the command's output, filtered.
fn bare_output(
    print_release: bool,
    mut stdout: impl Write + Send + 'static,
    mut stderr: impl Write + Send + 'static,
) -> impl FnMut(Report) + Send + 'static {
    let mut stdout_filter = Filter::default();
    let mut stderr_filter = Filter::default();
    // Write errors are ignored: a reader that went away shouldn't change what
    // the guest's command does or how it ends.
    move |report: Report| match report {
        Report::Ready(release) if print_release => {
            let _ = writeln!(stdout, "{release}");
        }
        Report::Ready(_) => {}
        Report::Stdout(bytes) => {
            let _ = stdout.write_all(&stdout_filter.push(bytes));
            let _ = stdout.flush();
        }
        Report::Stderr(bytes) => {
            let _ = stderr.write_all(&stderr_filter.push(bytes));
        }
        Report::Exec(..) | Report::ExecTook(_) => {}
    }
}

/// A run's machine: the agent's layers and `shares`, and a network device
/// on `link`, as [`boot_linked`] takes it, with its harness started as
/// `start` says, and sent `input` while it runs. It hands the harness's
/// output to `output`, unfiltered, on a thread of its own, and returns the
/// harness's status.
pub fn boot_agent(
    timeout: Duration,
    layers: Vec<Layer>,
    shares: Option<Shares>,
    link: OwnedFd,
    start: Start,
    input: Option<&Input>,
    output: impl FnMut(Report) + Send + 'static,
) -> Result<u8, Error> {
    let request = Some(ToGuest::Start(start));
    let devices = Devices {
        layers,
        shares,
        link: Some(link),
    };
    relayed(timeout, devices, request, input, output)
}

/// What a machine gets beside its kernel and init.
struct Devices {
    layers: Vec<Layer>,
    shares: Option<Shares>,
    link: Option<OwnedFd>,
}

/// Boots the pinned kernel with this program as its init, and relays what
/// the guest reports to `output`.
fn relayed(
    timeout: Duration,
    devices: Devices,
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
        kernel: kernel.into(),
        init: init.into(),
        files: &files,
        layers: devices.layers,
        request,
        timeout,
        confine: true,
        input,
    };
    let mut relay = Relay::start(timeout, output);
    let result = boot_linked(guest, devices.shares, devices.link, |report| match report {
        Report::Ready(release) => relay.send(Piece::Release(release.to_owned())),
        Report::Stdout(bytes) => relay.send(Piece::Stdout(bytes.to_vec())),
        Report::Stderr(bytes) => relay.send(Piece::Stderr(bytes.to_vec())),
        Report::Exec(id, message) => relay.send(Piece::Exec(id, message.clone())),
        Report::ExecTook(id) => relay.send(Piece::ExecTook(id)),
    });
    relay.finish();
    result.map(|status| status.unwrap_or(0))
}

enum Piece {
    Release(String),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exec(u32, ToHost),
    ExecTook(u32),
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
    /// A relay whose waits end `timeout` from now.
    fn start(timeout: Duration, output: impl FnMut(Report) + Send + 'static) -> Self {
        let (pieces, received) = mpsc::channel();
        let (acknowledge, written) = mpsc::channel();
        thread::spawn(move || forward(&received, &acknowledge, output));
        Relay {
            pieces,
            written,
            unwritten: 0,
            deadline: Instant::now() + timeout,
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
            Piece::Exec(id, message) => output(Report::Exec(*id, message)),
            Piece::ExecTook(id) => output(Report::ExecTook(*id)),
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
    boot_sharing(guest, None, on_report)
}

/// Boots `guest` as [`boot`] does, with `shares`, if it has any.
pub fn boot_sharing(
    guest: Guest,
    shares: Option<Shares>,
    on_report: impl FnMut(Report),
) -> Result<Option<u8>, Error> {
    boot_linked(guest, shares, None, on_report)
}

/// Boots `guest` as [`boot_sharing`] does, with a network device if it has a
/// `link`: a socket that keeps message boundaries, which gets each of the
/// guest's Ethernet frames as a message, and whose messages reach the guest
/// as frames. Nothing else gets the guest's frames.
pub fn boot_linked(
    guest: Guest,
    shares: Option<Shares>,
    link: Option<OwnedFd>,
    on_report: impl FnMut(Report),
) -> Result<Option<u8>, Error> {
    let share_count = shares.as_ref().map_or(0, |shares| shares.count);
    let command_line = command_line(&guest.layers, share_count, link.is_some())?;
    let run_frame = guest.request.as_ref().map(request_frame).transpose()?;
    let wants_status = run_frame.is_some();
    let mut entries = vec![Entry::File {
        path: "init",
        mode: 0o755,
        contents: &guest.init,
    }];
    entries.extend_from_slice(guest.files);
    let archive = initramfs::build(&entries)
        .map_err(|error| Error::Failed(format!("can't build the initramfs: {error:?}")))?;

    let vm_failed = |error: io::Error| failed("can't start the VM", error);
    let vm = Vm::new(GuestMemory::new(MEMORY).map_err(vm_failed)?).map_err(vm_failed)?;
    vm.add_interrupt_controllers().map_err(vm_failed)?;
    let bad_kernel = |error: loader::Error| Error::Failed(format!("bad kernel: {error:?}"));
    let entry = loader::load(vm.memory(), &guest.kernel, &command_line).map_err(bad_kernel)?;
    loader::load_initramfs(vm.memory(), &entry, &archive).map_err(bad_kernel)?;
    // The guest has its own copies now, and a run lasts long.
    drop(entries);
    drop(archive);
    drop(guest.kernel);
    drop(guest.init);
    let mut vcpu = vm.create_vcpu().map_err(vm_failed)?;
    vm.expose_cpu_features(&vcpu).map_err(vm_failed)?;
    loader::enter(&vcpu, &entry).map_err(vm_failed)?;

    let disks = guest
        .layers
        .into_iter()
        .map(|layer| {
            let name = root::name(&layer.digest).as_bytes().to_vec();
            Block::new(layer.disk, &name).map_err(|error| failed("can't read a layer", error))
        })
        .collect::<Result<_, Error>>()?;
    let net = link
        .map(|link| Net::new(link, net::MAC))
        .transpose()
        .map_err(|error| failed("can't attach the network", error))?;
    let relay = shares
        .map(|shares| ShareRelay::new(shares.socket, Instant::now() + guest.timeout))
        .transpose()
        .map_err(|error| failed("can't reach the share server", error))?;
    let parts = Parts {
        disks,
        shares: share_count,
        net,
        relay,
    };
    let lines = |line| vm.interrupt(line);
    let mut machine = Machine::new(vm.memory(), lines, parts, run_frame, guest.input, on_report);
    let waker = guest
        .input
        .map_or_else(Arc::default, |input| Arc::clone(&input.waker));
    if let Some((device, _)) = &machine.net {
        let receiver = device.receiver();
        let waker = Arc::clone(&waker);
        thread::spawn(move || receiver.run(|| waker.wake()));
    }
    if guest.confine {
        let input = guest.input.into_iter().flat_map(|input| input.sources);
        let share_server = machine.relay.as_ref().map(ShareRelay::descriptor);
        let link = machine.net.as_ref().map(|(device, _)| device.descriptor());
        let layers = machine
            .disks
            .iter()
            .filter_map(|(disk, _)| disk.descriptor());
        let readable: Vec<RawFd> = input
            .chain(share_server)
            .chain(link)
            .chain(layers)
            .collect();
        vmm::confine::confine(&readable).map_err(|error| failed("can't confine ap", error))?;
    }
    let stop = vcpu
        .run_waking(guest.timeout, &mut machine, &waker)
        .map_err(vm_failed)?;
    machine.outcome(stop, wants_status)
}

/// The kernel command line: a slot for each layer's disk after the vsock
/// device's, then one for each of the other `devices`, and the order to stack
/// the layers in.
fn command_line(layers: &[Layer], shares: usize, network: bool) -> Result<String, Error> {
    // The network device takes a slot of its own.
    let most = MAX_DEVICES - usize::from(network);
    if layers.len() + shares > most {
        return Err(Error::Failed(format!(
            "a run can have at most {most} layers and shares for now, and this one has {}",
            layers.len() + shares
        )));
    }
    let devices = shares + usize::from(network);
    let mut line = COMMAND_LINE.to_owned();
    let mut names = Vec::new();
    for layer in layers {
        let name = root::name(&layer.digest);
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(Error::Failed(format!(
                "a layer has a bad digest: {:?}",
                layer.digest
            )));
        }
        names.push(name);
    }
    for slot in 1..=(layers.len() + devices) as u64 {
        let address = DEVICE_ADDRESS + slot * DEVICE_SIZE;
        let irq = LINE as u64 + slot;
        line.push_str(&format!(" virtio_mmio.device=4K@{address:#x}:{irq}"));
    }
    if !names.is_empty() {
        line.push(' ');
        line.push_str(&root::parameter(&names));
    }
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

/// The serial port, the vsock device, the layers' disks, the shares'
/// devices, the network device, and the host's end of the channel. It all
/// runs on the vCPU thread, during the guest's exits.
struct Machine<'vm, F, I> {
    serial: Serial,
    vsock: Vsock,
    /// Each layer's disk, bottom first, and its interrupt.
    disks: Vec<(Block, I)>,
    /// Each share's device, in share order, and its interrupt.
    shares: Vec<(Fs, I)>,
    /// The network device and its interrupt, when the guest has a link.
    net: Option<(Net, I)>,
    /// The share server's socket, when there are shares.
    relay: Option<ShareRelay>,
    memory: &'vm GuestMemory,
    interrupt: I,
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
    /// The connection the guest opened for the keys of a harness in a
    /// terminal.
    keys: Option<Connection>,
    /// Connections the guest opened for commands beside the harness, which
    /// no command has yet.
    spares: VecDeque<Connection>,
    /// Each command's connection, and what it has sent of a frame so far.
    bound: BTreeMap<u32, (Connection, Vec<u8>)>,
}

/// What the guest reported about a command beside the harness, kept until the
/// lock on the commands is let go.
enum ExecReport {
    Message(u32, ToHost),
    Took(u32),
}

/// The devices a machine gets beside its vsock device, before each has an
/// interrupt line.
struct Parts {
    disks: Vec<Block>,
    shares: usize,
    net: Option<Net>,
    relay: Option<ShareRelay>,
}

/// An interrupt line that a device raises.
trait Line {
    fn pulse(&self);
}

impl Line for Interrupt<'_> {
    fn pulse(&self) {
        // A failed pulse leaves the guest waiting, and the timeout ends it.
        let _ = Interrupt::pulse(self);
    }
}

impl<'vm, F: FnMut(Report), I: Line> Machine<'vm, F, I> {
    /// A machine whose devices take the lines that `line` gives, in the order
    /// of their slots: the vsock device's first, then each disk's, each
    /// share's, and the network device's. It sends `run_frame` once the init
    /// is ready.
    fn new(
        memory: &'vm GuestMemory,
        line: impl Fn(u32) -> I,
        parts: Parts,
        run_frame: Option<Vec<u8>>,
        input: Option<&'vm Input>,
        on_report: F,
    ) -> Self {
        let mut lines = (LINE + 1..).map(&line);
        let disks = parts.disks.into_iter().zip(&mut lines).collect();
        let shares = (0..parts.shares)
            .map(|index| Fs::new(index as u32, share::tag(index).as_bytes()))
            .zip(&mut lines)
            .collect();
        let net = parts.net.zip(lines.next());
        // Only a run has keys for its harness, and commands beside it.
        let ports: &[u32] = match input {
            Some(_) => &[PORT, EXEC_PORT, KEYS_PORT],
            None => &[PORT],
        };
        Machine {
            serial: Serial::new(),
            vsock: Vsock::new(GUEST_CID, ports),
            disks,
            shares,
            net,
            relay: parts.relay,
            memory,
            interrupt: line(LINE),
            connection: None,
            frame: Vec::new(),
            run_frame,
            finished: false,
            ready: false,
            status: None,
            failure: None,
            on_report,
            input,
            keys: None,
            spares: VecDeque::new(),
            bound: BTreeMap::new(),
        }
    }

    /// How the machine ended, once it stopped this way: the status of the
    /// guest's command, if `wants_status`, or why it failed.
    fn outcome(self, stop: Stop, wants_status: bool) -> Result<Option<u8>, Error> {
        match (stop, self.failure, self.ready) {
            (Stop::TimedOut, ..) => Err(Error::TimedOut),
            (_, Some(reason), _) => Err(Error::Failed(reason)),
            (Stop::Shutdown, None, true) if wants_status && self.status.is_none() => Err(
                Error::Failed("the guest powered off before its command finished".to_owned()),
            ),
            (Stop::Shutdown, None, true) => Ok(self.status),
            (Stop::Shutdown, None, false) => Err(Error::Failed(
                "the guest powered off before its init reported ready".to_owned(),
            )),
            (stop, None, _) => Err(Error::Failed(format!(
                "the guest stopped unexpectedly: {stop:?}"
            ))),
        }
    }

    /// Takes what the guest has sent. Hanging up powers it off.
    fn run_host(&mut self) {
        self.accept();
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

    /// Takes the connections the guest has opened: the first to [`PORT`] is
    /// the init's, the first to [`KEYS_PORT`] takes the harness's keys, and
    /// each to [`EXEC_PORT`] waits for a command.
    fn accept(&mut self) {
        while let Some(connection) = self.vsock.accept() {
            match connection.port() {
                EXEC_PORT => self.spares.push_back(connection),
                KEYS_PORT if self.keys.is_none() => self.keys = Some(connection),
                KEYS_PORT => self.vsock.close(connection),
                _ if self.connection.is_none() => self.connection = Some(connection),
                _ => self.vsock.close(connection),
            }
        }
    }

    /// Gives each new command beside the harness a connection, passes on
    /// what was sent to each, and reports what each has done, as far as its
    /// window allows. The connection of a command whose `ap exec` is gone is
    /// reset, because a close would wait for the command to read its input.
    fn serve_execs(&mut self) {
        let Some(input) = self.input else {
            return;
        };
        let mut reports = Vec::new();
        let mut execs = input.execs();
        let Machine {
            vsock,
            spares,
            bound,
            ..
        } = self;
        let mut ended = Vec::new();
        for (id, exec) in execs.iter_mut() {
            if exec.hung_up {
                ended.push((*id, true));
                continue;
            }
            if !bound.contains_key(id) {
                let Some(spare) = spares.pop_front() else {
                    continue;
                };
                bound.insert(*id, (spare, Vec::new()));
            }
            let Some((connection, frame)) = bound.get_mut(id) else {
                continue;
            };
            while let Some(front) = exec.frames.front_mut() {
                let taken = vsock.send(*connection, front);
                front.drain(..taken);
                if !front.is_empty() {
                    break;
                }
                exec.frames.pop_front();
                reports.push(ExecReport::Took(*id));
            }
            if let Some(end) = receive_exec(vsock, *connection, frame, exec, *id, &mut reports) {
                reports.push(ExecReport::Message(*id, end));
                ended.push((*id, false));
            }
        }
        for (id, hung_up) in ended {
            execs.remove(&id);
            match bound.remove(&id) {
                Some((connection, _)) if hung_up => vsock.reset(connection),
                Some((connection, _)) => vsock.close(connection),
                None => {}
            }
        }
        drop(execs);
        for report in reports {
            match &report {
                ExecReport::Message(id, message) => (self.on_report)(Report::Exec(*id, message)),
                ExecReport::Took(id) => (self.on_report)(Report::ExecTook(*id)),
            }
        }
    }

    /// Hands queued input to the vsock device, once the harness has been
    /// asked to start, as much as fits: a stop on the init's connection, and
    /// keys on their own.
    fn pass_input(&mut self) {
        let Some(input) = self.input else {
            return;
        };
        if !self.ready || self.run_frame.is_some() || self.finished {
            return;
        }
        for (queue, connection) in [(&input.stops, self.connection), (&input.keys, self.keys)] {
            let Some(connection) = connection else {
                continue;
            };
            let mut queued = lock(queue);
            if !queued.is_empty() {
                let taken = self.vsock.send(connection, &queued);
                queued.drain(..taken);
            }
        }
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

/// Reads what the command on `connection` has sent, while its window has
/// room, into `reports`. Returns how it ended, if it has: its status, or why
/// the host gave up on it.
fn receive_exec(
    vsock: &mut Vsock,
    connection: Connection,
    frame: &mut Vec<u8>,
    exec: &mut Exec,
    id: u32,
    reports: &mut Vec<ExecReport>,
) -> Option<ToHost> {
    let mut buffer = [0; 4096];
    while exec.unread < EXEC_WINDOW {
        if !frame_is_complete(frame) {
            let len = vsock.recv(connection, &mut buffer);
            if len == 0 {
                return vsock.guest_finished(connection).then(|| {
                    ToHost::Failed(b"the guest closed the command's connection".to_vec())
                });
            }
            frame.extend_from_slice(&buffer[..len]);
            continue;
        }
        let mut rest = frame.as_slice();
        let message = ToHost::read(&mut rest);
        let used = frame.len() - rest.len();
        frame.drain(..used);
        match message {
            Ok(Some(output @ (ToHost::Stdout(_) | ToHost::Stderr(_)))) => {
                exec.unread += 1;
                reports.push(ExecReport::Message(id, output));
            }
            Ok(Some(end @ (ToHost::Exited(_) | ToHost::Failed(_)))) => return Some(end),
            _ => {
                return Some(ToHost::Failed(
                    b"the guest sent a malformed message for the command".to_vec(),
                ));
            }
        }
    }
    None
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
pub(crate) fn shown(reason: &[u8]) -> String {
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

impl<F: FnMut(Report), I: Line> Ports for Machine<'_, F, I> {
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
                interrupt.pulse();
            }
            return;
        }
        let share = slot.checked_sub(1 + self.disks.len());
        if let (Some((device, interrupt)), Some(relay)) = (
            share.and_then(|at| self.shares.get_mut(at)),
            self.relay.as_mut(),
        ) {
            if device.write(self.memory, offset, data, relay) {
                interrupt.pulse();
            }
            return;
        }
        if slot == 1 + self.disks.len() + self.shares.len()
            && let Some((device, interrupt)) = self.net.as_mut()
        {
            if device.write(self.memory, offset, data) {
                interrupt.pulse();
            }
            return;
        }
        if slot != 0 {
            return;
        }
        let mut interrupt = self.vsock.write(self.memory, offset, data);
        self.run_host();
        self.pass_input();
        self.serve_execs();
        interrupt |= self.vsock.service(self.memory);
        if interrupt {
            self.interrupt.pulse();
        }
    }

    fn woken(&mut self) {
        if let Some((device, interrupt)) = self.net.as_mut()
            && device.service(self.memory)
        {
            interrupt.pulse();
        }
        self.pass_input();
        self.serve_execs();
        if self.vsock.service(self.memory) {
            self.interrupt.pulse();
        }
    }

    fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        match device_offset(address) {
            Some((0, offset)) => self.vsock.read(offset, data),
            Some((slot, offset)) if slot <= self.disks.len() => {
                self.disks[slot - 1].0.read(offset, data);
            }
            Some((slot, offset)) if slot <= self.disks.len() + self.shares.len() => {
                self.shares[slot - 1 - self.disks.len()]
                    .0
                    .read(offset, data);
            }
            Some((slot, offset)) if slot == 1 + self.disks.len() + self.shares.len() => {
                match &self.net {
                    Some((device, _)) => device.read(offset, data),
                    None => data.fill(0xff),
                }
            }
            _ => data.fill(0xff),
        }
    }
}

/// Which device slot an address falls in, and where in it.
fn device_offset(address: u64) -> Option<(usize, u64)> {
    let offset = address.checked_sub(DEVICE_ADDRESS)?;
    let slot = usize::try_from(offset / DEVICE_SIZE).ok()?;
    (slot <= MAX_DEVICES).then_some((slot, offset % DEVICE_SIZE))
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
        agent::serve(&mut channel, Some(linux::connect_to_host)).map_err(protocol_failed)?;
        Ok(Event::PowerOff)
    }
}

fn protocol_failed(error: guest::protocol::ProtocolError) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use guest::protocol::Exec as ExecRequest;
    use std::cell::RefCell;
    use std::os::unix::net::{UnixDatagram, UnixStream};
    use std::rc::Rc;

    fn layer(digest: &str) -> Layer {
        Layer {
            digest: digest.to_owned(),
            disk: Box::new(Vec::new()),
        }
    }

    #[test]
    fn a_machine_without_layers_keeps_its_command_line() {
        assert_eq!(command_line(&[], 0, false).unwrap(), COMMAND_LINE);
    }

    #[test]
    fn each_layer_gets_the_next_slot_and_line_and_keeps_its_place() {
        let top = "b".repeat(64);
        let line = command_line(&[layer("a1"), layer(&top)], 0, false).unwrap();
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
        let most: Vec<Layer> = (0..MAX_DEVICES).map(|_| layer("ab")).collect();
        assert!(
            command_line(&most, 0, false)
                .unwrap()
                .ends_with(":15 ap.layers=ab,ab,ab,ab,ab,ab,ab,ab,ab,ab")
        );
        let too_many: Vec<Layer> = (0..=MAX_DEVICES).map(|_| layer("ab")).collect();
        let error = command_line(&too_many, 0, false).unwrap_err().to_string();
        assert!(error.contains("at most 10 layers and shares"), "{error}");
        let error = command_line(&most, 1, false).unwrap_err().to_string();
        assert!(error.contains("this one has 11"), "{error}");
        // The network device takes the last slot.
        let error = command_line(&most, 0, true).unwrap_err().to_string();
        assert!(
            error.contains("at most 9 layers and shares for now, and this one has 10"),
            "{error}"
        );
        assert!(
            command_line(&most[1..], 0, true)
                .unwrap()
                .ends_with(":15 ap.layers=ab,ab,ab,ab,ab,ab,ab,ab,ab")
        );
    }

    #[test]
    fn each_share_gets_a_slot_and_line_after_the_layers() {
        assert_eq!(
            command_line(&[], 2, false).unwrap(),
            format!(
                "{COMMAND_LINE} virtio_mmio.device=4K@0xd0001000:6 \
                 virtio_mmio.device=4K@0xd0002000:7"
            )
        );
        let line = command_line(&[layer("a1")], 1, false).unwrap();
        assert_eq!(
            line,
            format!(
                "{COMMAND_LINE} virtio_mmio.device=4K@0xd0001000:6 \
                 virtio_mmio.device=4K@0xd0002000:7 ap.layers=a1"
            )
        );
    }

    #[test]
    fn a_digest_that_could_break_the_command_line_is_refused() {
        for digest in ["", "a b", "a,b", "a=b"] {
            assert!(
                command_line(&[layer(digest)], 0, false).is_err(),
                "{digest:?}"
            );
        }
    }

    #[test]
    fn an_address_names_its_slot_and_offset() {
        assert_eq!(device_offset(DEVICE_ADDRESS), Some((0, 0)));
        assert_eq!(device_offset(DEVICE_ADDRESS + 0x1070), Some((1, 0x70)));
        assert_eq!(
            device_offset(DEVICE_ADDRESS + MAX_DEVICES as u64 * DEVICE_SIZE + 0xfff),
            Some((MAX_DEVICES, 0xfff))
        );
        assert_eq!(
            device_offset(DEVICE_ADDRESS + (MAX_DEVICES as u64 + 1) * DEVICE_SIZE),
            None
        );
        assert_eq!(device_offset(DEVICE_ADDRESS - 1), None);
    }

    #[test]
    fn a_machine_has_the_memory_spec_md_gives_a_run() {
        assert_eq!(MEMORY, 512 * 1024 * 1024);
    }

    #[test]
    fn too_many_layers_fail_before_a_vm_starts() {
        let guest = || Guest {
            kernel: Cow::Borrowed(&[]),
            init: Cow::Borrowed(&[]),
            files: &[],
            layers: (0..=MAX_DEVICES).map(|_| layer("ab")).collect(),
            request: None,
            timeout: Duration::from_secs(1),
            confine: false,
            input: None,
        };
        let failed = |result: Result<Option<u8>, Error>| matches!(result, Err(Error::Failed(reason)) if reason.contains("at most"));
        assert!(failed(boot(guest(), |_| {})));
        assert!(failed(boot_sharing(guest(), None, |_| {})));
        assert!(failed(boot_linked(guest(), None, None, |_| {})));
    }

    #[test]
    fn ap_boot_runs_its_command_or_only_reports_ready() {
        assert_eq!(bare_request(&[]), None);
        assert_eq!(
            bare_request(&[b"true".to_vec()]),
            Some(ToGuest::Run {
                argv: vec![b"true".to_vec()]
            })
        );
    }

    /// A writer whose bytes the test reads after handing it away.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ap_boot_prints_the_release_only_without_a_command_and_filters_output() {
        for print_release in [true, false] {
            let (stdout, stderr) = (Shared::default(), Shared::default());
            let mut print = bare_output(print_release, stdout.clone(), stderr.clone());
            print(Report::Ready("6.1.0"));
            print(Report::Stdout(b"a\x1b]52;c;aGk=\x07b"));
            print(Report::Stderr(b"c\x1b[21td"));
            let release: &[u8] = if print_release { b"6.1.0\n" } else { b"" };
            assert_eq!(stdout.bytes(), [release, b"ab"].concat());
            assert_eq!(stderr.bytes(), b"cd");
        }
    }

    #[test]
    fn a_relay_writes_every_piece_in_order_and_finishes_once_they_are_written() {
        let (seen, written) = mpsc::channel();
        let start = Instant::now();
        let mut relay = Relay::start(Duration::from_secs(10), move |report| {
            if let Report::Stdout(bytes) = report {
                thread::sleep(Duration::from_millis(1));
                seen.send(bytes.to_vec()).unwrap();
            }
        });
        let pieces = 0..(OUTPUT_WINDOW as u8 * 3);
        for piece in pieces.clone() {
            relay.send(Piece::Stdout(vec![piece]));
        }
        relay.finish();
        let written: Vec<Vec<u8>> = written.try_iter().collect();
        assert_eq!(written, pieces.map(|piece| vec![piece]).collect::<Vec<_>>());
        assert!(start.elapsed() < Duration::from_secs(5));
        // With nothing unwritten, finishing doesn't wait.
        relay.finish();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_reader_that_stops_reading_holds_the_relay_only_until_its_timeout() {
        let (release, released) = mpsc::channel::<()>();
        let (seen, written) = mpsc::channel();
        let timeout = Duration::from_millis(300);
        let start = Instant::now();
        let mut relay = Relay::start(timeout, move |report| {
            if let Report::Stdout(bytes) = report {
                if bytes == [0] {
                    let _ = released.recv();
                }
                seen.send(bytes.to_vec()).unwrap();
            }
        });
        // One more than the window, which waits, and is dropped at the timeout.
        for piece in 0..=OUTPUT_WINDOW as u8 {
            relay.send(Piece::Stdout(vec![piece]));
        }
        let waited = start.elapsed();
        assert!(waited >= timeout, "{waited:?}");
        relay.finish();
        assert!(start.elapsed() < timeout * 10, "{:?}", start.elapsed());
        release.send(()).unwrap();
        let mut got = Vec::new();
        while let Ok(piece) = written.recv_timeout(Duration::from_millis(500)) {
            got.push(piece);
        }
        let window: Vec<Vec<u8>> = (0..OUTPUT_WINDOW as u8).map(|piece| vec![piece]).collect();
        assert_eq!(got, window);
    }

    #[test]
    fn a_request_frame_reads_back_as_the_request() {
        let request = ToGuest::Run {
            argv: vec![b"echo".to_vec(), b"hi".to_vec()],
        };
        let frame = request_frame(&request).unwrap();
        assert_eq!(ToGuest::read(&mut frame.as_slice()).unwrap(), Some(request));
    }

    fn header(length: usize) -> Vec<u8> {
        (length as u32).to_le_bytes().to_vec()
    }

    #[test]
    fn a_frame_is_complete_once_its_body_has_arrived() {
        assert!(!frame_is_complete(&[]));
        assert!(!frame_is_complete(&[3, 0, 0]));
        assert!(!frame_is_complete(&[header(3), vec![1, 2]].concat()));
        assert!(frame_is_complete(&[header(3), vec![1, 2, 3]].concat()));
        assert!(frame_is_complete(&[header(3), vec![1, 2, 3, 4]].concat()));
        assert!(frame_is_complete(&header(0)));
        // Nothing waits for a body larger than a frame may hold.
        assert!(!frame_is_complete(&header(MAX_BODY)));
        assert!(frame_is_complete(&header(MAX_BODY + 1)));
        assert!(frame_is_complete(&header(u32::MAX as usize)));
    }

    #[test]
    fn a_reason_or_a_release_reaches_the_terminal_without_controls() {
        assert_eq!(shown(b"no\x1b[31m\n"), "no?[31m?");
        assert_eq!(printable(b"6.1.0-ap"), Some("6.1.0-ap".to_owned()));
        for release in [&b""[..], b"6.1 x", b"6.1\x1b", "6.1é".as_bytes()] {
            assert_eq!(printable(release), None, "{release:?}");
        }
    }

    #[test]
    fn input_queues_keys_a_stop_and_each_commands_frames() {
        let input = Input::new([-1, -1]);
        let key = ToGuest::Input(b"k".to_vec());
        input.send(&key).unwrap();
        assert_eq!(*lock(&input.keys), request_frame(&key).unwrap());
        input.terminate().unwrap();
        assert_eq!(
            *lock(&input.stops),
            request_frame(&ToGuest::Terminate).unwrap()
        );

        let exec = ToGuest::Exec(ExecRequest {
            argv: vec![b"ls".to_vec()],
            terminal: None,
        });
        // Nothing reaches a command that hasn't started.
        input.send_exec(7, &key).unwrap();
        assert!(input.execs().is_empty());
        input.send_exec(7, &exec).unwrap();
        input.send_exec(7, &exec).unwrap();
        input.send_exec(7, &key).unwrap();
        let frames = [request_frame(&exec).unwrap(), request_frame(&key).unwrap()];
        assert_eq!(input.execs()[&7].frames, frames);

        input.execs().get_mut(&7).unwrap().unread = 2;
        input.exec_taken(7);
        assert_eq!(input.execs()[&7].unread, 1);
        assert!(!input.execs()[&7].hung_up);
        input.exec_hung_up(7);
        assert!(input.execs()[&7].hung_up);
    }

    /// An interrupt line that records each pulse.
    struct TestLine(u32, Rc<RefCell<Vec<u32>>>);

    impl Line for TestLine {
        fn pulse(&self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    /// What the machine reported, owned.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Ready(String),
        Stdout(Vec<u8>),
        Stderr(Vec<u8>),
        Exec(u32, ToHost),
        ExecTook(u32),
    }

    type Reports<'a> = Box<dyn FnMut(Report) + 'a>;

    const QUEUE_SELECT: u64 = 0x030;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const STATUS: u64 = 0x070;
    const QUEUE_DESC: u64 = 0x080;
    const QUEUE_DRIVER: u64 = 0x090;
    const QUEUE_DEVICE: u64 = 0x0a0;
    const DEVICE_ID: u64 = 0x008;
    const DRIVER_OK: u32 = 4;
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const RX: u16 = 0;
    const TX: u16 = 1;
    const SIZE: u16 = 64;
    const TX_DATA: u64 = 0x10_0000;
    const RX_DATA: u64 = 0x20_0000;
    const RX_BUFFER: u32 = 4096;
    const PACKET_HEADER: usize = 44;
    const OP_REQUEST: u16 = 1;
    const OP_RST: u16 = 3;
    const OP_SHUTDOWN: u16 = 4;
    const OP_RW: u16 = 5;
    const SHUTDOWN_RECEIVE: u32 = 1;
    const SHUTDOWN_BOTH: u32 = 3;

    /// The guest port of the init's connection.
    const INIT: u32 = 5000;
    /// The guest ports of the connections for keys.
    const KEYS: u32 = 5001;
    const MORE_KEYS: u32 = 5002;
    /// The guest port of a connection for a command beside the harness.
    const COMMAND: u32 = 5003;

    fn table(queue: u16) -> u64 {
        0x1_0000 * (u64::from(queue) + 1)
    }

    fn available(queue: u16) -> u64 {
        table(queue) + 0x1000
    }

    fn used(queue: u16) -> u64 {
        table(queue) + 0x2000
    }

    fn memory() -> GuestMemory {
        GuestMemory::new(1 << 23).unwrap()
    }

    fn no_parts() -> Parts {
        Parts {
            disks: Vec::new(),
            shares: 0,
            net: None,
            relay: None,
        }
    }

    /// A fake guest, which drives the machine's vsock device as Linux's
    /// driver would, and keeps what the host sent on each of its ports.
    struct Driver<'a> {
        machine: Machine<'a, Reports<'a>, TestLine>,
        seen: Rc<RefCell<Vec<Seen>>>,
        pulses: Rc<RefCell<Vec<u32>>>,
        tx_published: u16,
        rx_published: u16,
        rx_seen: u16,
        bytes: BTreeMap<u32, Vec<u8>>,
        ops: BTreeMap<u32, Vec<u16>>,
    }

    impl<'a> Driver<'a> {
        fn new(
            memory: &'a GuestMemory,
            parts: Parts,
            request: Option<&ToGuest>,
            input: Option<&'a Input>,
        ) -> Self {
            let seen = Rc::new(RefCell::new(Vec::new()));
            let pulses = Rc::new(RefCell::new(Vec::new()));
            let into = Rc::clone(&seen);
            let reports: Reports<'a> = Box::new(move |report: Report| {
                into.borrow_mut().push(match report {
                    Report::Ready(release) => Seen::Ready(release.to_owned()),
                    Report::Stdout(bytes) => Seen::Stdout(bytes.to_vec()),
                    Report::Stderr(bytes) => Seen::Stderr(bytes.to_vec()),
                    Report::Exec(id, message) => Seen::Exec(id, message.clone()),
                    Report::ExecTook(id) => Seen::ExecTook(id),
                });
            });
            let lines = Rc::clone(&pulses);
            let machine = Machine::new(
                memory,
                move |line| TestLine(line, Rc::clone(&lines)),
                parts,
                request.map(|request| request_frame(request).unwrap()),
                input,
                reports,
            );
            let mut driver = Driver {
                machine,
                seen,
                pulses,
                tx_published: 0,
                rx_published: 0,
                rx_seen: 0,
                bytes: BTreeMap::new(),
                ops: BTreeMap::new(),
            };
            for queue in 0..3 {
                driver.set(QUEUE_SELECT, u32::from(queue));
                driver.set(QUEUE_NUM, u32::from(SIZE));
                driver.set(QUEUE_DESC, table(queue) as u32);
                driver.set(QUEUE_DRIVER, available(queue) as u32);
                driver.set(QUEUE_DEVICE, used(queue) as u32);
                driver.set(QUEUE_READY, 1);
            }
            driver.set(STATUS, DRIVER_OK);
            driver.post_rx(0..SIZE);
            driver
        }

        fn memory(&self) -> &GuestMemory {
            self.machine.memory
        }

        /// Writes a register of the device in `slot`.
        fn set_in(&mut self, slot: u64, offset: u64, value: u32) {
            let address = DEVICE_ADDRESS + slot * DEVICE_SIZE + offset;
            self.machine.mmio_write(address, &value.to_le_bytes());
        }

        /// Reads a register of the device in `slot`.
        fn get_in(&mut self, slot: u64, offset: u64) -> u32 {
            let mut value = [0; 4];
            let address = DEVICE_ADDRESS + slot * DEVICE_SIZE + offset;
            self.machine.mmio_read(address, &mut value);
            u32::from_le_bytes(value)
        }

        fn set(&mut self, offset: u64, value: u32) {
            self.set_in(0, offset, value);
        }

        fn descriptor(
            &self,
            queue: u16,
            index: u16,
            address: u64,
            len: u32,
            flags: u16,
            next: u16,
        ) {
            let at = table(queue) + u64::from(index) * 16;
            let memory = self.memory();
            memory.write(at, &address.to_le_bytes()).unwrap();
            memory.write(at + 8, &len.to_le_bytes()).unwrap();
            memory.write(at + 12, &flags.to_le_bytes()).unwrap();
            memory.write(at + 14, &next.to_le_bytes()).unwrap();
        }

        /// Gives the device these receive buffers, and notifies it.
        fn post_rx(&mut self, heads: impl IntoIterator<Item = u16>) {
            for head in heads {
                let address = RX_DATA + u64::from(head) * u64::from(RX_BUFFER);
                self.descriptor(RX, head, address, RX_BUFFER, WRITE, 0);
                let slot = u64::from(self.rx_published % SIZE);
                self.memory()
                    .write(available(RX) + 4 + slot * 2, &head.to_le_bytes())
                    .unwrap();
                self.rx_published = self.rx_published.wrapping_add(1);
            }
            let published = self.rx_published.to_le_bytes();
            self.memory().write(available(RX) + 2, &published).unwrap();
            self.set(QUEUE_NOTIFY, u32::from(RX));
            self.collect();
        }

        /// Keeps what the device has written to the receive buffers, and
        /// gives each buffer back.
        fn collect(&mut self) {
            let mut heads = Vec::new();
            loop {
                let mut index = [0; 2];
                self.memory().read(used(RX) + 2, &mut index).unwrap();
                if self.rx_seen == u16::from_le_bytes(index) {
                    break;
                }
                let at = used(RX) + 4 + u64::from(self.rx_seen % SIZE) * 8;
                let mut element = [0; 8];
                self.memory().read(at, &mut element).unwrap();
                let head = u16::from_le_bytes([element[0], element[1]]);
                let len = u32::from_le_bytes(element[4..].try_into().unwrap()) as usize;
                let address = RX_DATA + u64::from(head) * u64::from(RX_BUFFER);
                let mut packet = vec![0; len];
                self.memory().read(address, &mut packet).unwrap();
                let port = u32::from_le_bytes(packet[20..24].try_into().unwrap());
                let op = u16::from_le_bytes([packet[30], packet[31]]);
                if op == OP_RW {
                    let payload = &packet[PACKET_HEADER..];
                    self.bytes
                        .entry(port)
                        .or_default()
                        .extend_from_slice(payload);
                } else {
                    self.ops.entry(port).or_default().push(op);
                }
                self.rx_seen = self.rx_seen.wrapping_add(1);
                heads.push(head);
            }
            if !heads.is_empty() {
                self.post_rx(heads);
            }
        }

        /// Sends one packet from `port` to the host's `to`.
        fn packet(&mut self, port: u32, to: u32, op: u16, flags: u32, payload: &[u8]) {
            let mut header = Vec::with_capacity(PACKET_HEADER);
            header.extend_from_slice(&GUEST_CID.to_le_bytes());
            header.extend_from_slice(&2u64.to_le_bytes());
            header.extend_from_slice(&port.to_le_bytes());
            header.extend_from_slice(&to.to_le_bytes());
            header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            header.extend_from_slice(&1u16.to_le_bytes());
            header.extend_from_slice(&op.to_le_bytes());
            header.extend_from_slice(&flags.to_le_bytes());
            header.extend_from_slice(&(1u32 << 20).to_le_bytes());
            header.extend_from_slice(&0u32.to_le_bytes());
            self.memory().write(TX_DATA, &header).unwrap();
            self.memory().write(TX_DATA + 4096, payload).unwrap();
            self.descriptor(TX, 0, TX_DATA, PACKET_HEADER as u32, NEXT, 1);
            self.descriptor(TX, 1, TX_DATA + 4096, payload.len() as u32, 0, 0);
            let slot = u64::from(self.tx_published % SIZE);
            self.memory()
                .write(available(TX) + 4 + slot * 2, &0u16.to_le_bytes())
                .unwrap();
            self.tx_published = self.tx_published.wrapping_add(1);
            let published = self.tx_published.to_le_bytes();
            self.memory().write(available(TX) + 2, &published).unwrap();
            self.set(QUEUE_NOTIFY, u32::from(TX));
            self.collect();
        }

        fn connect(&mut self, port: u32, to: u32) {
            self.packet(port, to, OP_REQUEST, 0, &[]);
        }

        /// Sends these messages from `port` in one packet.
        fn say(&mut self, port: u32, to: u32, messages: &[ToHost]) {
            let mut bytes = Vec::new();
            for message in messages {
                message.write(&mut bytes).unwrap();
            }
            self.packet(port, to, OP_RW, 0, &bytes);
        }

        fn woken(&mut self) {
            self.machine.woken();
            self.collect();
        }

        /// What the host sent `port`, since this was last asked.
        fn received(&mut self, port: u32) -> Vec<u8> {
            self.bytes.remove(&port).unwrap_or_default()
        }

        fn shut_down(&self, port: u32) -> bool {
            self.ops
                .get(&port)
                .is_some_and(|ops| ops.contains(&OP_SHUTDOWN))
        }

        fn was_reset(&self, port: u32) -> bool {
            self.ops.get(&port).is_some_and(|ops| ops.contains(&OP_RST))
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.borrow_mut().drain(..).collect()
        }
    }

    fn frames(messages: &[ToGuest]) -> Vec<u8> {
        messages
            .iter()
            .flat_map(|message| request_frame(message).unwrap())
            .collect()
    }

    fn ready() -> ToHost {
        ToHost::Ready(b"6.1.0".to_vec())
    }

    fn run() -> ToGuest {
        ToGuest::Run {
            argv: vec![b"true".to_vec()],
        }
    }

    #[test]
    fn the_init_reports_ready_and_a_machine_with_no_request_hangs_up() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), None, None);
        driver.connect(INIT, PORT);
        // A frame after the hang-up is never read.
        driver.say(INIT, PORT, &[ready(), ToHost::Stdout(b"late".to_vec())]);
        assert_eq!(driver.seen(), [Seen::Ready("6.1.0".to_owned())]);
        assert!(driver.shut_down(INIT));
        assert_eq!(driver.machine.outcome(Stop::Shutdown, false).unwrap(), None);
    }

    #[test]
    fn the_request_follows_ready_and_the_command_reports_until_it_exits() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), Some(&run()), None);
        driver.connect(INIT, PORT);
        assert_eq!(driver.received(INIT), b"");
        driver.say(INIT, PORT, &[ready()]);
        assert_eq!(driver.received(INIT), frames(&[run()]));
        // A frame may arrive in pieces, and several in one.
        let mut output = Vec::new();
        ToHost::Stdout(b"out".to_vec()).write(&mut output).unwrap();
        driver.packet(INIT, PORT, OP_RW, 0, &output[..3]);
        assert_eq!(driver.seen(), [Seen::Ready("6.1.0".to_owned())]);
        driver.packet(INIT, PORT, OP_RW, 0, &output[3..]);
        let rest = [ToHost::Stderr(b"err".to_vec()), ToHost::Exited(3)];
        driver.say(INIT, PORT, &rest);
        let expected = [Seen::Stdout(b"out".to_vec()), Seen::Stderr(b"err".to_vec())];
        assert_eq!(driver.seen(), expected);
        assert!(driver.shut_down(INIT));
        assert_eq!(
            driver.machine.outcome(Stop::Shutdown, true).unwrap(),
            Some(3)
        );
    }

    #[test]
    fn a_failed_command_ends_the_machine_with_its_reason() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), Some(&run()), None);
        driver.connect(INIT, PORT);
        driver.say(
            INIT,
            PORT,
            &[ready(), ToHost::Failed(b"no\x1b[31m".to_vec())],
        );
        assert!(driver.shut_down(INIT));
        let error = driver.machine.outcome(Stop::Shutdown, true).unwrap_err();
        assert_eq!(error.to_string(), "no?[31m");
    }

    /// The failure of a machine whose guest sent `messages`, and what it
    /// reported.
    fn failure(request: Option<&ToGuest>, messages: &[ToHost]) -> (String, Vec<Seen>) {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), request, None);
        driver.connect(INIT, PORT);
        driver.say(INIT, PORT, messages);
        assert!(driver.shut_down(INIT));
        let seen = driver.seen();
        let error = driver.machine.outcome(Stop::Shutdown, true).unwrap_err();
        (error.to_string(), seen)
    }

    #[test]
    fn anything_but_ready_first_is_a_failure() {
        for first in [
            ToHost::Stdout(b"x".to_vec()),
            ToHost::Stderr(b"x".to_vec()),
            ToHost::Exited(0),
            ToHost::Failed(b"x".to_vec()),
        ] {
            for request in [None, Some(&run())] {
                let (error, seen) = failure(request, &[first.clone(), ready()]);
                assert_eq!(error, "the guest's init didn't report ready", "{first:?}");
                assert_eq!(seen, [], "{first:?}");
            }
        }
        let (error, seen) = failure(Some(&run()), &[ready(), ready()]);
        assert_eq!(error, "the guest's init didn't report ready");
        assert_eq!(seen, [Seen::Ready("6.1.0".to_owned())]);
    }

    #[test]
    fn a_release_with_controls_is_a_failure() {
        let (error, seen) = failure(None, &[ToHost::Ready(b"6.1\x1b[2J".to_vec())]);
        assert_eq!(error, "the guest reported an invalid release");
        assert_eq!(seen, []);
    }

    #[test]
    fn a_request_the_guest_won_t_receive_is_a_failure() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), Some(&run()), None);
        driver.connect(INIT, PORT);
        driver.packet(INIT, PORT, OP_SHUTDOWN, SHUTDOWN_RECEIVE, &[]);
        driver.say(INIT, PORT, &[ready()]);
        let error = driver.machine.outcome(Stop::Shutdown, true).unwrap_err();
        assert_eq!(error.to_string(), "can't send the command to the guest");
    }

    #[test]
    fn a_second_connection_to_the_init_s_port_is_closed() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), None, None);
        driver.connect(INIT, PORT);
        driver.connect(INIT + 100, PORT);
        assert!(driver.shut_down(INIT + 100));
        assert!(!driver.shut_down(INIT));
        driver.say(INIT, PORT, &[ready()]);
        assert_eq!(driver.seen(), [Seen::Ready("6.1.0".to_owned())]);
    }

    #[test]
    fn the_vsock_device_interrupts_on_its_own_line_when_it_answers() {
        let memory = memory();
        let mut driver = Driver::new(&memory, no_parts(), None, None);
        driver.pulses.borrow_mut().clear();
        driver.connect(INIT, PORT);
        assert_eq!(*driver.pulses.borrow(), [LINE]);
    }

    #[test]
    fn the_machine_ends_as_its_guest_did() {
        let memory = memory();
        let outcome = |stop, ready, status, failure: Option<&str>, wants_status| {
            let mut machine = Driver::new(&memory, no_parts(), None, None).machine;
            machine.ready = ready;
            machine.status = status;
            machine.failure = failure.map(str::to_owned);
            match machine.outcome(stop, wants_status) {
                Ok(status) => Ok(status),
                Err(Error::TimedOut) => Err("timed out".to_owned()),
                Err(error) => Err(error.to_string()),
            }
        };
        assert_eq!(
            outcome(Stop::Shutdown, true, Some(2), None, true),
            Ok(Some(2))
        );
        assert_eq!(outcome(Stop::Shutdown, true, None, None, false), Ok(None));
        let unfinished = Err("the guest powered off before its command finished".to_owned());
        assert_eq!(outcome(Stop::Shutdown, true, None, None, true), unfinished);
        let unready = Err("the guest powered off before its init reported ready".to_owned());
        assert_eq!(outcome(Stop::Shutdown, false, None, None, false), unready);
        let failed = Err("why".to_owned());
        assert_eq!(
            outcome(Stop::Shutdown, true, Some(0), Some("why"), true),
            failed
        );
        let timed_out = Err("timed out".to_owned());
        assert_eq!(
            outcome(Stop::TimedOut, true, Some(0), Some("why"), true),
            timed_out
        );
        let halted = outcome(Stop::Halted, true, Some(0), None, true).unwrap_err();
        assert!(halted.contains("stopped unexpectedly: Halted"), "{halted}");
    }

    /// A run's machine with its harness started and its keys' connection
    /// open.
    fn started<'a>(memory: &'a GuestMemory, input: &'a Input) -> Driver<'a> {
        let mut driver = Driver::new(memory, no_parts(), Some(&run()), Some(input));
        driver.connect(INIT, PORT);
        driver.connect(KEYS, KEYS_PORT);
        driver.say(INIT, PORT, &[ready()]);
        assert_eq!(driver.received(INIT), frames(&[run()]));
        driver
    }

    #[test]
    fn keys_and_a_stop_wait_until_the_harness_was_asked_to_start() {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let key = ToGuest::Input(b"k".to_vec());
        let mut driver = Driver::new(&memory, no_parts(), Some(&run()), Some(&input));
        driver.connect(INIT, PORT);
        driver.connect(KEYS, KEYS_PORT);
        driver.connect(MORE_KEYS, KEYS_PORT);
        assert!(driver.shut_down(MORE_KEYS));
        input.send(&key).unwrap();
        input.terminate().unwrap();
        driver.woken();
        assert_eq!(driver.received(KEYS), b"");
        assert_eq!(driver.received(INIT), b"");

        driver.say(INIT, PORT, &[ready()]);
        driver.woken();
        assert_eq!(driver.received(KEYS), frames(std::slice::from_ref(&key)));
        assert_eq!(driver.received(INIT), frames(&[run(), ToGuest::Terminate]));
        assert!(lock(&input.keys).is_empty());

        driver.say(INIT, PORT, &[ToHost::Exited(0)]);
        input.send(&key).unwrap();
        driver.woken();
        assert_eq!(driver.received(KEYS), b"");
    }

    #[test]
    fn keys_wait_for_the_init_even_without_a_request() {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let mut driver = Driver::new(&memory, no_parts(), None, Some(&input));
        driver.connect(INIT, PORT);
        driver.connect(KEYS, KEYS_PORT);
        input.send(&ToGuest::Input(b"k".to_vec())).unwrap();
        driver.woken();
        assert_eq!(driver.received(KEYS), b"");
    }

    fn exec() -> ToGuest {
        ToGuest::Exec(ExecRequest {
            argv: vec![b"ls".to_vec()],
            terminal: None,
        })
    }

    /// A run's machine with the command `7` started beside its harness.
    fn executing<'a>(memory: &'a GuestMemory, input: &'a Input) -> Driver<'a> {
        let mut driver = started(memory, input);
        driver.connect(COMMAND, EXEC_PORT);
        input.send_exec(7, &exec()).unwrap();
        driver.woken();
        assert_eq!(driver.received(COMMAND), frames(&[exec()]));
        assert_eq!(
            driver.seen(),
            [Seen::Ready("6.1.0".to_owned()), Seen::ExecTook(7)]
        );
        driver
    }

    #[test]
    fn a_command_beside_the_harness_gets_its_input_and_reports_until_it_exits() {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let mut driver = executing(&memory, &input);
        let typed = ToGuest::Input(b"in".to_vec());
        input.send_exec(7, &typed).unwrap();
        driver.woken();
        assert_eq!(driver.received(COMMAND), frames(&[typed]));
        assert_eq!(driver.seen(), [Seen::ExecTook(7)]);

        let output = [ToHost::Stdout(b"o".to_vec()), ToHost::Stderr(b"e".to_vec())];
        driver.say(COMMAND, EXEC_PORT, &output);
        driver.woken();
        let expected = [
            Seen::Exec(7, ToHost::Stdout(b"o".to_vec())),
            Seen::Exec(7, ToHost::Stderr(b"e".to_vec())),
        ];
        assert_eq!(driver.seen(), expected);

        driver.say(COMMAND, EXEC_PORT, &[ToHost::Exited(4)]);
        driver.woken();
        assert_eq!(driver.seen(), [Seen::Exec(7, ToHost::Exited(4))]);
        assert!(driver.shut_down(COMMAND));
        assert!(!driver.was_reset(COMMAND));
        assert!(input.execs().is_empty());
    }

    #[test]
    fn a_command_s_output_waits_for_room_in_its_window() {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let mut driver = executing(&memory, &input);
        let output = vec![ToHost::Stdout(b"o".to_vec()); EXEC_WINDOW + 2];
        driver.say(COMMAND, EXEC_PORT, &output);
        driver.woken();
        let one = || Seen::Exec(7, ToHost::Stdout(b"o".to_vec()));
        assert_eq!(driver.seen().len(), EXEC_WINDOW);
        input.exec_taken(7);
        driver.woken();
        assert_eq!(driver.seen(), [one()]);
        driver.woken();
        assert_eq!(driver.seen(), []);
    }

    #[test]
    fn a_command_whose_ap_exec_is_gone_is_reset() {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let mut driver = executing(&memory, &input);
        input.exec_hung_up(7);
        driver.woken();
        assert!(driver.was_reset(COMMAND));
        assert!(input.execs().is_empty());
        assert_eq!(driver.seen(), []);
    }

    /// Asserts that the command `7` fails for `reason` once the guest does
    /// `end`.
    fn ends_for(end: impl Fn(&mut Driver), reason: &[u8]) {
        let memory = memory();
        let input = Input::new([-1, -1]);
        let mut driver = executing(&memory, &input);
        end(&mut driver);
        driver.woken();
        let failed = Seen::Exec(7, ToHost::Failed(reason.to_vec()));
        assert_eq!(driver.seen(), [failed]);
        assert!(input.execs().is_empty());
    }

    #[test]
    fn a_command_s_connection_that_ends_or_breaks_ends_it() {
        ends_for(
            |driver| driver.packet(COMMAND, EXEC_PORT, OP_SHUTDOWN, SHUTDOWN_BOTH, &[]),
            b"the guest closed the command's connection",
        );
        ends_for(
            |driver| driver.say(COMMAND, EXEC_PORT, &[ready()]),
            b"the guest sent a malformed message for the command",
        );
    }

    #[test]
    fn each_device_has_its_slot_and_the_line_after_the_one_before() {
        let memory = memory();
        let (share_server, _) = UnixStream::pair().unwrap();
        let (link, _peer) = UnixDatagram::pair().unwrap();
        let parts = Parts {
            disks: vec![
                Block::new(vec![0; 512], b"a").unwrap(),
                Block::new(vec![0; 1024], b"b").unwrap(),
            ],
            shares: 2,
            net: Some(Net::new(link.into(), net::MAC).unwrap()),
            relay: Some(ShareRelay::new(share_server.into(), Instant::now()).unwrap()),
        };
        let mut driver = Driver::new(&memory, parts, None, None);
        let machine = &driver.machine;
        assert_eq!(machine.interrupt.0, LINE);
        fn lines<D>(devices: &[(D, TestLine)]) -> Vec<u32> {
            devices.iter().map(|(_, line)| line.0).collect()
        }
        assert_eq!(lines(&machine.disks), [6, 7]);
        assert_eq!(lines(&machine.shares), [8, 9]);
        assert_eq!(machine.net.as_ref().map(|(_, line)| line.0), Some(10));

        // Each slot reads and writes its own device.
        let ids: Vec<u32> = (0..8).map(|slot| driver.get_in(slot, DEVICE_ID)).collect();
        assert_eq!(ids, [19, 2, 2, 26, 26, 1, u32::MAX, u32::MAX]);
        for slot in 1..6 {
            driver.set_in(slot, STATUS, slot as u32);
        }
        let statuses: Vec<u32> = (0..6).map(|slot| driver.get_in(slot, STATUS)).collect();
        assert_eq!(statuses, [DRIVER_OK, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn without_a_network_device_its_slot_is_empty() {
        let memory = memory();
        let parts = Parts {
            disks: vec![Block::new(vec![0; 512], b"a").unwrap()],
            ..no_parts()
        };
        let mut driver = Driver::new(&memory, parts, None, None);
        driver.set_in(2, STATUS, 1);
        assert_eq!(driver.get_in(2, DEVICE_ID), u32::MAX);
        assert_eq!(driver.get_in(1, STATUS), 0);
    }

    #[test]
    fn the_serial_port_logs_and_answers_its_own_registers() {
        let memory = memory();
        let mut machine = Driver::new(&memory, no_parts(), None, None).machine;
        machine.write(0x3f8, b"h");
        assert_eq!(machine.serial.output(), b"h");
        let mut status = [0];
        assert!(machine.read(0x3fd, &mut status));
        assert_eq!(status[0] & 0x60, 0x60);
        // A port with nothing behind it reads as all ones.
        let mut nothing = [0];
        assert!(machine.read(0x80, &mut nothing));
        assert_eq!(nothing, [0xff]);
    }
}
