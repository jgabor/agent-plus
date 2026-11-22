//! The share server's rules, against the device's own relay in this
//! process: each request is a FUSE request as a guest's kernel, or a guest
//! that took its kernel over, would send it.

mod guest;

use guest::*;
use share::{Mode, Root, Server};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[test]
fn files_beneath_the_root_read_as_they_are() {
    let tree = Tree::new("read");
    let mut guest = Guest::new(&tree.root);
    assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
    assert_eq!(guest.read_file("dir/nested.txt").unwrap(), b"nested");
    // A change on the host shows at the next read.
    fs::write(tree.root.join("hello.txt"), b"changed").unwrap();
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"changed");
    let (node, mode) = guest.lookup(1, b"dir").unwrap();
    assert_eq!(kind(mode), S_IFDIR);
    let (error, attr) = guest.call(GETATTR, node, &[0; 16]);
    assert_eq!(error, 0);
    assert_eq!(attr.len(), 104);
    let (error, statfs) = guest.call(STATFS, 1, &[]);
    assert_eq!((error, statfs.len()), (0, 80));
}

#[test]
fn a_directory_lists_its_entries_across_requests() {
    let tree = Tree::new("list");
    let mut guest = Guest::new(&tree.root);
    let names = guest.list(1);
    let expected = [
        ".",
        "..",
        "absolute",
        "dir",
        "dirlink",
        "escape",
        "hello.txt",
        "link",
    ];
    assert_eq!(names, expected.into_iter().map(String::from).collect());
}

#[test]
fn a_name_that_isn_t_one_entry_is_refused() {
    let tree = Tree::new("names");
    let mut guest = Guest::new(&tree.root);
    let secret = tree.secret.to_str().unwrap().as_bytes().to_vec();
    for (name, error) in [
        (&b".."[..], ENOENT),
        (b".", ENOENT),
        (b"", EINVAL),
        (b"dir/nested.txt", EINVAL),
        (b"../outside/secret", EINVAL),
        (&secret[..], EINVAL),
        (b"/etc/passwd", EINVAL),
        (&[b'a'; 256], EINVAL),
    ] {
        assert_eq!(guest.lookup(1, name), Err(error), "{name:?}");
    }
    // A name with no end is refused, rather than read past.
    assert_eq!(guest.call(LOOKUP, 1, b"hello.txt").0, EINVAL);
    let (dir, _) = guest.lookup(1, b"dir").unwrap();
    assert_eq!(guest.lookup(dir, b".."), Err(ENOENT));
}

// spec(SHARE): The agent sees only what is under the share's root.
#[test]
fn a_symlink_reaches_the_guest_as_itself_and_leads_nowhere_on_the_host() {
    let tree = Tree::new("symlinks");
    let mut guest = Guest::new(&tree.root);
    let secret = tree.secret.to_str().unwrap().as_bytes().to_vec();
    for (name, target) in [
        (&b"link"[..], &b"hello.txt"[..]),
        (b"escape", b"../outside/secret"),
        (b"absolute", &secret),
        (b"dirlink", b"../outside"),
    ] {
        let (node, mode) = guest.lookup(1, name).unwrap();
        assert_eq!(kind(mode), S_IFLNK, "{name:?}");
        assert_eq!(guest.call(READLINK, node, &[]), (0, target.to_vec()));
        assert_eq!(guest.open(node, 0), Err(EINVAL));
        assert_eq!(guest.call(OPENDIR, node, &[0; 8]).0, ENOTDIR);
        assert_eq!(guest.lookup(node, b"secret"), Err(ENOTDIR));
    }
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(guest.call(READLINK, file, &[]).0, EINVAL);
    assert_eq!(guest.lookup(file, b"x"), Err(ENOTDIR));
}

#[test]
fn a_symlink_swapped_in_after_a_lookup_is_never_followed() {
    let tree = Tree::new("swap");
    let mut guest = Guest::new(&tree.root);
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    let (dir, _) = guest.lookup(1, b"dir").unwrap();
    let swapped = tree.root.join("swapped");
    symlink(&tree.secret, &swapped).unwrap();
    fs::rename(&swapped, tree.root.join("hello.txt")).unwrap();
    fs::rename(tree.root.join("dir"), tree.root.join("dir-old")).unwrap();
    symlink("../outside", tree.root.join("dir")).unwrap();
    // The file's name is a symlink now, which opening refuses to follow.
    assert_eq!(guest.open(file, 0), Err(ELOOP));
    // The directory is still the one looked up, wherever it moved.
    let (nested, _) = guest.lookup(dir, b"nested.txt").unwrap();
    assert!(guest.open(nested, 0).is_ok());
    assert_eq!(guest.lookup(dir, b"secret"), Err(ENOENT));
    // A file put in the old one's place is a different file.
    fs::remove_file(tree.root.join("hello.txt")).unwrap();
    fs::write(tree.root.join("hello.txt"), b"another").unwrap();
    assert_eq!(guest.open(file, 0), Err(ESTALE));
}

