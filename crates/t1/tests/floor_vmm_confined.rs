//! The floor: the process that runs a guest is confined. Code that gets
//! running in it once the guest has started can't start a process, run a
//! program, or open a host file.
//!
//! Each attempt runs in its own process, which boots a guest through the API
//! `ap boot` uses. The attempt runs in the callback for the guest's first
//! report, on the vCPU thread, which is where code a guest got running in the
//! VMM would be. Each attempt is one system call, which returns whether it
//! works or fails, so only a process killed at that call passes. A second
//! check watches a real `ap boot` while its guest runs, and finds it under a
//! filter of its own.

use ap::boot::{self, Guest, Report};
use ap::fetch;
use ap::store::Store;
use std::env;
use std::ffi::c_int;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use t1::{guest_binary, kernel};

const OWNER_TEST: &str = "confined_owner_process";
const ATTEMPT_VARIABLE: &str = "AP_FLOOR_ATTEMPT";
const CONFINE_VARIABLE: &str = "AP_FLOOR_CONFINE";
const LOG_VARIABLE: &str = "AP_FLOOR_LOG";
const KERNEL_VARIABLE: &str = "AP_FLOOR_KERNEL";
const INIT_VARIABLE: &str = "AP_FLOOR_INIT";
const TARGET_VARIABLE: &str = "AP_FLOOR_TARGET";
const TIMEOUT: Duration = Duration::from_secs(30);
const SIGSYS: i32 = 31;
const TARGET_CONTENTS: &str = "a host file the VMM doesn't need";

unsafe extern "C" {
    fn fork() -> c_int;
    fn _exit(status: c_int) -> !;
}

/// What the confined process tries, once its guest is running.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Attempt {
    /// Proves that a confined process still boots its guest.
    Nothing,
    Fork,
    Exec,
    Open,
}

const ATTEMPTS: [Attempt; 4] = [
    Attempt::Nothing,
    Attempt::Fork,
    Attempt::Exec,
    Attempt::Open,
];

impl Attempt {
    fn name(self) -> &'static str {
        match self {
            Attempt::Nothing => "nothing",
            Attempt::Fork => "fork",
            Attempt::Exec => "exec",
            Attempt::Open => "open",
        }
    }

    fn from_name(name: &str) -> Self {
        ATTEMPTS
            .into_iter()
            .find(|attempt| attempt.name() == name)
            .unwrap()
    }

    /// Makes the attempt, and says how the call came back. A process the
    /// filter kills never says anything.
    fn make(self, target: &Path) -> String {
        match self {
            Attempt::Nothing => "did nothing".to_owned(),
            Attempt::Fork => {
                // SAFETY: the child only calls `_exit`, which is safe after a
                // fork from a process with threads.
                match unsafe { fork() } {
                    -1 => format!("went through: {}", std::io::Error::last_os_error()),
                    0 => unsafe { _exit(0) },
                    child => format!("went through: started process {child}"),
                }
            }
            // The target isn't executable, so an exec the kernel runs comes
            // back with an error, rather than becoming a program that could
            // die on a call of its own.
            Attempt::Exec => format!("went through: {}", Command::new(target).exec()),
            Attempt::Open => format!("went through: {:?}", File::open(target).map(drop)),
        }
    }
}

fn kernel_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(kernel::vmlinux)
}

fn ap_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(guest_binary::ap)
}

/// Where one check keeps its files, named per process, because another run
/// of this suite may share the target directory.
fn scratch(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("floor-vmm-confined-{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Decides one attempt from how its process ended and what it logged.
fn judge(attempt: Attempt, exit: ExitStatus, log: &str) -> Result<(), String> {
    let name = attempt.name();
    let Some((_, after)) = log.split_once(&format!("attempting {name}\n")) else {
        return Err(format!(
            "{name}: the process ended before the attempt: {exit:?}, log {log:?}"
        ));
    };
    let outcome = after.lines().next().unwrap_or("");
    if attempt == Attempt::Nothing {
        return if exit.success() && outcome == "did nothing" && after.contains("booted: Ok") {
            Ok(())
        } else {
            Err(format!(
                "{name}: the confined process didn't boot its guest: {exit:?}, log {log:?}"
            ))
        };
    }
    if exit.signal() == Some(SIGSYS) && outcome.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{name}: the attempt went through: {exit:?}, log {log:?}"
    ))
}

