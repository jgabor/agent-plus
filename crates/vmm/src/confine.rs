//! Confines this process to the system calls that running a guest needs, with
//! a seccomp filter on every thread. Anything else kills the process, so code
//! a guest got running in the VMM gets no second try.
//!
//! What is left can't start a process or a program, or reach a host file:
//! every call that takes a path is missing, and memory can be mapped only
//! anonymously, so a file left open can't be read either. The exceptions are
//! the descriptors the caller names, which may be read, at an offset or not,
//! and nothing else.
//! Calls that could reach past this process are narrowed by their arguments,
//! as the kernel reads them.
//!
//! The share server, which reaches a run's shared files for its guest, has a
//! filter of its own, in [`confine_share_server`], and so does the network
//! process, which runs the guest's network, in [`confine_network`], or in
//! [`confine_network_connecting`] once it may connect out.

use std::ffi::{c_int, c_long, c_ulong};
use std::io;
use std::os::fd::RawFd;

use crate::kvm::{KVM_IRQ_LINE, KVM_RUN};

// From <linux/prctl.h>, <linux/seccomp.h>, <linux/filter.h>, and
// <linux/audit.h>.
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const SYS_SECCOMP: c_long = 317;
const SECCOMP_SET_MODE_FILTER: c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: c_ulong = 1;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const LOAD_WORD: u16 = 0x20;
const JUMP_EQUAL: u16 = 0x15;
const JUMP_AT_LEAST: u16 = 0x35;
const JUMP_SET: u16 = 0x45;
const AND: u16 = 0x54;
const RETURN: u16 = 0x06;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const X32_SYSCALLS: u32 = 0x4000_0000;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARGS: u32 = 16;

const ENOSYS: u32 = 38;
const CLONE_THREAD: u32 = 0x0001_0000;
const F_GETFD: u32 = 1;
const F_OFD_GETLK: u32 = 36;
const F_OFD_SETLK: u32 = 37;
const MAP_ANONYMOUS: u32 = 0x20;
const O_ACCMODE: u32 = 3;

// x86_64 system call numbers.
const READ: u32 = 0;
const WRITE: u32 = 1;
const CLOSE: u32 = 3;
const MMAP: u32 = 9;
const MPROTECT: u32 = 10;
const MUNMAP: u32 = 11;
const BRK: u32 = 12;
const RT_SIGACTION: u32 = 13;
const RT_SIGPROCMASK: u32 = 14;
const RT_SIGRETURN: u32 = 15;
const IOCTL: u32 = 16;
const SCHED_YIELD: u32 = 24;
const MREMAP: u32 = 25;
const MADVISE: u32 = 28;
const GETPID: u32 = 39;
const CLONE: u32 = 56;
const EXIT: u32 = 60;
const FCNTL: u32 = 72;
const SIGALTSTACK: u32 = 131;
const GETTID: u32 = 186;
const TKILL: u32 = 200;
const FUTEX: u32 = 202;
const SCHED_GETAFFINITY: u32 = 204;
const CLOCK_GETTIME: u32 = 228;
const FSTAT: u32 = 5;
const LSEEK: u32 = 8;
const PREAD64: u32 = 17;
const FSTATFS: u32 = 138;
const GETDENTS64: u32 = 217;
const READLINKAT: u32 = 267;
const OPENAT: u32 = 257;
const OPENAT2: u32 = 437;
const PWRITE64: u32 = 18;
const FLOCK: u32 = 73;
const FSYNC: u32 = 74;
const FDATASYNC: u32 = 75;
const FTRUNCATE: u32 = 77;
const FCHMOD: u32 = 91;
const MKDIRAT: u32 = 258;
const UNLINKAT: u32 = 263;
const LINKAT: u32 = 265;
const SYMLINKAT: u32 = 266;
const UTIMENSAT: u32 = 280;
const FALLOCATE: u32 = 285;
const RENAMEAT2: u32 = 316;
const AT_FDCWD: u32 = -100_i32 as u32;
const STDOUT: u32 = 1;
const STDERR: u32 = 2;
const EXIT_GROUP: u32 = 231;
const TGKILL: u32 = 234;
const SET_ROBUST_LIST: u32 = 273;
const RSEQ: u32 = 334;
const POLL: u32 = 7;
const SOCKET: u32 = 41;
const CONNECT: u32 = 42;
const SENDTO: u32 = 44;
const RECVFROM: u32 = 45;
const SHUTDOWN: u32 = 48;
const GETSOCKOPT: u32 = 55;
const PPOLL: u32 = 271;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;
const CLONE3: u32 = 435;
const GETRANDOM: u32 = 318;

