//! Boots the pinned kernel on real KVM.

use std::fs;
use std::time::Duration;
use vmm::serial::Serial;
use vmm::{GuestMemory, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
/// With no init to run the kernel panics, and `panic=-1` ends the VM at once
/// instead of leaving it to the timeout.
const COMMAND_LINE: &str = "console=ttyS0 panic=-1 reboot=t";

#[test]
fn the_pinned_kernel_prints_its_banner_on_the_serial_port() {
    let image = fs::read(t1::kernel::vmlinux()).unwrap();

    let vm = Vm::new(GuestMemory::new(MEMORY).unwrap()).unwrap();
    vm.add_interrupt_controllers().unwrap();
    let entry = boot::load(vm.memory(), &image, COMMAND_LINE).unwrap();
    let mut vcpu = vm.create_vcpu().unwrap();
    vm.expose_cpu_features(&vcpu).unwrap();
    boot::enter(&vcpu, &entry).unwrap();

    let mut serial = Serial::new();
    let stop = vcpu.run_ports(TIMEOUT, &mut serial).unwrap();

    let output = String::from_utf8_lossy(serial.output());
    let banner = format!("Linux version {}", t1::kernel::release());
    assert!(
        output.contains(&banner),
        "stopped with {stop:?} and printed:\n{output}"
    );
}
