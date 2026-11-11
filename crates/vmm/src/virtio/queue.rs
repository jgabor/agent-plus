//! Split virtqueues. The guest owns every byte of the rings, and it can change
//! them while the host reads, so each value is read once and checked as read.

use crate::memory::{GuestMemory, OutOfBounds};
use std::fmt;
use std::sync::atomic::{Ordering, fence};

const NEXT: u16 = 1;
const WRITE: u16 = 2;
const INDIRECT: u16 = 4;

/// One guest buffer in a chain, proven to lie inside guest memory.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Buffer {
    pub address: u64,
    pub len: u32,
    pub writable: bool,
}

/// A descriptor chain the guest made available.
#[derive(Debug, PartialEq, Eq)]
pub struct Chain {
    pub head: u16,
    pub buffers: Vec<Buffer>,
}

/// How a guest broke the rules of a queue.
#[derive(Debug, PartialEq, Eq)]
pub enum QueueError {
    /// A ring, a table, or a buffer doesn't fit inside guest memory.
    OutOfBounds(OutOfBounds),
    /// A head or a `next` field names a descriptor the queue doesn't have.
    BadIndex(u16),
    /// The available index ran more than a queue's worth ahead of the device.
    TooManyAvailable,
    /// A chain has more descriptors than the queue, so it loops, or holds more
    /// bytes than a used element can report.
    TooLong,
    /// A descriptor uses indirect tables, a feature the device doesn't offer.
    Indirect,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds(error) => error.fmt(f),
            Self::BadIndex(index) => write!(f, "descriptor {index} is outside the queue"),
            Self::TooManyAvailable => write!(f, "the available index is too far ahead"),
            Self::TooLong => write!(f, "a descriptor chain is too long"),
            Self::Indirect => write!(f, "a descriptor is indirect"),
        }
    }
}

impl std::error::Error for QueueError {}

impl From<OutOfBounds> for QueueError {
    fn from(error: OutOfBounds) -> Self {
        Self::OutOfBounds(error)
    }
}

/// One split virtqueue.
///
/// A queue that is ready always has a size that is a nonzero power of two, no
/// larger than `max_size`. A guest that breaks the rules loses the queue: it
/// stops being ready, so a device that loops on `pop` always ends.
pub struct Queue {
    pub(super) max_size: u16,
    pub(super) size: u16,
    pub(super) ready: bool,
    pub(super) descriptors: u64,
    pub(super) available: u64,
    pub(super) used: u64,
    next_available: u16,
    next_used: u16,
}

