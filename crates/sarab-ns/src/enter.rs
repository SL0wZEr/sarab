//! `sarab-ns --enter PID [--as UID:GID[:G1,G2...]] -- CMD [ARGS...]`: run a
//! command inside a running runtime, as `sarab exec` and every `pm`/`cmd` call
//! do. It is nsenter plus the one thing nsenter cannot do, supplementary
//! groups: Android gates on them (the resolver refuses a caller without
//! `inet`, and hidepid hides every process from one without `readproc`), so a
//! shell without adbd's groups is not the shell apps and scripts expect.
//!
//! The user namespace is joined first: that is what grants the capabilities
//! the other setns calls need, and our own credentials stay (the caller is
//! uid 0 in there, as `--preserve-credentials` has it). Then cgroup, ipc,
//! uts, net, pid and mount, nsenter's order, mount last because it moves the
//! root and cwd to the runtime's. The pid namespace applies only to children,
//! hence the fork; the parent waits like nsenter's `continue_as_child`, stops
//! when the child stops and passes on its exit status or signal. That signal
//! is often SIGKILL, which every process in the pid namespace gets when
//! Android stops under a command; its disposition cannot be set, so that
//! failure is ignored rather than returned, which printed "Error: EINVAL" and
//! exited 1 instead of dying of the same signal. The child
//! drops to `--as` (groups, then gid, then uid, the order that keeps the right
//! to change each) and execs with an empty environment: the caller passes
//! Android's own through `env -i`, and nothing of the host's gets in.
//!
//! Nor does the host's terminal. When any of our stdin, stdout or stderr is
//! one, a pseudo-terminal is opened from the runtime's devpts once the mount
//! namespace is joined (pty.rs); the child makes it its controlling terminal
//! in place of ours, and hands it to `--as` (`fchown`) as adbd's shell has
//! it, before dropping privileges. The parent then relays instead of waiting,
//! and `finish` ends us the way the command ended in both cases. When none of
//! the three is a terminal (a piped `sarab exec`, every `pm` call) the child
//! still starts a session of its own, so our controlling terminal, which
//! `/dev/tty` would open, does not come along either. Being out of our
//! session, it no longer gets the terminal's Ctrl-C with us, so the plain wait
//! passes on every signal that would end us (pty.rs's `Signals`, which also
//! brings SIGCHLD, so the wait sleeps until the command stops or ends), and we
//! end the way the command ends. It passes them to the command's whole process
//! group (`killpg`), which the child leads since its `setsid`, as the SIGCONT
//! after a stop is: `sh -c 'logcat | grep x'` would otherwise die as `sh`
//! alone, and the pipeline would live on inside Android, where freeze.rs
//! counts it as a host session and never pauses. A signal that comes before
//! the child's `setsid` finds no such group (ESRCH) and goes to the child
//! alone, which is then all there is.

use crate::pty;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, setns};
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Gid, Uid, execve, fchown, fork, getpid, setgroups, setresgid, setresuid, setsid};
use std::ffi::CString;
use std::fs::File;

const NAMESPACES: &[(&str, CloneFlags)] = &[
    ("cgroup", CloneFlags::CLONE_NEWCGROUP),
    ("ipc", CloneFlags::CLONE_NEWIPC),
    ("uts", CloneFlags::CLONE_NEWUTS),
    ("net", CloneFlags::CLONE_NEWNET),
    ("pid", CloneFlags::CLONE_NEWPID),
    ("mnt", CloneFlags::CLONE_NEWNS),
];

#[derive(Debug, PartialEq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

pub fn parse_ids(s: &str) -> Result<Ids> {
    let mut f = s.splitn(3, ':');
    let num = |v: &str| v.parse::<u32>().with_context(|| format!("--as {s}: {v:?} is not a number"));
    let uid = num(f.next().unwrap_or_default())?;
    let gid = num(f.next().context("--as needs UID:GID")?)?;
    let groups = match f.next() {
        Some("") | None => vec![],
        Some(g) => g.split(',').map(num).collect::<Result<_>>()?,
    };
    Ok(Ids { uid, gid, groups })
}

