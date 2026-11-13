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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};
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
// _IOW(0xAE, 0x61, struct kvm_irq_level)
pub(crate) const KVM_IRQ_LINE: c_ulong = 0x4008_AE61;
// _IOW(0xAE, 0x77, struct kvm_pit_config)
const KVM_CREATE_PIT2: c_ulong = 0x4040_AE77;
// _IOW(0xAE, 0x90, struct kvm_cpuid2)
const KVM_SET_CPUID2: c_ulong = 0x4008_AE90;
// _IOW(0xAE, 0x46, struct kvm_userspace_memory_region)
const KVM_SET_USER_MEMORY_REGION: c_ulong = 0x4020_AE46;
// _IO(0xAE, 0x80)
pub(crate) const KVM_RUN: c_ulong = 0xAE80;
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

/// `struct kvm_irq_level`.
#[repr(C)]
struct IrqLevel {
    irq: u32,
    level: u32,
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

/// The `mmio` arm of the exit union in `struct kvm_run`.
#[repr(C)]
#[derive(Clone, Copy)]
struct MmioExit {
    phys_addr: u64,
    data: [u8; 8],
    len: u32,
    is_write: u8,
}

/// The arms of the exit union in `struct kvm_run` that the VMM reads.
#[repr(C)]
union Exit {
    io: IoExit,
    mmio: MmioExit,
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
    exit: Exit,
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
    assert!(size_of::<IrqLevel>() == 8);
    assert!(offset_of!(MmioExit, data) == 8);
    assert!(offset_of!(MmioExit, len) == 16);
    assert!(offset_of!(MmioExit, is_write) == 20);
    assert!(offset_of!(KvmRun, exit) == 32);
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
    /// The guest wrote `data` to a physical address outside its RAM. With no
    /// device there, the write goes nowhere.
    fn mmio_write(&mut self, _address: u64, _data: &[u8]) {}
    /// The guest reads `data` from a physical address outside its RAM. An
    /// empty bus reads as all ones, like a port with no device, and Linux
    /// probes such addresses.
    fn mmio_read(&mut self, _address: u64, data: &mut [u8]) {
        data.fill(0xff);
    }
    /// A [`Waker`] woke the vCPU. It runs on the vCPU's thread, between the
    /// guest's exits, so it can do what the guest's own accesses do.
    fn woken(&mut self) {}
}

/// Wakes a vCPU that [`Vcpu::run_waking`] runs, from any thread, so that
/// [`Ports::woken`] runs on the vCPU's thread soon after. Wakes coalesce, and
/// one that comes before the run starts is handled when it does.
#[derive(Default)]
pub struct Waker {
    /// Whether a wake is waiting to be handled.
    pending: AtomicBool,
    /// The running vCPU's thread, and the address of its `immediate_exit`
    /// byte, for as long as it runs.
    running: Mutex<Option<(usize, usize)>>,
}

impl Waker {
    /// Asks the vCPU's thread to call [`Ports::woken`].
    pub fn wake(&self) {
        self.pending.store(true, Ordering::SeqCst);
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((thread, exit)) = *running {
            // SAFETY: `Running` clears these under this lock before the run
            // ends, so the thread and its run area outlive this use. The byte
            // is only ever accessed atomically, and the signal's handler does
            // nothing. As with the watchdog, the byte covers a vCPU that
            // isn't in the guest yet, and the signal one that is.
            unsafe {
                AtomicU8::from_ptr(exit as *mut u8).store(1, Ordering::SeqCst);
                sys::pthread_kill(thread, KICK_SIGNAL);
            }
        }
    }
}

/// Lets a [`Waker`] reach the vCPU's thread until it drops.
struct Running<'a>(&'a Waker);

