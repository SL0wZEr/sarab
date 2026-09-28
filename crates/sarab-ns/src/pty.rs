//! A terminal of Android's own for `--enter`, so the host's never goes in.
//! A terminal fd handed to a process in the runtime is the desktop's
//! terminal: whoever holds it can read what is typed next, change its modes,
//! and, where the kernel still allows `TIOCSTI`, type into the shell that
//! started `sarab exec`. So when any of stdin, stdout or stderr is a terminal
//! (`host_ttys`), `open` takes a pseudo-terminal from the runtime's own devpts
//! instance (`PTMX`, which sarab-ns mounted with `newinstance`), after the
//! mount namespace is joined, and opens its other end with `TIOCGPTPEER`, the
//! kernel's way to reach it without looking a path up. The child `attach`es
//! it: a new session, it as the controlling terminal, and it in place of
//! each terminal fd, while a pipe or a file stays as it was. The master and
//! the peer are close-on-exec, so the command holds only the peer.
//!
//! The parent stays outside the runtime's pid namespace and `relay`s: typed
//! bytes to the master, the master's output to the host's first terminal
//! among stdout and stderr (or nowhere, when both are redirected). Input
//! waits in `pending` while the master is full, and stdin is not read
//! meanwhile; the master is non-blocking, so neither direction can block the
//! other. The host terminal is raw while that runs (`Raw`, restored when
//! dropped), so Ctrl-C and Ctrl-Z reach the pseudo-terminal and Android's own
//! line discipline turns them into signals there.
//!
//! Nothing here runs on a timer. `Signals` blocks the signals the relay acts
//! on and reads them from a signalfd, polled with stdin and the master, so the
//! loop sleeps until one of the three has something: SIGCHLD when the command
//! stops or ends (checked once before the first poll too, since a SIGCHLD from
//! before the block was discarded), SIGWINCH to copy the window size, and a
//! signal that would end us (`ENDING`), on which the terminal is put back, the
//! command gets the hangup a closed terminal gives, and we end by that signal
//! after all (`die_of`, which unblocks it first), so a killed `sarab exec`
//! never leaves the desktop's terminal raw. A command that stops is mirrored:
//! the terminal goes back to normal, we stop, and on resume raw mode returns
//! and the command is continued. Once it has ended, what the master still
//! holds is drained for `DRAIN` at most (`drain`), since a background process
//! can keep the peer open for ever. The blocked mask is ours alone: the
//! command was forked before it was set.

use anyhow::{Context, Result};
use nix::sys::signal::{SigSet, Signal, kill};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpid, setsid};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

const PTMX: &str = "/dev/pts/ptmx";
const DRAIN: Duration = Duration::from_millis(200);

const ENDING: &[Signal] = &[Signal::SIGTERM, Signal::SIGHUP, Signal::SIGINT, Signal::SIGQUIT];

pub fn host_ttys() -> [bool; 3] {
    [0, 1, 2].map(|fd| unsafe { libc::isatty(fd) } == 1)
}

pub struct Pty {
    pub master: OwnedFd,
    pub peer: OwnedFd,
}

fn check(r: libc::c_int, what: &str) -> Result<libc::c_int> {
    if r < 0 { Err(std::io::Error::last_os_error()).context(what.to_string()) } else { Ok(r) }
}

pub fn open(ttys: [bool; 3]) -> Result<Pty> {
    let path = std::ffi::CString::new(PTMX)?;
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let m = check(unsafe { libc::open(path.as_ptr(), flags) }, PTMX)?;
    let master = unsafe { OwnedFd::from_raw_fd(m) };
    check(unsafe { libc::unlockpt(m) }, "unlockpt")?;
    let p = check(
        unsafe { libc::ioctl(m, libc::TIOCGPTPEER, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) },
        "TIOCGPTPEER",
    )?;
    let peer = unsafe { OwnedFd::from_raw_fd(p) };
    copy_size(ttys, m);
    Ok(Pty { master, peer })
}

fn copy_size(ttys: [bool; 3], master: RawFd) {
    let Some(from) = (0..3).find(|&fd| ttys[fd]) else { return };
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(from as RawFd, libc::TIOCGWINSZ, &mut ws) } == 0 {
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
    }
}

pub fn attach(peer: &OwnedFd, ttys: [bool; 3]) -> Result<()> {
    setsid().context("setsid")?;
    check(unsafe { libc::ioctl(peer.as_raw_fd(), libc::TIOCSCTTY, 0) }, "TIOCSCTTY")?;
    for fd in (0..3).filter(|&fd| ttys[fd]) {
        check(unsafe { libc::dup2(peer.as_raw_fd(), fd as RawFd) }, "dup2")?;
    }
    Ok(())
}

