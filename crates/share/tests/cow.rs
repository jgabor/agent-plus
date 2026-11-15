//! A copy-on-write share, against the device's own relay in this process:
//! what the agent changes lands in the share's pending directory, and your
//! files stay as they were.

mod guest;

use guest::*;
use share::{Mode, Server};
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A tree to share, and a place beside it for the pending directory.
fn pending(tree: &Tree) -> PathBuf {
    tree.base.join("pending")
}

/// Every file beneath `dir`, with its path from `dir`.
fn walk(dir: &Path) -> Vec<(String, fs::Metadata)> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                todo.push(path.clone());
            }
            let name = path.strip_prefix(dir).unwrap().to_str().unwrap().to_owned();
            found.push((name, meta));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

// spec(SHARE): | copy-on-write (default) | Allowed; kept in the agent's overlay | Unchanged until `ap apply` |
#[test]
fn the_agent_s_writes_land_in_pending_and_your_files_stay() {
    let tree = Tree::new("cow-writes");
    let before = walk(&tree.root);
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);

    let (file, handle) = guest.create(1, b"new.txt", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"from the agent"), Ok(14));
    guest.release(handle);
    guest.overwrite("hello.txt", b"changed").unwrap();
    let hello = guest.walk("hello.txt").unwrap();
    let handle = guest.open(hello, O_WRONLY | O_APPEND).unwrap();
    guest.write(handle, 0, b", appended").unwrap();
    guest.release(handle);
    assert_eq!(guest.setattr(file, FATTR_SIZE, 4, 0), 0);
    let nested = guest.walk("dir/nested.txt").unwrap();
    assert_eq!(guest.setattr(nested, FATTR_MODE, 0, 0o600), 0);
    assert_eq!(guest.setattr(nested, FATTR_MTIME, 0, 0), 0);
    let made = guest.mkdir(1, b"made").unwrap();
    guest.symlink(made, b"link", b"../new.txt").unwrap();
    assert_eq!(guest.remove(UNLINK, 1, b"link"), 0);
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), 0);
    assert_eq!(guest.remove(RMDIR, 1, b"dir"), 0);

    // The agent sees what it did.
    assert_eq!(guest.read_file("new.txt").unwrap(), b"from");
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"changed, appended");
    assert_eq!(guest.lookup(1, b"dir"), Err(ENOENT));
    assert_eq!(guest.lookup(1, b"link"), Err(ENOENT));
    let names = guest.list(1);
    let expected = [
        ".",
        "..",
        "absolute",
        "dirlink",
        "escape",
        "hello.txt",
        "made",
        "new.txt",
    ];
    assert_eq!(names, expected.into_iter().map(String::from).collect());
    assert_eq!(
        guest.list(made),
        [".", "..", "link"].into_iter().map(String::from).collect()
    );

    // Your files are as they were, and the agent's are pending.
    drop(guest);
    let after = walk(&tree.root);
    let names = |list: &[(String, fs::Metadata)]| {
        list.iter()
            .map(|(name, meta)| (name.clone(), meta.mode(), meta.len(), meta.mtime()))
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&after), names(&before));
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
    assert_eq!(
        fs::read(tree.root.join("dir/nested.txt")).unwrap(),
        b"nested"
    );
    assert_eq!(fs::read(pending.join("tree/new.txt")).unwrap(), b"from");
    assert_eq!(
        fs::read(pending.join("tree/hello.txt")).unwrap(),
        b"changed, appended"
    );
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
}

