//! A stand-in for a coding agent, which T1 checks run in the guest. It prints
//! what it can see as one line of JSON, and exits.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;

fn main() {
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
    println!(
        "{{\"arguments\":{},\"environment\":{{{}}},\"files\":{},\"network\":{{\"interfaces\":{},\"resolvers\":{}}}}}",
        array(&arguments),
        environment.join(","),
        array(&files),
        array(&interfaces()),
        array(&resolvers()),
    );
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
