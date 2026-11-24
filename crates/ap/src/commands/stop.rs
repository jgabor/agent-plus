//! `ap stop <run>`, from SPEC.md's `ap stop` section. It connects to the
//! run's socket, which the run's `ap run` serves, and asks it to stop, as
//! [`ap::runs`] says. It returns once `ap run` hangs up, which it does only
//! when it exits, after the run process that holds the VM. A suspended
//! `ap run` can't take the connection, or exit, so `ap stop` resumes it.
//! One in the background of its terminal can be suspended again as soon as
//! it resumes, before it takes the stop, so `ap stop` resumes it every
//! [`RESUME_EVERY`] until it hangs up.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ap::runs;
use store::Runs;

/// How often `ap stop` resumes the `ap run` it waits for.
const RESUME_EVERY: Duration = Duration::from_millis(10);

/// Reads `ap stop`'s arguments: one run's name. Anything else is bad usage.
pub fn parse(args: &[OsString]) -> Option<String> {
    match args {
        [name] if !name.as_encoded_bytes().starts_with(b"-") => Some(name.to_str()?.to_owned()),
        _ => None,
    }
}

/// Stops the run, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match stop(name) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn stop(name: &str) -> Result<(), String> {
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .ok_or("HOME isn't an absolute path, so there's no place for the store")?;
    let runs = Runs::new(&root);
    let missing = || format!("no run is named {name}");
    if !store::valid_run_name(name) || runs.get(name).map_err(|error| error.to_string())?.is_none()
    {
        return Err(missing());
    }
    if !runs.active(name).map_err(|error| error.to_string())? {
        return Ok(());
    }
    // A run that ends first takes its socket with it, and has stopped as
    // asked, so only a run still active after a failure is an error.
    let dir = runs::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &root);
    let ask = || -> io::Result<()> {
        // The path names the open directory, so it must stay open.
        let opened = File::open(&dir)?;
        let mut stream = UnixStream::connect(runs::short_path(&opened, &runs::socket_name(name)))?;
        let serving = server(&stream);
        runs::send_stop(&mut stream)?;
        stream.set_read_timeout(Some(RESUME_EVERY))?;
        loop {
            resume(serving.as_ref());
            match stream.read(&mut [0; 64]) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    };
    match ask() {
        Err(error) if runs.active(name).map_err(|error| error.to_string())? => {
            Err(format!("can't reach run {name}: {error}"))
        }
        _ => Ok(()),
    }
}

/// The `ap run` that serves the socket at the other end of `stream`, which
/// the kernel names, as a pidfd, so that no process that takes its id later
/// is resumed in its place.
fn server(stream: &UnixStream) -> Option<OwnedFd> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `length` are valid for the call, and
    // `length` is the size of `credentials`.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    };
    // A listener in another process namespace has no process id here.
    if read != 0 || credentials.pid <= 0 {
        return None;
    }
    // SAFETY: `pidfd_open` takes and returns plain integers.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, credentials.pid, 0) };
    // SAFETY: the kernel just returned this descriptor, and nothing else
    // owns it.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Resumes the `ap run` behind this pidfd, in case it is suspended.
/// `SIGCONT` does nothing to one that runs, so a failure here holds back no
/// stop that would reach the run anyway.
fn resume(serving: Option<&OwnedFd>) {
    if let Some(pidfd) = serving {
        // SAFETY: `pidfd_send_signal` takes plain integers, and a null
        // `siginfo_t`, which makes it act as `kill` does.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGCONT,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
    }
}
