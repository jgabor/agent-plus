//! Guest RAM.

use crate::sys::Mapping;
use std::fmt;
use std::io;
use std::ptr;

/// An access that doesn't fit inside guest memory.
#[derive(Debug, PartialEq, Eq)]
pub struct OutOfBounds {
    pub address: u64,
    pub len: usize,
}

impl fmt::Display for OutOfBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} bytes at guest address {:#x} are outside guest memory",
            self.len, self.address
        )
    }
}

impl std::error::Error for OutOfBounds {}

/// Guest RAM, which starts at guest address 0.
///
/// Addresses and lengths may come from the guest, so every access is checked.
pub struct GuestMemory {
    mapping: Mapping,
}

impl GuestMemory {
    /// Allocates `size` bytes of zeroed memory. KVM needs a multiple of the
    /// page size.
    pub fn new(size: usize) -> io::Result<Self> {
        Ok(Self {
            mapping: Mapping::anonymous(size)?,
        })
    }

    pub fn size(&self) -> usize {
        self.mapping.len()
    }

    pub(crate) fn start(&self) -> *mut u8 {
        self.mapping.start()
    }

    /// Fills `buffer` from guest memory at `address`.
    pub fn read(&self, address: u64, buffer: &mut [u8]) -> Result<(), OutOfBounds> {
        let offset = self.check(address, buffer.len())?;
        // SAFETY: `check` proved the source lies inside the mapping, and
        // `buffer` is host memory, so the two can't overlap.
        unsafe {
            ptr::copy_nonoverlapping(self.start().add(offset), buffer.as_mut_ptr(), buffer.len());
        }
        Ok(())
    }

    /// Copies `bytes` into guest memory at `address`.
    pub fn write(&self, address: u64, bytes: &[u8]) -> Result<(), OutOfBounds> {
        let offset = self.check(address, bytes.len())?;
        // SAFETY: `check` proved the destination lies inside the mapping, and
        // `bytes` is host memory, so the two can't overlap.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.start().add(offset), bytes.len());
        }
        Ok(())
    }

    /// Returns the mapping offset of an access that fits, without wrapping.
    fn check(&self, address: u64, len: usize) -> Result<usize, OutOfBounds> {
        let fits = address
            .checked_add(len as u64)
            .is_some_and(|end| end <= self.size() as u64);
        if fits {
            Ok(address as usize)
        } else {
            Err(OutOfBounds { address, len })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    fn memory() -> GuestMemory {
        GuestMemory::new(PAGE).unwrap()
    }

    #[test]
    fn a_write_reads_back() {
        let memory = memory();
        memory.write(100, &[1, 2, 3]).unwrap();
        let mut bytes = [0; 5];
        memory.read(99, &mut bytes).unwrap();
        assert_eq!(bytes, [0, 1, 2, 3, 0]);
    }

    #[test]
    fn the_last_byte_is_reachable() {
        let memory = memory();
        memory.write(PAGE as u64 - 1, &[7]).unwrap();
        let mut byte = [0];
        memory.read(PAGE as u64 - 1, &mut byte).unwrap();
        assert_eq!(byte, [7]);
    }

    #[test]
    fn an_access_may_end_at_the_last_byte_and_no_further() {
        let memory = memory();
        assert!(memory.write(PAGE as u64 - 2, &[0; 2]).is_ok());
        let error = memory.write(PAGE as u64 - 1, &[0; 2]).unwrap_err();
        assert_eq!(
            error,
            OutOfBounds {
                address: PAGE as u64 - 1,
                len: 2
            }
        );
    }

    #[test]
    fn a_read_past_the_end_is_refused_and_leaves_the_buffer() {
        let memory = memory();
        let mut bytes = [9; 2];
        assert!(memory.read(PAGE as u64 - 1, &mut bytes).is_err());
        assert_eq!(bytes, [9; 2]);
    }

    #[test]
    fn an_address_past_the_end_is_refused_even_when_empty() {
        let memory = memory();
        assert!(memory.write(PAGE as u64 + 1, &[]).is_err());
        assert!(memory.read(PAGE as u64 + 1, &mut []).is_err());
    }

    #[test]
    fn an_address_that_wraps_is_refused() {
        let memory = memory();
        assert!(memory.write(u64::MAX, &[0; 2]).is_err());
        assert!(memory.read(u64::MAX, &mut [0; 2]).is_err());
    }
}