impl<'a> Running<'a> {
    fn start(waker: &'a Waker, thread: usize, exit: &AtomicU8) -> Self {
        let mut running = waker.running.lock().unwrap_or_else(PoisonError::into_inner);
        *running = Some((thread, exit.as_ptr() as usize));
        Running(waker)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self
            .0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
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

    /// A handle that raises interrupt `line` on the interrupt controllers.
    pub fn interrupt(&self, line: u32) -> Interrupt<'_> {
        Interrupt {
            fd: self.fd.as_fd(),
            line,
        }
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

    /// Adds the VM's vCPU. A VM has only one, so a second call fails. Use it
    /// from the thread that runs it.
    pub fn create_vcpu(&self) -> io::Result<Vcpu> {
        // SAFETY: this request takes a vCPU id, not a pointer.
        let fd = unsafe { ioctl_value(self.fd.as_raw_fd(), KVM_CREATE_VCPU, 0)? };
        // SAFETY: KVM_CREATE_VCPU returned a new descriptor that nothing owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let run = Mapping::shared(fd.as_fd(), self.vcpu_mmap_size)?;
        Ok(Vcpu {
            fd,
            run,
            _memory: Rc::clone(&self.memory),
        })
    }
}

/// One interrupt line of a VM.
pub struct Interrupt<'vm> {
    fd: BorrowedFd<'vm>,
    line: u32,
}

