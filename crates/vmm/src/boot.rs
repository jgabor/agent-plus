//! Loads an uncompressed `vmlinux` and enters it through PVH.

use crate::{GuestMemory, OutOfBounds, Regs, Segment, Vcpu};
use std::fmt;
use std::io;

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: u64 = 56;
const ELF_MAGIC: [u8; 4] = *b"\x7fELF";
const ELF_CLASS_64: u8 = 2;
const ELF_LITTLE_ENDIAN: u8 = 1;
const ELF_EXECUTABLE: u16 = 2;
const ELF_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const NOTE_HEADER_SIZE: u64 = 12;
const XEN_NOTE_NAME: &[u8] = b"Xen\0";
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;

// Where the boot structures go. All of it is below the kernel, which a segment
// can't reach because it has to start at `HIGH_MEMORY` or above.
const GDT_ADDRESS: u64 = 0x500;
const START_INFO_ADDRESS: u64 = 0x6000;
const MEMORY_MAP_ADDRESS: u64 = 0x7000;
const COMMAND_LINE_ADDRESS: u64 = 0x2_0000;
/// `COMMAND_LINE_SIZE` on x86, which counts the final NUL.
const COMMAND_LINE_SIZE: usize = 2048;

/// The end of conventional memory. The PC reserves what follows, up to 1 MiB.
const LOW_MEMORY_END: u64 = 0x9_fc00;
const HIGH_MEMORY: u64 = 0x10_0000;

const START_INFO_MAGIC: u32 = 0x336e_c578;
const START_INFO_VERSION: u32 = 1;
const E820_RAM: u32 = 1;

const GDT: [u64; 4] = [
    0,
    0x00cf_9b00_0000_ffff, // Code: flat, 32-bit, read and execute.
    0x00cf_9300_0000_ffff, // Data: flat, read and write.
    0x0000_8b00_0000_0067, // A busy 32-bit TSS, which PVH asks for.
];
const CODE_SELECTOR: u16 = 0x08;
const DATA_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;

const CR0_PE: u64 = 1;
const CR0_ET: u64 = 1 << 4;
/// Bit 1 of RFLAGS is reserved, and must be set.
const RFLAGS_RESERVED: u64 = 2;

/// Why a kernel can't be loaded.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The file isn't a well-formed `vmlinux` that this VMM can enter.
    Malformed(&'static str),
    /// The command line is too long, or holds a NUL.
    CommandLine,
    /// Part of the kernel or its boot data doesn't fit in guest memory.
    Memory(OutOfBounds),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "not a bootable vmlinux: {reason}"),
            Self::CommandLine => write!(f, "the kernel command line is not valid"),
            Self::Memory(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<OutOfBounds> for Error {
    fn from(error: OutOfBounds) -> Self {
        Self::Memory(error)
    }
}

/// A loaded kernel, ready to be entered.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    address: u64,
}

/// `len` bytes of `file` at `offset`, if they are all there.
fn slice(file: &[u8], offset: u64, len: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    file.get(start..end)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}

/// Rounds up to the 4-byte alignment of ELF notes.
fn align_note(value: u64) -> u64 {
    (value + 3) & !3
}

/// Returns the PVH entry point from a segment of ELF notes, if it has one.
fn pvh_entry(notes: &[u8]) -> Result<Option<u64>, Error> {
    let malformed = Error::Malformed("an ELF note runs past its segment");
    let mut offset = 0_u64;
    while offset < notes.len() as u64 {
        let Some(header) = slice(notes, offset, NOTE_HEADER_SIZE) else {
            return Err(malformed);
        };
        let name_size = u64::from(u32_at(header, 0));
        let description_size = u64::from(u32_at(header, 4));
        let kind = u32_at(header, 8);
        let name_offset = offset + NOTE_HEADER_SIZE;
        let description_offset = name_offset + align_note(name_size);
        let (Some(name), Some(description)) = (
            slice(notes, name_offset, name_size),
            slice(notes, description_offset, description_size),
        ) else {
            return Err(malformed);
        };
        if name == XEN_NOTE_NAME && kind == XEN_ELFNOTE_PHYS32_ENTRY {
            return match description.len() {
                4 => Ok(Some(u64::from(u32_at(description, 0)))),
                8 => Ok(Some(u64_at(description, 0))),
                _ => Err(Error::Malformed("the PVH entry note has the wrong size")),
            };
        }
        offset = description_offset + align_note(description_size);
    }
    Ok(None)
}

