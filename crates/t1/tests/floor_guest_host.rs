//! The floor: the guest can't run anything on the host.
//!
//! The ways out of the guest, for what exists at this milestone, are the
//! channel, the virtio queues and registers, unclaimed port and MMIO exits,
//! output that reaches the user's terminal, and host state visible from
//! inside. KVM and the CPU are the trust base, and are out of scope.
//!
//! Each attack runs in its own guest, through the API `ap boot` uses, in its
//! own process under a seccomp filter that kills it on any fork or exec. The
//! filter acts at the system call, so a child that exits at once and an exec
//! that replaces the process can't slip between samples.
//!
//! Linux in the guest has no `/dev/mem`, `iopl`, modules, or kexec, so root
//! there can't reach a port or a queue directly. Those attacks are a small
//! kernel of their own, which is what a guest whose kernel has been taken over
//! could run.

use ap::boot::{self, Guest, Report};
use ap::filter::Filter;
use guest::protocol::{MAX_BODY, MAX_OUTPUT, ToHost};
use std::env;
use std::ffi::{c_int, c_ulong};
use std::fs::{self, File};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use t1::{guest_binary, kernel};
use vmm::initramfs::Entry;

const OWNER_DIRECTORY: &str = "AP_FLOOR_OWNER_DIRECTORY";
const OWNER_VMM: &str = "AP_FLOOR_OWNER_VMM";
const OWNER_KERNEL: &str = "AP_FLOOR_OWNER_KERNEL";
const OWNER_PROBE: &str = "AP_FLOOR_OWNER_PROBE";
const CANARY_VARIABLE: &str = "AP_FLOOR_CANARY";
const OWNER_TEST: &str = "vm_owner_process";
const TIMEOUT: Duration = Duration::from_secs(30);
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
const SIGSYS: i32 = 31;

const TAG_RUN: u8 = 1;
const TAG_EXITED: u8 = 4;
const TAG_READY: u8 = 5;
const NO_SUCH_TAGS: [u8; 3] = [0, 6, 255];

/// What the guest is told to do, in the files `guest-probe` reads.
enum Attack {
    Channel {
        before: Vec<u8>,
        after: Vec<u8>,
        await_run: bool,
        stop_early: bool,
    },
    Scan,
    /// A kernel of its own, with no Linux under it.
    Kernel(Vec<u8>),
}

/// How a case must end.
enum Expect {
    Status(u8),
    Refused(&'static str),
}

struct Case {
    name: String,
    attack: Attack,
    /// Whether `ap` asks the guest to run a command.
    command: bool,
    expect: Expect,
    /// Bytes the host must have received, which only a run case sends.
    marker: Option<Vec<u8>>,
    /// For the one case that ends at its timeout.
    short_timeout: bool,
}

/// Which VMM runs the case. All but `Real` live only in this file, and prove
/// that the check can fail.
#[derive(Clone, Copy, PartialEq)]
enum Vmm {
    Real,
    /// Creates the file a guest names, without starting a process.
    PlantedWritesFile,
    /// Starts a child that exits at once.
    PlantedStartsChild,
}

impl Vmm {
    fn name(self) -> &'static str {
        match self {
            Vmm::Real => "real",
            Vmm::PlantedWritesFile => "writes-file",
            Vmm::PlantedStartsChild => "starts-child",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "writes-file" => Vmm::PlantedWritesFile,
            "starts-child" => Vmm::PlantedStartsChild,
            _ => Vmm::Real,
        }
    }
}