impl Queue {
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: 0,
            ready: false,
            descriptors: 0,
            available: 0,
            used: 0,
            next_available: 0,
            next_used: 0,
        }
    }

    /// Forgets everything the guest set up.
    pub(super) fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    /// Makes a queue ready, at the start of its rings.
    pub(super) fn start(&mut self) {
        self.next_available = 0;
        self.next_used = 0;
        self.ready = true;
    }

    /// Whether the guest's size may be used.
    pub(super) fn size_is_valid(&self) -> bool {
        self.size.is_power_of_two() && self.size <= self.max_size
    }

    /// The next chain the guest made available, or `None` if there is none.
    pub fn pop(&mut self, memory: &GuestMemory) -> Result<Option<Chain>, QueueError> {
        if !self.ready {
            return Ok(None);
        }
        let chain = self.next_chain(memory);
        self.ready = chain.is_ok();
        chain
    }

    /// Hands a chain back to the guest, with how many bytes the device wrote.
    pub fn add_used(
        &mut self,
        memory: &GuestMemory,
        head: u16,
        len: u32,
    ) -> Result<(), QueueError> {
        if !self.ready {
            return Ok(());
        }
        let written = self.write_used(memory, head, len);
        self.ready = written.is_ok();
        written
    }

    fn next_chain(&mut self, memory: &GuestMemory) -> Result<Option<Chain>, QueueError> {
        let published = read_u16(memory, self.available, 2)?;
        let waiting = published.wrapping_sub(self.next_available);
        if waiting > self.size {
            return Err(QueueError::TooManyAvailable);
        }
        if waiting == 0 {
            return Ok(None);
        }
        // The ring entry and its descriptors were written before the index.
        fence(Ordering::Acquire);
        let slot = u64::from(self.next_available % self.size);
        let head = read_u16(memory, self.available, 4 + slot * 2)?;
        let buffers = self.walk(memory, head)?;
        self.next_available = self.next_available.wrapping_add(1);
        Ok(Some(Chain { head, buffers }))
    }

    fn walk(&self, memory: &GuestMemory, head: u16) -> Result<Vec<Buffer>, QueueError> {
        let mut buffers = Vec::new();
        let mut total = 0;
        let mut index = head;
        loop {
            if index >= self.size {
                return Err(QueueError::BadIndex(index));
            }
            if buffers.len() == usize::from(self.size) {
                return Err(QueueError::TooLong);
            }
            let descriptor = read_descriptor(memory, self.descriptors, index)?;
            if descriptor.flags & INDIRECT != 0 {
                return Err(QueueError::Indirect);
            }
            memory.check(descriptor.address, descriptor.len as usize)?;
            total += u64::from(descriptor.len);
            if total > u64::from(u32::MAX) {
                return Err(QueueError::TooLong);
            }
            buffers.push(Buffer {
                address: descriptor.address,
                len: descriptor.len,
                writable: descriptor.flags & WRITE != 0,
            });
            if descriptor.flags & NEXT == 0 {
                return Ok(buffers);
            }
            index = descriptor.next;
        }
    }

    fn write_used(&mut self, memory: &GuestMemory, head: u16, len: u32) -> Result<(), QueueError> {
        let slot = u64::from(self.next_used % self.size);
        let mut element = [0; 8];
        element[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        element[4..].copy_from_slice(&len.to_le_bytes());
        memory.write(field(self.used, 4 + slot * 8, 8)?, &element)?;
        // The guest must see the element before the index that announces it.
        fence(Ordering::Release);
        let next = self.next_used.wrapping_add(1);
        memory.write(field(self.used, 2, 2)?, &next.to_le_bytes())?;
        self.next_used = next;
        Ok(())
    }
}