// spec(SHARE): Files the agent has not written show your changes live.
#[test]
fn files_the_agent_has_not_written_show_your_changes_live() {
    let tree = Tree::new("cow-live");
    let mut guest = Guest::cow(&tree.root, &pending(&tree));
    guest.overwrite("dir/nested.txt", b"the agent's").unwrap();

    fs::write(tree.root.join("hello.txt"), b"yours, changed").unwrap();
    fs::write(tree.root.join("dir/added.txt"), b"yours, added").unwrap();
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"yours, changed");
    assert_eq!(guest.read_file("dir/added.txt").unwrap(), b"yours, added");
    let dir = guest.walk("dir").unwrap();
    assert!(guest.list(dir).contains("added.txt"));
    // A file the agent wrote keeps the agent's copy.
    fs::write(tree.root.join("dir/nested.txt"), b"yours again").unwrap();
    assert_eq!(guest.read_file("dir/nested.txt").unwrap(), b"the agent's");
    // A file you remove goes, unless the agent wrote it.
    fs::remove_file(tree.root.join("hello.txt")).unwrap();
    fs::remove_file(tree.root.join("dir/nested.txt")).unwrap();
    assert_eq!(guest.lookup(1, b"hello.txt"), Err(ENOENT));
    assert_eq!(guest.read_file("dir/nested.txt").unwrap(), b"the agent's");
}