/// What one case left behind.
struct Observation {
    /// `None` if the VM owner never recorded how the machine ended.
    outcome: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: ExitStatus,
    tripwire_exists: bool,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Decides one case. The order matters only for which reason is reported.
fn judge(
    case: &Case,
    seen: &Observation,
    canary: &[u8],
    host_strings: &[Vec<u8>],
) -> Result<(), String> {
    let name = &case.name;
    if seen.exit.signal() == Some(SIGSYS) {
        return Err(format!("{name}: the VM owner tried to fork or exec"));
    }
    if !seen.exit.success() {
        return Err(format!("{name}: the VMM died: {:?}", seen.exit));
    }
    if seen.tripwire_exists {
        return Err(format!(
            "{name}: the guest made the host create the tripwire"
        ));
    }
    let streams = [&seen.stdout, &seen.stderr];
    if streams.iter().any(|stream| contains(stream, canary)) {
        return Err(format!("{name}: the host's canary reached the guest"));
    }
    for string in host_strings {
        if streams.iter().any(|stream| contains(stream, string)) {
            let string = String::from_utf8_lossy(string);
            return Err(format!("{name}: the guest can see the host's {string:?}"));
        }
    }
    for control in FORBIDDEN_CONTROLS {
        if streams.iter().any(|stream| contains(stream, control)) {
            return Err(format!(
                "{name}: the forbidden control {control:?} reached the host"
            ));
        }
    }
    let Some(outcome) = &seen.outcome else {
        return Err(format!("{name}: the case didn't report how it ended"));
    };
    let as_expected = match case.expect {
        Expect::Status(status) => *outcome == format!("status {status}"),
        Expect::Refused(reason) => outcome.starts_with("refused") && outcome.contains(reason),
    };
    if !as_expected {
        return Err(format!("{name}: unexpected result: {outcome}"));
    }
    if let Some(marker) = &case.marker
        && !contains(&seen.stdout, marker)
    {
        return Err(format!("{name}: the guest's report never arrived"));
    }
    Ok(())
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
    bytes.push(tag);
    bytes.extend_from_slice(payload);
    bytes
}

fn encoded(message: &ToHost) -> Vec<u8> {
    let mut bytes = Vec::new();
    message.write(&mut bytes).unwrap();
    bytes
}

fn ready() -> Vec<u8> {
    encoded(&ToHost::Ready(b"probe".to_vec()))
}

fn stdout(bytes: &[u8]) -> Vec<u8> {
    encoded(&ToHost::Stdout(bytes.to_vec()))
}

fn stderr(bytes: &[u8]) -> Vec<u8> {
    encoded(&ToHost::Stderr(bytes.to_vec()))
}

fn exited() -> Vec<u8> {
    encoded(&ToHost::Exited(0))
}

/// A case that sends `before` first, with no command asked for.
fn before_ready(name: &str, before: Vec<u8>, reason: &'static str) -> Case {
    Case {
        name: format!("before-ready/{name}"),
        attack: Attack::Channel {
            before,
            after: Vec::new(),
            await_run: false,
            stop_early: false,
        },
        command: true,
        expect: Expect::Refused(reason),
        marker: None,
        short_timeout: false,
    }
}

/// A case that reports in, and then sends `attack` as a running command.
fn while_running(name: &str, attack: Vec<u8>, expect: Expect) -> Case {
    let name = format!("while-running/{name}");
    let marker = format!("ran {name}\n").into_bytes();
    let mut after = stdout(&marker);
    after.extend(attack);
    Case {
        name,
        attack: Attack::Channel {
            before: ready(),
            after,
            await_run: true,
            stop_early: false,
        },
        command: true,
        expect,
        marker: Some(marker),
        short_timeout: false,
    }
}

/// A frame that never finishes, from a guest that then powers off.
fn truncated_frame() -> Case {
    let mut case = while_running(
        "truncated",
        100_u32
            .to_le_bytes()
            .iter()
            .chain(b"abc")
            .copied()
            .collect(),
        Expect::Refused("before its command finished"),
    );
    if let Attack::Channel { stop_early, .. } = &mut case.attack {
        *stop_early = true;
    }
    case
}

/// Refused with the channel's reason, which only a frame that arrived earns.
const REFUSED_FRAME: Expect = Expect::Refused("didn't report ready");

fn channel_cases(tripwire: &Path) -> Vec<Case> {
    let path = tripwire.display();
    let spaced = format!("touch {path}; sh -c 'touch {path}'; $(touch {path}); `touch {path}`");
    let spaced = spaced.as_bytes();
    let printable = format!("$(touch${{IFS}}{path});touch${{IFS}}{path}");
    let not_ready = "didn't report ready";

    let mut cases = vec![
        before_ready(
            "ready-with-a-command",
            frame(TAG_READY, spaced),
            "invalid release",
        ),
        before_ready("stdout", stdout(spaced), not_ready),
        before_ready("stderr", stderr(spaced), not_ready),
        before_ready("exited", exited(), not_ready),
        before_ready("run", frame(TAG_RUN, spaced), not_ready),
        before_ready("empty-body", 0_u32.to_le_bytes().to_vec(), not_ready),
        before_ready(
            "oversized",
            ((MAX_BODY + 1) as u32).to_le_bytes().to_vec(),
            not_ready,
        ),
        // A release is shown to the user, and this one has no bad characters.
        // It's accepted, and must still do nothing.
        Case {
            name: "before-ready/printable-ready".to_owned(),
            attack: Attack::Channel {
                before: frame(TAG_READY, printable.as_bytes()),
                after: exited(),
                await_run: true,
                stop_early: false,
            },
            command: true,
            expect: Expect::Status(0),
            marker: None,
            short_timeout: false,
        },
        while_running(
            "stdout",
            [stdout(spaced), exited()].concat(),
            Expect::Status(0),
        ),
        while_running(
            "stderr",
            [stderr(spaced), exited()].concat(),
            Expect::Status(0),
        ),
        while_running("second-ready", frame(TAG_READY, spaced), REFUSED_FRAME),
        while_running(
            "exited-with-a-command",
            frame(TAG_EXITED, spaced),
            REFUSED_FRAME,
        ),
        while_running("exited-empty", frame(TAG_EXITED, b""), REFUSED_FRAME),
        while_running(
            "run-sent-to-the-host",
            frame(TAG_RUN, spaced),
            REFUSED_FRAME,
        ),
        while_running("empty-body", 0_u32.to_le_bytes().to_vec(), REFUSED_FRAME),
        while_running(
            "oversized",
            ((MAX_BODY + 1) as u32).to_le_bytes().to_vec(),
            REFUSED_FRAME,
        ),
        while_running(
            "largest-length",
            u32::MAX.to_le_bytes().to_vec(),
            REFUSED_FRAME,
        ),
        truncated_frame(),
        while_running(
            "largest-output",
            [stdout(&vec![b'a'; MAX_OUTPUT]), exited()].concat(),
            Expect::Status(0),
        ),
    ];
    for tag in NO_SUCH_TAGS {
        cases.push(before_ready(
            &format!("unknown-tag-{tag}"),
            frame(tag, spaced),
            not_ready,
        ));
        cases.push(while_running(
            &format!("unknown-tag-{tag}"),
            frame(tag, spaced),
            REFUSED_FRAME,
        ));
    }
    cases
}

/// Controls that must not reach the terminal, each complete.
const FORBIDDEN_CONTROLS: [&[u8]; 11] = [
    b"\x1b]52;c;cHduZWQ=\x07",
    b"\x1b]52;c;?\x1b\\",
    b"\x9d52;c;?\x9c",
    b"\xc2\x9d52;c;?\xc2\x9c",
    b"\x1b]5522;type=read\x07",
    b"\x1b[20t",
    b"\x1b[21t",
    b"\x9b21t",
    b"\xc2\x9b21t",
    b"\x1bP$qm\x1b\\",
    b"\x90$qm\x9c",
];

/// Queries about the terminal's own state, which interfaces wait on.
const ALLOWED_CONTROLS: [&[u8]; 4] = [
    b"\x1b[6n",
    b"\x1b]11;?\x07",
    b"\x1b]10;?\x1b\\",
    b"\x1b[31m",
];

/// Every control on both streams, cut into frames small enough to split them.
fn output_case() -> Case {
    let mut text = Vec::new();
    for (forbidden, allowed) in FORBIDDEN_CONTROLS
        .iter()
        .zip(ALLOWED_CONTROLS.iter().cycle())
    {
        text.extend_from_slice(forbidden);
        text.extend_from_slice(b"text");
        text.extend_from_slice(allowed);
    }
    let mut after = Vec::new();
    for chunk in text.chunks(5) {
        after.extend(stdout(chunk));
        after.extend(stderr(chunk));
    }
    after.extend(exited());
    Case {
        name: "output/controls".to_owned(),
        attack: Attack::Channel {
            before: ready(),
            after,
            await_run: true,
            stop_early: false,
        },
        command: true,
        expect: Expect::Status(0),
        marker: None,
        short_timeout: false,
    }
}

const DEVICE: u32 = 0xd000_0000;
const DEVICE_END: u32 = 0xd000_1000;
const PAST_RAM: u32 = 0x1000_0000;
const HIGH_HOLE: u32 = 0xf000_0000;
const KERNEL_ADDRESS: u64 = 0x20_0000;
const KERNEL_OFFSET: usize = 0x1000;
const NOTE_OFFSET: usize = 0x100;
const DESCRIPTORS: u32 = 0x30_0000;
const AVAILABLE: u32 = 0x30_1000;
const USED: u32 = 0x30_2000;
const QUEUE_SIZE: u32 = 8;
const NEXT: u16 = 1;
const INDIRECT: u16 = 4;

/// Hand-assembled 32-bit x86, as the PVH entry finds the CPU: paging off,
/// flat segments, and interrupts masked.
#[derive(Default)]
struct Code(Vec<u8>);

impl Code {
    fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.extend_from_slice(bytes);
        self
    }