#[test]
fn a_symlink_swapped_in_during_requests_never_reaches_outside() {
    let tree = Tree::new("race");
    let mut guest = Guest::new(&tree.root);
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (root, secret, stop) = (tree.root.clone(), tree.secret.clone(), Arc::clone(&stop));
        thread::spawn(move || {
            let (file, link) = (root.join("file.new"), root.join("link.new"));
            let mut swaps = 0;
            while !stop.load(Ordering::Relaxed) {
                fs::write(&file, INSIDE).unwrap();
                fs::rename(&file, root.join("target")).unwrap();
                let _ = fs::remove_file(&link);
                symlink(&secret, &link).unwrap();
                fs::rename(&link, root.join("target")).unwrap();
                swaps += 1;
            }
            swaps
        })
    };
    let mut read = 0;
    for _ in 0..2000 {
        if let Ok(data) = guest.read_file("target") {
            assert_eq!(data, INSIDE);
            read += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let swaps = swapper.join().unwrap();
    println!("{read} reads went through, across {swaps} swaps");
    assert!(swaps > 0);
}

#[test]
fn every_write_to_a_read_only_share_is_refused() {
    let tree = Tree::new("writes");
    let mut guest = Guest::new(&tree.root);
    let (file, mode) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(kind(mode), S_IFREG);
    for opcode in WRITES {
        for node in [1, file] {
            assert_eq!(guest.call(opcode, node, &[0; 64]).0, EROFS, "{opcode}");
        }
    }
    for flags in [O_WRONLY, O_RDWR, O_TRUNC, O_APPEND, O_WRONLY | O_TRUNC] {
        assert_eq!(guest.open(file, flags), Err(EROFS), "{flags:#o}");
    }
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
}

#[test]
fn a_node_or_a_handle_the_server_never_gave_is_refused() {
    let tree = Tree::new("forged");
    let mut guest = Guest::new(&tree.root);
    for node in [0, 2, 1000, u64::MAX] {
        assert_eq!(guest.call(GETATTR, node, &[0; 16]).0, ESTALE, "{node}");
        assert_eq!(guest.lookup(node, b"hello.txt"), Err(ESTALE));
        assert_eq!(guest.open(node, 0), Err(ESTALE));
    }
    for handle in [0, 1, 1000, u64::MAX] {
        assert_eq!(guest.read(READ, handle, 0, 16).0, EBADF);
        assert_eq!(guest.read(READDIR, handle, 0, 16).0, EBADF);
    }
    assert_eq!(guest.call_share(1, GETATTR, 1, &[0; 16]).0, ENODEV);
    assert_eq!(guest.call(9999, 1, &[]).0, ENOSYS);
}

#[test]
fn a_forgotten_node_is_gone() {
    let tree = Tree::new("forget");
    let mut guest = Guest::new(&tree.root);
    let (file, _) = guest.lookup(1, b"hello.txt").unwrap();
    assert_eq!(guest.lookup(1, b"hello.txt").unwrap().0, file);
    // Forgetting gets no answer.
    assert_eq!(
        guest.call(FORGET, file, &1u64.to_le_bytes()),
        (0, Vec::new())
    );
    assert_eq!(guest.call(GETATTR, file, &[0; 16]).0, 0);
    let mut batch = 1u32.to_le_bytes().to_vec();
    batch.extend([0; 4]);
    batch.extend(file.to_le_bytes());
    batch.extend(1u64.to_le_bytes());
    assert_eq!(guest.call(BATCH_FORGET, 0, &batch), (0, Vec::new()));
    assert_eq!(guest.call(GETATTR, file, &[0; 16]).0, ESTALE);
    // The root can't be forgotten.
    guest.call(FORGET, 1, &u64::MAX.to_le_bytes());
    assert_eq!(guest.call(GETATTR, 1, &[0; 16]).0, 0);
}

#[test]
fn a_malformed_request_gets_no_answer() {
    let tree = Tree::new("malformed");
    let mut guest = Guest::new(&tree.root);
    assert!(guest.raw(0, &[]).is_empty());
    assert!(guest.raw(0, &[0; 39]).is_empty());
    // A length past the request's end.
    let mut request = vec![0; 40];
    request[..4].copy_from_slice(&41u32.to_le_bytes());
    assert!(guest.raw(0, &request).is_empty());
    // The server still answers after.
    assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
}

#[test]
fn a_file_share_shows_its_file_and_nothing_beside_it() {
    let tree = Tree::new("file");
    for mode in [Mode::ReadOnly, Mode::ReadWrite] {
        let mut guest = Guest::file(&tree.root, "hello.txt", mode);
        assert_eq!(guest.read_file("hello.txt").unwrap(), INSIDE);
        for name in [
            "dir",
            "link",
            "escape",
            "absolute",
            "dirlink",
            "dir/nested.txt",
        ] {
            assert_eq!(guest.read_file(name), Err(ENOENT), "{name}");
        }
        // The directory that holds the file can't be listed, locked, or
        // changed, whatever the share allows.
        assert_eq!(guest.call(OPENDIR, 1, &[0; 8]).0, EPERM);
        let error = |code: i32| code == EPERM || code == EROFS;
        let file = guest.walk("hello.txt").unwrap();
        assert!(error(guest.create(1, b"new.txt", 0).unwrap_err()));
        assert!(error(guest.mkdir(1, b"made").unwrap_err()));
        assert!(error(guest.symlink(1, b"made", b"hello.txt").unwrap_err()));
        assert!(error(guest.link(file, 1, b"hard").unwrap_err()));
        assert!(error(guest.remove(UNLINK, 1, b"hello.txt")));
        assert!(error(guest.remove(UNLINK, 1, b"dir")));
        assert!(error(guest.rename(1, b"hello.txt", 1, b"moved", 0)));
        assert!(error(guest.setattr(1, FATTR_MODE, 0, 0o777)));
    }
    let names: BTreeSet<_> = fs::read_dir(&tree.root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let before = ["absolute", "dir", "dirlink", "escape", "hello.txt", "link"];
    assert_eq!(names, before.map(str::to_owned).into());
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), INSIDE);
}

#[test]
fn a_read_write_file_share_changes_the_file_alone() {
    let tree = Tree::new("file-rw");
    let mut guest = Guest::file(&tree.root, "hello.txt", Mode::ReadWrite);
    guest.overwrite("hello.txt", b"changed").unwrap();
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), b"changed");
    let file = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.setattr(file, FATTR_SIZE, 4, 0), 0);
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), b"chan");

    // A shared `Agentfile` stays as SPEC.md's SHARE section says.
    fs::write(tree.root.join("Agentfile"), INSIDE).unwrap();
    let mut guest = Guest::file(&tree.root, "Agentfile", Mode::ReadWrite);
    assert_eq!(guest.overwrite("Agentfile", b"changed"), Err(EPERM));
    assert_eq!(fs::read(tree.root.join("Agentfile")).unwrap(), INSIDE);
}

#[test]
fn a_file_share_shows_the_file_it_started_with() {
    let tree = Tree::new("file-replaced");
    let mut guest = Guest::file(&tree.root, "hello.txt", Mode::ReadOnly);
    fs::write(tree.root.join("hello.txt"), b"changed").unwrap();
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"changed");
    // Replaced on the host, it is another file, which the share never
    // shows.
    fs::write(tree.base.join("new"), b"replaced").unwrap();
    fs::rename(tree.base.join("new"), tree.root.join("hello.txt")).unwrap();
    assert_eq!(guest.read_file("hello.txt"), Err(ESTALE));
}