impl Interrupt<'_> {
    /// Raises and lowers the line. The controller latches the rising edge, so
    /// a guest that is busy still sees it, and no device has to track when
    /// the guest acknowledges.
    pub fn pulse(&self) -> io::Result<()> {
        self.set(1)?;
        self.set(0)
    }

    fn set(&self, level: u32) -> io::Result<()> {
        let mut level = IrqLevel {
            irq: self.line,
            level,
        };
        // SAFETY: `level` is the struct this request reads.
        unsafe { ioctl_pointer(self.fd.as_raw_fd(), KVM_IRQ_LINE, &raw mut level) }
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
        self.run_waking(timeout, ports, &Waker::default())
    }

    /// Runs the guest as [`Vcpu::run_ports`] does, and calls
    /// [`Ports::woken`] on this thread whenever `waker` wakes it.
    pub fn run_waking(
        &mut self,
        timeout: Duration,
        ports: &mut impl Ports,
        waker: &Waker,
    ) -> io::Result<Stop> {
        ignore_kick_signal()?;
        let _mask = unblock_kick_signal()?;
        let this = &*self;
        let kick = this.immediate_exit();
        kick.store(0, Ordering::SeqCst);
        let done = AtomicBool::new(false);
        let timed_out = AtomicBool::new(false);
        // SAFETY: this call has no preconditions. The thread stays alive for
        // as long as the watchdog can use the handle, because the scope below
        // joins the watchdog before this function returns, and as long as
        // the waker can, because `Running` lets go of it first.
        let vcpu_thread = unsafe { sys::pthread_self() };
        let _running = Running::start(waker, vcpu_thread, kick);

        thread::scope(|scope| {
            let watchdog = scope.spawn(|| watch(timeout, &done, &timed_out, kick, vcpu_thread));
            let _cancel = CancelWatchdog {
                done: &done,
                watchdog: watchdog.thread(),
            };
            this.run_until_stop(kick, &timed_out, waker, ports)
        })
    }

    fn run_until_stop(
        &self,
        kick: &AtomicU8,
        timed_out: &AtomicBool,
        waker: &Waker,
        ports: &mut impl Ports,
    ) -> io::Result<Stop> {
        loop {
            if waker.pending.swap(false, Ordering::SeqCst) {
                ports.woken();
            }
            // SAFETY: KVM_RUN takes no argument. It writes only to the run
            // area, which this value owns, and to guest memory.
            let stop = match unsafe { ioctl_value(self.fd.as_raw_fd(), KVM_RUN, 0) } {
                Ok(_) => self.handle_exit(ports)?,
                Err(error) => interrupted(error, kick, timed_out)?,
            };
            if let Some(stop) = stop {
                return Ok(stop);
            }
        }
    }

    /// Handles the exit that KVM wrote to the run area. Returns why the vCPU
    /// stops, or `None` if the guest can go on.
    fn handle_exit(&self, ports: &mut impl Ports) -> io::Result<Option<Stop>> {
        let run = self.run.start().cast::<KvmRun>();
        // SAFETY: the mapping holds at least a `KvmRun`, which `Vm::new`
        // checked. The kernel wrote it before KVM_RUN returned.
        let reason = unsafe { ptr::read_volatile(&raw const (*run).exit_reason) };
        match reason {
            KVM_EXIT_HLT => Ok(Some(Stop::Halted)),
            KVM_EXIT_SHUTDOWN => Ok(Some(Stop::Shutdown)),
            KVM_EXIT_IO => {
                // SAFETY: as above, and KVM_EXIT_IO selects this arm.
                let io = unsafe { ptr::read_volatile(&raw const (*run).exit.io) };
                let data = self.io_data(&io)?;
                let handled = match io.direction {
                    KVM_EXIT_IO_OUT => {
                        ports.write(io.port, data);
                        true
                    }
                    KVM_EXIT_IO_IN => ports.read(io.port, data),
                    _ => false,
                };
                Ok((!handled).then_some(Stop::Unhandled(reason)))
            }
            KVM_EXIT_MMIO => {
                // SAFETY: as above, and KVM_EXIT_MMIO selects this arm.
                let mmio = unsafe { ptr::read_volatile(&raw const (*run).exit.mmio) };
                let mut data = mmio.data;
                let len = (mmio.len as usize).min(data.len());
                if mmio.is_write != 0 {
                    ports.mmio_write(mmio.phys_addr, &data[..len]);
                } else {
                    ports.mmio_read(mmio.phys_addr, &mut data[..len]);
                    // SAFETY: as above. KVM reads these bytes when the vCPU
                    // re-enters the guest.
                    unsafe { ptr::write_volatile(&raw mut (*run).exit.mmio.data, data) };
                }
                Ok(None)
            }
            other => Ok(Some(Stop::Unhandled(other))),
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

/// Why a `KVM_RUN` that failed with `error` stops the vCPU, or `None` if the
/// guest can go on. Only the watchdog's kick ends a run that a signal
/// interrupted. A waker's kick is cleared, so the guest can go on. The
/// watchdog marks the timeout before it kicks, so a kick cleared here can't
/// hide one: either the mark is seen now, or the kick lands after the clear
/// and stops the next `KVM_RUN`.
fn interrupted(
    error: io::Error,
    kick: &AtomicU8,
    timed_out: &AtomicBool,
) -> io::Result<Option<Stop>> {
    if error.raw_os_error() != Some(EINTR) {
        return Err(error);
    }
    kick.store(0, Ordering::SeqCst);
    Ok(timed_out.load(Ordering::SeqCst).then_some(Stop::TimedOut))
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

/// Gives the kick signal a handler. It only needs to exist: the default action
/// would kill the process.
fn ignore_kick_signal() -> io::Result<()> {
    extern "C" fn ignore(_: c_int) {}
    // SAFETY: `ignore` does nothing, so it is safe in a signal handler.
    if unsafe { sys::signal(KICK_SIGNAL, ignore) } == sys::SIGNAL_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
fn watch(
    timeout: Duration,
    done: &AtomicBool,
    timed_out: &AtomicBool,
    kick: &AtomicU8,
    vcpu_thread: usize,
) {
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
    timed_out.store(true, Ordering::SeqCst);
    kick.store(1, Ordering::SeqCst);
    // SAFETY: `Vcpu::run` keeps `vcpu_thread` alive until this thread is
    // joined, and its handler for this signal does nothing.
    unsafe { sys::pthread_kill(vcpu_thread, KICK_SIGNAL) };
}

#[cfg(test)]
impl Vm {
    /// A VM whose descriptors aren't KVM's, so that every ioctl fails.
    fn fake() -> Self {
        Self {
            kvm: File::open("/dev/null").unwrap(),
            fd: File::open("/dev/null").unwrap().into(),
            vcpu_mmap_size: size_of::<KvmRun>(),
            memory: Rc::new(GuestMemory::new(4096).unwrap()),
        }
    }
}

#[cfg(test)]
impl Vcpu {
    /// A vCPU whose descriptor isn't KVM's, so that every ioctl fails. Its run
    /// area is `len` bytes of zeroed memory, which a test fills in as KVM
    /// would.
    pub(crate) fn fake(len: usize) -> Self {
        assert!(len >= size_of::<KvmRun>());
        Self {
            fd: File::open("/dev/null").unwrap().into(),
            run: Mapping::anonymous(len).unwrap(),
            _memory: Rc::new(GuestMemory::new(4096).unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_SIZE: usize = 4096;
    const DATA_OFFSET: u64 = 512;
    const PORT: u16 = 0x3f8;
    const ADDRESS: u64 = 0xd000_0000;
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Records what the guest does, and answers port reads with `answer`.
    #[derive(Default)]
    struct Recorder {
        writes: Vec<(u16, Vec<u8>)>,
        mmio_writes: Vec<(u64, Vec<u8>)>,
        answer: Option<u8>,
    }

    impl Ports for Recorder {
        fn write(&mut self, port: u16, data: &[u8]) {
            self.writes.push((port, data.to_vec()));
        }

        fn read(&mut self, _: u16, data: &mut [u8]) -> bool {
            self.answer.map(|byte| data.fill(byte)).is_some()
        }

        fn mmio_write(&mut self, address: u64, data: &[u8]) {
            self.mmio_writes.push((address, data.to_vec()));
        }
    }

    fn run_area(vcpu: &Vcpu) -> *mut KvmRun {
        vcpu.run.start().cast()
    }

    fn write_bytes(vcpu: &Vcpu, offset: u64, bytes: &[u8]) {
        assert!(offset as usize + bytes.len() <= vcpu.run.len());
        // SAFETY: the range is inside the mapping, and nothing else uses it.
        unsafe {
            let start = vcpu.run.start().add(offset as usize);
            ptr::copy_nonoverlapping(bytes.as_ptr(), start, bytes.len());
        }
    }

    fn read_bytes(vcpu: &Vcpu, offset: u64, len: usize) -> Vec<u8> {
        assert!(offset as usize + len <= vcpu.run.len());
        // SAFETY: as in `write_bytes`.
        unsafe { std::slice::from_raw_parts(vcpu.run.start().add(offset as usize), len) }.to_vec()
    }

    /// Writes the exit KVM would leave for `reason`.
    fn exit(vcpu: &Vcpu, reason: u32, exit: Exit) {
        // SAFETY: the mapping holds a `KvmRun`, which `Vcpu::fake` checked.
        unsafe {
            (*run_area(vcpu)).exit_reason = reason;
            (*run_area(vcpu)).exit = exit;
        }
    }

    fn port_exit(vcpu: &Vcpu, direction: u8, count: u32) {
        let io = IoExit {
            direction,
            size: 2,
            port: PORT,
            count,
            data_offset: DATA_OFFSET,
        };
        exit(vcpu, KVM_EXIT_IO, Exit { io });
    }

    fn mmio_exit(vcpu: &Vcpu, is_write: u8) {
        let mmio = MmioExit {
            phys_addr: ADDRESS,
            data: [1, 2, 3, 4, 5, 6, 7, 8],
            len: 4,
            is_write,
        };
        exit(vcpu, KVM_EXIT_MMIO, Exit { mmio });
    }

    fn mmio_data(vcpu: &Vcpu) -> [u8; 8] {
        // SAFETY: as in `exit`.
        unsafe { (*run_area(vcpu)).exit.mmio.data }
    }

    #[test]
    fn a_halt_or_a_shutdown_stops_the_vcpu() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        let mut ports = Recorder::default();
        exit(&vcpu, KVM_EXIT_HLT, Exit { io: port_io() });
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), Some(Stop::Halted));
        exit(&vcpu, KVM_EXIT_SHUTDOWN, Exit { io: port_io() });
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), Some(Stop::Shutdown));
        exit(&vcpu, 99, Exit { io: port_io() });
        assert_eq!(
            vcpu.handle_exit(&mut ports).unwrap(),
            Some(Stop::Unhandled(99))
        );
    }

    fn port_io() -> IoExit {
        IoExit {
            direction: 0,
            size: 0,
            port: 0,
            count: 0,
            data_offset: 0,
        }
    }

    #[test]
    fn a_port_write_reaches_the_ports_and_the_guest_goes_on() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        write_bytes(&vcpu, DATA_OFFSET, &[1, 2, 3, 4]);
        port_exit(&vcpu, KVM_EXIT_IO_OUT, 2);
        let mut ports = Recorder::default();
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), None);
        assert_eq!(ports.writes, [(PORT, vec![1, 2, 3, 4])]);
    }

    #[test]
    fn an_answered_port_read_goes_back_to_the_guest() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        port_exit(&vcpu, KVM_EXIT_IO_IN, 2);
        let mut ports = Recorder {
            answer: Some(7),
            ..Recorder::default()
        };
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), None);
        assert_eq!(read_bytes(&vcpu, DATA_OFFSET, 5), [7, 7, 7, 7, 0]);
    }

    #[test]
    fn an_unanswered_port_read_stops_the_vcpu() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        port_exit(&vcpu, KVM_EXIT_IO_IN, 1);
        let stop = vcpu.handle_exit(&mut Recorder::default()).unwrap();
        assert_eq!(stop, Some(Stop::Unhandled(KVM_EXIT_IO)));
    }

    #[test]
    fn a_port_access_with_an_unknown_direction_stops_the_vcpu() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        port_exit(&vcpu, 2, 1);
        let mut ports = Recorder::default();
        let stop = vcpu.handle_exit(&mut ports).unwrap();
        assert_eq!(stop, Some(Stop::Unhandled(KVM_EXIT_IO)));
        assert!(ports.writes.is_empty());
    }

    #[test]
    fn port_data_may_end_at_the_end_of_the_run_area_and_no_further() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        let io = |count| IoExit {
            direction: KVM_EXIT_IO_OUT,
            size: 4,
            port: PORT,
            count,
            data_offset: DATA_OFFSET,
        };
        let fits = (RUN_SIZE as u32 - DATA_OFFSET as u32) / 4;
        write_bytes(&vcpu, RUN_SIZE as u64 - 1, &[9]);
        let data = vcpu.io_data(&io(fits)).unwrap();
        assert_eq!(data.len(), RUN_SIZE - DATA_OFFSET as usize);
        assert_eq!(data.last(), Some(&9));
        assert!(vcpu.io_data(&io(fits + 1)).is_err());
        assert!(vcpu.io_data(&io(u32::MAX)).is_err());
        let wraps = IoExit {
            data_offset: u64::MAX,
            ..io(1)
        };
        assert!(vcpu.io_data(&wraps).is_err());
    }

    #[test]
    fn port_data_outside_the_run_area_is_an_error() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        port_exit(&vcpu, KVM_EXIT_IO_OUT, RUN_SIZE as u32);
        assert!(vcpu.handle_exit(&mut Recorder::default()).is_err());
    }

    #[test]
    fn an_mmio_write_reaches_the_ports() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        mmio_exit(&vcpu, 1);
        let mut ports = Recorder::default();
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), None);
        assert_eq!(ports.mmio_writes, [(ADDRESS, vec![1, 2, 3, 4])]);
        assert_eq!(mmio_data(&vcpu), [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn an_mmio_read_with_no_device_reads_as_all_ones() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        mmio_exit(&vcpu, 0);
        let mut ports = Recorder::default();
        assert_eq!(vcpu.handle_exit(&mut ports).unwrap(), None);
        assert!(ports.mmio_writes.is_empty());
        assert_eq!(mmio_data(&vcpu), [0xff, 0xff, 0xff, 0xff, 5, 6, 7, 8]);
    }

    #[test]
    fn writes_only_passes_writes_on_and_answers_no_read() {
        let mut writes = Vec::new();
        let mut ports = WritesOnly(|port, data: &[u8]| writes.push((port, data.to_vec())));
        ports.write(PORT, &[1]);
        assert!(!ports.read(PORT, &mut [0]));
        assert_eq!(writes, [(PORT, vec![1])]);
    }

    #[test]
    fn the_immediate_exit_flag_is_in_the_run_area() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        vcpu.immediate_exit().store(1, Ordering::SeqCst);
        assert_eq!(read_bytes(&vcpu, 0, 2), [0, 1]);
    }

    #[test]
    fn a_failed_ioctl_is_an_error() {
        let vcpu = Vcpu::fake(RUN_SIZE);
        assert!(vcpu.regs().is_err());
        assert!(vcpu.set_regs(&Regs::default()).is_err());
        assert!(vcpu.sregs().is_err());
        assert!(vcpu.set_sregs(&Sregs::default()).is_err());

        let vm = Vm::fake();
        assert!(vm.add_interrupt_controllers().is_err());
        assert!(vm.expose_cpu_features(&vcpu).is_err());
        assert!(vm.interrupt(4).pulse().is_err());
        assert!(vm.interrupt(4).set(0).is_err());
        assert!(vm.create_vcpu().is_err());
    }

    #[test]
    fn only_the_watchdog_ends_a_run_that_a_signal_interrupted() {
        let signal = || io::Error::from_raw_os_error(EINTR);
        let kick = AtomicU8::new(1);
        let timed_out = AtomicBool::new(false);
        // A waker's kick is cleared, so the next `KVM_RUN` enters the guest.
        assert_eq!(interrupted(signal(), &kick, &timed_out).unwrap(), None);
        assert_eq!(kick.load(Ordering::SeqCst), 0);
        timed_out.store(true, Ordering::SeqCst);
        assert_eq!(
            interrupted(signal(), &kick, &timed_out).unwrap(),
            Some(Stop::TimedOut)
        );
        let other = io::Error::from_raw_os_error(EINTR + 1);
        assert!(interrupted(other, &kick, &timed_out).is_err());
    }

    #[test]
    fn a_wake_before_the_run_waits_for_it_and_one_during_kicks_the_vcpu() {
        ignore_kick_signal().unwrap();
        let waker = Waker::default();
        waker.wake();
        assert!(waker.pending.load(Ordering::SeqCst));

        let kick = AtomicU8::new(0);
        // SAFETY: this call has no preconditions.
        let this_thread = unsafe { sys::pthread_self() };
        let running = Running::start(&waker, this_thread, &kick);
        waker.wake();
        assert_eq!(kick.load(Ordering::SeqCst), 1);
        drop(running);
        kick.store(0, Ordering::SeqCst);
        waker.wake();
        assert_eq!(
            kick.load(Ordering::SeqCst),
            0,
            "a run that ended was kicked"
        );
    }

    /// This thread's signal mask.
    fn signal_mask() -> sys::SignalSet {
        let mut mask = sys::EMPTY_SIGNAL_SET;
        // SAFETY: a null new set only reads the mask into `mask`.
        let error = unsafe { sys::pthread_sigmask(sys::SIG_SETMASK, ptr::null(), &raw mut mask) };
        assert_eq!(error, 0);
        mask
    }

    #[test]
    fn the_kick_signal_is_unblocked_until_the_guard_drops() {
        // A thread of its own, so that no other test sees its mask change.
        thread::spawn(|| {
            let all = [u64::MAX; 16];
            // SAFETY: `all` is a full set, and a null old set is allowed.
            let error =
                unsafe { sys::pthread_sigmask(sys::SIG_SETMASK, &raw const all, ptr::null_mut()) };
            assert_eq!(error, 0);
            let blocked = signal_mask();
            let kick_bit = 1 << (KICK_SIGNAL - 1);
            assert_ne!(blocked[0] & kick_bit, 0);

            let guard = unblock_kick_signal().unwrap();
            let mut expected = blocked;
            expected[0] &= !kick_bit;
            assert_eq!(signal_mask(), expected);
            drop(guard);
            assert_eq!(signal_mask(), blocked);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_watchdog_kicks_a_vcpu_that_outlives_the_timeout() {
        ignore_kick_signal().unwrap();
        let done = AtomicBool::new(false);
        let timed_out = AtomicBool::new(false);
        let kick = AtomicU8::new(0);
        // SAFETY: this call has no preconditions.
        let this_thread = unsafe { sys::pthread_self() };
        watch(
            Duration::from_millis(1),
            &done,
            &timed_out,
            &kick,
            this_thread,
        );
        assert_eq!(kick.load(Ordering::SeqCst), 1);
        assert!(timed_out.load(Ordering::SeqCst));
    }

    #[test]
    fn cancelling_the_watchdog_wakes_it_at_once() {
        let done = AtomicBool::new(false);
        let start = Instant::now();
        thread::scope(|scope| {
            let watchdog = scope.spawn(|| {
                while !done.load(Ordering::SeqCst) && start.elapsed() < TIMEOUT {
                    thread::park_timeout(TIMEOUT);
                }
            });
            drop(CancelWatchdog {
                done: &done,
                watchdog: watchdog.thread(),
            });
        });
        assert!(start.elapsed() < TIMEOUT);
        assert!(done.load(Ordering::SeqCst));
    }
}
