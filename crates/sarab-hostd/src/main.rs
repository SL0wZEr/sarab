//! sarab-hostd — the host half of the runtime.
//!
//! Android expects a set of binder services to exist on the host side:
//! clipboard, notifications, a package/user monitor and a hardware control
//! channel. The image's framework patches call them over binder; this serves
//! that contract from Rust, talking to the namespace's binder device from
//! outside the namespace.
//!
//! `--icon-helper PID PACKAGE` is a private mode: the icon extractor
//! re-executes this binary inside the runtime's namespaces, so `main` checks
//! for it before anything else, and before any thread is spawned (entering
//! namespaces requires a single-threaded process). `--wait`'s seconds argument
//! is optional, so the parser only consumes the next token when it parses as a
//! number.
//!
//! `add_service` is a transaction to handle 0, so Android's servicemanager has
//! to own the context manager slot first; `wait_for_servicemanager` polls for
//! that, the connect included, since handle 0 is a dead object until then (a
//! connect outside the loop lost that race after a `sarab restart`).
//! servicemanager starts very early in init, well before system_server binds
//! our services. The binder thread pool is joined for the life of the process;
//! if it ever returns, hostd fails (a non-zero exit, which start.rs treats as a
//! crash) unless it relays the display, which it then keeps doing.
//! `exit_with_runtime` retries its poll only when a signal interrupted it, and
//! stops watching on any other error rather than spinning. That wait and
//! `wait_for_runtime` are the two polls left in hostd, and on purpose: nothing
//! tells a host process that a pivoted sarab-ns has appeared, or that a binder
//! context manager now answers on handle 0, so looking is the only way to know.
//! Once servicemanager answers, everything else is told rather than looked for
//! (theme.rs). `exit_with_runtime` watches the runtime's init through a pidfd
//! (no periodic wakeups) and exits with it: our services are bound by that
//! Android's system_server, so after it is gone we would only hold an fd to a
//! dead binderfs.
//!
//! `--wayland-fd` and `--wayland-upstream` make it Android's display too
//! (wayland.rs): the first is a listening socket `sarab start` inherits to us,
//! the second the compositor's socket. The relay starts before anything else,
//! and from then on the process is display-critical: if it exits, every Android
//! window goes with it. So `exit_with_runtime` is armed as soon as the runtime
//! is found, before the services, and a service that fails to register while
//! relaying is logged, and the process stays up for the display until the
//! runtime exits.
//!
//! `theme` is not a binder service but a client: it makes Android's dark theme
//! follow the desktop's (theme.rs). It is listed with the services it starts
//! beside, and `--no-theme` turns it off like them, leaving Android's own
//! setting alone.

mod apk;
mod clipboard;
mod clipboard_native;
mod hardware;
mod notifications;
mod theme;
mod usermonitor;
mod wayland;
mod wire;

use anyhow::{Context, Result, bail};
use rsbinder::*;
use sarab_runtime::{add_service, check_service, connect, find_runtime};
use std::os::fd::RawFd;
use std::path::PathBuf;
use wire::logln;

const USAGE: &str = "\
sarab-hostd [options]
  --binder PATH        binder device (default: autodetect the running runtime)
  --pid N              runtime init pid (default: autodetect)
  --launcher PATH      sarab binary used in .desktop Exec lines (`sarab app launch`)
  --icons DIR          where app icons are written (default: $XDG_DATA_HOME/sarab/icons)
  --allow-shutdown     let Android's power menu stop the runtime
  --no-clipboard       skip the clipboard service
  --clipboard BACKEND  force a clipboard backend: native (wlr-data-control,
                       the default when the compositor has it) or
                       wl-clipboard (shell out to wl-copy/wl-paste)
  --no-notifications   skip the notification service
  --no-usermonitor     skip desktop-entry management
  --no-hardware        skip the hardware service
  --no-theme           leave Android's dark theme alone instead of following
                       the desktop's
  --sync               write desktop entries for installed apps at startup
  --wait [SECONDS]     wait for the runtime and its servicemanager (default 120)
  --wayland-fd FD      relay Android's display: accept on this inherited
                       listening socket (needs --wayland-upstream)
  --wayland-upstream PATH  the compositor's socket the relay connects to

Android binds these services once, during system server startup, so the
daemon must be registered before Android finishes booting. Start it with
--wait alongside the runtime rather than afterwards.";

struct Opts {
    binder: Option<PathBuf>,
    pid: Option<u32>,
    launcher: Option<PathBuf>,
    icons: Option<PathBuf>,
    allow_shutdown: bool,
    clipboard: bool,
    clipboard_backend: clipboard::Backend,
    notifications: bool,
    usermonitor: bool,
    hardware: bool,
    theme: bool,
    sync: bool,
    wait: Option<u64>,
    wayland: Option<(RawFd, PathBuf)>,
}