/// One BPF instruction, as `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Instruction {
    code: u16,
    if_true: u8,
    if_false: u8,
    value: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
struct Program {
    len: u16,
    instructions: *const Instruction,
}

unsafe extern "C" {
    fn prctl(option: c_int, ...) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
}

const fn op(code: u16, if_true: u8, if_false: u8, value: u32) -> Instruction {
    Instruction {
        code,
        if_true,
        if_false,
        value,
    }
}

/// What the filter does with one system call. An argument is compared by its
/// low 32 bits. Each one checked here is an `int` or `unsigned int` to the
/// kernel, or a flag word whose low half holds the flag, so those bits are
/// all the kernel reads.
enum Rule {
    Allow(u32),
    /// Allowed when the argument at this index is one of these values.
    AllowIf(u32, u32, Vec<u32>),
    /// Allowed when the argument at this index has every bit of the mask.
    AllowIfSet(u32, u32, u32),
    /// Allowed unless the argument at this index is this value.
    AllowUnless(u32, u32, u32),
    /// Allowed unless either argument at these indexes is this value.
    AllowUnlessEither(u32, [u32; 2], u32),
    /// Allowed unless the argument at the first index is this value, or
    /// the argument at the second has every bit of this mask.
    AllowUnlessOrAllSet(u32, (u32, u32), (u32, u32)),
    /// Allowed when the pointer at this index is null, in both halves.
    AllowIfNull(u32, u32),
    /// Answered with this error, without running it.
    Refuse(u32, u32),
}

/// The calls that the VMM, its watchdog, and the output relay make while a
/// guest runs, on glibc and musl. `pid` and `tid` are this process's and the
/// vCPU thread's, the only targets the watchdog signals. `readable` are the
/// descriptors that may be read.
fn rules(pid: u32, tid: u32, readable: &[RawFd]) -> Vec<Rule> {
    use Rule::{Allow, AllowIf, AllowIfSet, Refuse};
    let mut rules = vec![
        AllowIf(IOCTL, 1, vec![KVM_RUN as u32, KVM_IRQ_LINE as u32]),
        Allow(FUTEX),
        Allow(WRITE),
        AllowIfSet(MMAP, 3, MAP_ANONYMOUS),
        Allow(MUNMAP),
        Allow(MREMAP),
        Allow(MPROTECT),
        Allow(MADVISE),
        Allow(BRK),
        Allow(RT_SIGPROCMASK),
        Allow(RT_SIGACTION),
        Allow(RT_SIGRETURN),
        Allow(SIGALTSTACK),
        Allow(SCHED_YIELD),
        // glibc's allocator counts the CPUs when a thread first allocates.
        Allow(SCHED_GETAFFINITY),
        Allow(CLOCK_GETTIME),
        Allow(GETPID),
        Allow(GETTID),
        Allow(CLOSE),
        AllowIf(FCNTL, 1, vec![F_GETFD]),
        AllowIf(TGKILL, 0, vec![pid]),
        AllowIf(TKILL, 0, vec![tid]),
        // A thread only. `clone3` hides its flags from a filter, so it is
        // refused as missing, and the C library falls back to `clone`.
        AllowIfSet(CLONE, 0, CLONE_THREAD),
        Refuse(CLONE3, ENOSYS),
        Allow(SET_ROBUST_LIST),
        Allow(RSEQ),
        Allow(EXIT),
        Allow(EXIT_GROUP),
    ];
    if !readable.is_empty() {
        let readable: Vec<u32> = readable.iter().map(|fd| *fd as u32).collect();
        rules.push(AllowIf(READ, 0, readable.clone()));
        rules.push(AllowIf(PREAD64, 0, readable));
    }
    rules
}