struct Descriptor {
    address: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// The address of `offset` inside a guest structure at `base`. The guest
/// picks `base`, so it can wrap.
fn field(base: u64, offset: u64, len: usize) -> Result<u64, OutOfBounds> {
    base.checked_add(offset)
        .ok_or(OutOfBounds { address: base, len })
}

fn read_u16(memory: &GuestMemory, base: u64, offset: u64) -> Result<u16, OutOfBounds> {
    let mut bytes = [0; 2];
    memory.read(field(base, offset, bytes.len())?, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

/// Copies a descriptor in one read, so the guest can't change it between the
/// check and the use.
fn read_descriptor(
    memory: &GuestMemory,
    table: u64,
    index: u16,
) -> Result<Descriptor, OutOfBounds> {
    let mut raw = [0u8; 16];
    memory.read(field(table, u64::from(index) * 16, raw.len())?, &mut raw)?;
    let [
        a0,
        a1,
        a2,
        a3,
        a4,
        a5,
        a6,
        a7,
        l0,
        l1,
        l2,
        l3,
        f0,
        f1,
        n0,
        n1,
    ] = raw;
    Ok(Descriptor {
        address: u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
        len: u32::from_le_bytes([l0, l1, l2, l3]),
        flags: u16::from_le_bytes([f0, f1]),
        next: u16::from_le_bytes([n0, n1]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;
    const SIZE: u16 = 8;
    const DESCRIPTORS: u64 = 0x1000;
    const AVAILABLE: u64 = 0x2000;
    const USED: u64 = 0x3000;
    const DATA: u64 = 0x4000;

    fn memory() -> GuestMemory {
        GuestMemory::new(16 * PAGE).unwrap()
    }

    fn queue() -> Queue {
        let mut queue = Queue::new(SIZE);
        queue.size = SIZE;
        queue.ready = true;
        queue.descriptors = DESCRIPTORS;
        queue.available = AVAILABLE;
        queue.used = USED;
        queue
    }

    fn descriptor(memory: &GuestMemory, i: u16, address: u64, len: u32, flags: u16, next: u16) {
        let at = DESCRIPTORS + u64::from(i) * 16;
        memory.write(at, &address.to_le_bytes()).unwrap();
        memory.write(at + 8, &len.to_le_bytes()).unwrap();
        memory.write(at + 12, &flags.to_le_bytes()).unwrap();
        memory.write(at + 14, &next.to_le_bytes()).unwrap();
    }

    /// Makes `heads` available from the start of the ring.
    fn publish(memory: &GuestMemory, heads: &[u16]) {
        for (slot, head) in heads.iter().enumerate() {
            memory
                .write(AVAILABLE + 4 + slot as u64 * 2, &head.to_le_bytes())
                .unwrap();
        }
        let index = heads.len() as u16;
        memory.write(AVAILABLE + 2, &index.to_le_bytes()).unwrap();
    }

    fn refused(memory: &GuestMemory) -> QueueError {
        publish(memory, &[0]);
        let mut queue = queue();
        let error = queue.pop(memory).unwrap_err();
        assert!(!queue.ready, "a queue that was refused stays stopped");
        assert_eq!(queue.pop(memory), Ok(None));
        error
    }

    fn read_u16(memory: &GuestMemory, address: u64) -> u16 {
        let mut bytes = [0; 2];
        memory.read(address, &mut bytes).unwrap();
        u16::from_le_bytes(bytes)
    }

    fn read_u32(memory: &GuestMemory, address: u64) -> u32 {
        let mut bytes = [0; 4];
        memory.read(address, &mut bytes).unwrap();
        u32::from_le_bytes(bytes)
    }

    #[test]
    fn an_empty_queue_has_nothing_to_pop() {
        assert_eq!(queue().pop(&memory()), Ok(None));
    }

    #[test]
    fn a_queue_that_isnt_ready_has_nothing_to_pop() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 16, 0, 0);
        publish(&memory, &[0]);
        let mut queue = queue();
        queue.ready = false;
        assert_eq!(queue.pop(&memory), Ok(None));
    }

    #[test]
    fn a_chain_comes_back_in_order_with_its_flags() {
        let memory = memory();
        descriptor(&memory, 3, DATA, 16, NEXT, 5);
        descriptor(&memory, 5, DATA + 16, 32, WRITE, 0);
        publish(&memory, &[3]);
        let mut queue = queue();
        let chain = queue.pop(&memory).unwrap().unwrap();
        assert_eq!(chain.head, 3);
        assert_eq!(
            chain.buffers,
            [
                Buffer {
                    address: DATA,
                    len: 16,
                    writable: false
                },
                Buffer {
                    address: DATA + 16,
                    len: 32,
                    writable: true
                },
            ]
        );
        assert_eq!(queue.pop(&memory), Ok(None));
    }

    #[test]
    fn chains_pop_in_ring_order() {
        let memory = memory();
        descriptor(&memory, 1, DATA, 1, 0, 0);
        descriptor(&memory, 2, DATA, 2, 0, 0);
        publish(&memory, &[2, 1]);
        let mut queue = queue();
        assert_eq!(queue.pop(&memory).unwrap().unwrap().head, 2);
        assert_eq!(queue.pop(&memory).unwrap().unwrap().head, 1);
    }

    #[test]
    fn the_ring_and_its_index_wrap() {
        let memory = memory();
        descriptor(&memory, 4, DATA, 1, 0, 0);
        let mut queue = queue();
        queue.next_available = u16::MAX;
        // Slot 65535 % 8 is the last slot of the ring.
        memory
            .write(AVAILABLE + 4 + 7 * 2, &4u16.to_le_bytes())
            .unwrap();
        memory.write(AVAILABLE + 2, &0u16.to_le_bytes()).unwrap();
        assert_eq!(queue.pop(&memory).unwrap().unwrap().head, 4);
        assert_eq!(queue.pop(&memory), Ok(None));
    }

    #[test]
    fn a_full_ring_is_accepted() {
        let memory = memory();
        for i in 0..SIZE {
            descriptor(&memory, i, DATA, 1, 0, 0);
        }
        publish(&memory, &[0, 1, 2, 3, 4, 5, 6, 7]);
        let mut queue = queue();
        for _ in 0..SIZE {
            assert!(queue.pop(&memory).unwrap().is_some());
        }
        assert_eq!(queue.pop(&memory), Ok(None));
    }

    #[test]
    fn an_index_more_than_a_ring_ahead_is_refused() {
        let memory = memory();
        memory
            .write(AVAILABLE + 2, &(SIZE + 1).to_le_bytes())
            .unwrap();
        let mut queue = queue();
        assert_eq!(queue.pop(&memory), Err(QueueError::TooManyAvailable));
        assert!(!queue.ready);
    }

    #[test]
    fn a_head_outside_the_queue_is_refused() {
        let memory = memory();
        publish(&memory, &[SIZE]);
        let mut queue = queue();
        assert_eq!(queue.pop(&memory), Err(QueueError::BadIndex(SIZE)));
    }

    #[test]
    fn a_next_outside_the_queue_is_refused() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 1, NEXT, u16::MAX);
        assert_eq!(refused(&memory), QueueError::BadIndex(u16::MAX));
    }

    #[test]
    fn a_next_is_ignored_without_its_flag() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 1, 0, u16::MAX);
        publish(&memory, &[0]);
        assert_eq!(queue().pop(&memory).unwrap().unwrap().buffers.len(), 1);
    }