    /// `mov dword [address], value`
    fn store(&mut self, address: u32, value: u32) -> &mut Self {
        self.bytes(&[0xc7, 0x05]);
        self.bytes(&address.to_le_bytes());
        self.bytes(&value.to_le_bytes())
    }

    fn register(&mut self, offset: u32, value: u32) -> &mut Self {
        self.store(DEVICE + offset, value)
    }

    /// `mov eax, [address]`
    fn load(&mut self, address: u32) -> &mut Self {
        self.bytes(&[0xa1]);
        self.bytes(&address.to_le_bytes())
    }

    /// Runs `operation`, one port-sized instruction on `dx`, over every port.
    /// `mov edx, 0; L: op; inc edx; cmp edx, 0x10000; jne L`
    fn every_port(&mut self, operation: u8) -> &mut Self {
        self.bytes(&[0xb0, 0xa5, 0xba, 0, 0, 0, 0, operation, 0x42, 0x81, 0xfa]);
        self.bytes(&0x1_0000_u32.to_le_bytes());
        self.bytes(&[0x75, 0xf6])
    }

    /// Runs `operation` at each dword from `start` up to `end`. An operation
    /// is the two bytes of an instruction whose operand is `[ecx]`.
    /// `mov ecx, start; L: op; add ecx, 4; cmp ecx, end; jne L`
    fn every_dword(&mut self, operation: [u8; 2], start: u32, end: u32) -> &mut Self {
        self.bytes(&[0xb8, 0xa5, 0xa5, 0xa5, 0xa5, 0xb9]);
        self.bytes(&start.to_le_bytes());
        self.bytes(&operation);
        self.bytes(&[0x83, 0xc1, 0x04, 0x81, 0xf9]);
        self.bytes(&end.to_le_bytes());
        self.bytes(&[0x75, 0xf3])
    }

