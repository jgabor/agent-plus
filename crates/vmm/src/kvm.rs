//! KVM, through ioctls declared by hand.
//!
//! The structs mirror `linux/kvm.h` for x86_64. Their layouts are asserted
//! below, and each ioctl number encodes the size of the struct it takes.

use crate::GuestMemory;
use crate::sys::{self, Mapping};
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

const KVM_API_VERSION: c_int = 12;

// _IO(0xAE, 0x00)
const KVM_GET_API_VERSION: c_ulong = 0xAE00;
// _IO(0xAE, 0x01)
const KVM_CREATE_VM: c_ulong = 0xAE01;
// _IO(0xAE, 0x04)
const KVM_GET_VCPU_MMAP_SIZE: c_ulong = 0xAE04;
// _IOWR(0xAE, 0x05, struct kvm_cpuid2)
const KVM_GET_SUPPORTED_CPUID: c_ulong = 0xC008_AE05;
// _IO(0xAE, 0x41)
const KVM_CREATE_VCPU: c_ulong = 0xAE41;
// _IO(0xAE, 0x47)
const KVM_SET_TSS_ADDR: c_ulong = 0xAE47;
// _IO(0xAE, 0x60)
const KVM_CREATE_IRQCHIP: c_ulong = 0xAE60;
// _IOW(0xAE, 0x77, struct kvm_pit_config)
const KVM_CREATE_PIT2: c_ulong = 0x4040_AE77;
// _IOW(0xAE, 0x90, struct kvm_cpuid2)
const KVM_SET_CPUID2: c_ulong = 0x4008_AE90;
// _IOW(0xAE, 0x46, struct kvm_userspace_memory_region)
const KVM_SET_USER_MEMORY_REGION: c_ulong = 0x4020_AE46;
// _IO(0xAE, 0x80)
const KVM_RUN: c_ulong = 0xAE80;
// _IOR(0xAE, 0x81, struct kvm_regs)
const KVM_GET_REGS: c_ulong = 0x8090_AE81;
// _IOW(0xAE, 0x82, struct kvm_regs)
const KVM_SET_REGS: c_ulong = 0x4090_AE82;
// _IOR(0xAE, 0x83, struct kvm_sregs)
const KVM_GET_SREGS: c_ulong = 0x8138_AE83;
// _IOW(0xAE, 0x84, struct kvm_sregs)
const KVM_SET_SREGS: c_ulong = 0x4138_AE84;

const KVM_EXIT_IO: u32 = 2;
const KVM_EXIT_HLT: u32 = 5;
const KVM_EXIT_MMIO: u32 = 6;
const KVM_EXIT_SHUTDOWN: u32 = 8;
const KVM_EXIT_IO_IN: u8 = 0;
const KVM_EXIT_IO_OUT: u8 = 1;

/// Three pages just below the top of the 32-bit space, which RAM never
/// reaches. Intel hosts need somewhere to keep a real-mode TSS.
const TSS_ADDRESS: c_ulong = 0xFFFB_D000;

/// More CPUID entries than any current host reports.
const MAX_CPUID_ENTRIES: usize = 256;

const EINTR: i32 = 4;

/// The signal that kicks a vCPU out of `KVM_RUN`. It is a real-time signal
/// above the ones glibc and musl keep for themselves.
const KICK_SIGNAL: c_int = 40;

/// `struct kvm_userspace_memory_region`.
#[repr(C)]
struct MemoryRegion {
    slot: u32,
    flags: u32,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

/// `struct kvm_pit_config`.
#[repr(C)]
#[derive(Default)]
struct PitConfig {
    flags: u32,
    padding: [u32; 15],
}

/// `struct kvm_cpuid_entry2`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CpuidEntry {
    function: u32,
    index: u32,
    flags: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
    padding: [u32; 3],
}

/// `struct kvm_cpuid2`, with room for its trailing entries.
#[repr(C)]
struct Cpuid {
    nent: u32,
    padding: u32,
    entries: [CpuidEntry; MAX_CPUID_ENTRIES],
}

/// `struct kvm_regs`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// `struct kvm_segment`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
    pub padding: u8,
}

/// `struct kvm_dtable`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Dtable {
    pub base: u64,
    pub limit: u16,
    pub padding: [u16; 3],
}

/// `struct kvm_sregs`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Sregs {
    pub cs: Segment,
    pub ds: Segment,
    pub es: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub ss: Segment,
    pub tr: Segment,
    pub ldt: Segment,
    pub gdt: Dtable,
    pub idt: Dtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    pub interrupt_bitmap: [u64; 4],
}

