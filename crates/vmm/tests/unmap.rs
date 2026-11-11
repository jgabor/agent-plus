//! Guest memory is given back when it drops. This is the only test in its
//! binary, because it measures the whole process, and other tests map memory
//! of their own.

use std::fs;
use vmm::GuestMemory;

/// This process's virtual memory size, in KiB.
fn virtual_size() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmSize:"))
        .unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn dropping_guest_memory_unmaps_it() {
    // Nothing touches it, so it takes no RAM.
    const SIZE: u64 = 1 << 30;
    let memory = GuestMemory::new(SIZE as usize).unwrap();
    let mapped = virtual_size();
    drop(memory);
    assert!(virtual_size() <= mapped - SIZE / 1024);
}