// spec(SHARE): **Bringing changes back.** Unapplied copy-on-write changes are kept per run, and they survive the end of the run until you apply or discard them.
// spec(SHARE): The next run starts from your current files, not from earlier unapplied changes.
#[test]
fn changes_survive_the_run_and_the_next_run_starts_from_your_files() {
    let tree = Tree::new("cow-runs");
    let first = tree.base.join("first");
    let mut guest = Guest::cow(&tree.root, &first);
    guest.overwrite("hello.txt", b"from the first run").unwrap();
    drop(guest);
    // The server is gone, and what it kept is still there.
    assert_eq!(
        fs::read(first.join("tree/hello.txt")).unwrap(),
        b"from the first run"
    );
    assert!(has(&records(&first), r#"FILE "hello.txt" "#));

    let mut guest = Guest::cow(&tree.root, &tree.base.join("second"));
    assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
    drop(guest);
    // A run never takes over another run's pending directory.
    let root = OwnedFd::from(File::open(&tree.root).unwrap());
    let reused = OwnedFd::from(File::open(&first).unwrap());
    assert!(Server::new(vec![(root, Mode::CopyOnWrite(reused))]).is_err());
}

#[test]
fn each_kind_of_change_is_stored_as_itself_or_as_a_record() {
    let tree = Tree::new("cow-kinds");
    fs::write(tree.root.join("run.sh"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(tree.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);

    let (_, handle) = guest.create(1, b"file", 0).unwrap();
    guest.write(handle, 0, b"regular").unwrap();
    guest.release(handle);
    let made = guest.mkdir(1, b"made").unwrap();
    guest.symlink(made, b"link", b"../outside/secret").unwrap();
    assert!(guest.mknod(made, b"fifo", S_IFIFO | 0o644, 0).is_ok());
    assert!(guest.mknod(made, b"socket", S_IFSOCK | 0o644, 0).is_ok());
    // `/dev/null`: major 1, minor 3.
    assert!(
        guest
            .mknod(made, b"null", S_IFCHR | 0o666, (1 << 8) | 3)
            .is_ok()
    );
    let script = guest.walk("run.sh").unwrap();
    assert_eq!(guest.setattr(script, FATTR_MODE, 0, 0o700), 0);
    assert_eq!(guest.remove(UNLINK, 1, b"hello.txt"), 0);

    // The guest sees each kind as what the agent made.
    assert_eq!(kind(guest.mode("made/link").unwrap()), S_IFLNK);
    let link = guest.walk("made/link").unwrap();
    assert_eq!(
        guest.call(READLINK, link, &[]),
        (0, b"../outside/secret".to_vec())
    );
    assert_eq!(kind(guest.mode("made/fifo").unwrap()), S_IFIFO);
    assert_eq!(kind(guest.mode("made/socket").unwrap()), S_IFSOCK);
    assert_eq!(guest.mode("made/null").unwrap(), S_IFCHR | 0o666);
    assert_eq!(guest.mode("run.sh").unwrap(), S_IFREG | 0o700);
    assert_eq!(guest.mode("file").unwrap(), S_IFREG | 0o644);
    assert_eq!(guest.mode("made").unwrap(), S_IFDIR | 0o755);
    drop(guest);

    // In the pending directory, only regular files and directories, each
    // readable only by you: no symlink, and no device.
    let kept = walk(&pending);
    for (name, meta) in &kept {
        let mode = meta.mode() & 0o7777;
        match meta.is_dir() {
            true => assert_eq!(mode, 0o700, "{name}"),
            false => {
                assert!(meta.is_file(), "{name}");
                assert_eq!(mode, 0o600, "{name}");
            }
        }
    }
    let names: Vec<_> = kept.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"tree/file"), "{names:?}");
    assert!(names.contains(&"tree/made"), "{names:?}");
    assert!(names.contains(&"tree/run.sh"), "{names:?}");
    assert!(!names.contains(&"tree/hello.txt"), "{names:?}");
    assert_eq!(fs::read(pending.join("tree/file")).unwrap(), b"regular");
    // Symlinks, modes, special files, and deletions are records.
    let records = records(&pending);
    for line in [
        r#"FILE "file" 644"#,
        r#"DIR "made" 755"#,
        r#"SYMLINK "made/link" "../outside/secret""#,
        r#"FIFO "made/fifo" 644"#,
        r#"SOCKET "made/socket" 644"#,
        r#"CHAR "made/null" 666 1 3"#,
        r#"FILE "run.sh" 755"#,
        r#"FILE "run.sh" 700"#,
        r#"DELETE "hello.txt""#,
    ] {
        assert!(records.contains(&line.to_owned()), "{line}: {records:#?}");
    }
    let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o7777;
    assert_eq!(mode(&tree.root.join("run.sh")), 0o755);
    assert_eq!(mode(&pending.join("records")), 0o600);
}

#[test]
fn a_name_that_needs_it_is_escaped_in_a_record() {
    let tree = Tree::new("cow-names");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    for name in [
        &b"a \"quoted\" \\ name"[..],
        b"line\nbreak",
        b"\xff\xfe",
        "é".as_bytes(),
    ] {
        let (_, handle) = guest.create(1, name, 0).unwrap();
        guest.release(handle);
    }
    drop(guest);
    let records = records(&pending);
    for line in [
        r#"FILE "a \"quoted\" \\ name" 644"#,
        r#"FILE "line\x0abreak" 644"#,
        r#"FILE "\xff\xfe" 644"#,
        "FILE \"é\" 644",
    ] {
        assert!(records.contains(&line.to_owned()), "{line}: {records:#?}");
    }
}

/// Whether a record starts with `start`.
fn has(records: &[String], start: &str) -> bool {
    records.iter().any(|line| line.starts_with(start))
}

/// The state a `BASE` record keeps of your file: its inode, size,
/// modification time, and change time, or `none`.
fn base(pending: &Path, path: &str) -> String {
    let prefix = format!("BASE \"{path}\" ");
    let lines: Vec<_> = records(pending)
        .into_iter()
        .filter_map(|line| line.strip_prefix(&prefix).map(String::from))
        .collect();
    assert_eq!(lines.len(), 1, "{path}: {lines:?}");
    lines[0].clone()
}

fn state_of(path: &Path) -> String {
    let meta = fs::symlink_metadata(path).unwrap();
    format!(
        "{} {} {}.{:09} {}.{:09}",
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec()
    )
}

#[test]
fn the_record_of_your_file_tells_a_later_change_apart() {
    let tree = Tree::new("cow-base");
    let pending = pending(&tree);
    let path = tree.root.join("hello.txt");
    let mut guest = Guest::cow(&tree.root, &pending);
    guest.overwrite("hello.txt", b"agent").unwrap();
    // Only the first write records it.
    guest.overwrite("hello.txt", b"agent, again").unwrap();
    let (_, handle) = guest.create(1, b"new.txt", 0).unwrap();
    guest.release(handle);
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), 0);
    drop(guest);

    assert_eq!(base(&pending, "hello.txt"), state_of(&path));
    assert_eq!(base(&pending, "new.txt"), "none");
    assert_eq!(
        base(&pending, "dir/nested.txt"),
        state_of(&tree.root.join("dir/nested.txt"))
    );
    // A change of the same length, even one that puts the time back, still
    // differs from the record.
    let times = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"INSIDE THE SHARE").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(times)
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), INSIDE.len() as u64);
    assert_ne!(base(&pending, "hello.txt"), state_of(&path));
}

