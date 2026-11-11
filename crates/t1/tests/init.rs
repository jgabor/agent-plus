//! Boots the pinned kernel to a static init in an in-memory initramfs.

use std::fs;
use std::time::Duration;
use t1::{guest_binary, kernel};
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::{GuestMemory, Stop, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_LINE: &str = "console=ttyS0 reboot=t";
const MARKER: &str = "test-init reached";
/// A triple fault alone could be a crash. The kernel prints this first.
const CLEAN_RESTART: &str = "reboot: Restarting system";

#[test]
fn the_kernel_runs_init_from_the_initramfs_and_powers_off() {
    let image = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::build("test-init")).unwrap();
    let initramfs = initramfs::build(&[Entry::File {
        path: "init",
        mode: 0o755,
        contents: &init,
    }])
    .unwrap();

    let vm = Vm::new(GuestMemory::new(MEMORY).unwrap()).unwrap();
    vm.add_interrupt_controllers().unwrap();
    let entry = boot::load(vm.memory(), &image, COMMAND_LINE).unwrap();
    boot::load_initramfs(vm.memory(), &entry, &initramfs).unwrap();
    let mut vcpu = vm.create_vcpu().unwrap();
    vm.expose_cpu_features(&vcpu).unwrap();
    boot::enter(&vcpu, &entry).unwrap();

    let mut serial = Serial::new();
    let stop = vcpu.run_ports(TIMEOUT, &mut serial).unwrap();

    let output = String::from_utf8_lossy(serial.output());
    assert!(
        output.contains(MARKER),
        "stopped with {stop:?} and printed:\n{output}"
    );
    assert_eq!(stop, Stop::Shutdown, "printed:\n{output}");
    assert!(output.contains(CLEAN_RESTART), "printed:\n{output}");
}
