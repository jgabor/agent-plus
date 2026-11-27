//! A static program for `home_overlay`. It runs as root in a real guest,
//! under `ap`'s own init. It mounts the first share at `/state`, and an
//! overlay on `/home/agent`, with the image's `/home/agent` below and the
//! share's `upper` above. Then it runs the command it was given, and exits
//! with its status.

use std::env;
use std::ffi::{CString, c_char, c_ulong, c_void};
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, exit};

const STATE: &str = "/state";
const HOME: &str = "/home/agent";

unsafe extern "C" {
    fn mount(
        source: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> i32;
}

fn overlay() -> io::Result<()> {
    let options = format!("lowerdir={HOME},upperdir={STATE}/upper,workdir={STATE}/work");
    let text = |text: &str| CString::new(text).map_err(io::Error::other);
    let (source, target, kind, data) = (
        text("overlay")?,
        text(HOME)?,
        text("overlay")?,
        text(&options)?,
    );
    // SAFETY: every pointer comes from a `CString` that outlives the call.
    let mounted = unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            0,
            data.as_ptr().cast(),
        )
    };
    match mounted {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn main() {
    let mounted = guest::linux::mount_share(&guest::share::tag(0), Path::new(STATE))
        .and_then(|()| fs::create_dir_all(format!("{STATE}/upper")))
        .and_then(|()| fs::create_dir_all(format!("{STATE}/work")))
        .and_then(|()| overlay());
    if let Err(error) = mounted {
        eprintln!("home-probe: can't mount the home: {error}");
        let _ = Command::new("/bin/dmesg").status();
        exit(125);
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    let Some((program, args)) = args.split_first() else {
        exit(0);
    };
    let status = Command::new(program).args(args).status();
    exit(status.ok().and_then(|status| status.code()).unwrap_or(126));
}
