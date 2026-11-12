//! A stand-in for a coding agent, which T1 checks and the journey run in the
//! guest. It prints what it can see as one line of JSON, and exits.
//!
//! Two variables steer it, so an Agentfile's `ENV` lines can. After it
//! reports, it creates the file that `FAKE_HARNESS_WRITE` names, and it exits
//! with the status in `FAKE_HARNESS_STATUS`, or 0.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let environment: BTreeMap<String, String> = env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let environment: Vec<String> = environment
        .iter()
        .map(|(key, value)| format!("{}:{}", quote(key), quote(value)))
        .collect();
    let mut files = Vec::new();
    list_files(Path::new("."), "", &mut files);
    files.sort();
    let directory = env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!(
        "{{\"arguments\":{},\"directory\":{},\"environment\":{{{}}},\"files\":{},\"network\":{{\"interfaces\":{},\"resolvers\":{}}},\"user\":{}}}",
        array(&arguments),
        quote(&directory),
        environment.join(","),
        array(&files),
        array(&interfaces()),
        array(&resolvers()),
        quote(&user()),
    );
    if let Some(path) = env::var_os("FAKE_HARNESS_WRITE")
        && let Err(e) = fs::write(&path, "")
    {
        eprintln!(
            "fake-harness: couldn't write {}: {e}",
            path.to_string_lossy()
        );
        return ExitCode::FAILURE;
    }
    match env::var("FAKE_HARNESS_STATUS") {
        Err(_) => ExitCode::SUCCESS,
        Ok(status) => match status.parse::<u8>() {
            Ok(status) => ExitCode::from(status),
            Err(_) => {
                eprintln!("fake-harness: FAKE_HARNESS_STATUS isn't 0 to 255: {status}");
                ExitCode::FAILURE
            }
        },
    }
}

/// The name `/etc/passwd` gives this process's effective user, or its uid if
/// it has none there.
fn user() -> String {
    let uid = fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1).map(str::to_string))
        .unwrap_or_default();
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            (fields.nth(1)? == uid).then(|| name.to_string())
        })
        .unwrap_or(uid)
}

/// Every file below `directory`, without following symlinks. What can't be
/// read is left out, because the view from inside the guest is the point.
fn list_files(directory: &Path, prefix: &str, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = format!("{prefix}{}", entry.file_name().to_string_lossy());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                list_files(&entry.path(), &format!("{path}/"), files);
            }
            Ok(_) => files.push(path),
            Err(_) => {}
        }
    }
}

fn interfaces() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn resolvers() -> Vec<String> {
    fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .map(|address| address.trim().to_string())
        .collect()
}

fn array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| quote(item)).collect();
    format!("[{}]", quoted.join(","))
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