/// The calls the share server makes once it has opened each share's root.
/// It has one thread, so it starts none, and it reads requests from
/// `socket` alone. It opens a file only relative to a descriptor it holds,
/// one name at a time, and the opening code follows no symlink. A filter
/// can't read a name, so Landlock holds the server to the roots too, where
/// the kernel has it. Landlock checks an open only for reading or writing,
/// so the filter refuses access mode 3, which asks for neither, and
/// `openat2`, whose flags it can't read. A descriptor opened with `O_PATH`
/// takes neither `fchmod` nor `futimens`. It takes and checks locks on
/// the files it holds. With `writes`, for an `rw` share, it also changes
/// files: through a descriptor it holds, or by one name in a directory it
/// holds, which Landlock holds to the `rw` roots. Changing a file's times
/// takes no name at all. Landlock doesn't cover a file's mode or times, so
/// for those, an `ro` share beside an `rw` one rests on the server's own
/// checks.
fn share_server_rules(socket: RawFd, writes: bool) -> Vec<Rule> {
    use Rule::{
        Allow, AllowIf, AllowIfNull, AllowIfSet, AllowUnless, AllowUnlessEither,
        AllowUnlessOrAllSet,
    };
    let mut rules = vec![
        AllowIf(READ, 0, vec![socket as u32]),
        AllowIf(WRITE, 0, vec![socket as u32, STDERR]),
        AllowUnlessOrAllSet(OPENAT, (0, AT_FDCWD), (2, O_ACCMODE)),
        AllowUnless(READLINKAT, 0, AT_FDCWD),
        Allow(PREAD64),
        Allow(GETDENTS64),
        Allow(LSEEK),
        Allow(FSTAT),
        Allow(FSTATFS),
        Allow(CLOSE),
        // Rust checks a descriptor this way before it closes it. The rest
        // are locks, which never wait.
        AllowIf(FCNTL, 1, vec![F_GETFD, F_OFD_GETLK, F_OFD_SETLK]),
        Allow(FLOCK),
        AllowIfSet(MMAP, 3, MAP_ANONYMOUS),
        Allow(MUNMAP),
        Allow(MREMAP),
        Allow(MADVISE),
        Allow(BRK),
        Allow(FUTEX),
        Allow(RT_SIGPROCMASK),
        Allow(RT_SIGACTION),
        Allow(RT_SIGRETURN),
        Allow(SIGALTSTACK),
        Allow(EXIT),
        Allow(EXIT_GROUP),
    ];
    if writes {
        rules.extend([
            Allow(PWRITE64),
            Allow(FTRUNCATE),
            Allow(FALLOCATE),
            Allow(FSYNC),
            Allow(FDATASYNC),
            Allow(FCHMOD),
            AllowIfNull(UTIMENSAT, 1),
            AllowUnless(MKDIRAT, 0, AT_FDCWD),
            AllowUnless(UNLINKAT, 0, AT_FDCWD),
            AllowUnless(SYMLINKAT, 1, AT_FDCWD),
            AllowUnlessEither(LINKAT, [0, 2], AT_FDCWD),
            AllowUnlessEither(RENAMEAT2, [0, 2], AT_FDCWD),
        ]);
    }
    rules
}

/// The calls the network process makes while it passes the guest's frames
/// through its stack. It has one thread, so it starts none. It reads and
/// writes frames on `link`, waits on it, and writes to standard error. It
/// writes to standard output too, a socket to `ap run`, where it reports
/// what it blocked. It makes no socket, so nothing it does reaches past the
/// link and `ap run`.
fn network_rules(link: RawFd) -> Vec<Rule> {
    let mut rules = vec![Rule::AllowIf(RECVFROM, 0, vec![link as u32])];
    rules.extend(network_common_rules(link));
    rules
}

/// The calls of [`network_rules`], and those that connect out: it makes
/// internet sockets, and connects, reads, writes, and shuts them. It also
/// draws on the random source, to sign a certificate for a name it decrypts,
/// and for the TLS it speaks. It still can't start a process, or open a
/// file, and the only sockets it can make are IPv4 and IPv6 ones.
fn network_connecting_rules(link: RawFd) -> Vec<Rule> {
    use Rule::{Allow, AllowIf};
    let mut rules = vec![
        AllowIf(SOCKET, 0, vec![AF_INET, AF_INET6]),
        Allow(CONNECT),
        Allow(SENDTO),
        Allow(RECVFROM),
        Allow(GETSOCKOPT),
        Allow(SHUTDOWN),
        Allow(GETRANDOM),
    ];
    rules.extend(network_common_rules(link));
    rules
}

fn network_common_rules(link: RawFd) -> Vec<Rule> {
    use Rule::{Allow, AllowIf, AllowIfSet};
    vec![
        AllowIf(READ, 0, vec![link as u32]),
        AllowIf(WRITE, 0, vec![link as u32, STDOUT, STDERR]),
        Allow(POLL),
        Allow(PPOLL),
        Allow(CLOCK_GETTIME),
        Allow(CLOSE),
        AllowIf(FCNTL, 1, vec![F_GETFD]),
        AllowIfSet(MMAP, 3, MAP_ANONYMOUS),
        Allow(MUNMAP),
        Allow(MREMAP),
        Allow(MADVISE),
        Allow(BRK),
        Allow(FUTEX),
        Allow(RT_SIGPROCMASK),
        Allow(RT_SIGACTION),
        Allow(RT_SIGRETURN),
        Allow(SIGALTSTACK),
        Allow(EXIT),
        Allow(EXIT_GROUP),
    ]
}