// spec(SHARE): File locks pass through to the host.
#[test]
fn a_lock_on_a_file_the_agent_wrote_is_taken_on_its_pending_copy() {
    let tree = Tree::new("cow-locks");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    // A file the agent hasn't written locks your file.
    let nested = guest.walk("dir/nested.txt").unwrap();
    let handle = guest.open(nested, 0).unwrap();
    assert_eq!(guest.lock(SETLK, handle, 7, F_RDLCK, 0).0, 0);
    assert!(host_locked(
        &tree.root.join("dir/nested.txt"),
        libc::F_WRLCK as i16
    ));
    guest.release(handle);

    let hello = guest.walk("hello.txt").unwrap();
    let handle = guest.open(hello, O_RDWR).unwrap();
    assert_eq!(guest.lock(SETLK, handle, 7, F_WRLCK, 0).0, 0);
    let copy = pending.join("tree/hello.txt");
    assert!(host_locked(&copy, libc::F_RDLCK as i16));
    assert!(!host_locked(
        &tree.root.join("hello.txt"),
        libc::F_RDLCK as i16
    ));
    assert_eq!(guest.lock(SETLK, handle, 8, F_WRLCK, LK_FLOCK).0, 0);
    let held = File::open(&copy).unwrap();
    // SAFETY: flock takes no pointer.
    let free = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&held),
            libc::LOCK_SH | libc::LOCK_NB,
        )
    };
    assert_eq!(free, -1);
}

// spec(SHARE): In a copy-on-write share, renaming a directory that holds your files, or making a hard link, fails with an `Invalid cross-device link` error, as it does in overlayfs, so tools copy instead.
#[test]
fn a_rename_moves_the_agent_s_copy_and_leaves_yours() {
    let tree = Tree::new("cow-rename");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.rename(1, b"hello.txt", dir, b"moved.txt", 0), 0);
    assert_eq!(guest.lookup(1, b"hello.txt"), Err(ENOENT));
    assert_eq!(guest.read_file("dir/moved.txt").unwrap(), INSIDE);
    assert_eq!(
        guest.rename(dir, b"moved.txt", dir, b"nested.txt", RENAME_NOREPLACE),
        EEXIST
    );
    // A directory that holds your files can't move, as in overlayfs, so a
    // tool copies it instead.
    assert_eq!(guest.rename(1, b"dir", 1, b"elsewhere", 0), EXDEV);
    // One the agent made moves with everything in it.
    let made = guest.mkdir(1, b"made").unwrap();
    let inner = guest.mkdir(made, b"inner").unwrap();
    let (_, handle) = guest.create(inner, b"deep.txt", 0).unwrap();
    guest.write(handle, 0, b"deep").unwrap();
    guest.release(handle);
    assert_eq!(guest.rename(1, b"made", dir, b"landed", 0), 0);
    assert_eq!(
        guest.read_file("dir/landed/inner/deep.txt").unwrap(),
        b"deep"
    );
    assert_eq!(guest.lookup(1, b"made"), Err(ENOENT));
    // The nodes the guest holds follow the move.
    assert_eq!(guest.lookup(inner, b"deep.txt").map(|_| ()), Ok(()));
    // A hard link would need your file on both sides, so it isn't made.
    let hello = guest.walk("dir/moved.txt").unwrap();
    assert_eq!(guest.link(hello, 1, b"twin"), Err(EXDEV));
    // A directory with something in it can't be removed, or replaced.
    assert_eq!(guest.remove(RMDIR, 1, b"dir"), ENOTEMPTY);
    let empty = guest.mkdir(1, b"empty").unwrap();
    assert_eq!(guest.rename(1, b"empty", 1, b"dir", 0), ENOTEMPTY);
    let _ = empty;
    drop(guest);

    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
    assert!(!tree.root.join("dir/moved.txt").exists());
    assert!(!tree.root.join("dir/landed").exists());
    let records = records(&pending);
    for line in [
        r#"DELETE "hello.txt""#,
        r#"FILE "dir/moved.txt" "#,
        r#"DIR "dir/landed" 755"#,
        r#"DIR "dir/landed/inner" 755"#,
        r#"FILE "dir/landed/inner/deep.txt" 644"#,
        r#"DELETE "made""#,
    ] {
        assert!(has(&records, line), "{line}: {records:#?}");
    }
    assert_eq!(
        fs::read(pending.join("tree/dir/landed/inner/deep.txt")).unwrap(),
        b"deep"
    );
}

