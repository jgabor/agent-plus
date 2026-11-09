//! Hand-assembled 16-bit guest code, run on real KVM.

use std::time::{Duration, Instant};
use vmm::{GuestMemory, Stop, Vcpu, Vm};

const PAGE: usize = 4096;
const ENTRY: u64 = 0x1000;
const PORT: u16 = 0x10;
const TIMEOUT: Duration = Duration::from_secs(10);
const SHORT_TIMEOUT: Duration = Duration::from_millis(200);

/// Writes 0x42 to `PORT`, and halts.
const OUT_AND_HALT: [u8; 7] = [
    0xba, 0x10, 0x00, // mov dx, 0x10
    0xb0, 0x42, // mov al, 0x42
    0xee, // out dx, al
    HLT,
];
const HLT: u8 = 0xf4;

/// Starts `code` in real mode at `ENTRY`.
fn guest(code: &[u8]) -> (Vm, Vcpu) {
    let mut vm = Vm::new(GuestMemory::new(4 * PAGE).unwrap()).unwrap();
    vm.memory().write(ENTRY, code).unwrap();
    let vcpu = vm.create_vcpu().unwrap();

    let mut sregs = vcpu.sregs().unwrap();
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs).unwrap();

    let mut regs = vcpu.regs().unwrap();
    regs.rip = ENTRY;
    // Bit 1 of RFLAGS is reserved, and must be set.
    regs.rflags = 2;
    vcpu.set_regs(&regs).unwrap();
    (vm, vcpu)
}

#[test]
fn guest_writes_a_byte_to_a_port_and_halts() {
    let (_vm, mut vcpu) = guest(&OUT_AND_HALT);
    let mut writes = Vec::new();
    let stop = vcpu
        .run(TIMEOUT, |port, bytes| writes.push((port, bytes.to_vec())))
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(writes, [(PORT, vec![0x42])]);
}

#[test]
fn a_guest_that_spins_is_stopped_at_the_timeout() {
    // jmp $
    let (_vm, mut vcpu) = guest(&[0xeb, 0xfe]);
    let start = Instant::now();
    let stop = vcpu.run(SHORT_TIMEOUT, |_, _| {}).unwrap();
    assert_eq!(stop, Stop::TimedOut);
    assert!(start.elapsed() >= SHORT_TIMEOUT);
    assert!(start.elapsed() < TIMEOUT);
}

#[test]
fn a_guest_that_keeps_exiting_is_stopped_at_the_timeout() {
    // out dx, al, then jump back to it.
    let (_vm, mut vcpu) = guest(&[0xee, 0xeb, 0xfd]);
    let start = Instant::now();
    let mut writes = 0_u64;
    let stop = vcpu.run(SHORT_TIMEOUT, |_, _| writes += 1).unwrap();
    assert_eq!(stop, Stop::TimedOut);
    assert!(writes > 0);
    assert!(start.elapsed() < TIMEOUT);
}

#[test]
fn a_vcpu_can_run_again_after_a_timeout() {
    let (vm, mut vcpu) = guest(&[0xeb, 0xfe]);
    assert_eq!(vcpu.run(SHORT_TIMEOUT, |_, _| {}).unwrap(), Stop::TimedOut);
    vm.memory().write(ENTRY, &[HLT]).unwrap();
    let mut regs = vcpu.regs().unwrap();
    regs.rip = ENTRY;
    vcpu.set_regs(&regs).unwrap();
    assert_eq!(vcpu.run(TIMEOUT, |_, _| {}).unwrap(), Stop::Halted);
}

#[test]
fn a_port_read_is_reported_as_unhandled() {
    // in al, dx
    let (_vm, mut vcpu) = guest(&[0xec, HLT]);
    let stop = vcpu.run(TIMEOUT, |_, _| {}).unwrap();
    assert_eq!(stop, Stop::Unhandled(2));
}

#[test]
fn a_triple_fault_is_a_shutdown() {
    let (_vm, mut vcpu) = guest(&[
        0x0f, 0x01, 0x1e, 0x00, 0x20, // lidt [0x2000], which is all zeroes
        0xcc, // int3, which now has nowhere to go
    ]);
    assert_eq!(vcpu.run(TIMEOUT, |_, _| {}).unwrap(), Stop::Shutdown);
}

#[test]
fn memory_with_no_device_ignores_writes_and_reads_as_all_ones() {
    // Past the 16 KiB of RAM.
    let (_vm, mut vcpu) = guest(&[
        0xb0, 0x00, // mov al, 0
        0xa2, 0x00, 0x80, // mov [0x8000], al
        0xa0, 0x00, 0x80, // mov al, [0x8000]
        0xba, 0x10, 0x00, // mov dx, 0x10
        0xee, // out dx, al
        HLT,
    ]);
    let mut writes = Vec::new();
    let stop = vcpu
        .run(TIMEOUT, |port, bytes| writes.push((port, bytes.to_vec())))
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(writes, [(PORT, vec![0xff])]);
}

#[test]
fn a_vcpu_keeps_guest_memory_alive_after_its_vm_is_dropped() {
    let (vm, mut vcpu) = guest(&OUT_AND_HALT);
    drop(vm);
    let mut writes = Vec::new();
    let stop = vcpu
        .run(TIMEOUT, |port, bytes| writes.push((port, bytes.to_vec())))
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(writes, [(PORT, vec![0x42])]);
}

unsafe extern "C" {
    fn pthread_sigmask(how: i32, set: *const [u64; 16], old: *mut [u64; 16]) -> i32;
}

/// Runs on its own thread, so that a hang fails here instead of stalling the
/// test binary, and so that the changed signal mask dies with the thread.
#[test]
fn a_blocked_kick_signal_does_not_defeat_the_timeout() {
    const SIG_BLOCK: i32 = 0;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut blocked = [0_u64; 16];
        // Signal numbers count from 1, so signal 40 is bit 39.
        blocked[0] = 1 << 39;
        // SAFETY: both sets are as large as sigset_t on glibc and musl, and
        // a null old set is allowed.
        assert_eq!(
            unsafe { pthread_sigmask(SIG_BLOCK, &blocked, std::ptr::null_mut()) },
            0
        );
        let (_vm, mut vcpu) = guest(&[0xeb, 0xfe]);
        sender
            .send(vcpu.run(SHORT_TIMEOUT, |_, _| {}).unwrap())
            .unwrap();
    });
    assert_eq!(receiver.recv_timeout(TIMEOUT).unwrap(), Stop::TimedOut);
}

#[test]
fn a_panic_in_the_port_callback_unwinds_at_once_and_restores_the_signal_mask() {
    const SIG_BLOCK: i32 = 0;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut blocked = [0_u64; 16];
        blocked[0] = 1 << 39;
        // SAFETY: as in the test above.
        assert_eq!(
            unsafe { pthread_sigmask(SIG_BLOCK, &blocked, std::ptr::null_mut()) },
            0
        );
        let (_vm, mut vcpu) = guest(&OUT_AND_HALT);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vcpu.run(Duration::from_secs(3600), |_, _| panic!("callback"))
        }));
        assert!(result.is_err());
        let mut current = [0_u64; 16];
        let nothing = [0_u64; 16];
        // SAFETY: as above. Blocking nothing only reads the current mask.
        assert_eq!(
            unsafe { pthread_sigmask(SIG_BLOCK, &nothing, &mut current) },
            0
        );
        sender.send(current[0] & (1 << 39) != 0).unwrap();
    });
    assert!(receiver.recv_timeout(TIMEOUT).unwrap());
}
