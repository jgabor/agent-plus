//! The boot API that `ap boot` uses, on a real guest. The journey covers `ap`
//! itself as the init, because that needs its release build.

use ap::boot::{self, Error, Guest};
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
fn boot_silent_init(timeout: Duration) -> (Result<(), Error>, u32) {
    let init = fs::read(guest_binary::build("test-init")).unwrap();
    let guest = Guest {
        kernel: kernel_image(),
        init: &init,
        files: &[],
        timeout,
    };
    let mut ready_calls = 0;
    let result = boot::boot(&guest, |_| ready_calls += 1);
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