/// The `io` arm of the exit union in `struct kvm_run`.
#[repr(C)]
#[derive(Clone, Copy)]
struct IoExit {
    direction: u8,
    size: u8,
    port: u16,
    count: u32,
    data_offset: u64,
}

/// The start of `struct kvm_run`, up to the part of the union the VMM reads.
#[repr(C)]
struct KvmRun {
    request_interrupt_window: u8,
    immediate_exit: u8,
    padding: [u8; 6],
    exit_reason: u32,
    ready_for_interrupt_injection: u8,
    if_flag: u8,
    flags: u16,
    cr8: u64,
    apic_base: u64,
    io: IoExit,
}

const _: () = {
    assert!(size_of::<MemoryRegion>() == 32);
    assert!(offset_of!(MemoryRegion, userspace_addr) == 24);
    assert!(size_of::<PitConfig>() == 64);
    assert!(size_of::<CpuidEntry>() == 40);
    assert!(offset_of!(Cpuid, entries) == 8);
    assert!(size_of::<Regs>() == 144);
    assert!(offset_of!(Regs, rip) == 128);
    assert!(offset_of!(Regs, rflags) == 136);
    assert!(size_of::<Segment>() == 24);
    assert!(offset_of!(Segment, selector) == 12);
    assert!(offset_of!(Segment, unusable) == 22);
    assert!(size_of::<Dtable>() == 16);
    assert!(size_of::<Sregs>() == 312);
    assert!(offset_of!(Sregs, gdt) == 192);
    assert!(offset_of!(Sregs, cr0) == 224);
    assert!(offset_of!(Sregs, interrupt_bitmap) == 280);
    assert!(size_of::<IoExit>() == 16);
    assert!(offset_of!(IoExit, port) == 2);
    assert!(offset_of!(IoExit, data_offset) == 8);
    assert!(offset_of!(KvmRun, immediate_exit) == 1);
    assert!(offset_of!(KvmRun, exit_reason) == 8);
    assert!(offset_of!(KvmRun, io) == 32);
};

/// Why a vCPU stopped running.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The guest ran `hlt`.
    Halted,
    /// The guest triple-faulted. The kernel has no ACPI to power off with, so
    /// this is how it ends a VM: it restarts, and `reboot=t` on its command
    /// line makes that a triple fault at once.
    Shutdown,
    /// The timeout passed first.
    TimedOut,
    /// KVM exited with a `KVM_EXIT_*` reason the VMM doesn't handle.
    Unhandled(u32),
}

/// The devices behind a vCPU's I/O ports.
pub trait Ports {
    /// The guest wrote `data` to `port`.
    fn write(&mut self, port: u16, data: &[u8]);
    /// The guest reads `data` from `port`. Returns false if nothing answers,
    /// which stops the vCPU with [`Stop::Unhandled`].
    fn read(&mut self, port: u16, data: &mut [u8]) -> bool;
}

/// Port writes go to a callback, and reads have no answer.
struct WritesOnly<F>(F);

impl<F: FnMut(u16, &[u8])> Ports for WritesOnly<F> {
    fn write(&mut self, port: u16, data: &[u8]) {
        (self.0)(port, data);
    }

    fn read(&mut self, _: u16, _: &mut [u8]) -> bool {
        false
    }
}

/// Calls an ioctl that takes a plain integer, and returns its result.
///
/// # Safety
///
/// `request` must be an ioctl that doesn't treat `arg` as a pointer.
unsafe fn ioctl_value(fd: RawFd, request: c_ulong, arg: c_ulong) -> io::Result<c_int> {
    // SAFETY: the caller promises `arg` isn't used as a pointer.
    let result = unsafe { sys::ioctl(fd, request, arg) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result)
}