    /// Ends the guest with `ud2`, which is a triple fault with no interrupt
    /// table. `hlt` can't end one: with interrupts masked and KVM's interrupt
    /// controllers, it sleeps in the kernel until the timeout.
    fn stop(&mut self) -> Vec<u8> {
        self.bytes(&[0x0f, 0x0b]);
        std::mem::take(&mut self.0)
    }

    /// Spins forever, so that only the timeout ends the guest.
    fn spin(&mut self) -> Vec<u8> {
        self.bytes(&[0xeb, 0xfe]);
        std::mem::take(&mut self.0)
    }

    /// Takes the device through feature negotiation, with transmit queue
    /// tables at `descriptors`.
    fn start_device(&mut self, descriptors: u64) -> &mut Self {
        self.register(0x070, 1)
            .register(0x070, 3)
            .register(0x024, 1)
            .register(0x020, 1)
            .register(0x070, 11)
            .register(0x030, 1)
            .register(0x038, QUEUE_SIZE)
            .register(0x080, descriptors as u32)
            .register(0x084, (descriptors >> 32) as u32)
            .register(0x090, AVAILABLE)
            .register(0x094, 0)
            .register(0x0a0, USED)
            .register(0x0a4, 0)
            .register(0x044, 1)
            .register(0x070, 15)
    }

    fn descriptor(
        &mut self,
        index: u32,
        address: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) -> &mut Self {
        let at = DESCRIPTORS + 16 * index;
        self.store(at, address as u32)
            .store(at + 4, (address >> 32) as u32)
            .store(at + 8, len)
            .store(at + 12, u32::from(flags) | u32::from(next) << 16)
    }

    /// Makes `head` available, as ring entry `index - 1`, and rings the bell.
    fn publish(&mut self, head: u32, index: u32) -> &mut Self {
        self.store(AVAILABLE, index << 16)
            .store(AVAILABLE + 4, head)
            .register(0x050, 1)
            // The read proves the VMM came back from handling the bell.
            .load(DEVICE + 0x070)
    }
}

