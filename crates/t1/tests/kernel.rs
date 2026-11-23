//! Fetches the pinned kernel the way t1's tests do, into an empty directory.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;

#[test]
fn two_fetches_in_one_process_both_return_the_pinned_kernel() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kernel-fetch");
    let _ = fs::remove_dir_all(&directory);

    let paths = thread::scope(|scope| {
        let fetches = [(); 2].map(|()| scope.spawn(|| t1::kernel::vmlinux_in(&directory)));
        fetches.map(|fetch| fetch.join().unwrap())
    });

    let digest = include_str!("../../../guest/kernel/pin.toml")
        .lines()
        .find_map(|line| line.strip_prefix("sha256 = "))
        .unwrap()
        .trim_matches('"');
    for path in paths {
        let output = Command::new("sha256sum").arg(&path).output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.starts_with(digest.as_bytes()));
    }
    fs::remove_dir_all(&directory).unwrap();
}
