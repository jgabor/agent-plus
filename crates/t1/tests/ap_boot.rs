//! The boot API that `ap boot` uses, on a real guest. `ap_boot_command` runs
//! `ap` itself as the init.

use ap::boot::{self, Error, Guest, Report};
use guest::protocol::ToGuest;
use std::fs;
use std::sync::OnceLock;
use std::time::Duration;
use t1::{guest_binary, kernel};

/// Loaded once, because the helper's partial file is named per process and
/// these tests share one.
fn kernel_image() -> &'static [u8] {
    static IMAGE: OnceLock<Vec<u8>> = OnceLock::new();
    IMAGE.get_or_init(|| fs::read(kernel::vmlinux()).unwrap())
}

/// Boots an init that powers off without saying anything to the host.
fn boot_silent_init(timeout: Duration) -> (Result<Option<u8>, Error>, u32) {
    let init = fs::read(guest_binary::build("test-init")).unwrap();
    let guest = Guest {
        kernel: kernel_image(),
        init: &init,
        files: &[],
        layers: Vec::new(),
        request: None,
        timeout,
        confine: false,
    };
    let mut ready_calls = 0;
    let result = boot::boot(guest, |report| {
        if matches!(report, Report::Ready(_)) {
            ready_calls += 1;
        }
    });
    (result, ready_calls)
}

#[test]
fn a_guest_that_powers_off_without_reporting_ready_is_a_failure() {
    let (result, ready_calls) = boot_silent_init(Duration::from_secs(30));
    let error = result.unwrap_err();
    assert_eq!(error.exit_code(), 125, "{error}");
    assert!(error.to_string().contains("ready"), "{error}");
    assert_eq!(ready_calls, 0);
}

#[test]
fn a_guest_past_its_timeout_is_stopped_and_reported() {
    let (result, ready_calls) = boot_silent_init(Duration::from_millis(1));
    assert!(matches!(result, Err(Error::TimedOut)), "{result:?}");
    assert_eq!(ready_calls, 0);
}

#[test]
fn a_commands_streams_arrive_whole_and_separate_with_its_status() {
    let init = fs::read(guest_binary::build("output-init")).unwrap();
    let guest = Guest {
        kernel: kernel_image(),
        init: &init,
        files: &[],
        layers: Vec::new(),
        request: Some(ToGuest::Run {
            argv: vec![b"anything".to_vec()],
        }),
        timeout: Duration::from_secs(30),
        confine: false,
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = boot::boot(guest, |report| match report {
        Report::Ready(_) => {}
        Report::Stdout(bytes) => stdout.extend_from_slice(bytes),
        Report::Stderr(bytes) => stderr.extend_from_slice(bytes),
    })
    .unwrap();
    assert_eq!(status, Some(7));
    assert_eq!(stdout.len(), 4 << 20);
    assert!(stdout.iter().all(|byte| *byte == b'o'));
    assert_eq!(stderr, b"to stderr");
}