#[test]
fn a_file_share_must_be_of_a_file() {
    let tree = Tree::new("file-dir");
    for name in ["dir", "link", "missing"] {
        let directory = OwnedFd::from(File::open(&tree.root).unwrap());
        let name = std::ffi::CString::new(name).unwrap();
        let root = Root::file(directory, name, Mode::ReadOnly);
        assert!(Server::new(vec![root]).is_err());
    }
    // Copy-on-write isn't built for a file.
    let pending = tree.base.join("pending");
    fs::create_dir(&pending).unwrap();
    let directory = OwnedFd::from(File::open(&tree.root).unwrap());
    let pending = OwnedFd::from(File::open(&pending).unwrap());
    let name = std::ffi::CString::new("hello.txt").unwrap();
    let root = Root::file(directory, name, Mode::CopyOnWrite(pending));
    assert!(Server::new(vec![root]).is_err());
}

#[test]
fn a_root_must_be_a_directory() {
    let tree = Tree::new("root-file");
    let file = OwnedFd::from(File::open(tree.root.join("hello.txt")).unwrap());
    assert!(Server::new(vec![Root::new(file, Mode::ReadOnly)]).is_err());
}

#[test]
fn writes_in_a_read_write_share_change_the_host_s_files() {
    let tree = Tree::new("rw");
    let root = tree.root.clone();
    let mut guest = Guest::with(&root, Mode::ReadWrite);

    let (file, handle) = guest.create(1, b"new.txt", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"written by the agent"), Ok(20));
    assert_eq!(
        fs::read(root.join("new.txt")).unwrap(),
        b"written by the agent"
    );
    guest.release(handle);
    assert_eq!(guest.create(1, b"new.txt", O_EXCL), Err(EEXIST));

    guest.overwrite("hello.txt", b"changed").unwrap();
    assert_eq!(fs::read(root.join("hello.txt")).unwrap(), b"changed");
    let hello = guest.walk("hello.txt").unwrap();
    let handle = guest.open(hello, O_WRONLY | O_APPEND).unwrap();
    guest.write(handle, 0, b", appended").unwrap();
    guest.release(handle);
    assert_eq!(
        fs::read(root.join("hello.txt")).unwrap(),
        b"changed, appended"
    );

    assert_eq!(guest.setattr(file, FATTR_SIZE, 7, 0), 0);
    assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"written");
    assert_eq!(guest.setattr(file, FATTR_MODE, 0, 0o600), 0);
    let mode = fs::metadata(root.join("new.txt")).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
    assert_eq!(guest.setattr(file, FATTR_MTIME, 0, 0), 0);

    let made = guest.mkdir(1, b"made").unwrap();
    assert!(root.join("made").is_dir());
    guest.symlink(made, b"link", b"../new.txt").unwrap();
    assert_eq!(
        fs::read_link(root.join("made/link")).unwrap(),
        Path::new("../new.txt")
    );
    guest.link(file, made, b"hard").unwrap();
    assert_eq!(fs::read(root.join("made/hard")).unwrap(), b"written");

    assert_eq!(guest.rename(1, b"new.txt", made, b"moved.txt", 0), 0);
    assert!(!root.join("new.txt").exists());
    assert_eq!(fs::read(root.join("made/moved.txt")).unwrap(), b"written");
    assert_eq!(
        guest.rename(made, b"moved.txt", made, b"hard", RENAME_NOREPLACE),
        EEXIST
    );
    assert_eq!(
        guest.rename(made, b"moved.txt", 1, b"hello.txt", RENAME_EXCHANGE),
        0
    );
    assert_eq!(
        fs::read(root.join("made/moved.txt")).unwrap(),
        b"changed, appended"
    );

    assert_eq!(guest.remove(UNLINK, made, b"link"), 0);
    assert_eq!(guest.remove(UNLINK, made, b"hard"), 0);
    assert_eq!(guest.remove(UNLINK, made, b"moved.txt"), 0);
    assert_eq!(guest.remove(RMDIR, 1, b"made"), 0);
    assert!(!root.join("made").exists());
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
}

#[test]
fn a_write_never_reaches_outside_a_read_write_share() {
    let tree = Tree::new("rw-escape");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    // Through a symlink, every write lands on the symlink or fails.
    let escape = guest.walk("escape").unwrap();
    assert_eq!(guest.open(escape, O_WRONLY), Err(EINVAL));
    assert_eq!(guest.setattr(escape, FATTR_SIZE, 0, 0), ELOOP);
    let dirlink = guest.walk("dirlink").unwrap();
    assert_eq!(guest.create(dirlink, b"planted", 0), Err(ENOTDIR));
    assert_eq!(guest.mkdir(dirlink, b"planted"), Err(ENOTDIR));
    for name in [&b"../planted"[..], b"/tmp/planted", b"a/b", b"..", b"."] {
        assert!(guest.create(1, name, 0).is_err(), "{name:?}");
        assert!(guest.mkdir(1, name).is_err(), "{name:?}");
        assert_ne!(guest.rename(1, b"hello.txt", 1, name, 0), 0, "{name:?}");
    }
    assert_eq!(guest.remove(UNLINK, 1, b"escape"), 0);
    assert_eq!(fs::read(&tree.secret).unwrap(), CANARY);
    let outside: Vec<_> = fs::read_dir(tree.base.join("outside")).unwrap().collect();
    assert_eq!(outside.len(), 1);
    assert!(!tree.base.join("planted").exists());
}

// spec(SHARE): The agent sees only what is under the share's root.
#[test]
fn a_directory_the_host_moves_outside_the_root_is_no_longer_served() {
    let tree = Tree::new("moved-out");
    fs::create_dir(tree.root.join("dir/deeper")).unwrap();
    fs::write(tree.root.join("dir/deeper/kept.txt"), INSIDE).unwrap();
    let mut guest = Guest::new(&tree.root);
    let dir = guest.walk("dir").unwrap();
    let deeper = guest.walk("dir/deeper").unwrap();
    let nested = guest.walk("dir/nested.txt").unwrap();
    let (error, out) = guest.call(OPENDIR, dir, &[0; 8]);
    assert_eq!(error, 0);
    let listing = u64::from_le_bytes(out[..8].try_into().unwrap());
    let away = tree.base.join("outside/dir");
    fs::rename(tree.root.join("dir"), &away).unwrap();
    fs::write(away.join("new.txt"), CANARY).unwrap();
    fs::write(away.join("deeper/new.txt"), CANARY).unwrap();
    // Neither the directory nor one beneath it is served, wherever the
    // guest starts from.
    for node in [dir, deeper] {
        assert_eq!(guest.lookup(node, b"new.txt"), Err(ENOENT));
        assert_eq!(guest.call(OPENDIR, node, &[0; 8]).0, ENOENT);
    }
    assert_eq!(guest.lookup(deeper, b"kept.txt"), Err(ENOENT));
    assert_eq!(guest.open(nested, 0), Err(ENOENT));
    assert_eq!(guest.read(READDIR, listing, 0, 4096).0, ENOENT);
    // Moved back beneath the root, it is served again.
    fs::rename(&away, tree.root.join("back")).unwrap();
    assert_eq!(guest.read_file("back/deeper/new.txt").unwrap(), CANARY);
    assert!(guest.lookup(deeper, b"kept.txt").is_ok());
    assert!(guest.open(nested, 0).is_ok());
}

