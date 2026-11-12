//! The virtio-blk device: one read-only disk, held in host memory.
//!
//! Every request comes from the guest, so nothing in it is trusted. A request
//! the device can't serve fails with a status, and never reaches past the
//! disk.

use crate::memory::GuestMemory;
use crate::virtio::{Chain, Transport};

const DEVICE_ID: u32 = 2;
const QUEUE_SIZE: u16 = 256;
const REQUESTS: usize = 0;
const READ_ONLY: u64 = 1 << 5;

const SECTOR: u64 = 512;
const HEADER: usize = 16;
const ID_LEN: usize = 20;
/// What fills the bytes a request leaves unanswered.
static ZEROS: [u8; 64 << 10] = [0; 64 << 10];

const TYPE_IN: u32 = 0;
const TYPE_OUT: u32 = 1;
const TYPE_GET_ID: u32 = 8;

const STATUS_OK: u8 = 0;
const STATUS_IOERR: u8 = 1;
const STATUS_UNSUPP: u8 = 2;

/// A virtio-blk device that serves `disk` and refuses writes.
pub struct Block {
    transport: Transport,
    disk: Vec<u8>,
    id: [u8; ID_LEN],
}

impl Block {
    /// A device that serves `disk`, which is padded with zeros to a whole
    /// sector. `id` is what the guest reads as the serial, cut to 20 bytes.
    pub fn new(mut disk: Vec<u8>, id: &[u8]) -> Self {
        let sectors = (disk.len() as u64).div_ceil(SECTOR);
        disk.resize((sectors * SECTOR) as usize, 0);
        let mut padded = [0; ID_LEN];
        let len = id.len().min(ID_LEN);
        padded[..len].copy_from_slice(&id[..len]);
        Self {
            transport: Transport::new(
                DEVICE_ID,
                READ_ONLY,
                sectors.to_le_bytes().to_vec(),
                1,
                QUEUE_SIZE,
            ),
            disk,
            id: padded,
        }
    }

    /// Handles a guest read of the device's registers.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        self.transport.read(offset, data);
    }

    /// Handles a guest write to the device's registers, and serves every
    /// request it announces. Returns true if the guest should get an
    /// interrupt.
    pub fn write(&mut self, memory: &GuestMemory, offset: u64, data: &[u8]) -> bool {
        if self.transport.write(offset, data).is_none() {
            return false;
        }
        let mut used = false;
        // A ring holds at most a queue's worth of requests. A guest whose
        // answers publish more requests waits for its next notification, so
        // the vCPU thread always gets back to the guest.
        for _ in 0..QUEUE_SIZE {
            let Some(queue) = self.transport.queue(REQUESTS) else {
                break;
            };
            // A broken ring has already stopped the queue.
            let Ok(Some(chain)) = queue.pop(memory) else {
                break;
            };
            let len = self.serve(memory, &chain);
            let Some(queue) = self.transport.queue(REQUESTS) else {
                break;
            };
            if queue.add_used(memory, chain.head, len).is_err() {
                break;
            }
            used = true;
        }
        if used {
            self.transport.signal_used();
        }
        used
    }

    /// Serves one request, and returns how many bytes it wrote. The status
    /// is the last writable byte, and a chain with none gets no answer. The
    /// used length counts from the first writable byte, so every byte before
    /// the status is written, with zeros where the answer is shorter.
    fn serve(&self, memory: &GuestMemory, chain: &Chain) -> u32 {
        let Some(room) = writable_len(chain).checked_sub(1) else {
            return 0;
        };
        let (status, written) = match header(memory, chain) {
            Some((TYPE_IN, sector)) => self.read_disk(memory, chain, sector, room),
            Some((TYPE_GET_ID, _)) => {
                let len = room.min(ID_LEN);
                match write_at(memory, chain, 0, &self.id[..len]) {
                    true => (STATUS_OK, len),
                    false => (STATUS_IOERR, 0),
                }
            }
            Some((TYPE_OUT, _)) | None => (STATUS_IOERR, 0),
            Some(_) => (STATUS_UNSUPP, 0),
        };
        let mut at = written;
        while at < room {
            let len = (room - at).min(ZEROS.len());
            if !write_at(memory, chain, at, &ZEROS[..len]) {
                return 0;
            }
            at += len;
        }
        if !write_at(memory, chain, room, &[status]) {
            return 0;
        }
        // The queue refuses a chain of more than `u32::MAX` bytes.
        (room + 1) as u32
    }

    /// Copies `len` bytes from `sector` on, if they all lie on the disk.
    fn read_disk(
        &self,
        memory: &GuestMemory,
        chain: &Chain,
        sector: u64,
        len: usize,
    ) -> (u8, usize) {
        let bytes = sector
            .checked_mul(SECTOR)
            .and_then(|start| usize::try_from(start).ok())
            .and_then(|start| self.disk.get(start..start.checked_add(len)?));
        match bytes {
            Some(bytes) if write_at(memory, chain, 0, bytes) => (STATUS_OK, len),
            _ => (STATUS_IOERR, 0),
        }
    }
}

