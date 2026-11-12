//! A static program for T1, kept in an agent's top layer: it prints what it
//! sees of the guest's root, one `name=value` line each, and leaves a file
//! behind for the next boot to look for.

use std::fs;

const LEFT_BEHIND: &str = "/left-behind";

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("unreadable: {error}"))
}

fn main() {
    let root = read("/proc/mounts")
        .lines()
        .find(|line| line.split(' ').nth(1) == Some("/"))
        .unwrap_or_default()
        .to_owned();
    let exe = fs::read_link("/proc/self/exe")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unreadable: {error}"));
    let found = fs::exists(LEFT_BEHIND).unwrap_or(false);
    let wrote = fs::write(LEFT_BEHIND, "from an earlier boot").is_ok();
    println!("greeting={}", read("/etc/greeting"));
    println!("kept={}", read("/etc/kept"));
    println!("root={root}");
    println!("exe={exe}");
    println!("found={found}");
    println!("wrote={wrote}");
}