#[test]
fn a_write_never_reaches_a_directory_the_host_moves_outside_the_root() {
    let tree = Tree::new("rw-moved-out");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let dir = guest.walk("dir").unwrap();
    let nested = guest.walk("dir/nested.txt").unwrap();
    let away = tree.base.join("outside/dir");
    fs::rename(tree.root.join("dir"), &away).unwrap();
    assert_eq!(guest.create(dir, b"planted", 0), Err(ENOENT));
    assert_eq!(guest.mkdir(dir, b"planted"), Err(ENOENT));
    assert_eq!(guest.symlink(dir, b"planted", b"x"), Err(ENOENT));
    assert_eq!(guest.remove(UNLINK, dir, b"nested.txt"), ENOENT);
    assert_eq!(guest.rename(dir, b"nested.txt", 1, b"taken", 0), ENOENT);
    assert_eq!(guest.rename(1, b"hello.txt", dir, b"given", 0), ENOENT);
    assert_eq!(guest.link(nested, 1, b"taken"), Err(ENOENT));
    assert_eq!(guest.open(nested, O_WRONLY), Err(ENOENT));
    assert_eq!(guest.setattr(dir, FATTR_MODE, 0, 0o777), ENOENT);
    assert_eq!(guest.setattr(nested, FATTR_SIZE, 0, 0), ENOENT);
    let mut names: Vec<_> = fs::read_dir(&away)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["nested.txt"]);
    assert_eq!(fs::read(away.join("nested.txt")).unwrap(), b"nested");
    assert!(tree.root.join("hello.txt").exists());
    assert!(!tree.root.join("taken").exists());
}

// spec(SHARE): In `rw` shares, the files your host acts on are read-only to the agent.
// spec(SHARE): An entry named `Agentfile` or `Agentfile.lock` can't be created, changed, removed, or renamed, so an agent can't change what a later build does.
// spec(SHARE): A change these rules refuse fails with an `Operation not permitted` error.
#[test]
fn an_agentfile_can_t_be_created_changed_removed_or_renamed() {
    let tree = protected_tree("agentfile");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let app = guest.walk("app").unwrap();
    let dir = guest.walk("dir").unwrap();
    for path in ["Agentfile", "Agentfile.lock", "app/Agentfile"] {
        assert_eq!(guest.overwrite(path, b"agent"), Err(EPERM), "{path}");
        let node = guest.walk(path).unwrap();
        assert_eq!(guest.setattr(node, FATTR_SIZE, 0, 0), EPERM, "{path}");
        assert_eq!(guest.setattr(node, FATTR_MODE, 0, 0o777), EPERM, "{path}");
        assert_eq!(guest.link(node, dir, b"alias"), Err(EPERM), "{path}");
    }
    for (parent, name) in [
        (1, &b"Agentfile"[..]),
        (1, b"Agentfile.lock"),
        (app, b"Agentfile"),
    ] {
        assert_eq!(guest.remove(UNLINK, parent, name), EPERM);
        assert_eq!(guest.rename(parent, name, dir, b"moved", 0), EPERM);
        assert_eq!(guest.rename(1, b"hello.txt", parent, name, 0), EPERM);
        assert_eq!(
            guest.rename(1, b"hello.txt", parent, name, RENAME_EXCHANGE),
            EPERM
        );
    }
    for name in [&b"Agentfile"[..], b"Agentfile.lock"] {
        assert_eq!(guest.create(dir, name, 0), Err(EPERM));
        assert_eq!(guest.mkdir(dir, name), Err(EPERM));
        assert_eq!(guest.symlink(dir, name, b"../hello.txt"), Err(EPERM));
        let hello = guest.walk("hello.txt").unwrap();
        assert_eq!(guest.link(hello, dir, name), Err(EPERM));
    }
    for file in ["Agentfile", "Agentfile.lock", "app/Agentfile"] {
        assert_eq!(fs::read(tree.root.join(file)).unwrap(), b"host", "{file}");
    }
    assert!(!tree.root.join("dir/Agentfile").exists());
}

// spec(SHARE): An entry named `.git` can't be created, removed, or renamed, and when it is a file (as in worktrees), it can't be changed.
#[test]
fn a_git_entry_can_t_be_created_removed_or_renamed() {
    let tree = protected_tree("dotgit");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let dir = guest.walk("dir").unwrap();
    let worktree = guest.walk("worktree").unwrap();
    assert_eq!(guest.mkdir(dir, b".git"), Err(EPERM));
    assert_eq!(guest.create(dir, b".git", 0), Err(EPERM));
    assert_eq!(guest.symlink(dir, b".git", b"/elsewhere"), Err(EPERM));
    assert_eq!(
        guest.rename(1, b"dir", worktree, b".git", RENAME_EXCHANGE),
        EPERM
    );
    assert_eq!(guest.rename(1, b".git", dir, b"stolen", 0), EPERM);
    assert_eq!(guest.remove(RMDIR, 1, b".git"), EPERM);
    assert_eq!(guest.remove(UNLINK, worktree, b".git"), EPERM);
    assert_eq!(
        guest.overwrite("worktree/.git", b"gitdir: /elsewhere"),
        Err(EPERM)
    );
    let file = guest.walk("worktree/.git").unwrap();
    assert_eq!(guest.setattr(file, FATTR_SIZE, 0, 0), EPERM);
    assert_eq!(fs::read(tree.root.join("worktree/.git")).unwrap(), b"host");
    assert!(!tree.root.join("dir/.git").exists());
    // What else is in a `.git` directory changes, as git changes it.
    guest
        .overwrite(".git/HEAD", b"ref: refs/heads/agent")
        .unwrap();
    guest.overwrite(".git/refs/heads/main", b"agent").unwrap();
    let refs = guest.walk(".git/refs/heads").unwrap();
    let (_, handle) = guest.create(refs, b"config", 0).unwrap();
    guest.release(handle);
    assert_eq!(
        fs::read(tree.root.join(".git/HEAD")).unwrap(),
        b"ref: refs/heads/agent"
    );
}