fn check_elf_header(header: &[u8]) -> Result<(), Error> {
    if header[..4] != ELF_MAGIC {
        return Err(Error::Malformed("no ELF magic"));
    }
    if header[4] != ELF_CLASS_64 || header[5] != ELF_LITTLE_ENDIAN {
        return Err(Error::Malformed("not 64-bit little-endian"));
    }
    if u16_at(header, 16) != ELF_EXECUTABLE {
        return Err(Error::Malformed("not an executable"));
    }
    if u16_at(header, 18) != ELF_X86_64 {
        return Err(Error::Malformed("not x86_64"));
    }
    if u64::from(u16_at(header, 54)) != PROGRAM_HEADER_SIZE {
        return Err(Error::Malformed("unexpected program header size"));
    }
    Ok(())
}

/// Copies one loadable segment into guest memory, and returns the guest
/// range its file bytes landed in.
fn load_segment(memory: &GuestMemory, file: &[u8], header: &[u8]) -> Result<(u64, u64), Error> {
    let offset = u64_at(header, 8);
    let address = u64_at(header, 24);
    let file_size = u64_at(header, 32);
    let memory_size = u64_at(header, 40);
    if file_size > memory_size {
        return Err(Error::Malformed(
            "a segment's file size exceeds its memory size",
        ));
    }
    if address < HIGH_MEMORY {
        return Err(Error::Malformed("a segment overlaps the boot data"));
    }
    let bytes = slice(file, offset, file_size)
        .ok_or(Error::Malformed("a segment runs past the end of the file"))?;
    address
        .checked_add(memory_size)
        .filter(|end| *end <= memory.size() as u64)
        .ok_or(OutOfBounds {
            address,
            len: memory_size as usize,
        })?;
    memory.write(address, bytes)?;
    Ok((address, address + file_size))
}

/// Writes the structures a PVH kernel reads when it starts.
fn write_boot_data(memory: &GuestMemory, command_line: &str) -> Result<(), Error> {
    if command_line.len() >= COMMAND_LINE_SIZE || command_line.contains('\0') {
        return Err(Error::CommandLine);
    }
    // The final NUL is already there, because guest memory starts zeroed.
    memory.write(COMMAND_LINE_ADDRESS, command_line.as_bytes())?;

    let ram = [
        (0, LOW_MEMORY_END),
        (
            HIGH_MEMORY,
            (memory.size() as u64).saturating_sub(HIGH_MEMORY),
        ),
    ];
    for (index, (address, size)) in ram.iter().enumerate() {
        let mut entry = [0; 24];
        entry[..8].copy_from_slice(&address.to_le_bytes());
        entry[8..16].copy_from_slice(&size.to_le_bytes());
        entry[16..20].copy_from_slice(&E820_RAM.to_le_bytes());
        memory.write(MEMORY_MAP_ADDRESS + 24 * index as u64, &entry)?;
    }

    // `struct hvm_start_info`.
    let mut info = [0; 56];
    info[..4].copy_from_slice(&START_INFO_MAGIC.to_le_bytes());
    info[4..8].copy_from_slice(&START_INFO_VERSION.to_le_bytes());
    info[24..32].copy_from_slice(&COMMAND_LINE_ADDRESS.to_le_bytes());
    info[40..48].copy_from_slice(&MEMORY_MAP_ADDRESS.to_le_bytes());
    info[48..52].copy_from_slice(&(ram.len() as u32).to_le_bytes());
    memory.write(START_INFO_ADDRESS, &info)?;

    for (index, descriptor) in GDT.iter().enumerate() {
        memory.write(GDT_ADDRESS + 8 * index as u64, &descriptor.to_le_bytes())?;
    }
    Ok(())
}