fn parse() -> Result<Opts> {
    parse_args(std::env::args().skip(1))
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Opts> {
    let mut o = Opts {
        binder: None,
        pid: None,
        launcher: None,
        icons: None,
        allow_shutdown: false,
        clipboard: true,
        clipboard_backend: clipboard::Backend::Auto,
        notifications: true,
        usermonitor: true,
        hardware: true,
        theme: true,
        sync: false,
        wait: None,
        wayland: None,
    };
    let (mut wayland_fd, mut upstream) = (None, None);
    let mut it = args.peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--binder" => o.binder = Some(it.next().context("--binder needs a path")?.into()),
            "--pid" => o.pid = Some(it.next().context("--pid needs a number")?.parse()?),
            "--launcher" => o.launcher = Some(it.next().context("--launcher needs a path")?.into()),
            "--icons" => o.icons = Some(it.next().context("--icons needs a directory")?.into()),
            "--allow-shutdown" => o.allow_shutdown = true,
            "--no-clipboard" => o.clipboard = false,
            "--clipboard" => {
                o.clipboard_backend = clipboard::Backend::parse(&it.next().context("--clipboard needs a backend")?)?;
            }
            "--no-notifications" => o.notifications = false,
            "--no-usermonitor" => o.usermonitor = false,
            "--no-hardware" => o.hardware = false,
            "--no-theme" => o.theme = false,
            "--sync" => o.sync = true,
            "--wait" => {
                o.wait = Some(match it.peek().and_then(|v| v.parse().ok()) {
                    Some(n) => {
                        it.next();
                        n
                    }
                    None => 120,
                });
            }
            "--wayland-fd" => wayland_fd = Some(it.next().context("--wayland-fd needs a number")?.parse()?),
            "--wayland-upstream" => upstream = Some(it.next().context("--wayland-upstream needs a path")?.into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0)
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    o.wayland = match (wayland_fd, upstream) {
        (Some(fd), Some(path)) => Some((fd, path)),
        (None, None) => None,
        _ => bail!("--wayland-fd and --wayland-upstream go together"),
    };
    Ok(o)
}

fn default_launcher() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("sarab")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("sarab"))
}

