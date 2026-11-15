//! A static command for `floor_share_root`. It runs as root in a real guest,
//! under `ap`'s own init, mounts the first share, and prints one line for
//! each thing it tries, so that the host can check every result. It ends
//! with every file it can read under the share, so that the host can search
//! them for what must not be there. The guest never learns what that is.

use std::fs;
use std::io;
use std::path::Path;

const SHARE: &str = "/share";

/// A result as one word: the contents, or the error's number.
fn shown(result: io::Result<Vec<u8>>) -> String {
    match result {
        Ok(bytes) => format!("ok {}", String::from_utf8_lossy(&bytes)),
        Err(error) => format!("error {}", error.raw_os_error().unwrap_or(-1)),
    }
}

fn read(path: &str) -> String {
    shown(fs::read(Path::new(SHARE).join(path)))
}

fn link(path: &str) -> String {
    let target = fs::read_link(Path::new(SHARE).join(path));
    shown(target.map(|target| target.into_os_string().into_encoded_bytes()))
}

fn done(result: io::Result<()>) -> String {
    shown(result.map(|()| Vec::new()))
}

/// Every file under `directory`, without following a symlink, with what it
/// holds.
fn walk(directory: &Path, out: &mut String) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&path, out),
            Ok(kind) if kind.is_file() => {
                let contents = shown(fs::read(&path));
                out.push_str(&format!("walk {}: {contents}\n", path.display()));
            }
            _ => out.push_str(&format!("walk {}: other\n", path.display())),
        }
    }
}

fn main() {
    let mounted = guest::linux::mount_share(&guest::share::tag(0), Path::new(SHARE));
    let mut out = format!("mount: {}\n", done(mounted));
    let share = Path::new(SHARE);
    for (name, result) in [
        ("hello.txt", read("hello.txt")),
        ("dir/nested.txt", read("dir/nested.txt")),
        ("readlink link", link("link")),
        ("via link", read("link")),
        ("readlink escape", link("escape")),
        ("via escape", read("escape")),
        ("via absolute", read("absolute")),
        ("via dirlink", read("dirlink/secret")),
        ("via dotdot", read("../outside/secret")),
        ("create", done(fs::write(share.join("new.txt"), b"x"))),
        ("change", done(fs::write(share.join("hello.txt"), b"x"))),
        ("mkdir", done(fs::create_dir(share.join("new")))),
        ("remove", done(fs::remove_file(share.join("hello.txt")))),
        (
            "rename",
            done(fs::rename(share.join("link"), share.join("moved"))),
        ),
    ] {
        out.push_str(&format!("{name}: {result}\n"));
    }
    walk(share, &mut out);
    print!("{out}");
}
