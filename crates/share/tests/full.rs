//! A copy-on-write share on a disk that fills up, alone in its process,
//! because the limit it sets on file sizes is the whole process's.

mod guest;

use guest::*;

#[test]
fn a_record_is_a_whole_line_or_none() {
    let tree = Tree::new("cow-full");
    let pending = tree.base.join("pending");
    let mut guest = Guest::cow(&tree.root, &pending);
    let limit = |size| {
        let limit = libc::rlimit {
            rlim_cur: size,
            rlim_max: libc::RLIM_INFINITY,
        };
        // SAFETY: both calls take only what is passed here.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
    };
    limit(100);
    let mut refused = 0;
    for at in 0..20 {
        let name = format!("file{at}");
        match guest.create(1, name.as_bytes(), 0) {
            Ok((_, handle)) => guest.release(handle),
            Err(_) => refused += 1,
        }
    }
    limit(libc::RLIM_INFINITY);
    let (_, handle) = guest.create(1, b"after", 0).unwrap();
    guest.release(handle);
    drop(guest);
    assert!(refused > 0);
    let records = records(&pending);
    for line in &records {
        let words: Vec<_> = line.split(' ').collect();
        let whole = match words[0] {
            "BASE" => words.len() == 3 && words[2] == "none",
            "FILE" => words.len() == 3 && words[2] == "644",
            _ => false,
        };
        assert!(whole && words[1].starts_with('"'), "{line:?}: {records:#?}");
    }
    assert_eq!(records.last().unwrap(), r#"FILE "after" 644"#);
}
