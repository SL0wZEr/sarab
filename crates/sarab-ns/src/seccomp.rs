//! Scheduling-privilege shim, and the user-namespace ban.
//!
//! Android calls setpriority(<0), sched_setscheduler(FIFO/RR), ioprio_set(RT)
//! and setrlimit(NICE/RTPRIO/MEMLOCK) everywhere, and treats EPERM as fatal in
//! several places (system_server throws SecurityException on startup). Those
//! need CAP_SYS_NICE / CAP_SYS_RESOURCE in the *initial* user namespace, which
//! a rootless runtime can never have. This filter makes exactly those calls
//! return 0 without doing anything (`SECCOMP_RET_ERRNO` with errno 0 reads as
//! success), so Android believes it got its priorities and simply runs at
//! normal scheduling. Installed with CAP_SYS_ADMIN in our userns, so
//! no_new_privs is not required and file capabilities keep working.
//!
//! Matching: setpriority is faked unconditionally, because the kernel rejects
//! any *decrease* of nice (even 10 -> 0) without CAP_SYS_NICE and the
//! direction depends on state BPF cannot see. sched_setscheduler is faked only
//! for policy 1/2 (FIFO/RR), ioprio_set only when ioprio has bit 0x2000 set
//! (the RT class, class << 13), setrlimit only for resources 8/13/14
//! (MEMLOCK/NICE/RTPRIO). `off_arg` loads the low 32 bits of an argument from
//! `struct seccomp_data` (little-endian). The i386 block repeats the policy
//! with ia32 syscall numbers, since zygote_secondary and 32-bit apps use that
//! table; the jump that skips it takes its length from the block rather than
//! a hand count.
//!
//! The ban: `refuse_new_user_ns` fails unshare and clone with EPERM when their
//! flags ask for `CLONE_NEWUSER`, and clone3 with ENOSYS, since its flags sit
//! in a struct BPF cannot read; libc then falls back to clone, which is
//! checked (the same answer container runtimes give). sarab-ns also sets
//! `user.max_user_namespaces` to 0, but Android's init holds CAP_SYS_RESOURCE
//! in the namespace and could set it back, and nothing can remove a seccomp
//! filter once installed. An x86_64 syscall number with `X32_SYSCALL_BIT` set
//! is the x32 ABI, which nothing in Android uses and which would otherwise
//! reach unshare under a number no comparison here matches, so it gets ENOSYS
//! outright. Each block reloads the syscall number at its end,
//! which is what the blocks after it expect to find.
//!
//! `program` builds the filter and `load` installs it; the test loads it in a
//! forked child (under no_new_privs, which an unprivileged process needs) and
//! makes the real calls.
//!
//! x86_64 only (checked at runtime). The `unsafe` seccomp call passes a
//! pointer into the local filter vector; the kernel copies the program during
//! the call, so the vector only has to outlive the syscall.

use anyhow::{Result, bail};
use libc::{sock_filter, sock_fprog};

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
const I386_SETPRIORITY: u32 = 97;
const I386_SCHED_SETSCHEDULER: u32 = 156;
const I386_IOPRIO_SET: u32 = 289;
const I386_SETRLIMIT: u32 = 75;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const I386_UNSHARE: u32 = 310;
const I386_CLONE: u32 = 120;
const I386_CLONE3: u32 = 435;

const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
fn off_arg(n: u32) -> u32 {
    16 + 8 * n
}

const fn stmt(code: u16, k: u32) -> sock_filter {
    sock_filter { code, jt: 0, jf: 0, k }
}
const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JSET_K: u16 = 0x45;
const BPF_JMP_JGE_K: u16 = 0x35;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const BPF_RET_K: u16 = 0x06;