/// An uncompressed `vmlinux` that runs `code` from its PVH entry.
fn kernel_with(code: &[u8]) -> Vec<u8> {
    let mut file = vec![0; KERNEL_OFFSET + code.len()];
    file[..4].copy_from_slice(b"\x7fELF");
    file[4] = 2;
    file[5] = 1;
    file[16] = 2;
    file[18] = 62;
    file[32] = 64;
    file[54] = 56;
    file[56] = 2;
    let mut note = Vec::new();
    note.extend(4_u32.to_le_bytes());
    note.extend(8_u32.to_le_bytes());
    note.extend(18_u32.to_le_bytes());
    note.extend(b"Xen\0");
    note.extend(KERNEL_ADDRESS.to_le_bytes());
    let header =
        |file: &mut Vec<u8>, at: usize, kind: u32, offset: u64, address: u64, size: u64| {
            file[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            file[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            file[at + 24..at + 32].copy_from_slice(&address.to_le_bytes());
            file[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
            file[at + 40..at + 48].copy_from_slice(&size.to_le_bytes());
        };
    let size = code.len() as u64;
    header(&mut file, 64, 1, KERNEL_OFFSET as u64, KERNEL_ADDRESS, size);
    header(&mut file, 120, 4, NOTE_OFFSET as u64, 0, note.len() as u64);
    file[NOTE_OFFSET..NOTE_OFFSET + note.len()].copy_from_slice(&note);
    file[KERNEL_OFFSET..].copy_from_slice(code);
    file
}

/// A case whose whole guest is `code`. A VMM that survives reports the guest's
/// stop as a power-off. The control case shows that no instruction before the
/// stop faults on its own.
fn kernel_case(name: &str, code: Vec<u8>, reason: &'static str) -> Case {
    Case {
        name: format!("kernel/{name}"),
        attack: Attack::Kernel(kernel_with(&code)),
        command: false,
        expect: Expect::Refused(reason),
        marker: None,
        short_timeout: false,
    }
}

fn device_cases() -> Vec<Case> {
    const STOPPED: &str = "before its init reported ready";
    const WRITE: [u8; 2] = [0x89, 0x01];
    const READ: [u8; 2] = [0x8b, 0x01];
    const OUT: u8 = 0xee;
    const IN: u8 = 0xec;

    let far = 1_u64 << 40;
    let descriptor = |name: &str, build: &dyn Fn(&mut Code)| {
        let mut code = Code::default();
        code.start_device(u64::from(DESCRIPTORS));
        build(&mut code);
        kernel_case(&format!("queue/{name}"), code.stop(), STOPPED)
    };
    let mut cases = vec![
        descriptor("buffer-outside-ram", &|code| {
            code.descriptor(0, far, 4096, 0, 0).publish(0, 1);
        }),
        descriptor("length-that-wraps", &|code| {
            code.descriptor(0, u64::MAX - 7, 16, 0, 0).publish(0, 1);
        }),
        descriptor("chain-that-loops", &|code| {
            code.descriptor(0, 0x31_0000, 16, NEXT, 0).publish(0, 1);
        }),
        descriptor("longer-loop", &|code| {
            code.descriptor(0, 0x31_0000, 16, NEXT, 1)
                .descriptor(1, 0x31_0000, 16, NEXT, 0)
                .publish(0, 1);
        }),
        descriptor("next-outside-the-table", &|code| {
            code.descriptor(0, 0x31_0000, 16, NEXT, u16::MAX)
                .publish(0, 1);
        }),
        descriptor("head-outside-the-table", &|code| {
            code.publish(u32::from(u16::MAX), 1);
        }),
        descriptor("index-more-than-a-ring-ahead", &|code| {
            code.publish(0, 0xfff0);
        }),
        descriptor("indirect-descriptor", &|code| {
            code.descriptor(0, 0x31_0000, 16, INDIRECT, 0).publish(0, 1);
        }),
    ];
    let mut code = Code::default();
    code.start_device(far).publish(0, 1);
    cases.push(kernel_case("queue/table-outside-ram", code.stop(), STOPPED));

    let mut code = Code::default();
    code.every_port(OUT).every_port(IN);
    cases.push(kernel_case("every-port", code.stop(), STOPPED));

    let mut code = Code::default();
    code.every_dword(READ, DEVICE, DEVICE_END)
        .every_dword(WRITE, DEVICE, DEVICE_END);
    cases.push(kernel_case("every-device-register", code.stop(), STOPPED));

    let mut code = Code::default();
    code.every_dword(WRITE, DEVICE_END, DEVICE_END + 0x1000)
        .every_dword(READ, DEVICE_END, DEVICE_END + 0x1000)
        .every_dword(WRITE, PAST_RAM, PAST_RAM + 0x1000)
        .every_dword(READ, PAST_RAM, PAST_RAM + 0x1000)
        .every_dword(WRITE, HIGH_HOLE, HIGH_HOLE + 0x1000)
        .every_dword(READ, HIGH_HOLE, HIGH_HOLE + 0x1000);
    cases.push(kernel_case("unclaimed-mmio", code.stop(), STOPPED));

    let mut code = Code::default();
    // `vmcall`, which KVM answers without leaving the guest stopped.
    code.bytes(&[0x0f, 0x01, 0xc1]);
    cases.push(kernel_case("hypercall", code.stop(), STOPPED));

    // Every kind of instruction above, ending in a spin. A fault would end
    // this guest early, as a power-off, instead of at the timeout.
    let mut code = Code::default();
    code.start_device(u64::from(DESCRIPTORS))
        .descriptor(0, far, 4096, NEXT, 0)
        .publish(0, 1)
        .every_port(OUT)
        .every_port(IN)
        .every_dword(READ, DEVICE, DEVICE_END)
        .every_dword(WRITE, DEVICE, DEVICE_END)
        .bytes(&[0x0f, 0x01, 0xc1]);
    let mut control = kernel_case("control", code.spin(), "didn't finish");
    control.short_timeout = true;
    cases.push(control);
    cases
}

fn cases(tripwire: &Path) -> Vec<Case> {
    let mut cases = channel_cases(tripwire);
    cases.push(output_case());
    cases.push(Case {
        name: "scan/host-state".to_owned(),
        attack: Attack::Scan,
        command: true,
        expect: Expect::Status(0),
        // Proves the scan read the guest's own command line.
        marker: Some(b"virtio_mmio.device=4K@0xd0000000:5".to_vec()),
        short_timeout: false,
    });
    cases.extend(device_cases());
    cases
}

fn probe_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| guest_binary::build("guest-probe"))
}

fn kernel_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(kernel::vmlinux)
}