fn compile(rules: &[Rule]) -> Vec<Instruction> {
    let kill = op(RETURN, 0, 0, RET_KILL_PROCESS);
    let allow = op(RETURN, 0, 0, RET_ALLOW);
    let mut program = vec![
        op(LOAD_WORD, 0, 0, OFFSET_ARCH),
        op(JUMP_EQUAL, 1, 0, AUDIT_ARCH_X86_64),
        kill,
        op(LOAD_WORD, 0, 0, OFFSET_NR),
        op(JUMP_AT_LEAST, 0, 1, X32_SYSCALLS),
        kill,
    ];
    for rule in rules {
        let load = |index: u32| op(LOAD_WORD, 0, 0, OFFSET_ARGS + 8 * index);
        let (number, body) = match rule {
            Rule::Allow(number) => (*number, vec![allow]),
            Rule::Refuse(number, errno) => (*number, vec![op(RETURN, 0, 0, RET_ERRNO | errno)]),
            Rule::AllowIf(number, index, values) => {
                let mut body = vec![load(*index)];
                for value in values {
                    body.extend([op(JUMP_EQUAL, 0, 1, *value), allow]);
                }
                body.push(kill);
                (*number, body)
            }
            Rule::AllowIfSet(number, index, mask) => (
                *number,
                vec![load(*index), op(JUMP_SET, 0, 1, *mask), allow, kill],
            ),
            Rule::AllowUnless(number, index, value) => (
                *number,
                vec![load(*index), op(JUMP_EQUAL, 0, 1, *value), kill, allow],
            ),
            Rule::AllowUnlessEither(number, [first, second], value) => (
                *number,
                vec![
                    load(*first),
                    op(JUMP_EQUAL, 3, 0, *value),
                    load(*second),
                    op(JUMP_EQUAL, 1, 0, *value),
                    allow,
                    kill,
                ],
            ),
            Rule::AllowUnlessOrAllSet(number, (at, value), (index, mask)) => (
                *number,
                vec![
                    load(*at),
                    op(JUMP_EQUAL, 4, 0, *value),
                    load(*index),
                    op(AND, 0, 0, *mask),
                    op(JUMP_EQUAL, 1, 0, *mask),
                    allow,
                    kill,
                ],
            ),
            Rule::AllowIfNull(number, index) => (
                *number,
                vec![
                    load(*index),
                    op(JUMP_EQUAL, 0, 3, 0),
                    op(LOAD_WORD, 0, 0, OFFSET_ARGS + 8 * index + 4),
                    op(JUMP_EQUAL, 0, 1, 0),
                    allow,
                    kill,
                ],
            ),
        };
        program.push(op(JUMP_EQUAL, 0, body.len() as u8, number));
        program.extend(body);
    }
    program.push(kill);
    program
}

/// Confines every thread of this process, for the rest of its life. Call it
/// from the thread that will run the vCPU, once the VM is set up, because the
/// watchdog may signal only that thread, and no file can be opened after.
/// Only the descriptors in `readable` may be read.
pub fn confine(readable: &[RawFd]) -> io::Result<()> {
    let pid = std::process::id();
    // SAFETY: gettid takes no arguments and can't fail.
    let tid = unsafe { syscall(GETTID as c_long) } as u32;
    install(&rules(pid, tid, readable))
}

/// Confines the share server, which has one thread, for the rest of its
/// life, and lets it change files if `writes`. Call it once every share's
/// root is open. Requests come from `socket`, and answers go back on it.
pub fn confine_share_server(socket: RawFd, writes: bool) -> io::Result<()> {
    install(&share_server_rules(socket, writes))
}

/// Confines the network process, which has one thread, for the rest of its
/// life. Call it once its stack is set up. Frames come from `link`, and go
/// back on it.
pub fn confine_network(link: RawFd) -> io::Result<()> {
    install(&network_rules(link))
}

/// Confines the network process as [`confine_network`] does, but lets it
/// make IPv4 and IPv6 sockets, and connect out on them.
pub fn confine_network_connecting(link: RawFd) -> io::Result<()> {
    install(&network_connecting_rules(link))
}

