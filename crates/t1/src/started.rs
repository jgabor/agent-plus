//! A guard for a process a test starts, so a failed test leaves no `ap run`
//! or VM behind.

use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

/// How long a process has to exit after `SIGTERM` before it gets `SIGKILL`.
/// `timeout -k 10` sends its command `SIGKILL` 10 seconds after passing
/// `SIGTERM` on, and exits soon after.
const GRACE: Duration = Duration::from_secs(15);

/// A process a test started, stopped and reaped when it's dropped.
///
/// The guard sends `SIGTERM`, not `SIGKILL`, because `timeout` passes
/// `SIGTERM` on to its command but can't pass on `SIGKILL`. It then waits
/// for the process to exit, so an `ap run` under `timeout` has exited by
/// the time the guard is gone. A process that hasn't exited within
/// [`GRACE`] gets `SIGKILL`.
pub struct Started(pub Child);

impl Drop for Started {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            // The process is unreaped until the wait below, so its id is
            // still its own.
            let _ = guest::linux::terminate(self.0.id());
            let deadline = Instant::now() + GRACE;
            while let Ok(None) = self.0.try_wait() {
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
