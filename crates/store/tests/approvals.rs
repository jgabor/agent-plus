//! The store keeps the grants you approved with the canonical path of the
//! Agentfile you approved them for, not with the agent's name.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use agentfile::Grants;
use store::Approvals;

/// A fresh store root for one test.
fn store_root(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("approvals")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("store")
}

fn grants(text: &str) -> Grants {
    text.parse().unwrap()
}

// spec(Grants): The approval is stored with the Agentfile's canonical path, not with the agent's name, so another directory never inherits it.
#[test]
fn an_approval_is_kept_with_its_agentfile_path() {
    let root = store_root("kept");
    let approvals = Approvals::new(&root);
    let notes = Path::new("/home/me/notes/Agentfile");
    let approved = grants("PATH /home/me/shared\nNETWORK -\nPOLICY -\n");
    assert_eq!(approvals.get(notes).unwrap(), None);
    approvals.put(notes, &approved).unwrap();
    assert_eq!(approvals.get(notes).unwrap(), Some(approved));
    let copy = Path::new("/home/me/copy/Agentfile");
    assert_eq!(approvals.get(copy).unwrap(), None);
}

#[test]
fn a_new_approval_replaces_the_old_one() {
    let root = store_root("replaced");
    let approvals = Approvals::new(&root);
    let notes = Path::new("/home/me/notes/Agentfile");
    approvals
        .put(notes, &grants("PATH /home/me/shared\nNETWORK -\nPOLICY -\n"))
        .unwrap();
    let wider = grants("PATH /home/me/shared rw\nNETWORK +\nPOLICY -\n");
    approvals.put(notes, &wider).unwrap();
    assert_eq!(approvals.get(notes).unwrap(), Some(wider));
    let names: Vec<String> = fs::read_dir(root.join("approvals"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn a_path_with_spaces_and_controls_keys_an_approval() {
    let root = store_root("printed");
    let approvals = Approvals::new(&root);
    let odd = Path::new("/home/me/my \u{202e}notes/Agentfile");
    let approved = grants("NETWORK -\nPOLICY +\n");
    approvals.put(odd, &approved).unwrap();
    assert_eq!(approvals.get(odd).unwrap(), Some(approved));
}

#[test]
fn a_relative_path_can_not_key_an_approval() {
    let root = store_root("relative");
    let approvals = Approvals::new(&root);
    let error = approvals
        .put(Path::new("notes/Agentfile"), &grants("NETWORK -\nPOLICY -\n"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(!root.join("approvals").exists());
}

#[test]
fn a_damaged_approval_is_an_error() {
    let root = store_root("damaged");
    let approvals = Approvals::new(&root);
    let notes = Path::new("/home/me/notes/Agentfile");
    approvals
        .put(notes, &grants("NETWORK -\nPOLICY -\n"))
        .unwrap();
    let file = fs::read_dir(root.join("approvals"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = fs::read_to_string(&file).unwrap();
    fs::write(&file, text.replace("NETWORK -", "NETWORK ~")).unwrap();
    let error = approvals.get(notes).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(error.to_string().contains("line 2"), "{error}");
    fs::write(&file, "AGENTFILE /home/me/other/Agentfile\nNETWORK -\n").unwrap();
    assert_eq!(approvals.get(notes).unwrap_err().kind(), ErrorKind::InvalidData);
}