fn write_case_files(directory: &Path, case: &Case) {
    fs::create_dir_all(directory).unwrap();
    if case.short_timeout {
        fs::write(directory.join("short-timeout"), "").unwrap();
    }
    fs::write(
        directory.join("command"),
        if case.command { "1" } else { "" },
    )
    .unwrap();
    match &case.attack {
        Attack::Channel {
            before,
            after,
            await_run,
            stop_early,
        } => {
            fs::write(directory.join("before"), before).unwrap();
            fs::write(directory.join("after"), after).unwrap();
            if *await_run {
                fs::write(directory.join("await-run"), "").unwrap();
            }
            if *stop_early {
                fs::write(directory.join("stop-early"), "").unwrap();
            }
        }
        Attack::Scan => {
            fs::write(directory.join("before"), ready()).unwrap();
            fs::write(directory.join("await-run"), "").unwrap();
            fs::write(directory.join("scan"), "").unwrap();
        }
        Attack::Kernel(image) => fs::write(directory.join("kernel"), image).unwrap(),
    }
}

/// Runs one case in its own process, and reads what it left.
fn observe(directory: &Path, case: &Case, vmm: Vmm, canary: &str, tripwire: &Path) -> Observation {
    write_case_files(directory, case);
    let exit = Command::new(env::current_exe().unwrap())
        .args([OWNER_TEST, "--exact", "--test-threads=1"])
        .current_dir(directory)
        .env(OWNER_DIRECTORY, directory)
        .env(OWNER_VMM, vmm.name())
        .env(OWNER_KERNEL, kernel_path())
        .env(OWNER_PROBE, probe_path())
        .env(CANARY_VARIABLE, canary)
        .stdout(File::create(directory.join("owner.out")).unwrap())
        .stderr(File::create(directory.join("owner.err")).unwrap())
        .status()
        .unwrap();
    Observation {
        outcome: fs::read_to_string(directory.join("outcome")).ok(),
        stdout: fs::read(directory.join("stdout")).unwrap_or_default(),
        stderr: fs::read(directory.join("stderr")).unwrap_or_default(),
        exit,
        tripwire_exists: tripwire.exists(),
    }
}