/// The request's type and sector, from the first 16 readable bytes.
fn header(memory: &GuestMemory, chain: &Chain) -> Option<(u32, u64)> {
    let mut raw = [0; HEADER];
    let mut filled = 0;
    for buffer in chain.buffers.iter().filter(|buffer| !buffer.writable) {
        let take = (buffer.len as usize).min(HEADER - filled);
        memory
            .read(buffer.address, &mut raw[filled..filled + take])
            .ok()?;
        filled += take;
        if filled == HEADER {
            let kind = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let sector = u64::from_le_bytes(std::array::from_fn(|i| raw[8 + i]));
            return Some((kind, sector));
        }
    }
    None
}

fn writable_len(chain: &Chain) -> usize {
    chain
        .buffers
        .iter()
        .filter(|buffer| buffer.writable)
        .map(|buffer| buffer.len as usize)
        .sum()
}

/// Writes `bytes` across the chain's writable buffers, starting `skip` bytes
/// in. Returns false if they don't all fit.
fn write_at(memory: &GuestMemory, chain: &Chain, mut skip: usize, mut bytes: &[u8]) -> bool {
    for buffer in chain.buffers.iter().filter(|buffer| buffer.writable) {
        let len = buffer.len as usize;
        if skip >= len {
            skip -= len;
            continue;
        }
        let take = (len - skip).min(bytes.len());
        let (now, later) = bytes.split_at(take);
        if memory.write(buffer.address + skip as u64, now).is_err() {
            return false;
        }
        bytes = later;
        skip = 0;
        if bytes.is_empty() {
            break;
        }
    }
    bytes.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u16 = 8;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const QUEUE_DESC: u64 = 0x080;
    const QUEUE_DRIVER: u64 = 0x090;
    const QUEUE_DEVICE: u64 = 0x0a0;
    const DEVICE_ID_REGISTER: u64 = 0x008;
    const DEVICE_FEATURES: u64 = 0x010;
    const STATUS: u64 = 0x070;
    const CONFIG: u64 = 0x100;
    const DRIVER_OK: u32 = 4;
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const TABLE: u64 = 0x1000;
    const AVAILABLE: u64 = 0x2000;
    const USED: u64 = 0x3000;
    const HEADER_AT: u64 = 0x4000;
    const DATA: u64 = 0x5000;
    const STATUS_AT: u64 = 0x9000;
    /// Three sectors, each byte different from its neighbours.
    const DISK: usize = 3 * SECTOR as usize;

    fn disk() -> Vec<u8> {
        (0..DISK).map(|i| (i % 251) as u8).collect()
    }

    /// A fake guest driver, which writes the ring as Linux would.
    struct Driver {
        memory: GuestMemory,
        device: Block,
        published: u16,
    }

    impl Driver {
        fn new() -> Self {
            Self::with(disk(), b"layer-1234")
        }

        fn with(disk: Vec<u8>, id: &[u8]) -> Self {
            let mut driver = Self {
                memory: GuestMemory::new(1 << 16).unwrap(),
                device: Block::new(disk, id),
                published: 0,
            };
            driver.set(QUEUE_NUM, u32::from(SIZE));
            driver.set(QUEUE_DESC, TABLE as u32);
            driver.set(QUEUE_DRIVER, AVAILABLE as u32);
            driver.set(QUEUE_DEVICE, USED as u32);
            driver.set(QUEUE_READY, 1);
            driver.set(STATUS, DRIVER_OK);
            driver
        }

        fn set(&mut self, offset: u64, value: u32) -> bool {
            self.device
                .write(&self.memory, offset, &value.to_le_bytes())
        }

        fn get(&self, offset: u64) -> u32 {
            let mut data = [0; 4];
            self.device.read(offset, &mut data);
            u32::from_le_bytes(data)
        }

        fn descriptor(&self, index: u16, address: u64, len: u32, flags: u16) {
            let at = TABLE + u64::from(index) * 16;
            let next = if flags & NEXT != 0 { index + 1 } else { 0 };
            self.memory.write(at, &address.to_le_bytes()).unwrap();
            self.memory.write(at + 8, &len.to_le_bytes()).unwrap();
            self.memory.write(at + 12, &flags.to_le_bytes()).unwrap();
            self.memory.write(at + 14, &next.to_le_bytes()).unwrap();
        }

        /// Sends a request whose data lies in writable buffers of `lens`
        /// bytes, and returns the status and the used length.
        fn request(&mut self, kind: u32, sector: u64, lens: &[u32]) -> (u8, u32) {
            let mut header = [0; HEADER];
            header[..4].copy_from_slice(&kind.to_le_bytes());
            header[8..].copy_from_slice(&sector.to_le_bytes());
            self.memory.write(HEADER_AT, &header).unwrap();
            self.memory.write(STATUS_AT, &[0xff]).unwrap();
            self.descriptor(0, HEADER_AT, HEADER as u32, NEXT);
            let mut at = DATA;
            for (i, len) in lens.iter().enumerate() {
                let flags = if kind == TYPE_OUT { NEXT } else { WRITE | NEXT };
                self.descriptor(i as u16 + 1, at, *len, flags);
                at += u64::from(*len);
            }
            self.descriptor(lens.len() as u16 + 1, STATUS_AT, 1, WRITE);
            let slot = u64::from(self.published % SIZE);
            self.memory
                .write(AVAILABLE + 4 + slot * 2, &0u16.to_le_bytes())
                .unwrap();
            self.published += 1;
            self.memory
                .write(AVAILABLE + 2, &self.published.to_le_bytes())
                .unwrap();
            assert!(self.set(QUEUE_NOTIFY, 0), "the request got no interrupt");
            let mut status = [0];
            self.memory.read(STATUS_AT, &mut status).unwrap();
            let mut len = [0; 4];
            self.memory.read(USED + 4 + slot * 8 + 4, &mut len).unwrap();
            (status[0], u32::from_le_bytes(len))
        }

        fn data(&self, len: usize) -> Vec<u8> {
            let mut data = vec![0; len];
            self.memory.read(DATA, &mut data).unwrap();
            data
        }
    }

    #[test]
    fn the_device_is_a_read_only_disk_of_whole_sectors() {
        let driver = Driver::new();
        assert_eq!(driver.get(DEVICE_ID_REGISTER), 2);
        assert_eq!(driver.get(DEVICE_FEATURES), 1 << 5);
        assert_eq!(driver.get(CONFIG), 3);
        assert_eq!(driver.get(CONFIG + 4), 0);
    }

    #[test]
    fn a_partial_last_sector_reads_as_zeros() {
        let mut driver = Driver::with(vec![7; 513], b"");
        assert_eq!(driver.get(CONFIG), 2);
        assert_eq!(driver.request(TYPE_IN, 1, &[512]), (STATUS_OK, 513));
        let mut expected = vec![0; 512];
        expected[0] = 7;
        assert_eq!(driver.data(512), expected);
    }

    #[test]
    fn a_read_across_a_sector_boundary_spans_both_sectors() {
        let mut driver = Driver::new();
        // The second buffer starts in the middle of the second sector.
        assert_eq!(
            driver.request(TYPE_IN, 1, &[768, 256]),
            (STATUS_OK, 1024 + 1)
        );
        assert_eq!(driver.data(1024), disk()[512..1536]);
    }

    #[test]
    fn a_read_past_the_end_fails_and_copies_nothing() {
        let mut driver = Driver::new();
        driver.memory.write(DATA, &[0xaa; 1024]).unwrap();
        assert_eq!(driver.request(TYPE_IN, 2, &[1024]), (STATUS_IOERR, 1025));
        assert_eq!(driver.data(1024), [0; 1024]);
        assert_eq!(driver.request(TYPE_IN, 3, &[512]), (STATUS_IOERR, 513));
        assert_eq!(
            driver.request(TYPE_IN, u64::MAX, &[512]),
            (STATUS_IOERR, 513)
        );
        // The disk still serves its last sector.
        assert_eq!(driver.request(TYPE_IN, 2, &[512]), (STATUS_OK, 513));
    }

    #[test]
    fn a_write_is_refused_and_changes_nothing() {
        let mut driver = Driver::new();
        driver.memory.write(DATA, &[0xaa; 512]).unwrap();
        assert_eq!(driver.request(TYPE_OUT, 0, &[512]), (STATUS_IOERR, 1));
        assert_eq!(driver.request(TYPE_IN, 0, &[512]), (STATUS_OK, 513));
        assert_eq!(driver.data(512), disk()[..512]);
    }

    #[test]
    fn get_id_answers_the_serial() {
        let mut driver = Driver::new();
        assert_eq!(driver.request(TYPE_GET_ID, 0, &[20]), (STATUS_OK, 21));
        assert_eq!(driver.data(20), *b"layer-1234\0\0\0\0\0\0\0\0\0\0");
    }

    #[test]
    fn a_long_id_is_cut_to_twenty_bytes() {
        let mut driver = Driver::with(disk(), &[b'x'; 64]);
        driver.memory.write(DATA, &[0xaa; 32]).unwrap();
        assert_eq!(driver.request(TYPE_GET_ID, 0, &[32]), (STATUS_OK, 33));
        assert_eq!(driver.data(32), [&[b'x'; 20][..], &[0; 12]].concat());
    }

    #[test]
    fn a_request_the_device_doesnt_know_is_unsupported() {
        let mut driver = Driver::new();
        for kind in [4, 11, u32::MAX] {
            assert_eq!(driver.request(kind, 0, &[512]), (STATUS_UNSUPP, 513));
        }
    }

    #[test]
    fn a_short_header_fails() {
        let mut driver = Driver::new();
        driver.memory.write(STATUS_AT, &[0xff]).unwrap();
        driver.descriptor(0, HEADER_AT, 8, NEXT);
        driver.descriptor(1, STATUS_AT, 1, WRITE);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        assert!(driver.set(QUEUE_NOTIFY, 0));
        let mut status = [0];
        driver.memory.read(STATUS_AT, &mut status).unwrap();
        assert_eq!(status, [STATUS_IOERR]);
    }

    #[test]
    fn a_request_with_no_room_for_a_status_gets_no_answer() {
        let mut driver = Driver::new();
        driver.descriptor(0, HEADER_AT, HEADER as u32, 0);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        assert!(driver.set(QUEUE_NOTIFY, 0));
        let mut len = [0; 4];
        driver.memory.read(USED + 8, &mut len).unwrap();
        assert_eq!(u32::from_le_bytes(len), 0);
    }

    #[test]
    fn a_header_split_across_buffers_is_read_whole() {
        let mut driver = Driver::new();
        let mut header = [0; HEADER];
        header[8..].copy_from_slice(&1u64.to_le_bytes());
        driver.memory.write(HEADER_AT, &header).unwrap();
        driver.descriptor(0, HEADER_AT, 10, NEXT);
        driver.descriptor(1, HEADER_AT + 10, 6, NEXT);
        driver.descriptor(2, DATA, 512, WRITE | NEXT);
        driver.descriptor(3, STATUS_AT, 1, WRITE);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        assert!(driver.set(QUEUE_NOTIFY, 0));
        assert_eq!(driver.data(512), disk()[512..1024]);
    }

    #[test]
    fn answers_that_publish_requests_stop_at_a_queue_s_worth() {
        // Sector `s` starts with `s + 1`, and each request reads the sector
        // named by the available index into the available index.
        let mut disk = vec![0; 1024 * SECTOR as usize];
        for (sector, bytes) in disk.chunks_mut(SECTOR as usize).enumerate() {
            bytes[..2].copy_from_slice(&(sector as u16 + 1).to_le_bytes());
        }
        let mut driver = Driver::with(disk, b"");
        driver.descriptor(0, HEADER_AT, 8, NEXT);
        driver.descriptor(1, AVAILABLE + 2, 8, NEXT);
        driver.descriptor(2, AVAILABLE + 2, 2, WRITE | NEXT);
        driver.descriptor(3, STATUS_AT, 1, WRITE);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        assert!(driver.set(QUEUE_NOTIFY, 0));
        let mut served = [0; 2];
        driver.memory.read(USED + 2, &mut served).unwrap();
        assert_eq!(u16::from_le_bytes(served), QUEUE_SIZE);
    }

    #[test]
    fn nothing_is_served_before_the_driver_is_ready() {
        let mut driver = Driver::new();
        driver.set(STATUS, 0);
        driver.descriptor(0, HEADER_AT, HEADER as u32, NEXT);
        driver.descriptor(1, STATUS_AT, 1, WRITE);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        assert!(!driver.set(QUEUE_NOTIFY, 0));
    }
}