#[test]
fn a_directory_made_again_hides_what_you_had_in_it() {
    let tree = Tree::new("cow-opaque");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), 0);
    assert_eq!(guest.remove(RMDIR, 1, b"dir"), 0);
    let dir = guest.mkdir(1, b"dir").unwrap();
    assert_eq!(
        guest.list(dir),
        [".", ".."].into_iter().map(String::from).collect()
    );
    // Even a file you add there now stays out of it.
    fs::write(tree.root.join("dir/added.txt"), b"yours").unwrap();
    assert_eq!(guest.lookup(dir, b"added.txt"), Err(ENOENT));
    assert_eq!(guest.lookup(dir, b"nested.txt"), Err(ENOENT));
    drop(guest);
    let records = records(&pending);
    assert!(records.contains(&r#"DIR "dir" 755 opaque"#.to_owned()));
    assert_eq!(
        fs::read(tree.root.join("dir/nested.txt")).unwrap(),
        b"nested"
    );
}

#[test]
fn a_write_never_reaches_outside_a_copy_on_write_share() {
    let tree = Tree::new("cow-escape");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let escape = guest.walk("escape").unwrap();
    assert_eq!(guest.open(escape, O_WRONLY), Err(EINVAL));
    let dirlink = guest.walk("dirlink").unwrap();
    assert_eq!(guest.create(dirlink, b"planted", 0), Err(ENOTDIR));
    assert_eq!(guest.mkdir(dirlink, b"planted"), Err(ENOTDIR));
    for name in [&b"../planted"[..], b"/tmp/planted", b"a/b", b"..", b"."] {
        assert!(guest.create(1, name, 0).is_err(), "{name:?}");
        assert!(guest.mkdir(1, name).is_err(), "{name:?}");
        assert_ne!(guest.rename(1, b"hello.txt", 1, name, 0), 0, "{name:?}");
    }
    // Changing a symlink changes the agent's copy of it, not its target.
    assert_eq!(guest.setattr(escape, FATTR_MTIME, 0, 0), 0);
    assert_eq!(guest.remove(UNLINK, 1, b"escape"), 0);
    drop(guest);
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
    assert!(
        fs::symlink_metadata(tree.root.join("escape"))
            .unwrap()
            .is_symlink()
    );
    let outside: Vec<_> = fs::read_dir(tree.base.join("outside")).unwrap().collect();
    assert_eq!(outside.len(), 1);
    assert!(!tree.base.join("planted").exists());
}

#[test]
fn a_copy_that_fails_leaves_nothing_behind() {
    let tree = Tree::new("cow-failed-copy");
    let path = tree.root.join("hello.txt");
    let mut guest = Guest::cow(&tree.root, &pending(&tree));
    let hello = guest.walk("hello.txt").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(guest.open(hello, O_RDWR), Err(EACCES));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(kind(guest.mode("hello.txt").unwrap()), S_IFREG);
    guest.overwrite("hello.txt", b"agent").unwrap();
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"agent");
}

#[test]
fn a_file_open_for_reading_reads_the_agent_s_copy_once_it_has_one() {
    let tree = Tree::new("cow-reader");
    let mut guest = Guest::cow(&tree.root, &pending(&tree));
    let hello = guest.walk("hello.txt").unwrap();
    let reader = guest.open(hello, 0).unwrap();
    guest.overwrite("hello.txt", b"agent").unwrap();
    assert_eq!(guest.read(READ, reader, 0, 64), (0, b"agent".to_vec()));
}