/// Loads the ELF `file` and its boot data into `memory`, which must be
/// zeroed, as a new [`GuestMemory`] is. Nothing clears a segment's tail.
pub fn load(memory: &GuestMemory, file: &[u8], command_line: &str) -> Result<Entry, Error> {
    let header = file
        .get(..ELF_HEADER_SIZE)
        .ok_or(Error::Malformed("shorter than an ELF header"))?;
    check_elf_header(header)?;
    let table_size = u64::from(u16_at(header, 56)) * PROGRAM_HEADER_SIZE;
    let table = slice(file, u64_at(header, 32), table_size).ok_or(Error::Malformed(
        "the program headers run past the end of the file",
    ))?;

    let mut entry = None;
    let mut loaded = Vec::new();
    for header in table.as_chunks::<{ PROGRAM_HEADER_SIZE as usize }>().0 {
        match u32_at(header, 0) {
            PT_LOAD => loaded.push(load_segment(memory, file, header)?),
            PT_NOTE if entry.is_none() => {
                let notes = slice(file, u64_at(header, 8), u64_at(header, 32)).ok_or(
                    Error::Malformed("a note segment runs past the end of the file"),
                )?;
                entry = pvh_entry(notes)?;
            }
            _ => {}
        }
    }

    let address = entry.ok_or(Error::Malformed("no PVH entry note"))?;
    if !loaded
        .iter()
        .any(|(start, end)| (*start..*end).contains(&address))
    {
        return Err(Error::Malformed("the PVH entry point isn't in a segment"));
    }
    write_boot_data(memory, command_line)?;
    Ok(Entry { address })
}

fn flat_segment(selector: u16, type_: u8) -> Segment {
    Segment {
        base: 0,
        limit: u32::MAX,
        selector,
        type_,
        present: 1,
        s: 1,
        db: 1,
        g: 1,
        ..Segment::default()
    }
}