    #[test]
    fn a_buffer_outside_guest_memory_is_refused() {
        let memory = memory();
        let past = memory.size() as u64;
        descriptor(&memory, 0, past, 1, 0, 0);
        assert_eq!(
            refused(&memory),
            QueueError::OutOfBounds(OutOfBounds {
                address: past,
                len: 1
            })
        );
    }

    #[test]
    fn a_buffer_may_end_at_the_last_byte_and_no_further() {
        let memory = memory();
        let size = memory.size() as u64;
        descriptor(&memory, 0, size - 16, 16, 0, 0);
        publish(&memory, &[0]);
        assert!(queue().pop(&memory).unwrap().is_some());
        descriptor(&memory, 0, size - 16, 17, 0, 0);
        assert!(matches!(refused(&memory), QueueError::OutOfBounds(_)));
    }

    #[test]
    fn a_length_that_wraps_is_refused() {
        let memory = memory();
        descriptor(&memory, 0, u64::MAX - 7, 16, 0, 0);
        assert!(matches!(refused(&memory), QueueError::OutOfBounds(_)));
    }

    #[test]
    fn a_table_address_that_wraps_is_refused() {
        let memory = memory();
        publish(&memory, &[1]);
        let mut queue = queue();
        queue.descriptors = u64::MAX - 7;
        assert!(matches!(
            queue.pop(&memory),
            Err(QueueError::OutOfBounds(_))
        ));
    }

    #[test]
    fn rings_outside_guest_memory_are_refused() {
        let memory = memory();
        let mut queue = queue();
        queue.available = u64::MAX;
        assert!(matches!(
            queue.pop(&memory),
            Err(QueueError::OutOfBounds(_))
        ));
    }