pub fn run(pid: u32, ids: Option<Ids>, argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("--enter needs a command");
    }
    let open = |ns: &str| File::open(format!("/proc/{pid}/ns/{ns}")).with_context(|| format!("open ns/{ns} of {pid}"));
    let user = open("user")?;
    let others: Vec<(File, CloneFlags, &str)> =
        NAMESPACES.iter().map(|&(n, f)| Ok((open(n)?, f, n))).collect::<Result<_>>()?;
    setns(&user, CloneFlags::CLONE_NEWUSER).context("setns user")?;
    for (fd, flag, name) in &others {
        setns(fd, *flag).with_context(|| format!("setns {name}"))?;
    }
    drop((user, others));

    let ttys = pty::host_ttys();
    let pty = if ttys.contains(&true) { Some(pty::open(ttys)?) } else { None };
    let cargs: Vec<CString> = argv.iter().map(|a| CString::new(a.as_str())).collect::<Result<_, _>>()?;
    match unsafe { fork() }? {
        ForkResult::Child => {
            match &pty {
                Some(p) => {
                    pty::attach(&p.peer, ttys)?;
                    if let Some(ids) = &ids {
                        fchown(&p.peer, Some(Uid::from_raw(ids.uid)), None).context("fchown the terminal")?;
                    }
                }
                None => {
                    setsid().context("setsid")?;
                }
            }
            if let Some(ids) = ids {
                let groups: Vec<Gid> = ids.groups.iter().map(|&g| Gid::from_raw(g)).collect();
                setgroups(&groups).context("setgroups")?;
                let (g, u) = (Gid::from_raw(ids.gid), Uid::from_raw(ids.uid));
                setresgid(g, g, g).context("setresgid")?;
                setresuid(u, u, u).context("setresuid")?;
            }
            execve::<CString, CString>(&cargs[0], &cargs, &[]).with_context(|| format!("execve {}", argv[0]))?;
            unreachable!()
        }
        ForkResult::Parent { child } => match pty {
            Some(p) => {
                drop(p.peer);
                finish(pty::relay(p.master, ttys, child)?)
            }
            None => {
                let mut signals = pty::Signals::take(&[])?;
                loop {
                    match waitpid(child, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)) {
                        Err(nix::errno::Errno::EINTR) | Ok(WaitStatus::StillAlive) => {}
                        Err(e) => return Err(e).context("waitpid"),
                        Ok(WaitStatus::Stopped(..)) => {
                            let _ = kill(getpid(), Signal::SIGSTOP);
                            let _ = killpg(child, Signal::SIGCONT);
                        }
                        Ok(st @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => finish(st),
                        Ok(_) => {}
                    }
                    let mut p = libc::pollfd { fd: signals.raw(), events: libc::POLLIN, revents: 0 };
                    if unsafe { libc::poll(&mut p, 1, -1) } <= 0 {
                        continue;
                    }
                    for sig in signals.pending().into_iter().filter(|s| *s != Signal::SIGCHLD) {
                        if killpg(child, sig) == Err(nix::errno::Errno::ESRCH) {
                            let _ = kill(child, sig);
                        }
                    }
                }
            }
        },
    }
}

fn finish(st: WaitStatus) -> ! {
    match st {
        WaitStatus::Signaled(_, s, _) => pty::die_of(s),
        WaitStatus::Exited(_, c) => std::process::exit(c),
        _ => std::process::exit(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_take_an_optional_group_list() {
        assert_eq!(parse_ids("0:0").unwrap(), Ids { uid: 0, gid: 0, groups: vec![] });
        assert_eq!(parse_ids("2000:2000:").unwrap(), Ids { uid: 2000, gid: 2000, groups: vec![] });
        assert_eq!(parse_ids("2000:2000:3003,3009").unwrap(), Ids { uid: 2000, gid: 2000, groups: vec![3003, 3009] });
        assert!(parse_ids("2000").is_err());
        assert!(parse_ids("shell:2000").is_err());
        assert!(parse_ids("2000:2000:inet").is_err());
    }
}