fn install(rules: &[Rule]) -> io::Result<()> {
    let instructions = compile(rules);
    let program = Program {
        len: instructions.len() as u16,
        instructions: instructions.as_ptr(),
    };
    // SAFETY: both calls match the kernel's signatures for these options.
    // `program` and the instructions it points to outlive the second call,
    // which copies them.
    unsafe {
        if prctl(
            PR_SET_NO_NEW_PRIVS,
            1 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        match syscall(
            SYS_SECCOMP,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &raw const program,
        ) {
            0 => Ok(()),
            -1 => Err(io::Error::last_os_error()),
            thread => Err(io::Error::other(format!(
                "thread {thread} couldn't take the filter"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: u32 = 4000;
    const TID: u32 = 4001;
    const TIOCSTI: u32 = 0x5412;
    const FORK: u32 = 57;
    const VFORK: u32 = 58;
    const EXECVE: u32 = 59;
    const EXECVEAT: u32 = 322;
    const F_SETOWN: u32 = 8;
    const MAP_SHARED: u32 = 1;
    const MAP_PRIVATE: u32 = 2;
    const MAP_STACK: u32 = 0x2_0000;
    const CLONE_VFORK_VM: u32 = 0x4100;
    const LOAD_ARGUMENT_LIMIT: u32 = OFFSET_ARGS + 8 * 6;

    /// Runs the program the way the kernel does, on `struct seccomp_data`
    /// with the low halves of the arguments.
    fn run(arch: u32, number: u32, args: [u32; 6]) -> u32 {
        run_reading(&[], arch, number, args)
    }

    fn run_reading(readable: &[RawFd], arch: u32, number: u32, args: [u32; 6]) -> u32 {
        run_rules(&rules(PID, TID, readable), arch, number, args)
    }

    fn run_rules(rules: &[Rule], arch: u32, number: u32, args: [u32; 6]) -> u32 {
        run_wide(rules, arch, number, args.map(u64::from))
    }

    /// Runs the program on whole arguments, whose halves it loads apart.
    fn run_wide(rules: &[Rule], arch: u32, number: u32, args: [u64; 6]) -> u32 {
        let program = compile(rules);
        let mut accumulator = 0;
        let mut pc = 0;
        loop {
            let instruction = program[pc];
            pc += 1;
            let jump = |taken: bool| {
                usize::from(if taken {
                    instruction.if_true
                } else {
                    instruction.if_false
                })
            };
            match instruction.code {
                LOAD_WORD => {
                    accumulator = match instruction.value {
                        OFFSET_NR => number,
                        OFFSET_ARCH => arch,
                        offset if (OFFSET_ARGS..LOAD_ARGUMENT_LIMIT).contains(&offset) => {
                            let index = offset - OFFSET_ARGS;
                            let arg = args[(index / 8) as usize];
                            match index % 8 {
                                0 => arg as u32,
                                4 => (arg >> 32) as u32,
                                _ => panic!("a load inside an argument's half"),
                            }
                        }
                        offset => panic!("a load at {offset}"),
                    }
                }
                JUMP_EQUAL => pc += jump(accumulator == instruction.value),
                JUMP_AT_LEAST => pc += jump(accumulator >= instruction.value),
                JUMP_SET => pc += jump(accumulator & instruction.value != 0),
                AND => accumulator &= instruction.value,
                RETURN => return instruction.value,
                code => panic!("an unknown instruction {code:#x}"),
            }
        }
    }

    fn call(number: u32, args: [u32; 6]) -> u32 {
        run(AUDIT_ARCH_X86_64, number, args)
    }

    #[test]
    fn what_a_running_vmm_needs_is_allowed() {
        for number in [WRITE, FUTEX, MUNMAP, RT_SIGRETURN, EXIT_GROUP] {
            assert_eq!(call(number, [0; 6]), RET_ALLOW, "{number}");
        }
        for request in [KVM_RUN, KVM_IRQ_LINE] {
            assert_eq!(call(IOCTL, [5, request as u32, 0, 0, 0, 0]), RET_ALLOW);
        }
        assert_eq!(call(CLONE, [0x003d_0f00, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(TGKILL, [PID, TID, 42, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(TKILL, [TID, 42, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(FCNTL, [3, F_GETFD, 0, 0, 0, 0]), RET_ALLOW);
        let anonymous = MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK;
        assert_eq!(call(MMAP, [0, 4096, 3, anonymous, u32::MAX, 0]), RET_ALLOW);
    }

    #[test]
    fn starting_a_process_or_a_program_kills() {
        for number in [FORK, VFORK, EXECVE, EXECVEAT] {
            assert_eq!(call(number, [0; 6]), RET_KILL_PROCESS, "{number}");
        }
        assert_eq!(
            call(CLONE, [CLONE_VFORK_VM, 0, 0, 0, 0, 0]),
            RET_KILL_PROCESS
        );
        assert_eq!(call(CLONE3, [0; 6]), RET_ERRNO | ENOSYS);
    }

    #[test]
    fn opening_a_file_kills() {
        assert_eq!(call(OPENAT, [0; 6]), RET_KILL_PROCESS);
        assert_eq!(call(2, [0; 6]), RET_KILL_PROCESS);
    }

    #[test]
    fn mapping_a_file_that_is_already_open_kills() {
        for flags in [MAP_PRIVATE, MAP_SHARED] {
            assert_eq!(call(MMAP, [0, 4096, 1, flags, 0, 0]), RET_KILL_PROCESS);
        }
    }

    #[test]
    fn calls_that_reach_past_this_process_are_narrowed() {
        assert_eq!(call(IOCTL, [0, TIOCSTI, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(call(TGKILL, [1, 1, 9, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(call(TKILL, [1, 9, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(call(FCNTL, [3, F_SETOWN, 1, 0, 0, 0]), RET_KILL_PROCESS);
    }

    #[test]
    fn only_the_named_descriptor_can_be_read() {
        assert_eq!(call(READ, [0, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        let reading = |fd| run_reading(&[7, 9], AUDIT_ARCH_X86_64, READ, [fd, 0, 0, 0, 0, 0]);
        assert_eq!(reading(7), RET_ALLOW);
        assert_eq!(reading(9), RET_ALLOW);
        assert_eq!(reading(0), RET_KILL_PROCESS);
        assert_eq!(reading(8), RET_KILL_PROCESS);
    }

    #[test]
    fn only_the_named_descriptor_can_be_read_at_an_offset() {
        assert_eq!(call(PREAD64, [0, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        let reading = |fd| run_reading(&[7, 9], AUDIT_ARCH_X86_64, PREAD64, [fd, 0, 0, 0, 0, 0]);
        assert_eq!(reading(7), RET_ALLOW);
        assert_eq!(reading(9), RET_ALLOW);
        assert_eq!(reading(8), RET_KILL_PROCESS);
    }

    const SOCKET: u32 = 5;

    fn share_call(number: u32, args: [u32; 6]) -> u32 {
        run_rules(
            &share_server_rules(SOCKET as RawFd, false),
            AUDIT_ARCH_X86_64,
            number,
            args,
        )
    }

    fn writing_call(number: u32, args: [u64; 6]) -> u32 {
        let rules = share_server_rules(SOCKET as RawFd, true);
        run_wide(&rules, AUDIT_ARCH_X86_64, number, args)
    }

    const WRITING: [u32; 12] = [
        PWRITE64, FTRUNCATE, FALLOCATE, FSYNC, FDATASYNC, FCHMOD, UTIMENSAT, MKDIRAT, UNLINKAT,
        SYMLINKAT, LINKAT, RENAMEAT2,
    ];

    #[test]
    fn the_share_server_takes_locks_on_what_it_holds() {
        for command in [F_OFD_GETLK, F_OFD_SETLK] {
            assert_eq!(share_call(FCNTL, [4, command, 0, 0, 0, 0]), RET_ALLOW);
        }
        assert_eq!(share_call(FLOCK, [4, 0, 0, 0, 0, 0]), RET_ALLOW);
        // A lock that waits would hold the guest.
        let f_ofd_setlkw = 38;
        assert_eq!(
            share_call(FCNTL, [4, f_ofd_setlkw, 0, 0, 0, 0]),
            RET_KILL_PROCESS
        );
    }

    #[test]
    fn only_a_server_with_an_rw_share_changes_files() {
        for number in WRITING {
            assert_eq!(
                share_call(number, [4, 1, 4, 1, 0, 0]),
                RET_KILL_PROCESS,
                "{number}"
            );
            assert_eq!(
                writing_call(number, [4, 0, 4, 0, 0, 0]),
                RET_ALLOW,
                "{number}"
            );
        }
    }

    #[test]
    fn a_server_that_changes_files_does_so_beneath_what_it_holds() {
        let cwd = u64::from(AT_FDCWD);
        for (number, at) in [
            (MKDIRAT, 0),
            (UNLINKAT, 0),
            (SYMLINKAT, 1),
            (LINKAT, 0),
            (LINKAT, 2),
            (RENAMEAT2, 0),
            (RENAMEAT2, 2),
        ] {
            let mut args = [4, 1, 4, 1, 0, 0];
            args[at] = cwd;
            assert_eq!(
                writing_call(number, args),
                RET_KILL_PROCESS,
                "{number} {at}"
            );
        }
        // Times change only for a descriptor, never for a path, whichever
        // half of the pointer is set.
        for path in [1, 1 << 32, u64::MAX] {
            assert_eq!(
                writing_call(UTIMENSAT, [4, path, 0, 0, 0, 0]),
                RET_KILL_PROCESS
            );
        }
        assert_eq!(writing_call(UTIMENSAT, [4, 0, 1, 0, 0, 0]), RET_ALLOW);
        assert_eq!(writing_call(WRITE, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        for args in [[cwd, 0, 1, 0, 0, 0], [4, 0, 3, 0, 0, 0]] {
            assert_eq!(writing_call(OPENAT, args), RET_KILL_PROCESS);
        }
    }

    #[test]
    fn the_share_server_reads_files_beneath_what_it_holds() {
        for number in [PREAD64, GETDENTS64, LSEEK, FSTAT, FSTATFS, CLOSE] {
            assert_eq!(
                share_call(number, [4, 0, 0, 0, 0, 0]),
                RET_ALLOW,
                "{number}"
            );
        }
        assert_eq!(share_call(FCNTL, [4, F_GETFD, 0, 0, 0, 0]), RET_ALLOW);
        // Reading, writing, or `O_PATH`, which Landlock holds to the roots
        // or which changes nothing, with any other flag.
        let o_path = 0o1000_0000;
        for flags in [0, 1, 2, o_path, o_path | 1, 0o2_0000_0000 | 0o404_0000 | 2] {
            let args = [4, 0, flags, 0, 0, 0];
            assert_eq!(share_call(OPENAT, args), RET_ALLOW, "{flags:#o}");
        }
        assert_eq!(share_call(READLINKAT, [4, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(share_call(READ, [SOCKET, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(share_call(WRITE, [SOCKET, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(share_call(WRITE, [STDERR, 0, 0, 0, 0, 0]), RET_ALLOW);
    }

    #[test]
    fn the_share_server_can_t_open_by_path_start_anything_or_write_a_file() {
        for number in [OPENAT2, 2, FORK, VFORK, EXECVE, EXECVEAT, CLONE, CLONE3] {
            assert_eq!(
                share_call(number, [4, 0, 0, 0, 0, 0]),
                RET_KILL_PROCESS,
                "{number}"
            );
        }
        assert_eq!(
            share_call(OPENAT, [AT_FDCWD, 0, 0, 0, 0, 0]),
            RET_KILL_PROCESS
        );
        // Access mode 3 asks for neither reading nor writing, so Landlock
        // would check nothing, whatever else the flags hold.
        for flags in [3, 0o1000_0003, 0o2_0000_0003, u32::MAX] {
            let args = [4, 0, flags, 0, 0, 0];
            assert_eq!(share_call(OPENAT, args), RET_KILL_PROCESS, "{flags:#o}");
        }
        assert_eq!(
            share_call(READLINKAT, [AT_FDCWD, 0, 0, 0, 0, 0]),
            RET_KILL_PROCESS
        );
        assert_eq!(share_call(READ, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(share_call(WRITE, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(
            share_call(MMAP, [0, 4096, 1, MAP_PRIVATE, 4, 0]),
            RET_KILL_PROCESS
        );
        assert_eq!(
            share_call(IOCTL, [0, TIOCSTI, 0, 0, 0, 0]),
            RET_KILL_PROCESS
        );
        assert_eq!(
            share_call(FCNTL, [4, F_SETOWN, 1, 0, 0, 0]),
            RET_KILL_PROCESS
        );
    }

    fn network_call(number: u32, args: [u32; 6]) -> u32 {
        run_rules(
            &network_rules(SOCKET as RawFd),
            AUDIT_ARCH_X86_64,
            number,
            args,
        )
    }

    #[test]
    fn the_network_process_passes_frames_on_its_link() {
        assert_eq!(network_call(READ, [SOCKET, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(network_call(WRITE, [SOCKET, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(network_call(WRITE, [STDERR, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(network_call(WRITE, [1, 0, 0, 0, 0, 0]), RET_ALLOW);
        for number in [CLOCK_GETTIME, CLOSE, FUTEX, MUNMAP, BRK, EXIT_GROUP] {
            assert_eq!(network_call(number, [0; 6]), RET_ALLOW, "{number}");
        }
        let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
        assert_eq!(
            network_call(MMAP, [0, 4096, 3, anonymous, u32::MAX, 0]),
            RET_ALLOW
        );
    }

    #[test]
    fn the_network_process_can_t_start_anything_open_a_file_or_make_a_socket() {
        const SOCKET_CALL: u32 = 41;
        const CONNECT: u32 = 42;
        const SENDTO: u32 = 44;
        const SENDMSG: u32 = 46;
        const SOCKETPAIR: u32 = 53;
        for number in [
            OPENAT,
            2,
            OPENAT2,
            FORK,
            VFORK,
            EXECVE,
            EXECVEAT,
            CLONE,
            CLONE3,
            SOCKET_CALL,
            CONNECT,
            SENDTO,
            SENDMSG,
            SOCKETPAIR,
            IOCTL,
            PREAD64,
        ] {
            assert_eq!(network_call(number, [0; 6]), RET_KILL_PROCESS, "{number}");
        }
        assert_eq!(network_call(READ, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(network_call(WRITE, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(
            network_call(MMAP, [0, 4096, 1, MAP_PRIVATE, 4, 0]),
            RET_KILL_PROCESS
        );
        assert_eq!(
            network_call(FCNTL, [4, F_SETOWN, 1, 0, 0, 0]),
            RET_KILL_PROCESS
        );
    }

    fn connecting_call(number: u32, args: [u32; 6]) -> u32 {
        run_rules(
            &network_connecting_rules(SOCKET as RawFd),
            AUDIT_ARCH_X86_64,
            number,
            args,
        )
    }

    #[test]
    fn the_network_process_waits_on_its_link() {
        assert_eq!(network_call(RECVFROM, [SOCKET, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(network_call(RECVFROM, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(network_call(POLL, [0; 6]), RET_ALLOW);
        assert_eq!(network_call(PPOLL, [0; 6]), RET_ALLOW);
    }

    #[test]
    fn a_connecting_network_process_makes_only_internet_sockets() {
        for domain in [AF_INET, AF_INET6] {
            assert_eq!(
                connecting_call(super::SOCKET, [domain, 1, 0, 0, 0, 0]),
                RET_ALLOW
            );
        }
        const AF_UNIX: u32 = 1;
        const AF_NETLINK: u32 = 16;
        const AF_PACKET: u32 = 17;
        for domain in [AF_UNIX, AF_NETLINK, AF_PACKET] {
            assert_eq!(
                connecting_call(super::SOCKET, [domain, 1, 0, 0, 0, 0]),
                RET_KILL_PROCESS,
                "{domain}"
            );
        }
        for number in [CONNECT, SENDTO, RECVFROM, GETSOCKOPT, SHUTDOWN, POLL] {
            assert_eq!(connecting_call(number, [4, 0, 0, 0, 0, 0]), RET_ALLOW);
        }
    }

    #[test]
    fn a_connecting_network_process_draws_on_the_random_source() {
        assert_eq!(connecting_call(GETRANDOM, [0, 32, 0, 0, 0, 0]), RET_ALLOW);
    }

    #[test]
    fn a_connecting_network_process_still_can_t_start_anything_or_open_a_file() {
        const SENDMSG: u32 = 46;
        const SOCKETPAIR: u32 = 53;
        const BIND: u32 = 49;
        const LISTEN: u32 = 50;
        for number in [
            OPENAT, 2, OPENAT2, FORK, VFORK, EXECVE, EXECVEAT, CLONE, CLONE3, SENDMSG, SOCKETPAIR,
            BIND, LISTEN, IOCTL, PREAD64,
        ] {
            assert_eq!(
                connecting_call(number, [0; 6]),
                RET_KILL_PROCESS,
                "{number}"
            );
        }
        assert_eq!(connecting_call(READ, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(connecting_call(WRITE, [4, 0, 0, 0, 0, 0]), RET_KILL_PROCESS);
        assert_eq!(
            connecting_call(MMAP, [0, 4096, 1, MAP_PRIVATE, 4, 0]),
            RET_KILL_PROCESS
        );
    }

    #[test]
    fn another_architecture_or_the_x32_table_kills() {
        assert_eq!(run(0x4000_0003, WRITE, [0; 6]), RET_KILL_PROCESS);
        assert_eq!(call(X32_SYSCALLS | WRITE, [0; 6]), RET_KILL_PROCESS);
    }
}