/// Calls an ioctl that takes a pointer to `T`.
///
/// # Safety
///
/// `request` must read or write exactly one `T` through its argument.
unsafe fn ioctl_pointer<T>(fd: RawFd, request: c_ulong, arg: *mut T) -> io::Result<()> {
    // SAFETY: the caller promises `arg` matches what `request` accesses.
    let result = unsafe { sys::ioctl(fd, request, arg.cast::<c_void>()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A virtual machine with one region of RAM at guest address 0.
pub struct Vm {
    kvm: File,
    fd: OwnedFd,
    vcpu_mmap_size: usize,
    memory: Rc<GuestMemory>,
    next_vcpu: c_ulong,
}

impl Vm {
    /// Creates a VM that runs on `memory`.
    pub fn new(memory: GuestMemory) -> io::Result<Self> {
        let file: File = OpenOptions::new().read(true).write(true).open("/dev/kvm")?;
        let kvm = file.as_raw_fd();
        // SAFETY: neither request takes a pointer.
        let version = unsafe { ioctl_value(kvm, KVM_GET_API_VERSION, 0)? };
        if version != KVM_API_VERSION {
            return Err(io::Error::other(format!(
                "KVM API version {version}, expected {KVM_API_VERSION}"
            )));
        }
        // SAFETY: as above.
        let vcpu_mmap_size = unsafe { ioctl_value(kvm, KVM_GET_VCPU_MMAP_SIZE, 0)? } as usize;
        if vcpu_mmap_size < size_of::<KvmRun>() {
            return Err(io::Error::other("the KVM run area is too small"));
        }
        // SAFETY: as above. Machine type 0 is the default.
        let fd = unsafe { ioctl_value(kvm, KVM_CREATE_VM, 0)? };
        // SAFETY: KVM_CREATE_VM returned a new descriptor that nothing owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut region = MemoryRegion {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: memory.size() as u64,
            userspace_addr: memory.start() as u64,
        };
        // SAFETY: `region` is the struct this request reads. The VM's
        // descriptor closes before `memory` unmaps: `Vm` and every `Vcpu` hold
        // both, and declare the descriptor first.
        unsafe { ioctl_pointer(fd.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, &raw mut region)? };

        Ok(Self {
            kvm: file,
            fd,
            vcpu_mmap_size,
            memory: Rc::new(memory),
            next_vcpu: 0,
        })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    /// Adds the interrupt controllers and timer a kernel expects. Call it
    /// before adding a vCPU. KVM then handles `hlt` itself, so a vCPU no longer
    /// stops with [`Stop::Halted`].
    pub fn add_interrupt_controllers(&self) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        // SAFETY: this request takes a guest address, not a pointer.
        unsafe { ioctl_value(fd, KVM_SET_TSS_ADDR, TSS_ADDRESS)? };
        // SAFETY: this request takes no argument.
        unsafe { ioctl_value(fd, KVM_CREATE_IRQCHIP, 0)? };
        let mut pit = PitConfig::default();
        // SAFETY: `pit` is the struct this request reads.
        unsafe { ioctl_pointer(fd, KVM_CREATE_PIT2, &raw mut pit) }?;
        Ok(())
    }

    /// Shows `vcpu` every CPU feature that KVM supports on this host. A vCPU
    /// starts with none, and a kernel refuses to run on that.
    pub fn expose_cpu_features(&self, vcpu: &Vcpu) -> io::Result<()> {
        let mut cpuid = Box::new(Cpuid {
            nent: MAX_CPUID_ENTRIES as u32,
            padding: 0,
            entries: [CpuidEntry::default(); MAX_CPUID_ENTRIES],
        });
        let cpuid = &raw mut *cpuid;
        // SAFETY: `cpuid` has room for the `nent` entries it declares, which
        // is as many as this request may write.
        unsafe { ioctl_pointer(self.kvm.as_raw_fd(), KVM_GET_SUPPORTED_CPUID, cpuid) }?;
        // SAFETY: the kernel wrote `nent` entries, within those that fit, and
        // this request reads only those.
        unsafe { ioctl_pointer(vcpu.fd.as_raw_fd(), KVM_SET_CPUID2, cpuid) }
    }

    /// Adds a vCPU. Use it from the thread that runs it.
    pub fn create_vcpu(&mut self) -> io::Result<Vcpu> {
        // SAFETY: this request takes a vCPU id, not a pointer.
        let fd = unsafe { ioctl_value(self.fd.as_raw_fd(), KVM_CREATE_VCPU, self.next_vcpu)? };
        // SAFETY: KVM_CREATE_VCPU returned a new descriptor that nothing owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let run = Mapping::shared(fd.as_fd(), self.vcpu_mmap_size)?;
        self.next_vcpu += 1;
        Ok(Vcpu {
            fd,
            run,
            _memory: Rc::clone(&self.memory),
        })
    }
}

/// One virtual CPU.
pub struct Vcpu {
    fd: OwnedFd,
    run: Mapping,
    /// A vCPU's descriptor keeps the kernel's VM alive, so guest RAM has to
    /// stay mapped as long as the vCPU exists. Otherwise the guest could reach
    /// whatever the host maps at that address next.
    _memory: Rc<GuestMemory>,
}

impl Vcpu {
    pub fn regs(&self) -> io::Result<Regs> {
        let mut regs = Regs::default();
        // SAFETY: `regs` is the struct this request writes.
        unsafe { ioctl_pointer(self.fd.as_raw_fd(), KVM_GET_REGS, &raw mut regs)? };
        Ok(regs)
    }

    pub fn set_regs(&self, regs: &Regs) -> io::Result<()> {
        let mut regs = *regs;
        // SAFETY: `regs` is the struct this request reads.
        unsafe { ioctl_pointer(self.fd.as_raw_fd(), KVM_SET_REGS, &raw mut regs) }
    }

    pub fn sregs(&self) -> io::Result<Sregs> {
        let mut sregs = Sregs::default();
        // SAFETY: `sregs` is the struct this request writes.
        unsafe { ioctl_pointer(self.fd.as_raw_fd(), KVM_GET_SREGS, &raw mut sregs)? };
        Ok(sregs)
    }

    pub fn set_sregs(&self, sregs: &Sregs) -> io::Result<()> {
        let mut sregs = *sregs;
        // SAFETY: `sregs` is the struct this request reads.
        unsafe { ioctl_pointer(self.fd.as_raw_fd(), KVM_SET_SREGS, &raw mut sregs) }
    }

    /// Runs the guest on this thread until it halts, it does something the
    /// VMM doesn't handle, or `timeout` passes. `out` receives each port
    /// write as a port number and its bytes.
    pub fn run(&mut self, timeout: Duration, out: impl FnMut(u16, &[u8])) -> io::Result<Stop> {
        self.run_ports(timeout, &mut WritesOnly(out))
    }

    /// Runs the guest as [`Vcpu::run`] does, with `ports` answering port
    /// reads as well as taking port writes.
    pub fn run_ports(&mut self, timeout: Duration, ports: &mut impl Ports) -> io::Result<Stop> {
        extern "C" fn ignore(_: c_int) {}
        // SAFETY: `ignore` does nothing, so it is safe in a signal handler.
        // It only needs to exist: the default action would kill the process.
        if unsafe { sys::signal(KICK_SIGNAL, ignore) } == sys::SIGNAL_FAILED {
            return Err(io::Error::last_os_error());
        }

        let _mask = unblock_kick_signal()?;
        let this = &*self;
        let kick = this.immediate_exit();
        kick.store(0, Ordering::SeqCst);
        let done = AtomicBool::new(false);
        // SAFETY: this call has no preconditions. The thread stays alive for
        // as long as the watchdog can use the handle, because the scope below
        // joins the watchdog before this function returns.
        let vcpu_thread = unsafe { sys::pthread_self() };

        thread::scope(|scope| {
            let watchdog = scope.spawn(|| watch(timeout, &done, kick, vcpu_thread));
            let _cancel = CancelWatchdog {
                done: &done,
                watchdog: watchdog.thread(),
            };
            this.run_until_stop(kick, ports)
        })
    }

    fn run_until_stop(&self, kick: &AtomicU8, ports: &mut impl Ports) -> io::Result<Stop> {
        loop {
            // SAFETY: KVM_RUN takes no argument. It writes only to the run
            // area, which this value owns, and to guest memory.
            if let Err(error) = unsafe { ioctl_value(self.fd.as_raw_fd(), KVM_RUN, 0) } {
                if error.raw_os_error() != Some(EINTR) {
                    return Err(error);
                }
                if kick.load(Ordering::SeqCst) != 0 {
                    return Ok(Stop::TimedOut);
                }
                continue;
            }
            let run = self.run.start().cast::<KvmRun>();
            // SAFETY: the mapping holds at least a `KvmRun`, which `Vm::new`
            // checked. The kernel wrote it before KVM_RUN returned.
            let reason = unsafe { ptr::read_volatile(&raw const (*run).exit_reason) };
            match reason {
                KVM_EXIT_HLT => return Ok(Stop::Halted),
                KVM_EXIT_SHUTDOWN => return Ok(Stop::Shutdown),
                KVM_EXIT_IO => {
                    // SAFETY: as above, and KVM_EXIT_IO selects this arm.
                    let io = unsafe { ptr::read_volatile(&raw const (*run).io) };
                    let data = self.io_data(&io)?;
                    let handled = match io.direction {
                        KVM_EXIT_IO_OUT => {
                            ports.write(io.port, data);
                            true
                        }
                        KVM_EXIT_IO_IN => ports.read(io.port, data),
                        _ => false,
                    };
                    if !handled {
                        return Ok(Stop::Unhandled(reason));
                    }
                }
                KVM_EXIT_MMIO => {
                    // No device is mapped yet, so a read finds an empty bus:
                    // all ones, like a port with no device. Linux probes such
                    // addresses, and a write has nowhere to go.
                    // SAFETY: as above, and KVM_EXIT_MMIO selects the union's
                    // `mmio` arm, which starts where `io` does and holds its
                    // eight data bytes after an 8-byte address. KVM reads them
                    // when the vCPU re-enters.
                    unsafe {
                        let data = (&raw mut (*run).io).cast::<[u8; 8]>().add(1);
                        ptr::write_volatile(data, [0xff; 8]);
                    }
                }
                other => return Ok(Stop::Unhandled(other)),
            }
        }
    }

    /// The bytes of a port access, which KVM keeps inside the run area.
    #[allow(clippy::mut_from_ref)]
    fn io_data(&self, io: &IoExit) -> io::Result<&mut [u8]> {
        let len = usize::from(io.size)
            .checked_mul(io.count as usize)
            .filter(|len| {
                io.data_offset
                    .checked_add(*len as u64)
                    .is_some_and(|end| end <= self.run.len() as u64)
            })
            .ok_or_else(|| io::Error::other("KVM reported port data outside its run area"))?;
        // SAFETY: the range was just checked to lie inside the mapping, and
        // the vCPU isn't running, so nothing else uses it during this borrow.
        // `run_until_stop` drops the borrow before it enters the guest again.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.run.start().add(io.data_offset as usize), len)
        })
    }

    fn immediate_exit(&self) -> &AtomicU8 {
        let run = self.run.start().cast::<KvmRun>();
        // SAFETY: the byte lies inside the mapping, which lives as long as
        // the returned borrow. It is only ever accessed atomically here, and
        // the kernel reads it with READ_ONCE.
        unsafe { AtomicU8::from_ptr(&raw mut (*run).immediate_exit) }
    }
}

