//! The store keeps each built agent under its name: the directory it was built
//! from, its layers in order, and what its Agentfile declared. Removing or
//! rebuilding an agent frees the layers no other agent uses.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use store::{Agent, Agents, Declared, Layer, Layers, valid_name};

/// A fresh directory for one test, with `HOME` and the store root inside it.
fn store_root(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("agents")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    dir.join("home/.local/share/ap")
}

fn declared(name: &str, arguments: &[&str]) -> Declared {
    Declared {
        name: name.to_string(),
        arguments: arguments.iter().map(|a| a.to_string()).collect(),
    }
}

/// Puts each layer's bytes in the store, and returns them as an agent's
/// layers, keyed by `key <bytes>`.
fn layers(root: &Path, contents: &[&str]) -> Vec<Layer> {
    let store = Layers::new(root);
    contents
        .iter()
        .map(|content| Layer {
            key: format!("key {content}"),
            digest: store.put(content.as_bytes()).unwrap(),
        })
        .collect()
}

fn agent(name: &str, directory: &str, layers: Vec<Layer>) -> Agent {
    Agent {
        name: name.to_string(),
        directory: directory.to_string(),
        layers,
        declared: vec![declared("AGENT", &["claude"])],
    }
}

fn has_layer(root: &Path, digest: &str) -> bool {
    match Layers::new(root).get(digest) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => panic!("{error}"),
    }
}

// spec(Build): A name is 1 to 63 lowercase letters, digits, and `-`, starting and ending with a letter or digit, so it is safe in git branch names (see [SHARE](#share)).
#[test]
fn a_name_is_lowercase_letters_digits_and_dashes() {
    let longest = "a".repeat(63);
    for name in ["a", "7", "my-agent", "a1-b2", "x--y", longest.as_str()] {
        assert!(valid_name(name), "{name:?} is a name");
    }
    let too_long = "a".repeat(64);
    for name in [
        "",
        too_long.as_str(),
        "-a",
        "a-",
        "-",
        "Agent",
        "my_agent",
        "my.agent",
        "my agent",
        "é",
        ".a",
        "a/b",
    ] {
        assert!(!valid_name(name), "{name:?} isn't a name");
    }
}

#[test]
fn recording_an_agent_with_a_bad_name_is_refused() {
    let root = store_root("bad-name");
    let agents = Agents::new(&root);

    for name in ["", "Agent", "-a", "../a", ".lock"] {
        let error = agents
            .record(&agent(name, "/work/a", Vec::new()), false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{name:?}");
    }
    assert!(agents.list().unwrap().is_empty());
}

#[test]
fn a_recorded_agent_reads_back_the_same() {
    let root = store_root("round-trip");
    let agents = Agents::new(&root);
    let mut recorded = agent(
        "notes",
        "/home/me/my notes \"draft\"\n\\ \u{202e}",
        layers(&root, &["one", "two", "three"]),
    );
    recorded.declared = vec![
        declared("AGENT", &["claude"]),
        declared("ENV", &["GREETING=\"hello, $USER\""]),
        declared("SHARE", &["./a b", "/work", "rw"]),
        declared("PORT", &[""]),
        declared("SYSTEM", &[]),
    ];

    agents.record(&recorded, false).unwrap();

    assert_eq!(agents.get("notes").unwrap(), Some(recorded.clone()));
    assert_eq!(agents.list().unwrap(), vec![recorded]);
    assert_eq!(agents.get("other").unwrap(), None);
}

#[test]
fn a_record_is_written_in_the_agentfile_line_syntax() {
    let root = store_root("syntax");
    let agents = Agents::new(&root);
    let mut recorded = agent("notes", "/home/me/my notes", Vec::new());
    recorded.layers = vec![Layer {
        key: "ADD ./src /work".to_string(),
        digest: Layers::new(&root).put(b"src").unwrap(),
    }];
    recorded.declared = vec![
        declared("AGENT", &["claude"]),
        declared("SHARE", &["./out", "/work/out", "rw"]),
        declared("ENV", &["TAB=a\tb"]),
    ];

    agents.record(&recorded, false).unwrap();

    let digest = &recorded.layers[0].digest;
    assert_eq!(
        fs::read_to_string(root.join("agents/notes")).unwrap(),
        format!(
            "DIRECTORY \"/home/me/my notes\"\n\
             LAYER \"ADD ./src /work\" {digest}\n\
             AGENT claude\n\
             SHARE ./out /work/out rw\n\
             ENV TAB=a\\u{{9}}b\n"
        )
    );
}

#[test]
fn a_record_that_is_not_in_the_syntax_is_an_error() {
    let root = store_root("corrupt");
    let agents = Agents::new(&root);
    agents
        .record(&agent("notes", "/work/notes", Vec::new()), false)
        .unwrap();
    let path = root.join("agents/notes");

    for text in [
        "DIRECTORY /work/notes\nAGENT \"claude\n",
        "DIRECTORY /work/notes\nLAYER key\n",
        "DIRECTORY /work/notes\nLAYER key not-a-digest\n",
        "DIRECTORY /work/notes\nBOGUS word\n",
        "AGENT claude\n",
        "DIRECTORY /a\nDIRECTORY /b\n",
        "DIRECTORY relative\n",
        "DIRECTORY /work/notes\nENV \\x\n",
    ] {
        fs::write(&path, text).unwrap();
        let error = agents.get("notes").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData, "{text:?}");
        assert!(error.to_string().contains("notes"), "{error}");
        assert_eq!(agents.list().unwrap_err().kind(), ErrorKind::InvalidData);
    }
}

