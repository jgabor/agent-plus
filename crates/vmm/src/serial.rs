//! An emulated 16550 UART on the first PC serial port.
//!
//! The output is a diagnostic log. The UART polls: it never raises an
//! interrupt, and it never has input.

use crate::Ports;

const BASE: u16 = 0x3f8;
const REGISTERS: u16 = 8;

/// The guest controls how much it prints, so the log has a limit. Output past
/// it is dropped.
const MAX_OUTPUT: usize = 1 << 20;

const DATA: u16 = 0;
const INTERRUPT_ENABLE: u16 = 1;
const INTERRUPT_ID: u16 = 2;
const LINE_CONTROL: u16 = 3;
const MODEM_CONTROL: u16 = 4;
const LINE_STATUS: u16 = 5;
const MODEM_STATUS: u16 = 6;
const SCRATCH: u16 = 7;

const DIVISOR_LATCH: u8 = 0x80;
const NO_INTERRUPT: u8 = 0x01;
/// The transmitter is always empty, because a write completes at once.
const TRANSMITTER_EMPTY: u8 = 0x60;
/// Carrier detect, data set ready, and clear to send.
const MODEM_READY: u8 = 0xb0;

#[derive(Default)]
pub struct Serial {
    output: Vec<u8>,
    interrupt_enable: u8,
    line_control: u8,
    modem_control: u8,
    scratch: u8,
    divisor: [u8; 2],
}

impl Serial {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the guest has printed, up to the limit.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    fn divisor_selected(&self) -> bool {
        self.line_control & DIVISOR_LATCH != 0
    }

    fn write_register(&mut self, register: u16, byte: u8) {
        match register {
            DATA | INTERRUPT_ENABLE if self.divisor_selected() => {
                self.divisor[usize::from(register)] = byte;
            }
            DATA => {
                if self.output.len() < MAX_OUTPUT {
                    self.output.push(byte);
                }
            }
            INTERRUPT_ENABLE => self.interrupt_enable = byte,
            LINE_CONTROL => self.line_control = byte,
            MODEM_CONTROL => self.modem_control = byte,
            SCRATCH => self.scratch = byte,
            _ => {}
        }
    }

    fn read_register(&self, register: u16) -> u8 {
        match register {
            DATA | INTERRUPT_ENABLE if self.divisor_selected() => {
                self.divisor[usize::from(register)]
            }
            INTERRUPT_ENABLE => self.interrupt_enable,
            INTERRUPT_ID => NO_INTERRUPT,
            LINE_CONTROL => self.line_control,
            MODEM_CONTROL => self.modem_control,
            LINE_STATUS => TRANSMITTER_EMPTY,
            MODEM_STATUS => MODEM_READY,
            SCRATCH => self.scratch,
            _ => 0,
        }
    }
}

/// The offset of `port` in the UART, if it's one of its ports.
fn register(port: u16) -> Option<u16> {
    port.checked_sub(BASE).filter(|offset| *offset < REGISTERS)
}

impl Ports for Serial {
    fn write(&mut self, port: u16, data: &[u8]) {
        if let Some(register) = register(port) {
            for byte in data {
                self.write_register(register, *byte);
            }
        }
    }

    /// Ports with no device read as all ones, as on a PC with an empty bus.
    fn read(&mut self, port: u16, data: &mut [u8]) -> bool {
        let byte = register(port).map_or(0xff, |register| self.read_register(register));
        data.fill(byte);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(serial: &mut Serial, port: u16) -> u8 {
        let mut byte = [0];
        assert!(serial.read(port, &mut byte));
        byte[0]
    }

    #[test]
    fn bytes_written_to_the_data_port_are_the_output() {
        let mut serial = Serial::new();
        serial.write(BASE, b"hi");
        serial.write(BASE, b"!");
        assert_eq!(serial.output(), b"hi!");
    }

    #[test]
    fn writes_to_other_ports_are_not_output() {
        let mut serial = Serial::new();
        serial.write(BASE - 1, b"a");
        serial.write(BASE + REGISTERS, b"b");
        serial.write(BASE + SCRATCH, b"c");
        assert_eq!(serial.output(), b"");
    }

    #[test]
    fn the_transmitter_always_has_room() {
        assert_eq!(
            read(&mut Serial::new(), BASE + LINE_STATUS),
            TRANSMITTER_EMPTY
        );
    }

    #[test]
    fn the_divisor_latch_hides_the_data_port_from_the_log() {
        let mut serial = Serial::new();
        serial.write(BASE + LINE_CONTROL, &[DIVISOR_LATCH]);
        serial.write(BASE, &[12]);
        serial.write(BASE + INTERRUPT_ENABLE, &[3]);
        assert_eq!(read(&mut serial, BASE), 12);
        assert_eq!(read(&mut serial, BASE + INTERRUPT_ENABLE), 3);
        assert_eq!(serial.output(), b"");

        serial.write(BASE + LINE_CONTROL, &[0]);
        assert_eq!(read(&mut serial, BASE + INTERRUPT_ENABLE), 0);
        serial.write(BASE, b"x");
        assert_eq!(serial.output(), b"x");
    }

    #[test]
    fn plain_registers_read_back() {
        let mut serial = Serial::new();
        for register in [INTERRUPT_ENABLE, LINE_CONTROL, MODEM_CONTROL, SCRATCH] {
            serial.write(BASE + register, &[0x15]);
            assert_eq!(read(&mut serial, BASE + register), 0x15);
        }
    }

    #[test]
    fn a_port_with_no_device_reads_as_all_ones() {
        let mut data = [0; 2];
        assert!(Serial::new().read(0x60, &mut data));
        assert_eq!(data, [0xff; 2]);
    }

    #[test]
    fn the_log_stops_growing_at_its_limit() {
        let mut serial = Serial::new();
        serial.write(BASE, &vec![b'a'; MAX_OUTPUT + 10]);
        assert_eq!(serial.output().len(), MAX_OUTPUT);
    }
}