fn check(label: &str, vmm: Vmm) -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/floor-guest-host")
        .join(format!("{label}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let tripwire = root.join("tripwire");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let canary = format!("canary-{nanos:x}");
    let mut host_strings = vec![
        canary.clone().into_bytes(),
        root.as_os_str().as_encoded_bytes().to_vec(),
    ];
    if let Some(home) = env::var_os("HOME").filter(|home| home.len() > 1) {
        host_strings.push(home.as_encoded_bytes().to_vec());
    }

    let result = cases(&tripwire)
        .iter()
        .enumerate()
        .try_for_each(|(index, case)| {
            let directory = root.join(format!("case-{index}"));
            let seen = observe(&directory, case, vmm, &canary, &tripwire);
            println!(
                "{}: {}",
                case.name,
                seen.outcome.as_deref().unwrap_or("no result")
            );
            // Paths are only secret from the scan. The others were handed the
            // tripwire's path to attack with.
            let searched: &[Vec<u8>] = match case.attack {
                Attack::Scan => &host_strings,
                _ => &[],
            };
            judge(case, &seen, canary.as_bytes(), searched)?;
            check_allowed_controls(case, &seen)
        });
    if result.is_ok() {
        let _ = fs::remove_dir_all(&root);
    }
    result
}

fn check_allowed_controls(case: &Case, seen: &Observation) -> Result<(), String> {
    if case.name != "output/controls" {
        return Ok(());
    }
    for control in ALLOWED_CONTROLS {
        for stream in [&seen.stdout, &seen.stderr] {
            if !contains(stream, control) {
                return Err(format!("the allowed control {control:?} didn't arrive"));
            }
        }
    }
    Ok(())
}

// seccomp and BPF, from <linux/seccomp.h> and <linux/filter.h>.
const PR_SET_SECCOMP: c_int = 22;
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const SECCOMP_MODE_FILTER: c_ulong = 2;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO_ENOSYS: u32 = 0x0005_0000 | 38;
const RET_ALLOW: u32 = 0x7fff_0000;
const LOAD_WORD: u16 = 0x20;
const JUMP_EQUAL: u16 = 0x15;
const JUMP_AT_LEAST: u16 = 0x35;
const JUMP_SET: u16 = 0x45;
const RETURN: u16 = 0x06;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const X32_SYSCALLS: u32 = 0x4000_0000;
const CLONE_THREAD: u32 = 0x0001_0000;
const SYS_CLONE: u32 = 56;
const SYS_FORK: u32 = 57;
const SYS_VFORK: u32 = 58;
const SYS_EXECVE: u32 = 59;
const SYS_EXECVEAT: u32 = 322;
const SYS_CLONE3: u32 = 435;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0: u32 = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct Instruction {
    code: u16,
    if_true: u8,
    if_false: u8,
    value: u32,
}

#[repr(C)]
struct Program {
    len: u16,
    instructions: *const Instruction,
}

unsafe extern "C" {
    fn prctl(option: c_int, ...) -> c_int;
}

const fn op(code: u16, if_true: u8, if_false: u8, value: u32) -> Instruction {
    Instruction {
        code,
        if_true,
        if_false,
        value,
    }
}

/// Kills the process at any attempt to start another process or program.
/// Threads stay allowed, because the VMM's watchdog is one. `clone3` hides its
/// flags from a filter, so it answers "not implemented", and libc falls back
/// to `clone`, whose flags can be read.
const FILTER: [Instruction; 15] = [
    op(LOAD_WORD, 0, 0, OFFSET_ARCH),
    op(JUMP_EQUAL, 0, 11, AUDIT_ARCH_X86_64),
    op(LOAD_WORD, 0, 0, OFFSET_NR),
    op(JUMP_AT_LEAST, 9, 0, X32_SYSCALLS),
    op(JUMP_EQUAL, 9, 0, SYS_CLONE3),
    op(JUMP_EQUAL, 7, 0, SYS_FORK),
    op(JUMP_EQUAL, 6, 0, SYS_VFORK),
    op(JUMP_EQUAL, 5, 0, SYS_EXECVE),
    op(JUMP_EQUAL, 4, 0, SYS_EXECVEAT),
    op(JUMP_EQUAL, 0, 2, SYS_CLONE),
    op(LOAD_WORD, 0, 0, OFFSET_ARG0),
    op(JUMP_SET, 0, 1, CLONE_THREAD),
    op(RETURN, 0, 0, RET_ALLOW),
    op(RETURN, 0, 0, RET_KILL_PROCESS),
    op(RETURN, 0, 0, RET_ERRNO_ENOSYS),
];

fn forbid_new_processes() {
    let program = Program {
        len: FILTER.len() as u16,
        instructions: FILTER.as_ptr(),
    };
    // SAFETY: both calls match the prctl signatures for these options, and
    // `program` and the filter it points to outlive the second call, which
    // copies them.
    unsafe {
        assert_eq!(
            prctl(
                PR_SET_NO_NEW_PRIVS,
                1 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong
            ),
            0
        );
        assert_eq!(
            prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &raw const program),
            0
        );
    }
}

/// What a planted device does with what the guest sent.
fn plant(vmm: Vmm, bytes: &[u8]) {
    let Some(start) = bytes.windows(6).position(|window| window == b"touch ") else {
        return;
    };
    match vmm {
        Vmm::Real => {}
        Vmm::PlantedWritesFile => {
            let path = bytes[start + 6..]
                .split(|byte| matches!(byte, b' ' | b';' | b'\n'))
                .next()
                .unwrap();
            let _ = fs::write(String::from_utf8_lossy(path).as_ref(), "");
        }
        Vmm::PlantedStartsChild => {
            let _ = Command::new("/bin/true").status();
        }
    }
}

