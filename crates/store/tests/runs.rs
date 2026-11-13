//! The store keeps each run under its name, and a run is active while the
//! process that started it holds the lock on its record.

use std::fs;
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
    let (first, _) = runs.create("notes", 7).unwrap();
    let mut picks = vec![Ok("k3x9q2mf".to_owned()), Ok(first.name.clone())];
    let (second, _) = runs
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
    let (run, file) = runs.create("notes", 1).unwrap();
    assert!(!runs.active(&run.name).unwrap());
    file.lock().unwrap();
    assert!(runs.active(&run.name).unwrap());
    drop(file);
    assert!(!runs.active(&run.name).unwrap());
    assert!(!runs.active("k3x9q2mf").unwrap());
    assert_eq!(runs.get("k3x9q2mf").unwrap(), None);
    assert!(runs.get("../agents").is_err());
    assert!(runs.create("Notes", 1).is_err());
    assert!(random_name().is_ok());
}
