//! Landlock, which holds the share server to the shares' roots even if its
//! own code were turned against it. A filter can't read the flags of
//! `openat2`, so this is what keeps a file outside every root closed.

use std::ffi::c_long;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;

const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SYM: u64 = 1 << 12;
/// Every right the first version knows, which is every right but the ones
/// later versions add.
const VERSION_1: u64 = (1 << 13) - 1;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;

/// `struct landlock_ruleset_attr`, as far as the first version reads it.
#[repr(C)]
struct Ruleset {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr`, which the kernel packs.
#[repr(C, packed)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

/// What the share server may do beneath a root.
pub enum Access {
    Read,
    /// Reading, and changing files, directories, and symlinks. Never
    /// running a file, or making a device, a FIFO, or a socket.
    ReadWrite,
}

/// Every file right this kernel's Landlock knows of.
fn handled(version: c_long) -> u64 {
    let mut rights = VERSION_1;
    if version >= 2 {
        rights |= REFER;
    }
    if version >= 3 {
        rights |= TRUNCATE;
    }
    if version >= 5 {
        rights |= IOCTL_DEV;
    }
    rights
}

fn allowed(access: &Access) -> u64 {
    match access {
        Access::Read => READ_FILE | READ_DIR,
        Access::ReadWrite => {
            READ_FILE
                | READ_DIR
                | WRITE_FILE
                | REMOVE_DIR
                | REMOVE_FILE
                | MAKE_DIR
                | MAKE_REG
                | MAKE_SYM
                | REFER
                | TRUNCATE
        }
    }
}

fn check(result: c_long) -> io::Result<c_long> {
    match result {
        -1 => Err(io::Error::last_os_error()),
        value => Ok(value),
    }
}

/// Lets this process open files only beneath `roots`, each as its access
/// says, for the rest of its life. Returns false, and changes nothing, on a
/// kernel without Landlock.
pub fn restrict(roots: &[(BorrowedFd, Access)]) -> io::Result<bool> {
    // SAFETY: asking for the version takes no pointer.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<Ruleset>(),
            0,
            CREATE_RULESET_VERSION,
        )
    };
    if version < 1 {
        return Ok(false);
    }
    let handled = handled(version);
    let ruleset = Ruleset {
        handled_access_fs: handled,
    };
    // SAFETY: `ruleset` outlives the call, which returns a new descriptor.
    let fd = check(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const ruleset,
            size_of::<Ruleset>(),
            0,
        )
    })?;
    // SAFETY: the descriptor is new, and nothing else owns it.
    let ruleset_fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    for (root, access) in roots {
        let rule = PathBeneath {
            allowed_access: allowed(access) & handled,
            parent_fd: root.as_raw_fd(),
        };
        // SAFETY: `rule` outlives the call.
        check(unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset_fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const rule,
                0,
            )
        })?;
    }
    // SAFETY: both calls match the kernel's signatures. Landlock needs
    // `no_new_privs`, which the filter sets again later.
    unsafe {
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) as c_long)?;
        check(libc::syscall(
            libc::SYS_landlock_restrict_self,
            ruleset_fd.as_raw_fd(),
            0,
        ))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsFd;
    use std::path::Path;
    use std::process::Command;

    const ROOT_VARIABLE: &str = "AP_SHARE_LANDLOCK_ROOT";

    /// Restricts itself to a root, then tries files inside and outside it.
    /// It does nothing in a normal test run.
    #[test]
    fn restricted_process() {
        let Some(base) = std::env::var_os(ROOT_VARIABLE) else {
            return;
        };
        let base = Path::new(&base);
        let root = File::open(base.join("root")).unwrap();
        assert!(restrict(&[(root.as_fd(), Access::Read)]).unwrap());
        let inside = fs::read(base.join("root/inside")).map(drop);
        let listed = fs::read_dir(base.join("root")).map(drop);
        let outside = fs::read(base.join("outside")).map(drop);
        let written = OpenOptions::new()
            .write(true)
            .open(base.join("root/inside"))
            .map(drop);
        println!("inside {inside:?} listed {listed:?} outside {outside:?} written {written:?}");
    }

    #[test]
    fn a_restricted_process_reads_only_beneath_its_roots() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/landlock-test")
            .join(std::process::id().to_string());
        fs::create_dir_all(base.join("root")).unwrap();
        fs::write(base.join("root/inside"), "in").unwrap();
        fs::write(base.join("outside"), "out").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["landlock::tests::restricted_process", "--exact"])
            .args(["--nocapture", "--test-threads=1"])
            .env(ROOT_VARIABLE, &base)
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(&base);
        let output = String::from_utf8_lossy(&output.stdout);
        let denied = "Err(Os { code: 13, kind: PermissionDenied";
        assert!(
            output.contains(&format!("inside Ok(()) listed Ok(()) outside {denied}")),
            "{output}"
        );
        assert!(output.contains(&format!("written {denied}")), "{output}");
    }

    #[test]
    fn each_version_handles_the_rights_it_knows() {
        assert_eq!(handled(1), 0x1fff);
        assert_eq!(handled(2), 0x3fff);
        assert_eq!(handled(4), 0x7fff);
        assert_eq!(handled(6), 0xffff);
    }

    #[test]
    fn a_read_only_root_allows_reading_alone() {
        assert_eq!(allowed(&Access::Read), 0b1100);
    }

    #[test]
    fn a_read_write_root_never_runs_a_file_or_makes_a_special_one() {
        let rights = allowed(&Access::ReadWrite);
        let execute = 1;
        let special = (1 << 6) | (1 << 9) | (1 << 10) | (1 << 11);
        assert_eq!(rights & (execute | special | IOCTL_DEV), 0);
        assert_eq!(rights & (REFER | TRUNCATE), REFER | TRUNCATE);
        assert_eq!(rights.count_ones(), 10);
    }
}