/// The process that owns the VM. It does nothing in a normal test run.
#[test]
fn vm_owner_process() {
    let Some(directory) = env::var_os(OWNER_DIRECTORY).map(PathBuf::from) else {
        return;
    };
    let vmm = Vmm::from_name(&env::var(OWNER_VMM).unwrap());
    let optional = |name: &str| fs::read(directory.join(name)).ok();
    let kernel =
        optional("kernel").unwrap_or_else(|| fs::read(env::var_os(OWNER_KERNEL).unwrap()).unwrap());
    let init = fs::read(env::var_os(OWNER_PROBE).unwrap()).unwrap();
    let before = optional("before");
    let after = optional("after");
    let empty: &[u8] = b"";
    let mut files = Vec::new();
    for (path, contents) in [("before", &before), ("after", &after)] {
        if let Some(contents) = contents {
            files.push(Entry::File {
                path,
                mode: 0o644,
                contents,
            });
        }
    }
    for marker in ["await-run", "stop-early", "scan"] {
        if directory.join(marker).exists() {
            files.push(Entry::File {
                path: marker,
                mode: 0o644,
                contents: empty,
            });
        }
    }
    let wants_command = !fs::read(directory.join("command")).unwrap().is_empty();
    let command = [b"anything".to_vec()];
    let guest = Guest {
        kernel: &kernel,
        init: &init,
        files: &files,
        command: wants_command.then_some(command.as_slice()),
        timeout: if directory.join("short-timeout").exists() {
            SHORT_TIMEOUT
        } else {
            TIMEOUT
        },
    };

    forbid_new_processes();

    let (mut stdout_filter, mut stderr_filter) = (Filter::default(), Filter::default());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = boot::boot(&guest, |report| match report {
        Report::Ready(_) => {}
        Report::Stdout(bytes) => {
            plant(vmm, bytes);
            out.extend(stdout_filter.push(bytes));
        }
        Report::Stderr(bytes) => err.extend(stderr_filter.push(bytes)),
    });
    let outcome = match result {
        Ok(status) => format!("status {}", status.unwrap_or(0)),
        Err(error) => format!("refused: {error}"),
    };
    fs::write(directory.join("stdout"), out).unwrap();
    fs::write(directory.join("stderr"), err).unwrap();
    fs::write(directory.join("outcome"), outcome).unwrap();
}

#[test]
fn floor_guest_host() {
    check("real", Vmm::Real).unwrap();
}

#[test]
fn the_floor_fails_when_a_device_creates_what_the_guest_names() {
    let error = check("planted-file", Vmm::PlantedWritesFile).unwrap_err();
    assert!(error.contains("tripwire"), "{error}");
}

#[test]
fn the_floor_fails_when_a_device_starts_a_child_that_exits_at_once() {
    let error = check("planted-child", Vmm::PlantedStartsChild).unwrap_err();
    assert!(error.contains("fork or exec"), "{error}");
}

fn clean() -> Observation {
    Observation {
        outcome: Some("status 0".to_owned()),
        stdout: b"ran it".to_vec(),
        stderr: Vec::new(),
        exit: ExitStatus::from_raw(0),
        tripwire_exists: false,
    }
}

fn sample() -> Case {
    Case {
        name: "sample".to_owned(),
        attack: Attack::Scan,
        command: true,
        expect: Expect::Status(0),
        marker: Some(b"ran".to_vec()),
        short_timeout: false,
    }
}

#[track_caller]
fn assert_judged(change: impl FnOnce(&mut Observation), reason: &str) {
    let mut seen = clean();
    change(&mut seen);
    let error = judge(&sample(), &seen, b"CANARY", &[b"/host".to_vec()]).unwrap_err();
    assert!(error.contains(reason), "{error}");
}

#[test]
fn a_clean_case_passes_the_judge() {
    assert_eq!(
        judge(&sample(), &clean(), b"CANARY", &[b"/host".to_vec()]),
        Ok(())
    );
}

#[test]
fn the_judge_fails_each_way_out() {
    assert_judged(|seen| seen.outcome = None, "didn't report");
    assert_judged(
        |seen| seen.outcome = Some("status 3".to_owned()),
        "unexpected",
    );
    assert_judged(|seen| seen.stdout = b"nothing".to_vec(), "never arrived");
    assert_judged(
        |seen| seen.exit = ExitStatus::from_raw(SIGSYS),
        "fork or exec",
    );
    assert_judged(|seen| seen.exit = ExitStatus::from_raw(6), "VMM died");
    assert_judged(|seen| seen.exit = ExitStatus::from_raw(1 << 8), "VMM died");
    assert_judged(|seen| seen.tripwire_exists = true, "tripwire");
    assert_judged(|seen| seen.stderr = b"xCANARYx".to_vec(), "canary");
    assert_judged(|seen| seen.stdout = b"ran /host/x".to_vec(), "host's");
    assert_judged(
        |seen| seen.stderr = b"\x1b[21t".to_vec(),
        "forbidden control",
    );
}