/// Ends the watchdog on drop, so that a panic in the port callback doesn't
/// wait out the timeout.
struct CancelWatchdog<'a> {
    done: &'a AtomicBool,
    watchdog: &'a Thread,
}

impl Drop for CancelWatchdog<'_> {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        self.watchdog.unpark();
    }
}

/// Puts this thread's signal mask back on drop. It isn't `Send`, because a
/// signal mask belongs to one thread.
struct RestoreSignalMask {
    mask: sys::SignalSet,
    _thread_bound: PhantomData<*const ()>,
}

impl Drop for RestoreSignalMask {
    fn drop(&mut self) {
        // SAFETY: `mask` is a full set, and a null old set is allowed. With a
        // valid `how`, this call can't fail.
        unsafe { sys::pthread_sigmask(sys::SIG_SETMASK, &raw const self.mask, ptr::null_mut()) };
    }
}

/// Lets the kick signal reach this thread until the guard drops. A blocked
/// signal couldn't interrupt a running guest.
fn unblock_kick_signal() -> io::Result<RestoreSignalMask> {
    let mut set = sys::EMPTY_SIGNAL_SET;
    set[0] = 1 << (KICK_SIGNAL - 1);
    let mut mask = sys::EMPTY_SIGNAL_SET;
    // SAFETY: both pointers refer to sets of the size the C library reads.
    let error = unsafe { sys::pthread_sigmask(sys::SIG_UNBLOCK, &raw const set, &raw mut mask) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    Ok(RestoreSignalMask {
        mask,
        _thread_bound: PhantomData,
    })
}

/// Waits for `timeout`, and then stops the vCPU unless it is `done`.
///
/// `immediate_exit` covers a vCPU that isn't in the guest yet, and the signal
/// covers one that is. `immediate_exit` is set first, so whichever side of
/// KVM's checks the signal lands on, the vCPU can't re-enter the guest.
fn watch(timeout: Duration, done: &AtomicBool, kick: &AtomicU8, vcpu_thread: usize) {
    let start = Instant::now();
    loop {
        if done.load(Ordering::SeqCst) {
            return;
        }
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        thread::park_timeout(left);
    }
    kick.store(1, Ordering::SeqCst);
    // SAFETY: `Vcpu::run` keeps `vcpu_thread` alive until this thread is
    // joined, and its handler for this signal does nothing.
    unsafe { sys::pthread_kill(vcpu_thread, KICK_SIGNAL) };
}
