//! Confines this process to the system calls that running a guest needs, with
//! a seccomp filter on every thread. Anything else kills the process, so code
//! a guest got running in the VMM gets no second try.
//!
//! What is left can't start a process or a program, or reach a host file:
//! every call that takes a path is missing, and memory can be mapped only
//! anonymously, so a file left open can't be read either. Calls that could
//! reach past this process are narrowed by their arguments, as the kernel
//! reads them.

use std::ffi::{c_int, c_long, c_ulong};
use std::io;

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
const RETURN: u16 = 0x06;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const X32_SYSCALLS: u32 = 0x4000_0000;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARGS: u32 = 16;

const ENOSYS: u32 = 38;
const CLONE_THREAD: u32 = 0x0001_0000;
const F_GETFD: u32 = 1;
const MAP_ANONYMOUS: u32 = 0x20;

// x86_64 system call numbers.
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
const EXIT_GROUP: u32 = 231;
const TGKILL: u32 = 234;
const SET_ROBUST_LIST: u32 = 273;
const RSEQ: u32 = 334;
const CLONE3: u32 = 435;

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
    /// Answered with this error, without running it.
    Refuse(u32, u32),
}

/// The calls that the VMM, its watchdog, and the output relay make while a
/// guest runs, on glibc and musl. `pid` and `tid` are this process's and the
/// vCPU thread's, the only targets the watchdog signals.
fn rules(pid: u32, tid: u32) -> Vec<Rule> {
    use Rule::{Allow, AllowIf, AllowIfSet, Refuse};
    vec![
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
pub fn confine() -> io::Result<()> {
    let pid = std::process::id();
    // SAFETY: gettid takes no arguments and can't fail.
    let tid = unsafe { syscall(GETTID as c_long) } as u32;
    let instructions = compile(&rules(pid, tid));
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
    const OPENAT: u32 = 257;
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
        let program = compile(&rules(PID, TID));
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
                            assert_eq!(index % 8, 0, "a load of an argument's high half");
                            args[(index / 8) as usize]
                        }
                        offset => panic!("a load at {offset}"),
                    }
                }
                JUMP_EQUAL => pc += jump(accumulator == instruction.value),
                JUMP_AT_LEAST => pc += jump(accumulator >= instruction.value),
                JUMP_SET => pc += jump(accumulator & instruction.value != 0),
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
    fn another_architecture_or_the_x32_table_kills() {
        assert_eq!(run(0x4000_0003, WRITE, [0; 6]), RET_KILL_PROCESS);
        assert_eq!(call(X32_SYSCALLS | WRITE, [0; 6]), RET_KILL_PROCESS);
    }
}
