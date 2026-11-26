//! A static program for T1 that stands in for one of `ap`'s confined
//! processes whose heap can't grow. It maps a page where the program break
//! would grow, so `brk` fails, and musl's allocator maps its pages instead,
//! with no access, and then calls `mprotect` on them. It confines itself as
//! its argument says, as the `network` process or the `share-server`, and
//! allocates. It exits 0 if it lived.

use std::ffi::{c_int, c_long, c_void};

const BRK: c_long = 12;
const PAGE: usize = 4096;
const PROT_NONE: c_int = 0;
const MAP_PRIVATE: c_int = 2;
const MAP_ANONYMOUS: c_int = 0x20;
const MAP_FIXED_NOREPLACE: c_int = 0x10_0000;
/// Each allocation is larger than musl maps on its own, so each takes a
/// record of its own, and enough of them fill more than one page of records.
const LARGE: usize = 256 << 10;
const ALLOCATIONS: usize = 1000;

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
}

/// Maps a page past the program break, and checks that the break can't
/// grow into it.
fn block_the_break() {
    // SAFETY: `brk` with 0 changes nothing, and returns the break.
    let start = unsafe { syscall(BRK, 0) } as usize;
    let page = start.next_multiple_of(PAGE);
    let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE;
    // SAFETY: the mapping is new, and replaces nothing.
    let mapped = unsafe { mmap(page as *mut c_void, PAGE, PROT_NONE, flags, -1, 0) };
    assert_eq!(mapped as usize, page, "a page past the break");
    // SAFETY: `brk` returns the break it kept when it can't move it.
    let grown = unsafe { syscall(BRK, page + 2 * PAGE) } as usize;
    assert_eq!(grown, start, "the break grew past the page");
}

fn main() {
    let process = std::env::args().nth(1).expect("which process to be");
    block_the_break();
    match process.as_str() {
        "network" => vmm::confine::confine_network(0),
        "share-server" => {
            vmm::confine::confine_share_server(0, vmm::confine::Changes::Nothing, &[], None)
        }
        other => panic!("no process {other}"),
    }
    .unwrap();
    let mut kept = Vec::with_capacity(ALLOCATIONS);
    for _ in 0..ALLOCATIONS {
        let mut allocation = Vec::<u8>::with_capacity(LARGE);
        allocation.push(1);
        kept.push(allocation);
    }
    assert!(kept.iter().all(|allocation| allocation[0] == 1));
}