// spec(SHARE): In every git directory, including `.git/modules/*` and `.git/worktrees/*`, these are read-only, because git runs hooks and configured commands on your host: `config`, `config.worktree`, `commondir`, `gitdir`, and `hooks/`.
#[test]
fn a_git_directory_s_config_and_hooks_are_read_only() {
    let tree = protected_tree("gitdir");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    for path in [
        ".git/config",
        ".git/config.worktree",
        ".git/hooks/pre-commit",
        ".git/modules/sub/config",
        ".git/modules/sub/hooks/post-checkout",
        ".git/modules/libs/deep/config",
        ".git/worktrees/wt/commondir",
        ".git/worktrees/wt/gitdir",
        ".git/worktrees/wt/config.worktree",
        "nested/.git/config",
    ] {
        assert_eq!(guest.overwrite(path, b"agent"), Err(EPERM), "{path}");
        let node = guest.walk(path).unwrap();
        assert_eq!(guest.setattr(node, FATTR_MODE, 0, 0o777), EPERM, "{path}");
        let (parent, name) = path.rsplit_once('/').unwrap();
        let parent = guest.walk(parent).unwrap();
        assert_eq!(
            guest.remove(UNLINK, parent, name.as_bytes()),
            EPERM,
            "{path}"
        );
        assert_eq!(
            guest.rename(parent, name.as_bytes(), 1, b"out", 0),
            EPERM,
            "{path}"
        );
        assert_eq!(
            guest.rename(1, b"hello.txt", parent, name.as_bytes(), 0),
            EPERM
        );
    }
    for dir in [
        ".git",
        ".git/modules/sub",
        ".git/worktrees/wt",
        "nested/.git",
    ] {
        let parent = guest.walk(dir).unwrap();
        for name in [&b"config"[..], b"config.worktree", b"commondir", b"gitdir"] {
            if guest.lookup(parent, name).is_err() {
                assert_eq!(guest.create(parent, name, 0), Err(EPERM), "{dir}");
                assert_eq!(guest.symlink(parent, name, b"/x"), Err(EPERM), "{dir}");
            }
        }
        if guest.lookup(parent, b"hooks").is_err() {
            assert_eq!(guest.mkdir(parent, b"hooks"), Err(EPERM), "{dir}");
        }
    }
    // Nothing in `hooks/` changes, and nothing joins it.
    let hooks = guest.walk(".git/hooks").unwrap();
    assert_eq!(guest.create(hooks, b"post-merge", 0), Err(EPERM));
    assert_eq!(guest.mkdir(hooks, b"more"), Err(EPERM));
    assert_eq!(
        guest.rename(1, b"hello.txt", hooks, b"post-merge", 0),
        EPERM
    );
    let git = guest.walk(".git").unwrap();
    assert_eq!(guest.remove(RMDIR, git, b"hooks"), EPERM);
    // A directory the agent filled can't move into a place git reads as a
    // git directory, and neither can a symlink.
    let staged = guest.mkdir(1, b"staged").unwrap();
    guest
        .create(staged, b"config", 0)
        .map(|(_, h)| guest.release(h))
        .unwrap();
    let modules = guest.walk(".git/modules").unwrap();
    assert_eq!(guest.rename(1, b"staged", modules, b"evil", 0), EPERM);
    assert_eq!(guest.symlink(modules, b"evil", b"../../staged"), Err(EPERM));
    assert_eq!(
        guest.rename(1, b"staged", git, b"worktrees", RENAME_EXCHANGE),
        EPERM
    );
    for file in [
        ".git/config",
        ".git/hooks/pre-commit",
        ".git/modules/sub/config",
    ] {
        assert_eq!(fs::read(tree.root.join(file)).unwrap(), b"host", "{file}");
    }
    assert!(!tree.root.join(".git/hooks/post-merge").exists());
    assert!(!tree.root.join(".git/modules/evil").exists());
}

