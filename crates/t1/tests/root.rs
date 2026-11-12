//! An agent's layers, stacked under overlayfs as the guest's root.

use ap::boot::{self, Guest, Layer, Report};
use erofs::{Dir, File, Node};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process;
use std::time::Duration;
use store::Layers;
use t1::{guest_binary, kernel};

const TIMEOUT: Duration = Duration::from_secs(30);
const LEVEL: i8 = 3;
const PROBE: &[&[u8]] = &[b"/bin/probe"];

fn file(mode: u16, content: Vec<u8>) -> Node {
    Node::File(File {
        mode,
        size: content.len() as u64,
        content: Box::new(std::io::Cursor::new(content)),
    })
}

fn dir(entries: Vec<(&str, Node)>) -> Dir {
    Dir {
        mode: 0o755,
        entries: entries
            .into_iter()
            .map(|(name, node)| (name.as_bytes().to_vec(), node))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn image(root: Dir) -> Vec<u8> {
    let mut image = Vec::new();
    erofs::write(root, &mut image).unwrap();
    image
}

/// The bottom layer has two files in `/etc`. The top one replaces one of them,
/// and adds the program.
fn layers(probe: Vec<u8>) -> [Vec<u8>; 2] {
    let bottom = dir(vec![(
        "etc",
        Node::Dir(dir(vec![
            ("greeting", file(0o644, b"from the bottom layer".to_vec())),
            ("kept", file(0o644, b"kept from the bottom layer".to_vec())),
        ])),
    )]);
    let top = dir(vec![
        ("bin", Node::Dir(dir(vec![("probe", file(0o755, probe))]))),
        (
            "etc",
            Node::Dir(dir(vec![(
                "greeting",
                file(0o644, b"from the top layer".to_vec()),
            )])),
        ),
    ]);
    [image(bottom), image(top)]
}

/// Boots `ap` as the init, with these layers, runs the probe, and returns what
/// it printed, by name.
fn boot_with(init: &[u8], kernel: &[u8], layers: Vec<Layer>) -> BTreeMap<String, String> {
    let guest = Guest {
        kernel,
        init,
        files: &[],
        layers,
        command: Some(&PROBE.iter().map(|arg| arg.to_vec()).collect::<Vec<_>>()),
        timeout: TIMEOUT,
        confine: false,
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = boot::boot(guest, |report| match report {
        Report::Ready(_) => {}
        Report::Stdout(bytes) => stdout.extend_from_slice(bytes),
        Report::Stderr(bytes) => stderr.extend_from_slice(bytes),
    });
    let stdout = String::from_utf8(stdout).unwrap();
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(
        status.unwrap(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn the_guest_runs_from_its_layers_under_a_tmpfs_upper_layer() {
    let store = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("root-{}", process::id()));
    let _ = fs::remove_dir_all(&store);
    let probe = fs::read(guest_binary::build("root-probe")).unwrap();
    let stored = Layers::new(&store);
    let digests: Vec<String> = layers(probe)
        .iter()
        .map(|layer| stored.put(layer, LEVEL).unwrap())
        .collect();
    let read = || -> Vec<Layer> {
        digests
            .iter()
            .map(|digest| Layer {
                digest: digest.clone(),
                disk: stored.get(digest).unwrap(),
            })
            .collect()
    };
    let (first, second) = (read(), read());
    fs::remove_dir_all(&store).unwrap();

    let kernel = fs::read(kernel::vmlinux()).unwrap();
    let init = fs::read(guest_binary::ap()).unwrap();

    let seen = boot_with(&init, &kernel, first);
    assert_eq!(seen["greeting"], "from the top layer", "{seen:?}");
    assert_eq!(seen["kept"], "kept from the bottom layer", "{seen:?}");
    let root: Vec<&str> = seen["root"].split(' ').collect();
    assert_eq!(root.get(2), Some(&"overlay"), "{seen:?}");
    assert_eq!(seen["exe"], "/bin/probe", "{seen:?}");
    assert_eq!(seen["found"], "false", "{seen:?}");
    assert_eq!(seen["wrote"], "true", "{seen:?}");

    let again = boot_with(&init, &kernel, second);
    assert_eq!(
        again["found"], "false",
        "a write outlived its VM: {again:?}"
    );
}
