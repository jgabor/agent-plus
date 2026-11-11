//! Boots the pinned kernel, and checks in the guest that it has what the Run
//! milestone mounts and drives.

use std::fs;
use std::time::Duration;
use t1::{guest_binary, kernel};
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::{GuestMemory, Stop, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_LINE: &str = "console=ttyS0 reboot=t";
const NEEDED: [&str; 5] = ["erofs", "overlay", "tmpfs", "devpts", "virtio_blk"];

#[test]
fn the_guest_has_erofs_overlay_tmpfs_devpts_and_virtio_blk() {
    let image = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::build("features-init")).unwrap();
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
    assert_eq!(stop, Stop::Shutdown, "printed:\n{output}");
    let report = output
        .lines()
        .find_map(|line| line.split_once("features: ").map(|(_, report)| report))
        .unwrap_or_else(|| panic!("no report, printed:\n{output}"));
    let offered: Vec<&str> = report.split(' ').collect();
    assert_eq!(offered.last(), Some(&"end"), "printed:\n{output}");
    let missing: Vec<&str> = NEEDED
        .into_iter()
        .filter(|name| !offered.contains(name))
        .collect();
    assert!(missing.is_empty(), "missing {missing:?} from: {report}");
}
