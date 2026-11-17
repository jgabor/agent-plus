//! The store keeps each run under its name, and a run is active while the
//! process that started it holds the lock on its record.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use store::{Runs, random_name};

/// A fresh store root for one test.
fn store_root(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("runs")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    dir.join("home/.local/share/ap")
}

// spec(Run): `ap` picks the name at random, and picks again if the store already has a run with that name.
#[test]
fn a_name_the_store_has_is_picked_again() {
    let root = store_root("again");
    let runs = Runs::new(&root);
    let first = runs.create("notes", 7).unwrap();
    let mut picks = vec![Ok("k3x9q2mf".to_owned()), Ok(first.name.clone())];
    let second = runs
        .create_with("notes", 9, || picks.pop().unwrap())
        .unwrap();
    assert!(picks.is_empty());
    assert_eq!(second.name, "k3x9q2mf");
    assert_eq!(runs.get(&first.name).unwrap().unwrap().started, 7);
    assert_eq!(runs.get("k3x9q2mf").unwrap(), Some(second));
    // Only the two records are left, with no unfinished file.
    assert_eq!(fs::read_dir(root.join("runs")).unwrap().count(), 2);
}

#[test]
fn a_run_is_active_while_its_record_is_locked() {
    let runs = Runs::new(store_root("active"));
    let run = runs.create("notes", 1).unwrap();
    assert!(!runs.active(&run.name).unwrap());
    let file = runs.hold(&run.name).unwrap();
    assert!(runs.active(&run.name).unwrap());
    drop(file);
    assert!(!runs.active(&run.name).unwrap());
    assert!(!runs.active("k3x9q2mf").unwrap());
    assert_eq!(runs.get("k3x9q2mf").unwrap(), None);
    assert!(runs.get("../agents").is_err());
    assert!(runs.hold("k3x9q2mf").is_err());
    assert!(runs.create("Notes", 1).is_err());
    assert!(random_name().is_ok());
}

#[test]
fn a_run_keeps_its_status_and_log_until_it_is_removed() {
    let root = store_root("kept");
    let runs = Runs::new(&root);
    assert_eq!(runs.list().unwrap(), Vec::new());
    let run = runs.create("notes", 1).unwrap();
    let other = runs.create("other", 2).unwrap();
    assert!(runs.log(&run.name).unwrap().is_none());
    let mut log = runs.create_log(&run.name).unwrap();
    log.write_all(b"said").unwrap();
    assert!(runs.create_log(&run.name).is_err());
    assert_eq!(
        io::read_to_string(runs.log(&run.name).unwrap().unwrap()).unwrap(),
        "said"
    );

    let held = runs.hold(&run.name).unwrap();
    store::record_exit(&held, 3).unwrap();
    drop(held);
    assert_eq!(runs.get(&run.name).unwrap().unwrap().exited, Some(3));
    let mut listed = runs.list().unwrap();
    listed.sort_by_key(|run| run.started);
    assert_eq!(
        listed
            .iter()
            .map(|run| (&*run.agent, run.exited))
            .collect::<Vec<_>>(),
        [("notes", Some(3)), ("other", None)]
    );

    runs.remove(&run.name).unwrap();
    runs.remove(&run.name).unwrap();
    assert_eq!(runs.get(&run.name).unwrap(), None);
    assert!(runs.log(&run.name).unwrap().is_none());
    assert_eq!(runs.list().unwrap(), vec![other]);
    assert!(runs.remove("../agents").is_err());
    assert!(runs.log("../agents").is_err());
}

// spec(SHARE): The next run starts from your current files, not from earlier unapplied changes.
#[test]
fn each_run_gets_a_pending_directory_of_its_own() {
    use std::os::unix::fs::PermissionsExt;
    let root = store_root("pending");
    let runs = Runs::new(&root);
    let run = runs.create("notes", 1).unwrap();
    let pending = runs.create_pending(&run.name).unwrap();
    assert_eq!(pending, root.join("pending").join(&run.name));
    for dir in [&pending, &root.join("pending")] {
        let mode = fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
    }
    // A run never takes over another's changes.
    fs::write(pending.join("records"), "").unwrap();
    let error = runs.create_pending(&run.name).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let other = runs.create("notes", 2).unwrap();
    assert!(runs.create_pending(&other.name).is_ok());
    assert!(runs.create_pending("../escape").is_err());
}

// spec(SHARE): `ap` never gives a run the name of an agent in the store.
#[test]
fn a_run_never_takes_an_agent_s_name() {
    let root = store_root("agent-name");
    let agents = store::Agents::new(&root);
    agents
        .record(
            &store::Agent {
                name: "k3x9q2mf".to_owned(),
                directory: "/work/k3x9q2mf".to_owned(),
                agentfile: "/work/k3x9q2mf/Agentfile".to_owned(),
                layers: Vec::new(),
                kinds: Vec::new(),
                declared: vec![store::Declared {
                    name: "AGENT".to_owned(),
                    arguments: vec!["claude".to_owned()],
                }],
            },
            false,
        )
        .unwrap();
    let runs = Runs::new(&root);
    let mut picks = vec![Ok("a1b2c3d4".to_owned()), Ok("k3x9q2mf".to_owned())];
    let run = runs
        .create_with("notes", 1, || picks.pop().unwrap())
        .unwrap();
    assert!(picks.is_empty());
    assert_eq!(run.name, "a1b2c3d4");
    assert_eq!(runs.get("k3x9q2mf").unwrap(), None);
}

// spec(SHARE): `ap discard <agent | run>` throws the changes away.
#[test]
fn discard_removes_a_directory_you_can_t_write_to() {
    use std::os::unix::fs::PermissionsExt;
    let root = store_root("discard-read-only");
    let runs = Runs::new(&root);
    let run = runs.create("notes", 1).unwrap();
    runs.create_pending(&run.name).unwrap();
    let share = runs
        .create_pending_share(&run.name, Path::new("/x"))
        .unwrap();
    let hooks = share.join("git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("pre-commit"), "").unwrap();
    fs::set_permissions(&hooks, fs::Permissions::from_mode(0o555)).unwrap();
    // A symlink to a directory you can't write to isn't followed.
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
    std::os::unix::fs::symlink(&outside, share.join("git/link")).unwrap();
    runs.discard(&run.name).unwrap();
    assert!(!root.join("pending").join(&run.name).exists());
    let mode = fs::metadata(&outside).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o500);
}
