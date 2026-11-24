//! A static program for T1, kept in an agent's top layer: it writes to `/tmp`,
//! then to the root, then to `/tmp` again, until each is full or holds what
//! it was asked to, and prints, one `name=value` line each, how many MiB each
//! took, what the write that stopped it saw, and how `/tmp` is mounted.

use std::fs::{self, File};
use std::io::Write;

const MIB: usize = 1 << 20;
/// What the probe writes to `/tmp`, in MiB, before it turns to the root.
const IN_TMP: usize = 192;
/// The most the probe writes to the root, in MiB.
const IN_ROOT: usize = 128;

/// Writes up to `limit` MiB to `path`, and returns how many whole MiB it
/// wrote, and the error that stopped it, if one did.
fn fill(path: &str, limit: usize) -> (usize, String) {
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(error) => return (0, error.to_string()),
    };
    let chunk = vec![0x5a; MIB];
    for written in 0..limit {
        if let Err(error) = file.write_all(&chunk) {
            return (written, error.to_string());
        }
    }
    (limit, "none".to_owned())
}

fn main() {
    let (tmp, tmp_error) = fill("/tmp/fill", IN_TMP);
    let (root, root_error) = fill("/fill", IN_ROOT);
    let (after, after_error) = fill("/tmp/after", 1);
    println!("tmp={tmp}");
    println!("tmp_error={tmp_error}");
    println!("root={root}");
    println!("root_error={root_error}");
    println!("after={after}");
    println!("after_error={after_error}");
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let line = mounts
        .lines()
        .find(|line| line.split(' ').nth(1) == Some("/tmp"))
        .unwrap_or_default();
    println!("tmp_options={}", line.split(' ').nth(3).unwrap_or_default());
    let mode = fs::metadata("/tmp").map(|metadata| {
        use std::os::unix::fs::PermissionsExt;
        format!("{:o}", metadata.permissions().mode() & 0o7777)
    });
    println!(
        "tmp_mode={}",
        mode.unwrap_or_else(|error| error.to_string())
    );
}
