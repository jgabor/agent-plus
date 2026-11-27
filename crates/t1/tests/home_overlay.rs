//! The guest's overlayfs over the image's `/home/agent`, with its upper
//! directory on a share of kept state, which `PERSIST home` builds on. A
//! static `ap` builds an agent of the fake harness whose image holds files
//! in `/home/agent`. Machines boot it in turn, each with a share server for
//! the same host directory, and the home probe mounts the overlay before it
//! runs a script. One changes the home, and the next finds each change. The
//! guest runs through the API `ap boot` uses.
//!
//! It reads the host's extended attributes as Linux names them, so it
//! builds on Linux alone.

#![cfg(target_os = "linux")]

use ap::boot::{self, Guest, Layer, Report, Shares};
use ap::fetch;
use ap::store::Store;
use guest::protocol::ToGuest;
use share::{Mode, Root};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::Duration;
use t1::{guest_binary, kernel, os};

/// How long one machine may run before the VMM stops it.
const TIMEOUT: Duration = Duration::from_secs(60);
const AGENT: &str = "home";
const LEVEL: i8 = 3;

/// Changes the home: a file it creates, a file of the image it changes,
/// one it deletes, and a directory of the image it replaces.
const CHANGE: &str = "set -e
cd /home/agent/seed
echo made > created.txt
echo changed > changed.txt
rm deleted.txt
rm -r replaced
mkdir replaced
echo new > replaced/new.txt
";

/// Prints what the home holds, one `name=value` line each.
const LOOK: &str = "cd /home/agent/seed
for name in created.txt changed.txt kept.txt replaced/new.txt; do
  echo \"$name=$(cat $name)\"
done
for name in deleted.txt replaced/old.txt; do
  [ -e $name ] && echo \"$name=there\" || echo \"$name=gone\"
done
echo \"replaced=$(ls -A replaced)\"
";

