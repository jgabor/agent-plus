//! `ap fmt [--check] [dir]`, from SPEC.md's `ap fmt` section. The rules are
//! [`agentfile::format`]'s. This reads the file, and replaces it.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The arguments of `ap fmt`.
pub struct Args {
    check: bool,
    /// The directory, or `None` for the current one.
    dir: Option<PathBuf>,
}

/// Reads `ap fmt`'s arguments: `--check` and a directory, each at most once,
/// in any order. Any other flag is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut check = false;
    let mut dir = None;
    for arg in args {
        if arg == "--check" && !check {
            check = true;
        } else if arg.as_encoded_bytes().starts_with(b"-") || dir.is_some() {
            return None;
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    Some(Args { check, dir })
}

/// Formats the Agentfile, and returns the exit status. Messages go to
/// standard error.
pub fn run(args: &Args) -> u8 {
    match format(args) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn format(args: &Args) -> Result<(), String> {
    let path = match &args.dir {
        Some(dir) => {
            if !fs::metadata(dir).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(format!("ap: {} isn't a directory", dir.display()));
            }
            dir.join("Agentfile")
        }
        None => PathBuf::from("Agentfile"),
    };
    let shown = path.display();
    let bytes = fs::read(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => match &args.dir {
            Some(dir) => format!("ap: no Agentfile in {}", dir.display()),
            None => "ap: no Agentfile in the current directory".to_string(),
        },
        _ => format!("ap: {shown}: {error}"),
    })?;
    let source = String::from_utf8(bytes).map_err(|_| format!("ap: {shown} isn't UTF-8 text"))?;
    let formatted = agentfile::format(&source).map_err(|errors| {
        let lines: Vec<String> = errors
            .iter()
            .map(|error| format!("{shown}:{}: {}", error.line, error.message))
            .collect();
        lines.join("\n")
    })?;
    if formatted == source {
        return Ok(());
    }
    if args.check {
        return Err(format!("ap: {shown} isn't formatted"));
    }
    replace(&path, formatted.as_bytes()).map_err(|error| format!("ap: {shown}: {error}"))
}

/// Replaces a file with new contents in one rename, so a reader sees the old
/// file or the new one, and never part of either. The new file keeps the old
/// one's mode. A symlink stays, and its target is replaced.
fn replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path)?;
    let permissions = fs::metadata(&target)?.permissions();
    let mut name = OsString::from(".");
    name.push(target.file_name().unwrap_or_default());
    name.push(format!(".ap-fmt-{}", std::process::id()));
    let temporary = target.with_file_name(name);
    let written = (|| {
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.set_permissions(permissions)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}