fn refuse_new_user_ns(unshare: u32, clone: u32, clone3: u32) -> Vec<sock_filter> {
    let flag = libc::CLONE_NEWUSER as u32;
    let eperm = stmt(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32);
    let mut f = Vec::new();
    for nr in [unshare, clone] {
        f.extend([
            stmt(BPF_LD_W_ABS, OFF_NR),
            jump(BPF_JMP_JEQ_K, nr, 0, 3),
            stmt(BPF_LD_W_ABS, off_arg(0)),
            jump(BPF_JMP_JSET_K, flag, 0, 1),
            eperm,
        ]);
    }
    f.extend([
        stmt(BPF_LD_W_ABS, OFF_NR),
        jump(BPF_JMP_JEQ_K, clone3, 0, 1),
        stmt(BPF_RET_K, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
        stmt(BPF_LD_W_ABS, OFF_NR),
    ]);
    f
}

pub fn install() -> Result<()> {
    if std::env::consts::ARCH != "x86_64" {
        bail!("seccomp shim only implemented for x86_64");
    }
    let mut f = program();
    if unsafe { load(&mut f) } != 0 {
        bail!("seccomp(SET_MODE_FILTER): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

unsafe fn load(f: &mut [sock_filter]) -> libc::c_long {
    let prog = sock_fprog { len: f.len() as u16, filter: f.as_mut_ptr() };
    unsafe { libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0u32, &prog as *const sock_fprog) }
}

fn program() -> Vec<sock_filter> {
    let ok = stmt(BPF_RET_K, SECCOMP_RET_ALLOW);
    let fake_ok = stmt(BPF_RET_K, SECCOMP_RET_ERRNO);

    let mut f: Vec<sock_filter> = vec![stmt(BPF_LD_W_ABS, OFF_ARCH)];
    let mut i386_block = refuse_new_user_ns(I386_UNSHARE, I386_CLONE, I386_CLONE3);
    i386_block.extend([
        jump(BPF_JMP_JEQ_K, I386_SETPRIORITY, 0, 1),
        fake_ok,
        jump(BPF_JMP_JEQ_K, I386_SCHED_SETSCHEDULER, 0, 4),
        stmt(BPF_LD_W_ABS, off_arg(1)),
        jump(BPF_JMP_JEQ_K, 1, 1, 0),
        jump(BPF_JMP_JEQ_K, 2, 0, 1),
        fake_ok,
        stmt(BPF_LD_W_ABS, OFF_NR),
        jump(BPF_JMP_JEQ_K, I386_IOPRIO_SET, 0, 3),
        stmt(BPF_LD_W_ABS, off_arg(2)),
        jump(BPF_JMP_JSET_K, 0x2000, 0, 1),
        fake_ok,
        stmt(BPF_LD_W_ABS, OFF_NR),
        jump(BPF_JMP_JEQ_K, I386_SETRLIMIT, 0, 5),
        stmt(BPF_LD_W_ABS, off_arg(0)),
        jump(BPF_JMP_JEQ_K, 13, 2, 0),
        jump(BPF_JMP_JEQ_K, 14, 1, 0),
        jump(BPF_JMP_JEQ_K, 8, 0, 1),
        fake_ok,
        ok,
    ]);
    f.push(jump(BPF_JMP_JEQ_K, AUDIT_ARCH_I386, 0, i386_block.len() as u8));
    f.extend(i386_block);
    f.extend([
        stmt(BPF_LD_W_ABS, OFF_ARCH),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        ok,
        stmt(BPF_LD_W_ABS, OFF_NR),
        jump(BPF_JMP_JGE_K, X32_SYSCALL_BIT, 0, 1),
        stmt(BPF_RET_K, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
    ]);
    f.extend(refuse_new_user_ns(libc::SYS_unshare as u32, libc::SYS_clone as u32, libc::SYS_clone3 as u32));
    f.extend([jump(BPF_JMP_JEQ_K, libc::SYS_setpriority as u32, 0, 1), fake_ok]);
    f.extend([
        jump(BPF_JMP_JEQ_K, libc::SYS_sched_setscheduler as u32, 0, 4),
        stmt(BPF_LD_W_ABS, off_arg(1)),
        jump(BPF_JMP_JEQ_K, 1, 1, 0),
        jump(BPF_JMP_JEQ_K, 2, 0, 1),
        fake_ok,
        stmt(BPF_LD_W_ABS, OFF_NR),
    ]);
    f.extend([
        jump(BPF_JMP_JEQ_K, libc::SYS_ioprio_set as u32, 0, 3),
        stmt(BPF_LD_W_ABS, off_arg(2)),
        jump(BPF_JMP_JSET_K, 0x2000, 0, 1),
        fake_ok,
        stmt(BPF_LD_W_ABS, OFF_NR),
    ]);
    f.extend([
        jump(BPF_JMP_JEQ_K, libc::SYS_setrlimit as u32, 0, 5),
        stmt(BPF_LD_W_ABS, off_arg(0)),
        jump(BPF_JMP_JEQ_K, 13, 2, 0),
        jump(BPF_JMP_JEQ_K, 14, 1, 0),
        jump(BPF_JMP_JEQ_K, 8, 0, 1),
        fake_ok,
    ]);
    f.push(ok);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_fakes_scheduling_and_refuses_user_namespaces() {
        let mut f = program();
        let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        match unsafe { libc::fork() } {
            0 => unsafe {
                let code = if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 || load(&mut f) != 0 {
                    10
                } else if libc::unshare(libc::CLONE_NEWUSER) != -1 || errno() != libc::EPERM {
                    11
                } else if libc::syscall(libc::SYS_clone3, 0usize, 0usize) != -1 || errno() != libc::ENOSYS {
                    12
                } else if libc::syscall(libc::SYS_unshare as libc::c_long | 0x4000_0000, libc::CLONE_NEWUSER) != -1
                    || errno() != libc::ENOSYS
                {
                    13
                } else if libc::setpriority(libc::PRIO_PROCESS, 0, -20) != 0 {
                    14
                } else {
                    0
                };
                libc::_exit(code)
            },
            -1 => panic!("fork: {}", std::io::Error::last_os_error()),
            pid => {
                let mut st = 0;
                assert_eq!(unsafe { libc::waitpid(pid, &mut st, 0) }, pid);
                assert!(libc::WIFEXITED(st), "the child died of a signal: {st:#x}");
                assert_eq!(libc::WEXITSTATUS(st), 0, "10 load, 11 unshare, 12 clone3, 13 x32, 14 setpriority");
            }
        }
    }
}
