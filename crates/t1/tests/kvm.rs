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

/// Starts `code` in real mode at `ENTRY`. The returned `Vm` must outlive the
/// `Vcpu`, because it owns the guest's memory.
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