// spec(SHARE): A directory that holds `HEAD`, and either `commondir` or both `objects` and `refs`, is a git directory too, because that is how git finds a bare repository.
// spec(SHARE): None of those entries can be removed or moved out of it.
#[test]
fn a_bare_repository_s_config_and_hooks_are_read_only() {
    let tree = Tree::new("bare");
    let root = &tree.root;
    for dir in [
        "repo.git/objects",
        "repo.git/refs",
        "repo.git/hooks",
        "linked",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for file in [
        "repo.git/HEAD",
        "repo.git/config",
        "repo.git/hooks/pre-receive",
        "linked/HEAD",
        "linked/commondir",
        "linked/config.worktree",
    ] {
        fs::write(root.join(file), b"host").unwrap();
    }
    let mut guest = Guest::with(root, Mode::ReadWrite);
    for path in [
        "repo.git/config",
        "repo.git/hooks/pre-receive",
        "linked/commondir",
        "linked/config.worktree",
    ] {
        assert_eq!(guest.overwrite(path, b"agent"), Err(EPERM), "{path}");
    }
    let repo = guest.walk("repo.git").unwrap();
    let hooks = guest.walk("repo.git/hooks").unwrap();
    assert_eq!(guest.create(hooks, b"post-receive", 0), Err(EPERM));
    assert_eq!(guest.create(repo, b"config.worktree", 0), Err(EPERM));
    // `HEAD` changes, as git changes it, by a rename over it.
    guest
        .overwrite("repo.git/HEAD", b"ref: refs/heads/agent")
        .unwrap();
    let (_, handle) = guest.create(repo, b"HEAD.lock", 0).unwrap();
    guest.release(handle);
    assert_eq!(guest.rename(repo, b"HEAD.lock", repo, b"HEAD", 0), 0);
    // What git finds the repository by stays.
    for (name, opcode) in [
        (&b"HEAD"[..], UNLINK),
        (b"objects", RMDIR),
        (b"refs", RMDIR),
    ] {
        assert_eq!(guest.remove(opcode, repo, name), EPERM, "{name:?}");
        assert_eq!(guest.rename(repo, name, 1, b"out", 0), EPERM, "{name:?}");
    }
    let linked = guest.walk("linked").unwrap();
    assert_eq!(guest.remove(UNLINK, linked, b"HEAD"), EPERM);
    // So does a root that is one, or that is beneath one.
    let mut bare = Guest::with(&root.join("repo.git"), Mode::ReadWrite);
    assert_eq!(bare.overwrite("config", b"agent"), Err(EPERM));
    let mut beneath = Guest::with(&root.join("repo.git/hooks"), Mode::ReadWrite);
    assert_eq!(beneath.overwrite("pre-receive", b"agent"), Err(EPERM));
    for file in [
        "repo.git/config",
        "repo.git/hooks/pre-receive",
        "linked/commondir",
    ] {
        assert_eq!(fs::read(root.join(file)).unwrap(), b"host", "{file}");
    }
    assert!(root.join("repo.git/objects").is_dir());
    assert!(!root.join("repo.git/hooks/post-receive").exists());
}

// spec(SHARE): Anywhere else, the agent can't make the entry that would complete that set, so nothing the agent wrote becomes a bare repository.
#[test]
fn no_directory_the_agent_writes_becomes_a_bare_repository() {
    let tree = Tree::new("make-bare");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let made = |guest: &mut Guest, parent, name: &[u8]| {
        let (_, handle) = guest.create(parent, name, 0)?;
        guest.release(handle);
        Ok::<(), i32>(())
    };
    // Part of the set is no git directory, so it can be made.
    let refs_last = guest.mkdir(1, b"refs-last").unwrap();
    made(&mut guest, refs_last, b"HEAD").unwrap();
    made(&mut guest, refs_last, b"config").unwrap();
    guest.mkdir(refs_last, b"objects").unwrap();
    assert_eq!(guest.mkdir(refs_last, b"refs"), Err(EPERM));
    assert_eq!(guest.symlink(refs_last, b"refs", b"/x"), Err(EPERM));
    guest.mkdir(1, b"staged").unwrap();
    assert_eq!(guest.rename(1, b"staged", refs_last, b"refs", 0), EPERM);
    // `HEAD` can't be made in any way, in any case.
    let head_last = guest.mkdir(1, b"head-last").unwrap();
    guest.mkdir(head_last, b"objects").unwrap();
    guest.mkdir(head_last, b"refs").unwrap();
    for name in [&b"HEAD"[..], b"head"] {
        assert_eq!(made(&mut guest, head_last, name), Err(EPERM), "{name:?}");
    }
    assert_eq!(guest.symlink(head_last, b"HEAD", b"refs"), Err(EPERM));
    let hello = guest.walk("hello.txt").unwrap();
    assert_eq!(guest.link(hello, head_last, b"HEAD"), Err(EPERM));
    assert_eq!(guest.rename(1, b"hello.txt", head_last, b"HEAD", 0), EPERM);
    // `commondir` with `HEAD` is enough, in either order.
    let common = guest.mkdir(1, b"common").unwrap();
    made(&mut guest, common, b"HEAD").unwrap();
    assert_eq!(made(&mut guest, common, b"commondir"), Err(EPERM));
    let head = guest.mkdir(1, b"head").unwrap();
    made(&mut guest, head, b"commondir").unwrap();
    assert_eq!(made(&mut guest, head, b"HEAD"), Err(EPERM));
    for path in [
        "refs-last/refs",
        "head-last/HEAD",
        "head-last/head",
        "common/commondir",
        "head/HEAD",
    ] {
        assert!(
            fs::symlink_metadata(tree.root.join(path)).is_err(),
            "{path}"
        );
    }
    assert!(tree.root.join("staged").is_dir());
}

// spec(SHARE): Names match case-insensitively, as they do on macOS filesystems.
#[test]
fn protected_names_match_without_regard_to_case() {
    let tree = protected_tree("case");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let dir = guest.walk("dir").unwrap();
    for name in [
        &b"AGENTFILE"[..],
        b"agentfile.LOCK",
        // The Kelvin sign folds to `k` on macOS.
        "agentfile.loc\u{212a}".as_bytes(),
        b".GIT",
        b".Git",
        // HFS+ ignores some code points in names, as git knows.
        ".g\u{200c}it".as_bytes(),
    ] {
        assert_eq!(guest.create(dir, name, 0), Err(EPERM), "{name:?}");
        assert_eq!(guest.mkdir(dir, name), Err(EPERM), "{name:?}");
    }
    let git = guest.walk(".git").unwrap();
    for name in [
        &b"CONFIG"[..],
        b"Config.Worktree",
        b"HOOKS",
        b"CommonDir",
        b"GITDIR",
    ] {
        assert_eq!(guest.create(git, name, 0), Err(EPERM), "{name:?}");
    }
    assert!(!tree.root.join("dir/AGENTFILE").exists());
}

// spec(SHARE): File locks pass through to the host.
// spec(SHARE): A lock that would wait for another holder fails at once with an `Operation not supported` error, because the agent's machine waits for each answer from your host.
#[test]
fn file_locks_pass_through_to_the_host() {
    for mode in [Mode::ReadWrite, Mode::ReadOnly] {
        let tree = Tree::new(&format!("locks-{mode:?}"));
        let path = tree.root.join("hello.txt");
        let mut guest = Guest::with(&tree.root, mode);
        let mut init = Vec::new();
        for value in [7u32, 31, 1 << 17, u32::MAX] {
            init.extend(value.to_le_bytes());
        }
        init.resize(64, 0);
        let (_, out) = guest.call(INIT, 0, &init);
        let flags = u32::from_le_bytes(out[12..16].try_into().unwrap());
        assert_eq!(
            flags & (POSIX_LOCKS | FLOCK_LOCKS),
            POSIX_LOCKS | FLOCK_LOCKS
        );

        let node = guest.walk("hello.txt").unwrap();
        let handle = guest.open(node, 0).unwrap();
        // A POSIX lock the guest takes is one the host sees.
        assert_eq!(guest.lock(SETLK, handle, 7, F_RDLCK, 0).0, 0);
        assert!(host_locked(&path, libc::F_WRLCK as i16));
        assert!(!host_locked(&path, libc::F_RDLCK as i16));
        // Another owner in the guest shares it, as a reader.
        assert_eq!(guest.lock(SETLK, handle, 8, F_RDLCK, 0).0, 0);
        assert_eq!(guest.lock(SETLK, handle, 7, F_UNLCK, 0).0, 0);
        assert_eq!(guest.lock(SETLK, handle, 8, F_UNLCK, 0).0, 0);
        assert!(!host_locked(&path, libc::F_WRLCK as i16));

        // A lock the host holds stops the guest's, which never waits.
        let host = File::options().read(true).write(true).open(&path).unwrap();
        assert!(host_lock(&host, libc::F_WRLCK as i16));
        assert_eq!(guest.lock(SETLK, handle, 7, F_RDLCK, 0).0, EAGAIN);
        assert_eq!(guest.lock(SETLKW, handle, 7, F_RDLCK, 0).0, EOPNOTSUPP);
        let (error, conflict) = guest.lock(GETLK, handle, 7, F_RDLCK, 0);
        assert_eq!(error, 0);
        assert_eq!(
            u32::from_le_bytes(conflict[16..20].try_into().unwrap()),
            F_WRLCK
        );
        drop(host);
        assert_eq!(guest.lock(SETLKW, handle, 7, F_RDLCK, 0).0, 0);
        let (_, free) = guest.lock(GETLK, handle, 9, F_RDLCK, 0);
        assert_eq!(
            u32::from_le_bytes(free[16..20].try_into().unwrap()),
            F_UNLCK
        );
        // Closing the file in the guest lets the owner's locks go.
        let mut flush = handle.to_le_bytes().to_vec();
        flush.extend([0; 8]);
        flush.extend(7u64.to_le_bytes());
        assert_eq!(guest.call(FLUSH, node, &flush).0, 0);
        assert!(!host_locked(&path, libc::F_WRLCK as i16));

        // `flock` passes through as `flock`.
        assert_eq!(guest.lock(SETLK, handle, 7, F_RDLCK, LK_FLOCK).0, 0);
        assert!(!host_flock(&path, libc::LOCK_EX));
        assert!(host_flock(&path, libc::LOCK_SH));
        guest.release(handle);
        assert!(host_flock(&path, libc::LOCK_EX));
    }
}

/// Takes a lock of `kind` on the whole file, without waiting.
fn host_lock(file: &File, kind: i16) -> bool {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    // SAFETY: as in `host_locked`.
    unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(file),
            libc::F_OFD_SETLK,
            &mut lock,
        ) == 0
    }
}