fn wait_for_runtime(secs: u64) -> Result<(u32, PathBuf)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Ok(found) = find_runtime() {
            return Ok(found);
        }
        if std::time::Instant::now() >= deadline {
            bail!("no runtime appeared within {secs}s");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn wait_for_servicemanager(binder: &std::path::Path, secs: u64) -> Result<SIBinder> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let probe = connect(binder).and_then(|sm| check_service(&sm, "sarab.probe.nonexistent").map(|_| sm));
        match probe {
            Ok(sm) => return Ok(sm),
            Err(e) if std::time::Instant::now() >= deadline => {
                if secs == 0 {
                    bail!("Android's servicemanager is not answering: {e:#}");
                }
                bail!("Android's servicemanager did not answer within {secs}s: {e:#}");
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--icon-helper") {
        let pid: u32 = argv.get(2).context("--icon-helper needs a pid")?.parse()?;
        let package = argv.get(3).context("--icon-helper needs a package")?;
        return apk::icon_helper_main(pid, package);
    }

    let opts = parse()?;
    let relaying = opts.wayland.is_some();
    if let Some((fd, upstream)) = &opts.wayland {
        wayland::serve(wayland::listener(*fd)?, upstream.clone());
    }
    let (pid, binder) = match (opts.pid, opts.binder.clone()) {
        (Some(p), Some(b)) => (p, b),
        _ => {
            let (p, b) = match opts.wait {
                Some(secs) => wait_for_runtime(secs)?,
                None => find_runtime().context("locating the running runtime")?,
            };
            (opts.pid.unwrap_or(p), opts.binder.clone().unwrap_or(b))
        }
    };
    logln!("binder {} (runtime pid {pid})", binder.display());
    exit_with_runtime(pid);
    match register(&opts, pid, binder) {
        Ok(()) => {
            let why = match ProcessState::join_thread_pool() {
                Ok(()) => "ended".to_string(),
                Err(e) => format!("failed: {e:?}"),
            };
            if !relaying {
                bail!("the binder thread pool {why}");
            }
            logln!("the binder thread pool {why}; still relaying the display until the runtime exits");
        }
        Err(e) if relaying => logln!("{e:#}; still relaying the display until the runtime exits"),
        Err(e) => return Err(e),
    }
    if relaying {
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

fn register(opts: &Opts, pid: u32, binder: PathBuf) -> Result<()> {
    let sm = wait_for_servicemanager(&binder, opts.wait.unwrap_or(0))?;
    ProcessState::start_thread_pool();

    let gate = wire::Gate::for_runtime(pid)?;
    let mut registered: Vec<&str> = Vec::new();

    if opts.clipboard {
        match clipboard::open(opts.clipboard_backend) {
            Ok(backend) => {
                let svc = clipboard::ClipboardService::new(backend, gate);
                add_service(&sm, clipboard::SERVICE_NAME, &Binder::new(svc).as_binder())?;
                registered.push(clipboard::SERVICE_NAME);
            }
            Err(e) => logln!("clipboard: disabled ({e})"),
        }
    }

    if opts.notifications {
        match notifications::Notifications::new(binder.clone(), gate) {
            Ok(svc) => {
                add_service(&sm, notifications::SERVICE_NAME, &Binder::new(svc).as_binder())?;
                registered.push(notifications::SERVICE_NAME);
            }
            Err(e) => logln!("notifications: disabled ({e})"),
        }
    }

    let (unlocked_tx, unlocked) = std::sync::mpsc::channel();
    let mut unlock_told = false;
    if opts.usermonitor {
        let launcher = opts.launcher.clone().unwrap_or_else(default_launcher);
        match usermonitor::UserMonitor::new(binder.clone(), pid, launcher, opts.icons.clone()) {
            Ok(monitor) => {
                let mut svc = usermonitor::serve(monitor, opts.sync, gate)?;
                svc.tell_on_unlock(unlocked_tx);
                unlock_told = true;
                add_service(&sm, usermonitor::SERVICE_NAME, &Binder::new(svc).as_binder())?;
                registered.push(usermonitor::SERVICE_NAME);
            }
            Err(e) => logln!("usermonitor: disabled ({e})"),
        }
    }

    if opts.hardware {
        let svc = hardware::Hardware::new(pid, opts.allow_shutdown, gate);
        add_service(&sm, hardware::SERVICE_NAME, &Binder::new(svc).as_binder())?;
        registered.push(hardware::SERVICE_NAME);
    }

    if opts.theme && !unlock_told {
        logln!("theme: disabled (it waits for usermonitor to say Android is ready, and usermonitor is off)");
    } else if opts.theme {
        match theme::follow(sm.clone(), unlocked) {
            Ok(()) => registered.push(theme::SERVICE_NAME),
            Err(e) => logln!("theme: disabled ({e:#})"),
        }
    }

    if registered.is_empty() {
        bail!("no services could be started");
    }
    logln!("serving: {}", registered.join(", "));
    Ok(())
}

fn exit_with_runtime(pid: u32) {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        logln!("pidfd_open({pid}) failed; will not exit with the runtime");
        return;
    }
    std::thread::spawn(move || {
        let mut pfd = libc::pollfd { fd: fd as i32, events: libc::POLLIN, revents: 0 };
        while unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return logln!("watching runtime pid {pid} failed ({e}); will not exit with it");
            }
        }
        logln!("runtime pid {pid} exited; shutting down");
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Opts> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn clipboard_backend_flag() {
        assert_eq!(args(&[]).unwrap().clipboard_backend, clipboard::Backend::Auto);
        assert_eq!(args(&["--clipboard", "native"]).unwrap().clipboard_backend, clipboard::Backend::Native);
        assert_eq!(
            args(&["--clipboard", "wl-clipboard", "--sync"]).unwrap().clipboard_backend,
            clipboard::Backend::WlClipboard
        );
        assert!(args(&["--clipboard"]).is_err());
        assert!(args(&["--clipboard", "xclip"]).is_err());
        let o = args(&["--no-clipboard", "--wait", "5"]).unwrap();
        assert!(!o.clipboard);
        assert_eq!(o.wait, Some(5));
    }

    #[test]
    fn the_display_relay_needs_both_its_socket_and_the_compositors() {
        assert!(args(&[]).unwrap().wayland.is_none());
        let o = args(&["--wait", "--wayland-fd", "3", "--wayland-upstream", "/run/user/1000/wayland-1"]).unwrap();
        assert_eq!((o.wait, o.wayland), (Some(120), Some((3, PathBuf::from("/run/user/1000/wayland-1")))));
        assert!(args(&["--wayland-fd", "3"]).is_err());
        assert!(args(&["--wayland-upstream", "/x"]).is_err());
        assert!(args(&["--wayland-fd", "x", "--wayland-upstream", "/x"]).is_err());
    }
}