/// A home of its own, with the kernel and the OS layer in its store and the
/// fake harness in its download cache, so that `ap` reads none of the
/// owner's files and fetches nothing. It holds the agent's build context,
/// and the directory the share keeps the home's upper layer in.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("home-overlay-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Build contexts are canonical, so the test's paths are too.
        let root = root.canonicalize().unwrap();
        let home = root.join("home");
        let data = home.join(".local/share/ap");
        Store::new(&data)
            .put(
                fetch::kernel().sha256,
                File::open(kernel::vmlinux()).unwrap(),
            )
            .unwrap();
        os::put(&data);
        fs::create_dir_all(root.join("state")).unwrap();
        Scratch { root, home }
    }

    fn data(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    /// Builds the agent, whose image holds `seed` in `/home/agent`, and
    /// returns its layers, with one more on top that holds the probe.
    fn build(&self) -> Vec<String> {
        let fake = guest_binary::fake_harness();
        let digest = sha256(&fake);
        let cache = self.home.join(".cache/ap");
        fs::create_dir_all(&cache).unwrap();
        fs::copy(&fake, cache.join(&digest)).unwrap();
        let dir = self.root.join(AGENT);
        fs::create_dir_all(dir.join("seed/replaced")).unwrap();
        for name in ["changed.txt", "deleted.txt", "kept.txt", "replaced/old.txt"] {
            fs::write(
                dir.join("seed").join(name),
                format!("{name} from the image"),
            )
            .unwrap();
        }
        let agentfile = "AGENT claude:0.0.0\nOS minimal\nNETWORK -\nADD ./seed /home/agent/seed\n";
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        let lock = format!("HARNESS claude 0.0.0 {digest}\n");
        fs::write(dir.join("Agentfile.lock"), lock).unwrap();
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let built = Command::new(guest_binary::ap())
            .arg("build")
            .arg(format!("--approve={}", approved.display()))
            .arg(&dir)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(built.status.code(), Some(0), "{built:?}");
        let agent = store::Agents::new(self.data()).get(AGENT).unwrap().unwrap();
        let mut layers: Vec<String> = agent.layers.into_iter().map(|layer| layer.digest).collect();
        let probe = fs::read(guest_binary::build("home-probe")).unwrap();
        let top = erofs::Dir {
            mode: 0o755,
            entries: BTreeMap::from([(
                b"home-probe".to_vec(),
                erofs::Node::File(erofs::File {
                    mode: 0o755,
                    size: probe.len() as u64,
                    content: Box::new(std::io::Cursor::new(probe)),
                }),
            )]),
        };
        let mut image = Vec::new();
        erofs::write(top, &mut image).unwrap();
        layers.push(store::Layers::new(self.data()).put(&image, LEVEL).unwrap());
        layers
    }

    /// Boots the agent's layers with the state directory shared, runs
    /// `script` in the probe's home, and returns what it printed, once the
    /// share server has ended too.
    fn boot(&self, layers: &[String], script: &str) -> String {
        let stored = store::Layers::new(self.data());
        let layers = layers
            .iter()
            .map(|digest| Layer {
                digest: digest.clone(),
                disk: Box::new(stored.open(digest).unwrap()),
            })
            .collect();
        let ap = guest_binary::ap();
        let state = File::open(self.root.join("state")).unwrap();
        let servers = share::start(&ap, &[Root::new(state.as_fd(), Mode::State)], &[], None);
        let servers = servers.unwrap();
        drop(state);
        let kernel = fs::read(kernel::vmlinux()).unwrap();
        let init = fs::read(&ap).unwrap();
        let argv = ["/home-probe", "/bin/sh", "-c", script];
        let guest = Guest {
            kernel: (&kernel).into(),
            init: (&init).into(),
            files: &[],
            layers,
            request: Some(ToGuest::Run {
                argv: argv.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
            }),
            timeout: TIMEOUT,
            confine: false,
            input: None,
        };
        let (processes, sockets) = (servers.started.into_iter())
            .map(|server| (server.process, server.socket))
            .unzip::<_, _, Vec<_>, Vec<_>>();
        let shares = Shares {
            sockets,
            serving: servers.serving,
        };
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let status = boot::boot_sharing(guest, Some(shares), |report| match report {
            Report::Stdout(bytes) => stdout.extend_from_slice(bytes),
            Report::Stderr(bytes) => stderr.extend_from_slice(bytes),
            Report::Ready(_) | Report::Exec(..) | Report::ExecTook(_) => {}
        });
        // The machine's end of each socket is closed, so its server is done
        // with the state, and ends.
        for mut process in processes {
            assert!(process.wait().unwrap().success());
        }
        let stdout = String::from_utf8(stdout).unwrap();
        let stderr = String::from_utf8_lossy(&stderr);
        assert_eq!(
            status.unwrap(),
            Some(0),
            "stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        stdout
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum failed on {path:?}");
    String::from_utf8(output.stdout).unwrap()[..64].to_owned()
}

/// The names of the extended attributes of the file at `path`, which this
/// doesn't follow if it is a symlink.
fn xattrs(path: &Path) -> Vec<String> {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut names = vec![0u8; 64 << 10];
    // SAFETY: `path` is NUL-terminated, and the buffer is as long as the
    // call is told it is.
    let len = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    assert!(len >= 0, "{}", std::io::Error::last_os_error());
    names.truncate(len as usize);
    (names.split(|&byte| byte == 0))
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// Every file beneath `dir`, without following a symlink.
fn walk(dir: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            walk(&path, out);
        }
        out.push((path, metadata));
    }
}

#[test]
fn the_home_overlay_keeps_its_upper_layer_on_a_share_of_kept_state() {
    let scratch = Scratch::new();
    let layers = scratch.build();
    // Before anything changes, the home is the image's.
    let image = scratch.boot(&layers, LOOK);
    assert!(
        image.contains("changed.txt=changed.txt from the image\n"),
        "{image}"
    );
    assert!(image.contains("deleted.txt=there\n"), "{image}");

    scratch.boot(&layers, CHANGE);
    let looked = scratch.boot(&layers, LOOK);
    assert_eq!(
        looked,
        "created.txt=made
changed.txt=changed
kept.txt=kept.txt from the image
replaced/new.txt=new
deleted.txt=gone
replaced/old.txt=gone
replaced=new.txt
"
    );

    // On the host, the state holds nothing but regular files and
    // directories, and none of overlayfs's attributes.
    let state = scratch.root.join("state");
    let mut found = Vec::new();
    walk(&state, &mut found);
    for (path, metadata) in &found {
        let kind = metadata.file_type();
        assert!(kind.is_file() || kind.is_dir(), "{path:?} is {kind:?}");
        for name in xattrs(path) {
            let overlay = name.starts_with("trusted.") || name.starts_with("user.overlay.");
            assert!(!overlay, "{path:?} has {name}");
        }
    }
    // A whiteout, an opaque directory, and overlayfs's attributes are
    // records.
    let records = fs::read_to_string(state.join("records")).unwrap();
    for line in [
        "CHAR \"upper/seed/deleted.txt\" 0 0 0\n",
        "XATTR \"upper/seed/replaced\" \"trusted.overlay.opaque\" \"y\"\n",
    ] {
        assert!(records.contains(line), "no {line:?} in\n{records}");
    }
    let whiteout = fs::symlink_metadata(state.join("tree/upper/seed/deleted.txt")).unwrap();
    assert!(whiteout.is_file() && whiteout.len() == 0, "{whiteout:?}");
}