struct Raw {
    saved: Option<libc::termios>,
}

impl Raw {
    fn on(stdin_tty: bool) -> Self {
        if !stdin_tty {
            return Self { saved: None };
        }
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return Self { saved: None };
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &t) };
        Self { saved: Some(saved) }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(t) = &self.saved {
            unsafe { libc::tcsetattr(0, libc::TCSADRAIN, t) };
        }
    }
}

pub struct Signals {
    fd: SignalFd,
}

impl Signals {
    pub fn take(extra: &[Signal]) -> Result<Self> {
        let mut mask = SigSet::empty();
        for &sig in ENDING.iter().chain([&Signal::SIGCHLD]).chain(extra) {
            mask.add(sig);
        }
        mask.thread_block().context("block signals")?;
        let fd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK).context("signalfd")?;
        Ok(Self { fd })
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn pending(&mut self) -> Vec<Signal> {
        let mut got = Vec::new();
        while let Ok(Some(info)) = self.fd.read_signal() {
            if let Ok(sig) = Signal::try_from(info.ssi_signo as i32) {
                got.push(sig);
            }
        }
        got
    }
}

pub fn die_of(sig: Signal) -> ! {
    unsafe { libc::signal(sig as libc::c_int, libc::SIG_DFL) };
    let _ = SigSet::all().thread_unblock();
    let _ = nix::sys::signal::raise(sig);
    std::process::exit(128 + sig as i32)
}

fn read_some(fd: RawFd, buf: &mut [u8]) -> Option<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    (n > 0).then_some(n as usize)
}

fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n <= 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        data = &data[n as usize..];
    }
}

pub fn relay(master: OwnedFd, ttys: [bool; 3], child: Pid) -> Result<WaitStatus> {
    let m = master.as_raw_fd();
    let out = (1..3).find(|&fd| ttys[fd]).map(|fd| fd as RawFd);
    let mut signals = Signals::take(&[Signal::SIGWINCH])?;
    let mut raw = Raw::on(ttys[0]);
    let mut stdin_open = ttys[0];
    let mut master_open = true;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut check_child = true;
    loop {
        if check_child {
            check_child = false;
            match waitpid(child, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Stopped(..)) => {
                    drop(raw);
                    let _ = kill(getpid(), Signal::SIGSTOP);
                    raw = Raw::on(ttys[0]);
                    copy_size(ttys, m);
                    let _ = kill(child, Signal::SIGCONT);
                }
                Ok(st @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => {
                    drain(m, master_open, out, &mut buf);
                    return Ok(st);
                }
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(e) => return Err(e).context("waitpid"),
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: if stdin_open && pending.is_empty() { 0 } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if master_open { m } else { -1 },
                events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
            libc::pollfd { fd: signals.raw(), events: libc::POLLIN, revents: 0 },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) } <= 0 {
            continue;
        }
        if fds[2].revents != 0 {
            for sig in signals.pending() {
                match sig {
                    Signal::SIGCHLD => check_child = true,
                    Signal::SIGWINCH => copy_size(ttys, m),
                    ending => {
                        drop(raw);
                        let _ = kill(child, Signal::SIGHUP);
                        die_of(ending);
                    }
                }
            }
        }
        if fds[0].revents != 0 {
            match read_some(0, &mut buf) {
                Some(k) => pending.extend_from_slice(&buf[..k]),
                None => stdin_open = false,
            }
        }
        if fds[1].revents & libc::POLLOUT != 0 && !pending.is_empty() {
            let k = unsafe { libc::write(m, pending.as_ptr().cast(), pending.len()) };
            if k > 0 {
                pending.drain(..k as usize);
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_some(m, &mut buf) {
                Some(k) => {
                    if let Some(o) = out {
                        write_all(o, &buf[..k]);
                    }
                }
                None => master_open = false,
            }
        }
    }
}

fn drain(m: RawFd, mut open: bool, out: Option<RawFd>, buf: &mut [u8]) {
    let t0 = Instant::now();
    while open && t0.elapsed() < DRAIN {
        let mut p = libc::pollfd { fd: m, events: libc::POLLIN, revents: 0 };
        let left = DRAIN.saturating_sub(t0.elapsed()).as_millis().max(1) as i32;
        if unsafe { libc::poll(&mut p, 1, left) } <= 0 {
            break;
        }
        match read_some(m, buf) {
            Some(k) => {
                if let Some(o) = out {
                    write_all(o, &buf[..k]);
                }
            }
            None => open = false,
        }
    }
}