/// Puts `vcpu` in the state the PVH boot protocol starts a kernel in: 32-bit
/// protected mode, paging off, flat segments, and the start info in `rbx`.
pub fn enter(vcpu: &Vcpu, entry: &Entry) -> io::Result<()> {
    let mut sregs = vcpu.sregs()?;
    let data = flat_segment(DATA_SELECTOR, 0x3);
    sregs.cs = flat_segment(CODE_SELECTOR, 0xb);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = Segment {
        limit: 0x67,
        selector: TSS_SELECTOR,
        type_: 0xb,
        present: 1,
        ..Segment::default()
    };
    sregs.gdt.base = GDT_ADDRESS;
    sregs.gdt.limit = (size_of_val(&GDT) - 1) as u16;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    sregs.cr0 = CR0_PE | CR0_ET;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0;
    vcpu.set_sregs(&sregs)?;

    vcpu.set_regs(&Regs {
        rip: entry.address,
        rbx: START_INFO_ADDRESS,
        rflags: RFLAGS_RESERVED,
        ..Regs::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: usize = 4 << 20;
    const ADDRESS: u64 = 2 << 20;
    const ENTRY: u64 = ADDRESS + 4;
    const CODE: &[u8] = b"kernel-code!";
    const CODE_OFFSET: usize = 0x200;
    const NOTE_OFFSET: usize = 0x100;

    struct ProgramHeader {
        kind: u32,
        offset: u64,
        address: u64,
        file_size: u64,
        memory_size: u64,
    }

    fn note(name: &[u8], kind: u32, description: &[u8]) -> Vec<u8> {
        let mut note = Vec::new();
        note.extend((name.len() as u32).to_le_bytes());
        note.extend((description.len() as u32).to_le_bytes());
        note.extend(kind.to_le_bytes());
        note.extend(name);
        note.resize(note.len().next_multiple_of(4), 0);
        note.extend(description);
        note.resize(note.len().next_multiple_of(4), 0);
        note
    }

    fn entry_note(entry: u64) -> Vec<u8> {
        note(
            XEN_NOTE_NAME,
            XEN_ELFNOTE_PHYS32_ENTRY,
            &entry.to_le_bytes(),
        )
    }

    fn elf(segments: &[ProgramHeader], notes: &[u8]) -> Vec<u8> {
        let mut file = vec![0; CODE_OFFSET + CODE.len()];
        file[..4].copy_from_slice(&ELF_MAGIC);
        file[4] = ELF_CLASS_64;
        file[5] = ELF_LITTLE_ENDIAN;
        file[16..18].copy_from_slice(&ELF_EXECUTABLE.to_le_bytes());
        file[18..20].copy_from_slice(&ELF_X86_64.to_le_bytes());
        file[32..40].copy_from_slice(&(ELF_HEADER_SIZE as u64).to_le_bytes());
        file[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        file[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (index, segment) in segments.iter().enumerate() {
            let at = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE as usize;
            file[at..at + 4].copy_from_slice(&segment.kind.to_le_bytes());
            file[at + 8..at + 16].copy_from_slice(&segment.offset.to_le_bytes());
            file[at + 24..at + 32].copy_from_slice(&segment.address.to_le_bytes());
            file[at + 32..at + 40].copy_from_slice(&segment.file_size.to_le_bytes());
            file[at + 40..at + 48].copy_from_slice(&segment.memory_size.to_le_bytes());
        }
        file[NOTE_OFFSET..NOTE_OFFSET + notes.len()].copy_from_slice(notes);
        file[CODE_OFFSET..].copy_from_slice(CODE);
        file
    }

    fn code_segment() -> ProgramHeader {
        ProgramHeader {
            kind: PT_LOAD,
            offset: CODE_OFFSET as u64,
            address: ADDRESS,
            file_size: CODE.len() as u64,
            memory_size: CODE.len() as u64 + 16,
        }
    }

    fn note_segment(len: usize) -> ProgramHeader {
        ProgramHeader {
            kind: PT_NOTE,
            offset: NOTE_OFFSET as u64,
            address: 0,
            file_size: len as u64,
            memory_size: 0,
        }
    }

    fn valid() -> Vec<u8> {
        let notes = entry_note(ENTRY);
        elf(&[code_segment(), note_segment(notes.len())], &notes)
    }

    fn memory() -> GuestMemory {
        GuestMemory::new(SIZE).unwrap()
    }

    fn load_with(file: &[u8]) -> Result<Entry, Error> {
        load(&memory(), file, "console=ttyS0")
    }

    fn read_u32(memory: &GuestMemory, address: u64) -> u32 {
        let mut bytes = [0; 4];
        memory.read(address, &mut bytes).unwrap();
        u32::from_le_bytes(bytes)
    }

    fn read_u64(memory: &GuestMemory, address: u64) -> u64 {
        let mut bytes = [0; 8];
        memory.read(address, &mut bytes).unwrap();
        u64::from_le_bytes(bytes)
    }

    fn assert_malformed(result: Result<Entry, Error>) {
        assert!(matches!(result, Err(Error::Malformed(_))), "{result:?}");
    }

    #[test]
    fn a_segment_lands_at_its_physical_address() {
        let memory = memory();
        load(&memory, &valid(), "").unwrap();
        let mut bytes = vec![0; CODE.len()];
        memory.read(ADDRESS, &mut bytes).unwrap();
        assert_eq!(bytes, CODE);
    }

    #[test]
    fn the_entry_comes_from_the_pvh_note() {
        assert_eq!(load_with(&valid()), Ok(Entry { address: ENTRY }));
    }

    #[test]
    fn a_four_byte_entry_note_works() {
        let notes = note(
            XEN_NOTE_NAME,
            XEN_ELFNOTE_PHYS32_ENTRY,
            &(ENTRY as u32).to_le_bytes(),
        );
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_eq!(load_with(&file), Ok(Entry { address: ENTRY }));
    }

    #[test]
    fn other_notes_are_skipped() {
        let mut notes = note(b"GNU\0", 3, b"abcdef");
        notes.extend(note(XEN_NOTE_NAME, 5, b"xen-3.0"));
        notes.extend(entry_note(ENTRY));
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_eq!(load_with(&file), Ok(Entry { address: ENTRY }));
    }

    #[test]
    fn start_info_describes_the_command_line_and_memory() {
        let memory = memory();
        load(&memory, &valid(), "console=ttyS0").unwrap();

        assert_eq!(read_u32(&memory, START_INFO_ADDRESS), START_INFO_MAGIC);
        assert_eq!(
            read_u32(&memory, START_INFO_ADDRESS + 4),
            START_INFO_VERSION
        );
        assert_eq!(
            read_u64(&memory, START_INFO_ADDRESS + 24),
            COMMAND_LINE_ADDRESS
        );
        assert_eq!(
            read_u64(&memory, START_INFO_ADDRESS + 40),
            MEMORY_MAP_ADDRESS
        );
        assert_eq!(read_u32(&memory, START_INFO_ADDRESS + 48), 2);

        let mut line = [0xaa; 14];
        memory.read(COMMAND_LINE_ADDRESS, &mut line).unwrap();
        assert_eq!(&line, b"console=ttyS0\0");

        assert_eq!(read_u64(&memory, MEMORY_MAP_ADDRESS), 0);
        assert_eq!(read_u64(&memory, MEMORY_MAP_ADDRESS + 8), LOW_MEMORY_END);
        assert_eq!(read_u32(&memory, MEMORY_MAP_ADDRESS + 16), E820_RAM);
        assert_eq!(read_u64(&memory, MEMORY_MAP_ADDRESS + 24), HIGH_MEMORY);
        assert_eq!(
            read_u64(&memory, MEMORY_MAP_ADDRESS + 32),
            SIZE as u64 - HIGH_MEMORY
        );
        assert_eq!(read_u32(&memory, MEMORY_MAP_ADDRESS + 40), E820_RAM);
    }

    #[test]
    fn the_gdt_is_in_guest_memory() {
        let memory = memory();
        load(&memory, &valid(), "").unwrap();
        for (index, descriptor) in GDT.iter().enumerate() {
            assert_eq!(
                read_u64(&memory, GDT_ADDRESS + 8 * index as u64),
                *descriptor
            );
        }
    }

    #[test]
    fn a_command_line_must_fit_and_hold_no_nul() {
        let longest = "a".repeat(COMMAND_LINE_SIZE - 1);
        assert!(load(&memory(), &valid(), &longest).is_ok());
        let long = "a".repeat(COMMAND_LINE_SIZE);
        assert_eq!(load(&memory(), &valid(), &long), Err(Error::CommandLine));
        assert_eq!(load(&memory(), &valid(), "a\0b"), Err(Error::CommandLine));
    }

    #[test]
    fn a_short_file_is_refused() {
        assert_malformed(load_with(&[]));
        assert_malformed(load_with(&valid()[..ELF_HEADER_SIZE - 1]));
    }

    #[test]
    fn a_wrong_header_field_is_refused() {
        for (at, value) in [(0, 0), (4, 1), (5, 2), (16, 3), (18, 3), (54, 57)] {
            let mut file = valid();
            file[at] = value;
            assert_malformed(load_with(&file));
        }
    }

    #[test]
    fn program_headers_past_the_end_are_refused() {
        let mut file = valid();
        file[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_malformed(load_with(&file));

        let mut file = valid();
        file[56..58].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_malformed(load_with(&file));
    }

    fn with_segment(change: impl FnOnce(&mut ProgramHeader)) -> Vec<u8> {
        let notes = entry_note(ENTRY);
        let mut segment = code_segment();
        change(&mut segment);
        elf(&[segment, note_segment(notes.len())], &notes)
    }

    #[test]
    fn a_segment_past_the_end_of_the_file_is_refused() {
        assert_malformed(load_with(&with_segment(|s| s.offset = u64::MAX)));
        assert_malformed(load_with(&with_segment(|s| {
            s.file_size = 1 << 16;
            s.memory_size = 1 << 16;
        })));
    }

    #[test]
    fn a_file_size_above_the_memory_size_is_refused() {
        assert_malformed(load_with(&with_segment(|s| s.memory_size = 1)));
    }

    #[test]
    fn a_segment_outside_guest_memory_is_refused() {
        let beyond = with_segment(|s| s.address = SIZE as u64 - 1);
        assert!(matches!(load_with(&beyond), Err(Error::Memory(_))));
        // The tail that holds no file bytes has to fit too.
        let tail = with_segment(|s| s.memory_size = SIZE as u64);
        assert!(matches!(load_with(&tail), Err(Error::Memory(_))));
        let wraps = with_segment(|s| s.memory_size = u64::MAX);
        assert!(matches!(load_with(&wraps), Err(Error::Memory(_))));
    }

    #[test]
    fn a_segment_may_end_at_the_end_of_memory() {
        let file = with_segment(|s| {
            s.address = SIZE as u64 - s.memory_size;
        });
        let notes = entry_note(SIZE as u64 - code_segment().memory_size);
        let mut file = file;
        file[NOTE_OFFSET..NOTE_OFFSET + notes.len()].copy_from_slice(&notes);
        assert!(load_with(&file).is_ok());
    }

    #[test]
    fn a_segment_over_the_boot_data_is_refused() {
        assert_malformed(load_with(&with_segment(|s| s.address = HIGH_MEMORY - 1)));
    }

    #[test]
    fn a_file_with_no_entry_note_is_refused() {
        assert_malformed(load_with(&elf(&[code_segment()], &[])));
        let notes = note(b"GNU\0", XEN_ELFNOTE_PHYS32_ENTRY, &ENTRY.to_le_bytes());
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_malformed(load_with(&file));
    }

    #[test]
    fn a_broken_note_is_refused() {
        let mut notes = entry_note(ENTRY);
        // The name size claims more than the segment holds.
        notes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_malformed(load_with(&file));

        let mut notes = entry_note(ENTRY);
        notes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_malformed(load_with(&file));

        // Too short to be a note header.
        let file = elf(&[code_segment(), note_segment(8)], &[1; 8]);
        assert_malformed(load_with(&file));

        // A note segment that leaves the file.
        let mut segment = note_segment(8);
        segment.offset = u64::MAX;
        assert_malformed(load_with(&elf(&[code_segment(), segment], &[])));
    }

    #[test]
    fn an_entry_note_of_the_wrong_size_is_refused() {
        let notes = note(XEN_NOTE_NAME, XEN_ELFNOTE_PHYS32_ENTRY, &[0; 6]);
        let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
        assert_malformed(load_with(&file));
    }

    #[test]
    fn an_entry_outside_the_loaded_bytes_is_refused() {
        for entry in [
            ADDRESS - 1,
            ADDRESS + CODE.len() as u64,
            ADDRESS + CODE.len() as u64 + 8,
        ] {
            let notes = entry_note(entry);
            let file = elf(&[code_segment(), note_segment(notes.len())], &notes);
            assert_malformed(load_with(&file));
        }
    }

    #[test]
    fn memory_too_small_for_the_boot_data_is_refused() {
        let memory = GuestMemory::new(4096).unwrap();
        assert!(matches!(load(&memory, &valid(), ""), Err(Error::Memory(_))));
    }
}
