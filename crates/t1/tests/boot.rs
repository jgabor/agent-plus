//! Boots the pinned kernel on real KVM.

use std::fs;
use std::time::Duration;
use vmm::serial::Serial;
use vmm::{GuestMemory, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_LINE: &str = "console=ttyS0";
const BANNER: &str = "Linux version 7.3.3-ap.1";

#[test]
fn the_pinned_kernel_prints_its_banner_on_the_serial_port() {
    let image = fs::read(t1::kernel::vmlinux()).unwrap();

    let mut vm = Vm::new(GuestMemory::new(MEMORY).unwrap()).unwrap();
    vm.add_interrupt_controllers().unwrap();
    let entry = boot::load(vm.memory(), &image, COMMAND_LINE).unwrap();
    let mut vcpu = vm.create_vcpu().unwrap();
    vm.expose_cpu_features(&vcpu).unwrap();
    boot::enter(&vcpu, &entry).unwrap();

    let mut serial = Serial::new();
    let stop = vcpu.run_ports(TIMEOUT, &mut serial).unwrap();

    let output = String::from_utf8_lossy(serial.output());
    assert!(
        output.contains(BANNER),
        "stopped with {stop:?} and printed:\n{output}"
    );
}