#[test]
fn recording_a_declaration_that_is_not_an_instruction_is_refused() {
    let root = store_root("bad-declaration");
    let agents = Agents::new(&root);
    for name in ["DIRECTORY", "LAYER", "share", "BOGUS", ""] {
        let mut recorded = agent("notes", "/work/notes", Vec::new());
        recorded.declared = vec![declared(name, &["x"])];
        let error = agents.record(&recorded, false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{name:?}");
    }
    assert_eq!(agents.get("notes").unwrap(), None);
}

#[test]
fn recording_an_agent_with_a_relative_directory_is_refused() {
    let root = store_root("relative");
    let agents = Agents::new(&root);
    for directory in ["", "work/notes", "./notes"] {
        let error = agents
            .record(&agent("notes", directory, Vec::new()), false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{directory:?}");
    }
}

#[test]
fn recording_a_layer_with_a_bad_digest_or_empty_key_is_refused() {
    let root = store_root("bad-layer");
    let agents = Agents::new(&root);
    let digest = Layers::new(&root).put(b"x").unwrap();
    for (key, digest) in [
        ("key", "abc".to_string()),
        ("key", digest.to_uppercase()),
        ("", digest.clone()),
    ] {
        let layer = Layer {
            key: key.to_string(),
            digest,
        };
        let error = agents
            .record(&agent("notes", "/work/notes", vec![layer]), false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}

#[test]
fn recording_a_layer_the_store_does_not_have_is_not_found() {
    let root = store_root("missing-layer");
    let agents = Agents::new(&root);
    let layer = Layer {
        key: "key".to_string(),
        digest: "0".repeat(64),
    };

    let error = agents
        .record(&agent("notes", "/work/notes", vec![layer]), false)
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert_eq!(agents.get("notes").unwrap(), None);
}

// spec(Build): The store remembers which directory each name was built from, and building a different directory under an existing name is an error.
#[test]
fn a_different_directory_under_an_existing_name_is_an_error() {
    let root = store_root("other-directory");
    let agents = Agents::new(&root);
    let first = agent("notes", "/work/notes", layers(&root, &["first"]));
    agents.record(&first, false).unwrap();

    let moved = agent("notes", "/work/moved", layers(&root, &["moved"]));
    let error = agents.record(&moved, false).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("/work/notes"), "{error}");
    assert_eq!(agents.get("notes").unwrap(), Some(first));
}

#[test]
fn rebuilding_the_same_directory_replaces_the_record() {
    let root = store_root("rebuild");
    let agents = Agents::new(&root);
    agents
        .record(&agent("notes", "/work/notes", layers(&root, &["a"])), false)
        .unwrap();

    let rebuilt = agent("notes", "/work/notes", layers(&root, &["b"]));
    agents.record(&rebuilt, false).unwrap();

    assert_eq!(agents.get("notes").unwrap(), Some(rebuilt));
}

#[test]
fn asking_to_replace_takes_over_the_name() {
    let root = store_root("replace");
    let agents = Agents::new(&root);
    agents
        .record(&agent("notes", "/work/notes", Vec::new()), false)
        .unwrap();

    let moved = agent("notes", "/work/moved", Vec::new());
    agents.record(&moved, true).unwrap();

    assert_eq!(agents.get("notes").unwrap(), Some(moved));
}

#[test]
fn agents_are_listed_by_name() {
    let root = store_root("list");
    let agents = Agents::new(&root);
    assert!(agents.list().unwrap().is_empty());
    for name in ["zeta", "alpha", "mid-1"] {
        agents
            .record(&agent(name, &format!("/work/{name}"), Vec::new()), false)
            .unwrap();
    }

    let names: Vec<String> = agents.list().unwrap().into_iter().map(|a| a.name).collect();

    assert_eq!(names, ["alpha", "mid-1", "zeta"]);
}

#[test]
fn removing_an_agent_frees_only_the_layers_no_other_agent_uses() {
    let root = store_root("remove");
    let agents = Agents::new(&root);
    let a = layers(&root, &["shared", "only a"]);
    let b = layers(&root, &["shared", "only b"]);
    agents
        .record(&agent("a", "/work/a", a.clone()), false)
        .unwrap();
    agents
        .record(&agent("b", "/work/b", b.clone()), false)
        .unwrap();

    assert!(agents.remove("a").unwrap());

    assert_eq!(agents.get("a").unwrap(), None);
    assert!(!has_layer(&root, &a[1].digest));
    assert!(has_layer(&root, &a[0].digest));
    assert!(has_layer(&root, &b[1].digest));

    assert!(agents.remove("b").unwrap());

    assert!(!has_layer(&root, &b[0].digest));
    assert!(!has_layer(&root, &b[1].digest));
    assert!(agents.list().unwrap().is_empty());
}

#[test]
fn removing_an_agent_leaves_layers_no_agent_recorded() {
    let root = store_root("unrecorded");
    let agents = Agents::new(&root);
    let recorded = layers(&root, &["recorded"]);
    // A build that has put its layer and not yet recorded its agent.
    let building = layers(&root, &["building"]);
    agents
        .record(&agent("a", "/work/a", recorded.clone()), false)
        .unwrap();

    agents.remove("a").unwrap();

    assert!(!has_layer(&root, &recorded[0].digest));
    assert!(has_layer(&root, &building[0].digest));
}

#[test]
fn removing_an_agent_that_is_not_there_does_nothing() {
    let root = store_root("remove-missing");
    let agents = Agents::new(&root);

    assert!(!agents.remove("notes").unwrap());
    assert_eq!(
        agents.remove("Not-A-Name").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn rebuilding_frees_the_layers_only_the_old_record_used() {
    let root = store_root("rebuild-frees");
    let agents = Agents::new(&root);
    let old = layers(&root, &["kept", "dropped", "shared"]);
    let other = layers(&root, &["shared"]);
    agents
        .record(&agent("other", "/work/other", other), false)
        .unwrap();
    agents
        .record(&agent("notes", "/work/notes", old.clone()), false)
        .unwrap();

    let new = layers(&root, &["kept", "added"]);
    agents
        .record(&agent("notes", "/work/notes", new.clone()), false)
        .unwrap();

    assert!(has_layer(&root, &old[0].digest));
    assert!(!has_layer(&root, &old[1].digest));
    assert!(has_layer(&root, &old[2].digest));
    assert!(has_layer(&root, &new[1].digest));
}

#[test]
fn a_layer_key_maps_to_the_digest_an_agent_recorded() {
    let root = store_root("key");
    let agents = Agents::new(&root);
    let recorded = layers(&root, &["one", "two"]);
    assert_eq!(agents.layer("key one").unwrap(), None);

    agents
        .record(&agent("notes", "/work/notes", recorded.clone()), false)
        .unwrap();

    assert_eq!(
        agents.layer("key one").unwrap(),
        Some(recorded[0].digest.clone())
    );
    assert_eq!(
        agents.layer("key two").unwrap(),
        Some(recorded[1].digest.clone())
    );
    assert_eq!(agents.layer("key three").unwrap(), None);

    agents.remove("notes").unwrap();

    assert_eq!(agents.layer("key one").unwrap(), None);
}

#[test]
fn an_unfinished_record_is_never_taken_for_an_agent() {
    let root = store_root("unfinished");
    let agents = Agents::new(&root);
    let dir = root.join("agents");
    fs::create_dir_all(&dir).unwrap();
    // Every name this process could pick next, as a writer with the same
    // process id would have left them when it was killed.
    let stale: Vec<PathBuf> = (0..1000)
        .map(|id| dir.join(format!(".notes.{}.{id}", std::process::id())))
        .collect();
    for path in &stale {
        fs::write(path, "DIRECTORY /wo").unwrap();
    }

    assert!(agents.list().unwrap().is_empty());
    let recorded = agent("notes", "/work/notes", Vec::new());
    agents.record(&recorded, false).unwrap();
    assert_eq!(agents.list().unwrap(), vec![recorded]);
    assert!(stale.iter().all(|path| path.exists()));
}

#[test]
fn a_reader_sees_a_whole_record_or_the_one_before_it() {
    let root = store_root("atomic");
    let versions: Vec<Agent> = ["a", "b"]
        .iter()
        .map(|fill| {
            let mut version = agent("notes", "/work/notes", Vec::new());
            // Long enough that a write in place would be seen half done.
            version.declared = (0..2000)
                .map(|i| declared("SHARE", &[&format!("./{fill}{i}"), "/work"]))
                .collect();
            version
        })
        .collect();
    Agents::new(&root).record(&versions[0], false).unwrap();
    let done = Arc::new(AtomicBool::new(false));

    let writer = {
        let (root, versions, done) = (root.clone(), versions.clone(), done.clone());
        thread::spawn(move || {
            let agents = Agents::new(root);
            for i in 0..200 {
                agents.record(&versions[(i + 1) % 2], false).unwrap();
            }
            done.store(true, Ordering::Relaxed);
        })
    };
    let agents = Agents::new(&root);
    let mut reads = 0;
    while !done.load(Ordering::Relaxed) {
        let read = agents.get("notes").unwrap().unwrap();
        assert!(versions.contains(&read), "a torn record");
        reads += 1;
    }
    writer.join().unwrap();
    assert!(reads > 0);
}

/// Puts the layers and records the agent, as a build would. A layer that
/// another writer freed after the put is not found, and the build puts it
/// again.
fn build(agents: &Agents, root: &Path, name: &str, contents: &[&str]) -> Agent {
    loop {
        let recorded = agent(name, &format!("/work/{name}"), layers(root, contents));
        match agents.record(&recorded, false) {
            Ok(()) => return recorded,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => panic!("{error}"),
        }
    }
}

#[test]
fn concurrent_builds_and_removals_never_record_a_freed_layer() {
    let root = store_root("concurrent");

    let writers: Vec<_> = (0..8)
        .map(|i| {
            let root = root.clone();
            thread::spawn(move || {
                let agents = Agents::new(&root);
                let name = format!("agent-{i}");
                for _ in 0..50 {
                    let built = build(&agents, &root, &name, &["shared", &name]);
                    // No other writer frees a layer this record names.
                    for _ in 0..5 {
                        for layer in &built.layers {
                            assert!(has_layer(&root, &layer.digest), "{name} lost a layer");
                        }
                        thread::yield_now();
                    }
                    assert!(agents.remove(&name).unwrap());
                }
                build(&agents, &root, &name, &["shared", &name]);
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }

    let agents = Agents::new(&root).list().unwrap();
    assert_eq!(agents.len(), 8);
    for agent in agents {
        for layer in agent.layers {
            assert!(
                has_layer(&root, &layer.digest),
                "{} lost a layer",
                agent.name
            );
        }
    }
}
