//! The arguments and refusals of `ap boot`. Nothing here boots, because a test
//! binary is GNU-linked. The boots themselves are in the package `t1`.

use ap::boot;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

/// `HOME` points inside the build directory, so that nothing can touch the
/// owner's store.
fn ap(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ap"))
        .args(args)
        .env_clear()
        .env("HOME", Path::new(env!("CARGO_TARGET_TMPDIR")))
        .output()
        .unwrap()
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    for args in [
        &["boot", "--timeout"][..],
        &["boot", "--timeout", "soon"],
        &["boot", "--timeout", "-1"],
        &["boot", "--timeout", "1", "--timeout", "2"],
        &["boot", "--nope"],
        &["boot", "extra"],
    ] {
        let output = ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage"),
            "{args:?}"
        );
    }
}

#[test]
fn a_dynamically_linked_ap_refuses_to_boot_and_exits_125() {
    for args in [&["boot"][..], &["boot", "--timeout", "30"]] {
        let output = ap(args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("dynamically linked"), "{stderr}");
    }
}

#[test]
fn only_pid_1_called_slash_init_is_the_init() {
    let init = OsStr::new("/init");
    assert!(boot::is_init(1, Some(init)));
    assert!(!boot::is_init(2, Some(init)));
    assert!(!boot::is_init(1, Some(OsStr::new("ap"))));
    assert!(!boot::is_init(1, Some(OsStr::new("/usr/bin/ap"))));
    assert!(!boot::is_init(1, None));
}

/// An ELF file holding these program header types.
fn elf(kinds: &[u32]) -> Vec<u8> {
    const HEADERS_AT: usize = 64;
    const HEADER_SIZE: usize = 56;
    let mut file = vec![0; HEADERS_AT + kinds.len() * HEADER_SIZE];
    file[..4].copy_from_slice(b"\x7fELF");
    file[32..40].copy_from_slice(&(HEADERS_AT as u64).to_le_bytes());
    file[54..56].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    file[56..58].copy_from_slice(&(kinds.len() as u16).to_le_bytes());
    for (index, kind) in kinds.iter().enumerate() {
        let at = HEADERS_AT + index * HEADER_SIZE;
        file[at..at + 4].copy_from_slice(&kind.to_le_bytes());
    }
    file
}

#[test]
fn a_program_is_static_unless_it_names_a_loader() {
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    assert!(boot::is_static(&elf(&[PT_LOAD, PT_LOAD])));
    assert!(!boot::is_static(&elf(&[PT_LOAD, PT_INTERP])));
}

#[test]
fn the_gnu_linked_test_build_is_not_static() {
    let file = std::fs::read(env!("CARGO_BIN_EXE_ap")).unwrap();
    assert!(!boot::is_static(&file));
}

#[test]
fn what_cannot_be_read_as_elf_is_not_static() {
    assert!(!boot::is_static(b""));
    assert!(!boot::is_static(b"#!/bin/sh\n"));
    let mut cut_short = elf(&[1]);
    cut_short.truncate(70);
    assert!(!boot::is_static(&cut_short));
    let mut far_away = elf(&[1]);
    far_away[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(!boot::is_static(&far_away));
}

#[test]
fn a_kvm_device_that_cannot_be_opened_is_refused_with_status_125() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-kvm");
    let error = boot::check_kvm(&missing).unwrap_err();
    assert_eq!(error.exit_code(), 125);
    assert!(error.to_string().contains("/dev/kvm"), "{error}");
}

#[test]
fn a_timeout_exits_124_and_nothing_else_does() {
    assert_eq!(boot::Error::TimedOut.exit_code(), 124);
    assert_eq!(boot::Error::DynamicallyLinked.exit_code(), 125);
    assert_eq!(boot::Error::Failed(String::new()).exit_code(), 125);
}

#[test]
fn a_command_needs_something_after_the_separator() {
    for args in [&["boot", "--"][..], &["boot", "--timeout", "1", "--"]] {
        let output = ap(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn a_command_is_accepted_with_or_without_a_timeout() {
    for args in [
        &["boot", "--", "ap", "--version"][..],
        &["boot", "--timeout", "30", "--", "ap", "--timeout"],
    ] {
        let output = ap(args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("dynamically linked"), "{stderr}");
    }
}
