//! Echoes bytes through a real guest over virtio-vsock.

use std::fs;
use std::time::Duration;
use t1::{guest_binary, kernel};
use vmm::initramfs::{self, Entry};
use vmm::serial::Serial;
use vmm::vsock::{Connection, Vsock};
use vmm::{GuestMemory, Interrupt, Ports, Stop, Vm, boot};

const MEMORY: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_ADDRESS: u64 = 0xd000_0000;
const DEVICE_SIZE: u64 = 0x1000;
const LINE: u32 = 5;
const COMMAND_LINE: &str = "console=ttyS0 reboot=t virtio_mmio.device=4K@0xd0000000:5";
const GUEST_CID: u64 = 3;
const PORT: u32 = 1024;
/// Larger than the guest's receive buffer and socket buffer, so the bytes only
/// arrive if credit keeps flowing in both directions.
const PAYLOAD: usize = 1 << 20;

/// The serial port, the device, and a host that sends the payload and checks
/// what comes back. It all runs on the vCPU thread, during the guest's exits.
struct Machine<'vm> {
    serial: Serial,
    vsock: Vsock,
    memory: &'vm GuestMemory,
    interrupt: Interrupt<'vm>,
    connection: Option<Connection>,
    payload: Vec<u8>,
    sent: usize,
    echoed: Vec<u8>,
}

impl Machine<'_> {
    fn run_host(&mut self) {
        if self.connection.is_none() {
            self.connection = self.vsock.accept();
        }
        let Some(connection) = self.connection else {
            return;
        };
        self.sent += self.vsock.send(connection, &self.payload[self.sent..]);
        let mut buffer = [0; 8192];
        loop {
            let len = self.vsock.recv(connection, &mut buffer);
            if len == 0 {
                break;
            }
            self.echoed.extend_from_slice(&buffer[..len]);
        }
        if self.echoed.len() == self.payload.len() {
            self.vsock.close(connection);
        }
    }
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
        let mut interrupt = self.vsock.write(self.memory, offset, data);
        self.run_host();
        interrupt |= self.vsock.service(self.memory);
        if interrupt {
            self.interrupt.pulse().unwrap();
        }
    }

    fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        match device_offset(address) {
            Some(offset) => self.vsock.read(offset, data),
            None => data.fill(0xff),
        }
    }
}

fn device_offset(address: u64) -> Option<u64> {
    address
        .checked_sub(DEVICE_ADDRESS)
        .filter(|offset| *offset < DEVICE_SIZE)
}

#[test]
fn bytes_echo_through_a_real_guest_over_vsock() {
    let image = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::build("vsock-echo")).unwrap();
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

    let payload: Vec<u8> = (0..PAYLOAD).map(|i| (i % 251) as u8).collect();
    let mut machine = Machine {
        serial: Serial::new(),
        vsock: Vsock::new(GUEST_CID, &[PORT]),
        memory: vm.memory(),
        interrupt: vm.interrupt(LINE),
        connection: None,
        payload,
        sent: 0,
        echoed: Vec::new(),
    };
    let stop = vcpu.run_ports(TIMEOUT, &mut machine).unwrap();

    let output = String::from_utf8_lossy(machine.serial.output()).into_owned();
    assert_eq!(stop, Stop::Shutdown, "printed:\n{output}");
    assert!(
        output.contains(&format!("vsock-echo: Ok({PAYLOAD})")),
        "printed:\n{output}"
    );
    assert!(machine.echoed == machine.payload, "the echo differs");
}