    #[test]
    fn a_descriptor_that_points_at_itself_is_refused() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 1, NEXT, 0);
        assert_eq!(refused(&memory), QueueError::TooLong);
    }

    #[test]
    fn a_longer_loop_is_refused() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 1, NEXT, 1);
        descriptor(&memory, 1, DATA, 1, NEXT, 2);
        descriptor(&memory, 2, DATA, 1, NEXT, 1);
        assert_eq!(refused(&memory), QueueError::TooLong);
    }

    #[test]
    fn a_chain_as_long_as_the_queue_is_accepted() {
        let memory = memory();
        for i in 0..SIZE - 1 {
            descriptor(&memory, i, DATA, 1, NEXT, i + 1);
        }
        descriptor(&memory, SIZE - 1, DATA, 1, 0, 0);
        publish(&memory, &[0]);
        assert_eq!(
            queue().pop(&memory).unwrap().unwrap().buffers.len(),
            usize::from(SIZE)
        );
    }

    #[test]
    fn a_chain_holding_too_many_bytes_is_refused() {
        // Nothing touches these pages, so the mapping costs no memory.
        let memory = GuestMemory::new(1 << 31).unwrap();
        descriptor(&memory, 0, 0, 1 << 31, NEXT, 1);
        descriptor(&memory, 1, 0, 1 << 31, 0, 0);
        assert_eq!(refused(&memory), QueueError::TooLong);
    }

    #[test]
    fn the_largest_chain_a_used_element_can_report_is_accepted() {
        let memory = GuestMemory::new(1 << 31).unwrap();
        descriptor(&memory, 0, 0, 1 << 31, NEXT, 1);
        descriptor(&memory, 1, 0, (1 << 31) - 1, 0, 0);
        publish(&memory, &[0]);
        assert!(queue().pop(&memory).unwrap().is_some());
    }

    #[test]
    fn an_indirect_descriptor_is_refused() {
        let memory = memory();
        descriptor(&memory, 0, DATA, 16, INDIRECT, 0);
        assert_eq!(refused(&memory), QueueError::Indirect);
    }

    #[test]
    fn a_used_element_reaches_the_ring_and_moves_the_index() {
        let memory = memory();
        let mut queue = queue();
        queue.add_used(&memory, 3, 77).unwrap();
        assert_eq!(read_u32(&memory, USED + 4), 3);
        assert_eq!(read_u32(&memory, USED + 8), 77);
        assert_eq!(read_u16(&memory, USED + 2), 1);
        queue.add_used(&memory, 5, 9).unwrap();
        assert_eq!(read_u32(&memory, USED + 12), 5);
        assert_eq!(read_u16(&memory, USED + 2), 2);
    }

    #[test]
    fn the_used_ring_wraps() {
        let memory = memory();
        let mut queue = queue();
        for head in 0..=SIZE {
            queue.add_used(&memory, head, 0).unwrap();
        }
        assert_eq!(read_u32(&memory, USED + 4), u32::from(SIZE));
        assert_eq!(read_u16(&memory, USED + 2), SIZE + 1);
    }

    #[test]
    fn a_used_ring_outside_guest_memory_is_refused() {
        let memory = memory();
        let mut queue = queue();
        queue.used = u64::MAX - 1;
        assert!(matches!(
            queue.add_used(&memory, 0, 0),
            Err(QueueError::OutOfBounds(_))
        ));
        assert!(!queue.ready);
    }

    #[test]
    fn a_queue_that_isnt_ready_writes_nothing() {
        let memory = memory();
        let mut queue = Queue::new(SIZE);
        queue.used = USED;
        queue.add_used(&memory, 1, 1).unwrap();
        assert_eq!(read_u16(&memory, USED + 2), 0);
    }

    #[test]
    fn an_error_says_what_the_guest_did() {
        assert_eq!(
            QueueError::BadIndex(9).to_string(),
            "descriptor 9 is outside the queue"
        );
        assert_eq!(
            QueueError::OutOfBounds(OutOfBounds {
                address: 0x10,
                len: 2
            })
            .to_string(),
            "2 bytes at guest address 0x10 are outside guest memory"
        );
    }
}