/// Whether the host takes a `flock` of `kind` without waiting.
fn host_flock(path: &Path, kind: i32) -> bool {
    let file = File::open(path).unwrap();
    // SAFETY: flock takes no pointer.
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), kind | libc::LOCK_NB) == 0 }
}

#[test]
fn a_root_in_a_git_directory_keeps_its_place() {
    let tree = protected_tree("root-in-git");
    let mut guest = Guest::with(&tree.root.join(".git"), Mode::ReadWrite);
    assert_eq!(guest.overwrite("config", b"agent"), Err(EPERM));
    assert_eq!(guest.overwrite("modules/sub/config", b"agent"), Err(EPERM));
    guest.overwrite("HEAD", b"agent").unwrap();
    let mut hooks = Guest::with(&tree.root.join(".git/hooks"), Mode::ReadWrite);
    assert_eq!(hooks.overwrite("pre-commit", b"agent"), Err(EPERM));
    assert_eq!(hooks.create(1, b"post-merge", 0), Err(EPERM));
    assert_eq!(fs::read(tree.root.join(".git/config")).unwrap(), b"host");
    assert_eq!(
        fs::read(tree.root.join(".git/hooks/pre-commit")).unwrap(),
        b"host"
    );
}

// spec(SHARE): A file with more than one hard link can't be changed in place in an `rw` share, because another of its names may be one of these.
#[test]
fn a_file_with_another_name_can_t_be_changed_in_place() {
    let tree = protected_tree("alias");
    fs::hard_link(tree.root.join("Agentfile"), tree.root.join("alias")).unwrap();
    fs::hard_link(tree.root.join("hello.txt"), tree.root.join("dir/twin")).unwrap();
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    // The other name is found first, so nothing says it is an Agentfile.
    let alias = guest.walk("alias").unwrap();
    assert_eq!(guest.overwrite("alias", b"agent"), Err(EPERM));
    assert_eq!(guest.create(1, b"alias", O_TRUNC), Err(EPERM));
    assert_eq!(guest.setattr(alias, FATTR_SIZE, 0, 0), EPERM);
    assert_eq!(guest.setattr(alias, FATTR_MODE, 0, 0o777), EPERM);
    assert_eq!(guest.overwrite("dir/twin", b"agent"), Err(EPERM));
    assert_eq!(fs::read(tree.root.join("Agentfile")).unwrap(), b"host");
    // Its names still move and go, and with one name left, it changes.
    let dir = guest.walk("dir").unwrap();
    assert_eq!(guest.rename(dir, b"twin", 1, b"moved", 0), 0);
    assert_eq!(guest.remove(UNLINK, 1, b"moved"), 0);
    guest.overwrite("hello.txt", b"agent").unwrap();
    // A link the agent makes takes the change away too.
    let hello = guest.walk("hello.txt").unwrap();
    guest.link(hello, dir, b"again").unwrap();
    assert_eq!(guest.overwrite("hello.txt", b"more"), Err(EPERM));
    assert_eq!(fs::read(tree.root.join("hello.txt")).unwrap(), b"agent");
    // Nor does a file open from before the link, even to append.
    let (file, handle) = guest.create(1, b"open.txt", O_APPEND).unwrap();
    assert_eq!(guest.write(handle, 0, b"one"), Ok(3));
    fs::hard_link(tree.root.join("open.txt"), tree.root.join("Agentfile.copy")).unwrap();
    assert_eq!(guest.write(handle, 0, b"two"), Err(EPERM));
    let mut range = handle.to_le_bytes().to_vec();
    for value in [0u64, 4096] {
        range.extend(value.to_le_bytes());
    }
    range.extend([0; 8]);
    assert_eq!(guest.call(43, file, &range).0, EPERM);
    assert_eq!(fs::read(tree.root.join("open.txt")).unwrap(), b"one");
}

