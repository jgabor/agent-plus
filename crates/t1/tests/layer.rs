//! Mounts a stored layer in a real guest, from a read-only virtio-blk disk.

use erofs::{Dir, File, Node};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process;
use std::time::Duration;
use store::Layers;
use t1::{guest_binary, kernel};
use vmm::block::Block;
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_ADDRESS: u64 = 0xd000_0000;
const DEVICE_SIZE: u64 = 0x1000;
const LINE: u32 = 5;
const COMMAND_LINE: &str = "console=ttyS0 reboot=t virtio_mmio.device=4K@0xd0000000:5";
const GREETING: &[u8] = b"hello from a stored layer";
const LEVEL: i8 = 3;

/// The serial port and the disk, served on the vCPU thread during the guest's
/// exits.
struct Machine<'vm> {
    serial: Serial,
    block: Block,
    memory: &'vm GuestMemory,
    interrupt: Interrupt<'vm>,
}

impl Ports for Machine<'_> {
    fn write(&mut self, port: u16, data: &[u8]) {
        self.serial.write(port, data);
    }

    fn read(&mut self, port: u16, data: &mut [u8]) -> bool {
        self.serial.read(port, data)
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) {
        let Some(offset) = device_offset(address) else {
            return;
        };
        if self.block.write(self.memory, offset, data) {
            self.interrupt.pulse().unwrap();
        }
    }

    fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        match device_offset(address) {
            Some(offset) => self.block.read(offset, data),
            None => data.fill(0xff),
        }
    }
}

fn device_offset(address: u64) -> Option<u64> {
    address
        .checked_sub(DEVICE_ADDRESS)
        .filter(|offset| *offset < DEVICE_SIZE)
}

/// A layer that holds the directory `dir`, with the file `greeting` in it.
fn layer() -> Vec<u8> {
    let greeting = Node::File(File {
        mode: 0o644,
        size: GREETING.len() as u64,
        content: Box::new(GREETING),
    });
    let dir = Node::Dir(Dir {
        mode: 0o755,
        entries: BTreeMap::from([(b"greeting".to_vec(), greeting)]),
    });
    let root = Dir {
        mode: 0o755,
        entries: BTreeMap::from([(b"dir".to_vec(), dir)]),
    };
    let mut image = Vec::new();
    erofs::write(root, &mut image).unwrap();
    image
}

#[test]
fn the_guest_mounts_a_stored_layer_read_only_as_erofs() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("layer-{}", process::id()));
    let _ = fs::remove_dir_all(&root);
    let layers = Layers::new(&root);
    let digest = layers.put(&layer(), LEVEL).unwrap();
    let disk = layers.get(&digest).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let image = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::build("layer-init")).unwrap();
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

    let serial = &digest[..20];
    let mut machine = Machine {
        serial: Serial::new(),
        block: Block::new(disk, serial.as_bytes()),
        memory: vm.memory(),
        interrupt: vm.interrupt(LINE),
    };
    let stop = vcpu.run_ports(TIMEOUT, &mut machine).unwrap();

    let output = String::from_utf8_lossy(machine.serial.output()).into_owned();
    assert_eq!(stop, Stop::Shutdown, "printed:\n{output}");
    let report = output
        .lines()
        .find_map(|line| line.split_once("layer: ").map(|(_, report)| report))
        .unwrap_or_else(|| panic!("no report, printed:\n{output}"));
    let fields: Vec<&str> = report.split('|').collect();
    let [status, mount, read_only, id, entries, greeting] = fields[..] else {
        panic!("the guest reported: {report}");
    };
    assert_eq!(status, "ok");
    let mount: Vec<&str> = mount.split(' ').collect();
    assert_eq!(mount[..3], ["/dev/vda", "/layer", "erofs"], "{mount:?}");
    assert!(mount[3].split(',').any(|option| option == "ro"), "{mount:?}");
    assert_eq!(read_only, "1", "the disk itself should be read-only");
    assert_eq!(id, serial);
    assert_eq!(entries, "dir");
    assert_eq!(greeting.as_bytes(), GREETING);
}