#[test]
fn a_file_the_agent_removed_still_changes_through_its_handle() {
    let tree = Tree::new("cow-removed-open");
    let mut guest = Guest::cow(&tree.root, &pending(&tree));
    let (file, handle) = guest.create(1, b"temp", 0).unwrap();
    guest.write(handle, 0, b"temporary").unwrap();
    assert_eq!(guest.remove(UNLINK, 1, b"temp"), 0);
    // `FATTR_SIZE | FATTR_FH`, to 4 bytes, through the handle.
    let mut body = (FATTR_SIZE | 64).to_le_bytes().to_vec();
    body.extend([0; 4]);
    body.extend(handle.to_le_bytes());
    body.extend(4u64.to_le_bytes());
    body.resize(88, 0);
    assert_eq!(guest.call(SETATTR, file, &body).0, 0);
    assert_eq!(guest.read(READ, handle, 0, 64), (0, b"temp".to_vec()));
    // Your file, open for reading, never changes through a handle.
    let dir = guest.walk("dir").unwrap();
    let nested = guest.walk("dir/nested.txt").unwrap();
    let reader = guest.open(nested, 0).unwrap();
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), 0);
    let mut body = (FATTR_SIZE | FATTR_MTIME | 64).to_le_bytes().to_vec();
    body.extend([0; 4]);
    body.extend(reader.to_le_bytes());
    body.resize(88, 0);
    assert_ne!(guest.call(SETATTR, nested, &body).0, 0);
    assert_eq!(
        fs::read(tree.root.join("dir/nested.txt")).unwrap(),
        b"nested"
    );
}

#[test]
fn a_lock_moves_to_the_agent_s_copy_once_it_has_one() {
    let tree = Tree::new("cow-lock-moves");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let hello = guest.walk("hello.txt").unwrap();
    let reader = guest.open(hello, 0).unwrap();
    assert_eq!(guest.lock(GETLK, reader, 7, F_WRLCK, 0).0, 0);
    let writer = guest.open(hello, O_RDWR).unwrap();
    assert_eq!(guest.lock(SETLK, writer, 7, F_WRLCK, 0).0, 0);
    assert!(host_locked(
        &pending.join("tree/hello.txt"),
        libc::F_RDLCK as i16
    ));
}

#[test]
fn a_directory_you_removed_is_recorded_before_the_agent_changes_it() {
    let tree = Tree::new("cow-orphan");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    guest.overwrite("dir/nested.txt", b"agent").unwrap();
    fs::remove_dir_all(tree.root.join("dir")).unwrap();
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.setattr(dir, FATTR_MODE, 0, 0o700), 0);
    drop(guest);
    assert_eq!(base(&pending, "dir"), "none");
}

#[test]
fn a_rename_over_a_directory_records_what_it_replaced() {
    let tree = Tree::new("cow-replaced");
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let target = guest.mkdir(1, b"target").unwrap();
    let (_, handle) = guest.create(target, b"ghost", 0).unwrap();
    guest.release(handle);
    assert_eq!(guest.remove(UNLINK, target, b"ghost"), 0);
    guest.mkdir(1, b"source").unwrap();
    assert_eq!(guest.rename(1, b"source", 1, b"target", 0), 0);
    // An exchange isn't a rename this share makes.
    guest.mkdir(1, b"other").unwrap();
    assert_eq!(
        guest.rename(1, b"other", 1, b"target", RENAME_EXCHANGE),
        EINVAL
    );
    drop(guest);
    // Read in order, the records leave nothing of the old directory.
    let records = records(&pending);
    let last = |line: &str| records.iter().rposition(|found| found == line).unwrap();
    let cleared = last(r#"DELETE "target""#);
    assert!(last(r#"DELETE "target/ghost""#) < cleared, "{records:#?}");
    assert!(cleared < last(r#"DIR "target" 755"#), "{records:#?}");
}