#[test]
fn files_in_a_git_directory_beneath_modules_still_change() {
    let tree = protected_tree("nested-head");
    fs::write(tree.root.join(".git/modules/sub/HEAD"), b"host").unwrap();
    fs::write(tree.root.join(".git/worktrees/wt/HEAD"), b"host").unwrap();
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    guest.overwrite(".git/modules/sub/HEAD", b"agent").unwrap();
    guest.overwrite(".git/worktrees/wt/HEAD", b"agent").unwrap();
    let sub = guest.walk(".git/modules/sub").unwrap();
    let (_, handle) = guest.create(sub, b"index.lock", 0).unwrap();
    assert_eq!(guest.write(handle, 0, b"index"), Ok(5));
    assert_eq!(
        fs::read(tree.root.join(".git/modules/sub/HEAD")).unwrap(),
        b"agent"
    );
}

// spec(SHARE): An agent's first POSIX lock on a file fails if the name the agent last reached the file by is gone.
#[test]
fn a_first_lock_needs_the_file_s_name() {
    let tree = Tree::new("lock-unlinked");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let (_, handle) = guest.create(1, b"temp", 0).unwrap();
    assert_eq!(guest.lock(SETLK, handle, 7, F_WRLCK, 0).0, 0);
    assert_eq!(guest.remove(UNLINK, 1, b"temp"), 0);
    // The owner that locked it keeps its lock file.
    assert_eq!(guest.lock(SETLK, handle, 7, F_UNLCK, 0).0, 0);
    assert_eq!(guest.lock(SETLK, handle, 8, F_WRLCK, 0).0, ENOENT);
    // `flock` needs no name.
    assert_eq!(guest.lock(SETLK, handle, 8, F_WRLCK, LK_FLOCK).0, 0);
}

// spec(SHARE): The agent can't make a device, a FIFO, or a socket in an `rw` share, change a file's owner, or set an extended attribute.
#[test]
fn an_rw_share_makes_no_special_file_and_changes_no_owner() {
    let tree = Tree::new("special");
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    for (kind, name) in [
        (0o020_000u32, &b"char"[..]),
        (0o060_000, b"block"),
        (0o010_000, b"fifo"),
        (0o140_000, b"socket"),
        (0o100_000, b"regular"),
    ] {
        let mut body = (kind | 0o644).to_le_bytes().to_vec();
        body.extend([0; 12]);
        body.extend(named(name));
        assert_eq!(guest.call(MKNOD, 1, &body).0, EPERM, "{kind:o}");
        assert!(fs::symlink_metadata(tree.root.join(std::str::from_utf8(name).unwrap())).is_err());
    }
    let file = guest.walk("hello.txt").unwrap();
    for owner in [FATTR_UID, 4] {
        assert_eq!(guest.setattr(file, owner, 0, 0), EPERM);
    }
    let setxattr = 21;
    let mut body = 4u32.to_le_bytes().to_vec();
    body.extend([0; 4]);
    body.extend(named(b"user.agent"));
    body.extend(b"data");
    assert_eq!(guest.call(setxattr, file, &body).0, ENOSYS);
    assert_eq!(guest.call(24, file, &named(b"user.agent")).0, ENOSYS);
}

// spec(SHARE): Beneath `.git/modules` and `.git/worktrees`, every directory counts as a git directory, because a submodule's name can hold `/`.
// spec(SHARE): A directory or a symlink can't be moved or linked beneath `.git/modules` or `.git/worktrees`, or to `modules` or `worktrees` in a git directory, and no symlink can be made in those places, so nothing the agent wrote becomes a git directory.
#[test]
fn nothing_the_agent_wrote_becomes_a_git_directory() {
    let tree = protected_tree("holders");
    fs::create_dir_all(tree.root.join(".git/modules/sub/refs/heads")).unwrap();
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    // Deep beneath `modules`, the names are protected too.
    let heads = guest.walk(".git/modules/sub/refs/heads").unwrap();
    assert_eq!(guest.create(heads, b"config", 0), Err(EPERM));
    let libs = guest.walk(".git/modules/libs").unwrap();
    assert_eq!(guest.create(libs, b"config", 0), Err(EPERM));
    // A payload can't take the place of `modules` or `worktrees`, as a
    // symlink or as a directory.
    let payload = guest.mkdir(1, b"payload").unwrap();
    guest.mkdir(payload, b"sub").unwrap();
    let git = guest.walk(".git").unwrap();
    assert_eq!(guest.rename(git, b"modules", 1, b"moved-away", 0), 0);
    assert_eq!(guest.symlink(git, b"modules", b"../payload"), Err(EPERM));
    assert_eq!(guest.symlink(git, b"MODULES", b"../payload"), Err(EPERM));
    assert_eq!(guest.rename(1, b"payload", git, b"modules", 0), EPERM);
    guest.symlink(1, b"aim", b"payload").unwrap();
    assert_eq!(guest.rename(1, b"aim", git, b"modules", 0), EPERM);
    assert_eq!(
        guest.rename(1, b"aim", git, b"worktrees", RENAME_EXCHANGE),
        EPERM
    );
    let aim = guest.walk("aim").unwrap();
    assert_eq!(guest.link(aim, git, b"modules"), Err(EPERM));
    assert!(fs::symlink_metadata(tree.root.join(".git/modules")).is_err());
    assert!(tree.root.join(".git/worktrees").is_dir());
    // Nor can a symlink land beneath them.
    let worktrees = guest.walk(".git/worktrees").unwrap();
    assert_eq!(
        guest.symlink(worktrees, b"wt2", b"../../payload"),
        Err(EPERM)
    );
    assert_eq!(guest.rename(1, b"aim", worktrees, b"wt2", 0), EPERM);
    // A symlink lands elsewhere in a git directory, as git makes them.
    guest.symlink(git, b"elsewhere", b"refs").unwrap();
}

#[test]
fn a_file_is_truncated_only_if_it_is_the_one_looked_up() {
    let tree = Tree::new("stale-trunc");
    fs::write(tree.root.join("a"), b"a").unwrap();
    fs::write(tree.root.join("b"), b"b").unwrap();
    let mut guest = Guest::with(&tree.root, Mode::ReadWrite);
    let b = guest.walk("b").unwrap();
    guest.walk("a").unwrap();
    // The host replaces `b`, as an editor saves it.
    fs::rename(tree.root.join("a"), tree.root.join("b")).unwrap();
    assert_eq!(guest.open(b, O_WRONLY | O_TRUNC), Err(ESTALE));
    assert_eq!(fs::read(tree.root.join("b")).unwrap(), b"a");
}