/// Runs each attempt in its own process, and returns every failure.
fn check(label: &str, confine: bool) -> Result<(), String> {
    let directory = scratch(label);
    let target = directory.join("unrelated");
    fs::write(&target, TARGET_CONTENTS).unwrap();
    let mut failures = Vec::new();
    for attempt in ATTEMPTS {
        let log = directory.join(format!("{}.log", attempt.name()));
        let exit = Command::new(env::current_exe().unwrap())
            .args([OWNER_TEST, "--exact", "--test-threads=1"])
            .env(ATTEMPT_VARIABLE, attempt.name())
            .env(CONFINE_VARIABLE, if confine { "on" } else { "off" })
            .env(LOG_VARIABLE, &log)
            .env(KERNEL_VARIABLE, kernel_path())
            .env(INIT_VARIABLE, ap_path())
            .env(TARGET_VARIABLE, &target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        let log = fs::read_to_string(&log).unwrap_or_default();
        println!("{}: {exit:?}, {log:?}", attempt.name());
        if let Err(failure) = judge(attempt, exit, &log) {
            failures.push(failure);
        }
    }
    if failures.is_empty() {
        let _ = fs::remove_dir_all(&directory);
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// The process that boots a guest and makes one attempt. It does nothing in
/// a normal test run.
#[test]
fn confined_owner_process() {
    let Some(log) = env::var_os(LOG_VARIABLE) else {
        return;
    };
    let attempt = Attempt::from_name(&env::var(ATTEMPT_VARIABLE).unwrap());
    let kernel = fs::read(env::var_os(KERNEL_VARIABLE).unwrap()).unwrap();
    let init = fs::read(env::var_os(INIT_VARIABLE).unwrap()).unwrap();
    let target = PathBuf::from(env::var_os(TARGET_VARIABLE).unwrap());
    // Opened before the filter, which allows writes to what is already open.
    let mut log = File::create(log).unwrap();
    let guest = Guest {
        kernel: &kernel,
        init: &init,
        files: &[],
        layers: Vec::new(),
        command: None,
        timeout: TIMEOUT,
        confine: env::var(CONFINE_VARIABLE).unwrap() == "on",
    };
    let mut attempted = false;
    let result = boot::boot(guest, |report| {
        if matches!(report, Report::Ready(_)) && !attempted {
            attempted = true;
            log.write_all(format!("attempting {}\n", attempt.name()).as_bytes())
                .unwrap();
            let outcome = attempt.make(&target);
            log.write_all(format!("{outcome}\n").as_bytes()).unwrap();
        }
    });
    log.write_all(format!("booted: {:?}\n", result.map_err(|e| e.to_string())).as_bytes())
        .unwrap();
}

fn status_field(status: &str, field: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|value| value.trim().parse().ok())
}

/// Whether every thread of `pid` is under one more seccomp filter than this
/// test is, in filter mode, with `no_new_privs`. It needs more than one
/// thread, so that a thread `ap` started before the filter is among them.
fn every_thread_confined(pid: u32, own_filters: u32) -> bool {
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return false;
    };
    let mut threads = 0;
    for task in tasks {
        let Ok(status) = task.and_then(|task| fs::read_to_string(task.path().join("status")))
        else {
            return false;
        };
        if status_field(&status, "Seccomp") != Some(2)
            || status_field(&status, "Seccomp_filters").unwrap_or(0) <= own_filters
            || status_field(&status, "NoNewPrivs") != Some(1)
        {
            return false;
        }
        threads += 1;
    }
    threads > 1
}

/// Runs a real `ap boot`, and reads the status of each of its threads while
/// it runs.
fn check_ap_boot() -> Result<(), String> {
    let home = scratch("ap-boot");
    let store = Store::new(home.join(".local/share/ap"));
    store
        .put(fetch::kernel().sha256, File::open(kernel_path()).unwrap())
        .unwrap();
    let own = fs::read_to_string("/proc/self/status").unwrap();
    let own_filters = status_field(&own, "Seccomp_filters").unwrap_or(0);

    let mut ap = Command::new(ap_path())
        .args(["boot", "--timeout", "30"])
        .env_clear()
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut confined = false;
    let exit = loop {
        if every_thread_confined(ap.id(), own_filters) {
            confined = true;
        }
        if let Some(exit) = ap.try_wait().unwrap() {
            break exit;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = fs::remove_dir_all(&home);
    match (exit.success(), confined) {
        (true, true) => Ok(()),
        (false, _) => Err(format!("ap boot failed: {exit:?}")),
        (true, false) => {
            Err("ap boot ran its guest with a thread outside a filter of its own".to_owned())
        }
    }
}

#[test]
fn floor_vmm_confined() {
    check("confined", true).unwrap();
    check_ap_boot().unwrap();
}

#[test]
fn the_floor_fails_when_the_process_is_not_confined() {
    let error = check("unconfined", false).unwrap_err();
    for attempt in ["fork", "exec", "open"] {
        assert!(
            error.contains(&format!("{attempt}: the attempt went through")),
            "{error}"
        );
    }
    assert!(!error.contains("nothing:"), "{error}");
}

fn exited(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

#[test]
fn the_judge_passes_a_process_killed_at_the_attempt() {
    let killed = ExitStatus::from_raw(SIGSYS);
    assert_eq!(judge(Attempt::Fork, killed, "attempting fork\n"), Ok(()));
}

#[test]
fn the_judge_fails_a_process_killed_before_the_attempt() {
    let killed = ExitStatus::from_raw(SIGSYS);
    let error = judge(Attempt::Fork, killed, "").unwrap_err();
    assert!(error.contains("before the attempt"), "{error}");
    let error = judge(Attempt::Nothing, killed, "attempting nothing\n").unwrap_err();
    assert!(error.contains("didn't boot"), "{error}");
}

#[test]
fn the_judge_fails_a_call_that_came_back() {
    let refused = "attempting exec\nwent through: Permission denied\n";
    let error = judge(Attempt::Exec, exited(0), refused).unwrap_err();
    assert!(error.contains("went through"), "{error}");
    let killed_later = ExitStatus::from_raw(SIGSYS);
    let opened = "attempting open\nwent through: Ok(())\n";
    let error = judge(Attempt::Open, killed_later, opened).unwrap_err();
    assert!(error.contains("went through"), "{error}");
}

#[test]
fn the_judge_fails_an_exec_that_replaced_the_process() {
    let error = judge(Attempt::Exec, exited(0), "attempting exec\n").unwrap_err();
    assert!(error.contains("went through"), "{error}");
}
