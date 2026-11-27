//! A server holds a descriptor for each file the guest looked up, so it
//! raises its limit on open files, whatever limit the process started with.
//! The limit belongs to the whole process, so this is a program of its own.

#![cfg(target_os = "linux")]

mod guest;

use guest::*;
use std::fs;

#[test]
fn a_server_looks_up_more_files_than_its_starting_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both calls read or write only `limit`.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = 256;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
    let tree = Tree::new("descriptors");
    for file in 0..300 {
        fs::write(tree.root.join(format!("file{file}")), INSIDE).unwrap();
    }
    let mut guest = Guest::new(&tree.root);
    for file in 0..300 {
        let name = format!("file{file}");
        assert!(guest.lookup(1, name.as_bytes()).is_ok(), "{name}");
    }
}
