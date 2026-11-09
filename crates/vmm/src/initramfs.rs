//! The in-memory initramfs: an uncompressed cpio archive in the `newc` format,
//! which the kernel unpacks into its root filesystem.

use std::collections::BTreeMap;
use std::fmt;

const MAGIC: &str = "070701";
const TRAILER: &str = "TRAILER!!!";
const ALIGNMENT: usize = 4;
/// `PATH_MAX`, which counts the final NUL. The kernel refuses longer names.
const PATH_MAX: usize = 4096;
/// Only the permission bits of a mode are the caller's to choose.
const PERMISSIONS: u32 = 0o7777;

const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const DIRECTORY_MODE: u32 = S_IFDIR | 0o755;
const SYMLINK_MODE: u32 = S_IFLNK | 0o777;

/// One thing to put in the guest's root filesystem. A path is relative to the
/// root, and the directories above it are created for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry<'a> {
    File {
        path: &'a str,
        mode: u32,
        contents: &'a [u8],
    },
    Symlink {
        path: &'a str,
        target: &'a str,
    },
}

/// Why an archive can't be built.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The path isn't a plain relative path, or it's too long.
    BadPath(String),
    /// The symlink target is empty, too long, or holds a NUL.
    BadTarget(String),
    /// Two entries claim the same path, or a file is used as a directory.
    Conflict(String),
    /// The file is too large for the format's 32-bit size field.
    TooLarge(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPath(path) => write!(f, "not a valid initramfs path: {path:?}"),
            Self::BadTarget(path) => write!(f, "not a valid symlink target for {path:?}"),
            Self::Conflict(path) => write!(f, "the initramfs lists {path:?} twice"),
            Self::TooLarge(path) => write!(f, "too large for an initramfs: {path:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// Appends archive records, numbering their inodes.
struct Archive {
    bytes: Vec<u8>,
    next_inode: u32,
}

impl Archive {
    fn pad(&mut self) {
        self.bytes
            .resize(self.bytes.len().next_multiple_of(ALIGNMENT), 0);
    }

    fn record(&mut self, name: &str, mode: u32, data: &[u8]) {
        let inode = self.next_inode;
        self.next_inode += 1;
        // These fit: `checked` bounds the name and the data before this.
        let (size, name_size) = (data.len() as u32, name.len() as u32 + 1);
        let links = 1;
        // Inode, mode, uid, gid, links, mtime, size, four device numbers, name
        // size, and a checksum that this format leaves at zero.
        let fields = [inode, mode, 0, 0, links, 0, size, 0, 0, 0, 0, name_size, 0];
        self.bytes.extend(MAGIC.as_bytes());
        for field in fields {
            self.bytes.extend(format!("{field:08x}").as_bytes());
        }
        self.bytes.extend(name.as_bytes());
        self.bytes.push(0);
        self.pad();
        self.bytes.extend(data);
        self.pad();
    }
}

/// Refuses a path the kernel would resolve to somewhere other than it says.
fn check_path(path: &str) -> Result<(), Error> {
    let plain = path
        .split('/')
        .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    // The kernel stops unpacking at a record with the trailer's name.
    let top = path.split('/').next();
    if plain && top != Some(TRAILER) && path.len() < PATH_MAX {
        Ok(())
    } else {
        Err(Error::BadPath(path.to_string()))
    }
}

fn parts<'a>(entry: &Entry<'a>) -> Result<(&'a str, u32, &'a [u8]), Error> {
    match *entry {
        Entry::File {
            path,
            mode,
            contents,
        } => {
            if u32::try_from(contents.len()).is_err() {
                return Err(Error::TooLarge(path.to_string()));
            }
            Ok((path, S_IFREG | (mode & PERMISSIONS), contents))
        }
        Entry::Symlink { path, target } => {
            if target.is_empty() || target.len() >= PATH_MAX || target.contains('\0') {
                return Err(Error::BadTarget(path.to_string()));
            }
            Ok((path, SYMLINK_MODE, target.as_bytes()))
        }
    }
}

/// Builds the archive. It holds `entries` in order, each after its parents.
pub fn build(entries: &[Entry]) -> Result<Vec<u8>, Error> {
    let mut archive = Archive {
        bytes: Vec::new(),
        next_inode: 1,
    };
    // Whether each path so far is a directory.
    let mut seen = BTreeMap::new();
    for entry in entries {
        let (path, mode, data) = parts(entry)?;
        check_path(path)?;
        for (end, _) in path.match_indices('/') {
            let parent = &path[..end];
            match seen.get(parent) {
                Some(true) => {}
                Some(false) => return Err(Error::Conflict(parent.to_string())),
                None => {
                    seen.insert(parent, true);
                    archive.record(parent, DIRECTORY_MODE, &[]);
                }
            }
        }
        if seen.insert(path, false).is_some() {
            return Err(Error::Conflict(path.to_string()));
        }
        archive.record(path, mode, data);
    }
    archive.record(TRAILER, 0, &[]);
    Ok(archive.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER_SIZE: usize = 110;

    #[derive(Debug, PartialEq, Eq)]
    struct Record {
        inode: u32,
        mode: u32,
        links: u32,
        name: String,
        data: Vec<u8>,
    }

    fn field(header: &[u8], index: usize) -> u32 {
        let start = MAGIC.len() + index * 8;
        let text = str::from_utf8(&header[start..start + 8]).unwrap();
        u32::from_str_radix(text, 16).unwrap()
    }

    /// Reads an archive strictly, as the kernel's parser does.
    fn parse(archive: &[u8]) -> Vec<Record> {
        assert_eq!(archive.len() % ALIGNMENT, 0, "the archive isn't aligned");
        let mut records = Vec::new();
        let mut at = 0;
        loop {
            let header = &archive[at..at + HEADER_SIZE];
            assert_eq!(&header[..MAGIC.len()], MAGIC.as_bytes());
            let (size, name_size) = (field(header, 6) as usize, field(header, 11) as usize);
            let name_start = at + HEADER_SIZE;
            let name = &archive[name_start..name_start + name_size];
            assert_eq!(name.last(), Some(&0), "the name isn't terminated");
            let data_start = (name_start + name_size).next_multiple_of(ALIGNMENT);
            assert!(
                archive[name_start + name_size..data_start]
                    .iter()
                    .all(|b| *b == 0)
            );
            let data_end = data_start + size;
            let next = data_end.next_multiple_of(ALIGNMENT);
            assert!(archive[data_end..next].iter().all(|b| *b == 0));
            let record = Record {
                inode: field(header, 0),
                mode: field(header, 1),
                links: field(header, 4),
                name: str::from_utf8(&name[..name_size - 1]).unwrap().to_string(),
                data: archive[data_start..data_end].to_vec(),
            };
            let done = record.name == TRAILER;
            records.push(record);
            at = next;
            if done {
                assert_eq!(at, archive.len(), "bytes follow the trailer");
                return records;
            }
        }
    }

    fn file<'a>(path: &'a str, contents: &'a [u8]) -> Entry<'a> {
        Entry::File {
            path,
            mode: 0o755,
            contents,
        }
    }

    fn names(archive: &[u8]) -> Vec<String> {
        parse(archive).into_iter().map(|r| r.name).collect()
    }

    #[test]
    fn an_empty_list_is_only_a_trailer() {
        let archive = build(&[]).unwrap();
        assert_eq!(names(&archive), [TRAILER]);
    }

    #[test]
    fn the_header_is_the_exact_newc_format() {
        let archive = build(&[file("a", b"xy")]).unwrap();
        let expected = concat!(
            "070701", "00000001", "000081ed", "00000000", "00000000", "00000001", "00000000",
            "00000002", "00000000", "00000000", "00000000", "00000000", "00000002", "00000000",
            "a\0", "xy\0\0",
        );
        assert_eq!(&archive[..expected.len()], expected.as_bytes());
    }

    #[test]
    fn a_file_keeps_its_contents_and_permissions() {
        let archive = build(&[Entry::File {
            path: "init",
            mode: 0o640,
            contents: b"hello",
        }])
        .unwrap();
        let records = parse(&archive);
        assert_eq!(records[0].name, "init");
        assert_eq!(records[0].mode, S_IFREG | 0o640);
        assert_eq!(records[0].data, b"hello");
        assert_eq!(records[0].links, 1);
    }

    #[test]
    fn only_permission_bits_of_a_mode_count() {
        let archive = build(&[Entry::File {
            path: "a",
            mode: S_IFDIR | 0o4755,
            contents: b"",
        }])
        .unwrap();
        assert_eq!(parse(&archive)[0].mode, S_IFREG | 0o4755);
    }

    #[test]
    fn a_symlink_holds_its_target_with_no_nul() {
        let archive = build(&[Entry::Symlink {
            path: "ap",
            target: "/init",
        }])
        .unwrap();
        let records = parse(&archive);
        assert_eq!(records[0].name, "ap");
        assert_eq!(records[0].mode, S_IFLNK | 0o777);
        assert_eq!(records[0].data, b"/init");
    }

    #[test]
    fn every_name_and_data_length_stays_aligned() {
        for length in 1..9 {
            let name = "n".repeat(length);
            let data = vec![7; length];
            let records = parse(&build(&[file(&name, &data)]).unwrap());
            assert_eq!(records[0].name, name);
            assert_eq!(records[0].data, data);
        }
    }

    #[test]
    fn parents_come_first_and_only_once() {
        let archive = build(&[
            file("usr/bin/a", b""),
            file("usr/bin/b", b""),
            file("usr/c", b""),
        ])
        .unwrap();
        assert_eq!(
            names(&archive),
            ["usr", "usr/bin", "usr/bin/a", "usr/bin/b", "usr/c", TRAILER]
        );
        let records = parse(&archive);
        assert_eq!(records[0].mode, S_IFDIR | 0o755);
        assert!(records[0].data.is_empty());
    }

    #[test]
    fn inodes_are_unique() {
        let records = parse(&build(&[file("a/b", b""), file("c", b"")]).unwrap());
        let inodes: Vec<u32> = records.iter().map(|r| r.inode).collect();
        assert_eq!(inodes, [1, 2, 3, 4]);
    }

    #[test]
    fn a_path_that_is_not_plain_is_refused() {
        for path in [
            "", "/init", "init/", "a//b", ".", "a/./b", "..", "a/../b", "a\0b",
        ] {
            assert_eq!(
                build(&[file(path, b"")]),
                Err(Error::BadPath(path.to_string())),
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_trailer_name_is_refused_even_as_a_parent() {
        for path in [TRAILER, "TRAILER!!!/a"] {
            assert_eq!(
                build(&[file(path, b"")]),
                Err(Error::BadPath(path.to_string())),
            );
        }
        assert!(build(&[file("a/TRAILER!!!", b"")]).is_ok());
    }

    #[test]
    fn a_path_must_fit_in_path_max() {
        let longest = "a".repeat(PATH_MAX - 1);
        assert!(build(&[file(&longest, b"")]).is_ok());
        let long = "a".repeat(PATH_MAX);
        assert_eq!(build(&[file(&long, b"")]), Err(Error::BadPath(long)));
    }

    #[test]
    fn a_bad_symlink_target_is_refused() {
        let long = "a".repeat(PATH_MAX);
        for target in ["", "a\0b", &long] {
            assert_eq!(
                build(&[Entry::Symlink { path: "l", target }]),
                Err(Error::BadTarget("l".to_string())),
            );
        }
        let longest = "a".repeat(PATH_MAX - 1);
        let entry = Entry::Symlink {
            path: "l",
            target: &longest,
        };
        assert!(build(&[entry]).is_ok());
    }

    #[test]
    fn a_path_can_be_listed_only_once() {
        let conflict = Err(Error::Conflict("a".to_string()));
        assert_eq!(build(&[file("a", b""), file("a", b"")]), conflict);
        // As a directory, whichever comes first.
        assert_eq!(build(&[file("a", b""), file("a/b", b"")]), conflict);
        assert_eq!(build(&[file("a/b", b""), file("a", b"")]), conflict);
    }
}
