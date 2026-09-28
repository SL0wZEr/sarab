//! Getting a runtime up, and down, from a command that returns.
//!
//! `sarab start --foreground` (start.rs) is the runtime's owner: it stays in
//! the foreground for the whole life of Android and takes the helpers with it
//! when it ends. Everything a person types wants the opposite -- to get the
//! prompt back once Android is up -- so `sarab start`, `restart`, and the
//! commands that imply a runtime (`app launch`, `app install`, ...) come
//! through here, which starts the foreground owner somewhere else and waits.
//!
//! *Somewhere else* is the systemd user unit when one is installed, and a
//! detached process otherwise (own session via setsid; the same binary, so it
//! resolves the same directories; it is never waited on, init reaps it). There
//! used to be two routes that did not know about each other: a launcher click
//! spawned a detached session, and `systemctl --user start sarab` on top of it
//! booted a second runtime, which systemd could not see and `systemctl status`
//! called inactive. Now there is one decision, made here, and the foreground
//! owner refuses to start a second runtime besides.
//!
//! `ensure_running` serialises on a flock of `sarab-start.lock`, not a pid
//! file: the kernel drops it on every exit path, and two launcher clicks in a
//! row must make one runtime. Before it starts anything it requires pasta
//! (net.rs), so a missing one is said in the terminal or the launcher's
//! notification rather than as a runtime that exited. "Booted" means the
//! platform service answers with `sys.boot_completed=1`; the binder device
//! exists as soon as sarab-ns pivots, long before anything is registered on it.
//! `BOOT_DEADLINE` is 120 s against a measured 1.5 s cold boot: hitting it
//! means something is wrong. `refit` runs before a command opens a window:
//! Android sizes its display and density once, when it starts (screen.rs), so
//! after a scale change or a new screen, the size the running Android was
//! started with (its prop file) no longer matches what `sarab start` would pick
//! now. It restarts Android then, about two seconds, so the app is laid out
//! anew at the right density rather than stretched; but only with no Android
//! window open and no host session (freeze.rs's `host_sessions`: a `sarab exec`
//! shell, a `logs -f`), either of which a restart would kill. Otherwise it only
//! says so. A frozen runtime has no window by definition; a running one is
//! asked with a `getprop` inside the namespace, never binder, because rsbinder
//! binds a process to one binder device for good and the launch that follows
//! must reach the new runtime's. A screen that cannot be measured is never a
//! reason to restart.
//!
//! `EXIT_ALREADY_RUNNING` and `EXIT_UNFIT` are listed in the unit's
//! RestartPreventExitStatus=, so systemd does not retry a start that can never
//! succeed: a runtime already there, or a host that fails a check no retry
//! changes (not set up, no pasta, no GPU Android can use; start.rs). Those
//! host checks also run here, before the unit is started, so the refusal is
//! said where the person typed the command. While waiting for the boot, a
//! unit that failed or is waiting to be restarted (`unit_failing`) ends the
//! wait at once: systemd reports a pending restart as "activating", which
//! `unit_active` rightly counts, since `sarab stop` must still stop it. `stop(direct)`
//! with `direct` is the unit's own ExecStop and must not call back into
//! systemd.

use crate::android::User;
use crate::paths::Dirs;
use crate::screen::from_props;
use anyhow::{Context, Result, anyhow, bail};
use sarab_runtime::{Platform, find_binder, find_runtime, is_frozen};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const UNIT: &str = "sarab.service";

pub const BOOT_DEADLINE: Duration = Duration::from_secs(120);

pub const EXIT_ALREADY_RUNNING: i32 = 4;
pub const EXIT_UNFIT: i32 = 5;

fn systemctl(args: &[&str]) -> Option<String> {
    let o = Command::new("systemctl").arg("--user").args(args).stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub fn unit_installed() -> bool {
    systemctl(&["show", "-p", "LoadState", "--value", UNIT]).is_some_and(|s| s == "loaded")
}

pub fn unit_active() -> bool {
    systemctl(&["is-active", UNIT]).is_some_and(|s| s == "active" || s == "activating" || s == "reloading")
}

fn unit_failing() -> bool {
    let state = systemctl(&["show", "-p", "ActiveState", "-p", "SubState", "--value", UNIT]).unwrap_or_default();
    state.lines().any(|l| l == "failed" || l == "auto-restart" || l == "inactive")
}

pub fn booted() -> Option<PathBuf> {
    let dev = find_binder().ok()?;
    let p = Platform::connect(&dev).ok()?;
    (p.getprop("sys.boot_completed", "0").ok()? == "1").then_some(dev)
}

fn wait_booted(mut alive: impl FnMut() -> Result<()>) -> Result<(PathBuf, Duration)> {
    let t0 = Instant::now();
    loop {
        if let Some(dev) = booted() {
            return Ok((dev, t0.elapsed()));
        }
        alive()?;
        if t0.elapsed() >= BOOT_DEADLINE {
            bail!("Android did not finish booting within {} s", BOOT_DEADLINE.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn detached_log(dirs: &Dirs) -> PathBuf {
    dirs.state.join("sarab.log")
}

fn spawn_detached(dirs: &Dirs) -> Result<Child> {
    let exe = crate::paths::own_exe()?;
    let path = detached_log(dirs);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut cmd = Command::new(&exe);
    cmd.args(["start", "--foreground"]).current_dir("/").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    cmd.spawn().with_context(|| format!("spawn {} start --foreground", exe.display()))
}

pub fn ensure_running(dirs: &Dirs, say: &dyn Fn(&str)) -> Result<PathBuf> {
    if find_runtime().is_ok() {
        sarab_runtime::thaw()?;
        return wait_booted(|| {
            find_runtime().map(drop).map_err(|_| anyhow!("the runtime went away while booting (sarab logs --daemon)"))
        })
        .map(|(dev, _)| dev);
    }
    if !dirs.is_set_up() {
        bail!("Sarab is not set up yet: run `sarab start` in a terminal, or `sarab setup`");
    }
    crate::net::require_pasta()?;
    crate::host::require_gpu()?;
    for d in [&dirs.state, &dirs.runtime] {
        std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }
    let path = dirs.runtime.join("sarab-start.lock");
    let lock = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("flock {}", path.display()));
    }
    if find_runtime().is_ok() {
        drop(lock);
        return ensure_running(dirs, say);
    }
    let (dev, took) = if unit_installed() {
        say("starting Android (sarab.service) ...");
        if Command::new("systemctl").args(["--user", "start", UNIT]).status().map(|s| !s.success()).unwrap_or(true) {
            bail!("systemctl --user start {UNIT} failed (journalctl --user -u sarab)");
        }
        wait_booted(|| {
            if unit_failing() {
                Err(anyhow!("{UNIT} stopped before Android booted (journalctl --user -u sarab)"))
            } else {
                Ok(())
            }
        })?
    } else {
        say("starting Android ...");
        let mut child = spawn_detached(dirs)?;
        let log = detached_log(dirs);
        wait_booted(|| match child.try_wait() {
            Ok(Some(st)) => Err(anyhow!("the runtime exited ({st}) before Android booted (log: {})", log.display())),
            _ => Ok(()),
        })?
    };
    say(&format!("Android is up ({:.1} s)", took.as_secs_f64()));
    Ok(dev)
}

pub fn stop(direct: bool) -> Result<()> {
    if !direct && unit_active() {
        let st = Command::new("systemctl").args(["--user", "stop", UNIT]).status().context("systemctl")?;
        if !st.success() {
            bail!("systemctl --user stop {UNIT} failed");
        }
        if find_runtime().is_err() {
            println!("stopped");
            return Ok(());
        }
    }
    crate::stop::stop()
}

pub fn restart(dirs: &Dirs) -> Result<()> {
    if unit_active() {
        println!("restarting Android (sarab.service) ...");
        let st = Command::new("systemctl").args(["--user", "restart", UNIT]).status().context("systemctl")?;
        if !st.success() {
            bail!("systemctl --user restart {UNIT} failed (journalctl --user -u sarab)");
        }
        let t0 = Instant::now();
        while booted().is_some() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let (_, took) = wait_booted(|| {
            if unit_active() {
                Ok(())
            } else {
                Err(anyhow!("{UNIT} stopped before Android booted (journalctl --user -u sarab)"))
            }
        })?;
        println!("Android is up ({:.1} s)", took.as_secs_f64());
        return Ok(());
    }
    if find_runtime().is_ok() {
        crate::stop::stop()?;
    }
    ensure_running(dirs, &|s| println!("{s}")).map(drop)
}

pub fn refit(dirs: &Dirs, say: &dyn Fn(&str)) -> Result<()> {
    let Ok((rt, _)) = find_runtime() else { return Ok(()) };
    let Some(running) = std::fs::read_to_string(crate::start::prop_file(dirs)).ok().and_then(|t| from_props(&t)) else {
        return Ok(());
    };
    let Some(wanted) = crate::start::measured() else { return Ok(()) };
    if running == wanted {
        return Ok(());
    }
    let busy = crate::freeze::host_sessions(rt) > 0
        || (!is_frozen()
            && crate::android::run(User::Root, &["getprop", "waydroid.open_windows"])
                .map_or(true, |v| v.trim().parse::<u32>().map_or(true, |n| n > 0)));
    let change = format!("{}x{} to {}x{}", running.width, running.height, wanted.width, wanted.height);
    if busy {
        say(&format!("the screen changed ({change}); Android keeps its size until its windows close"));
        return Ok(());
    }
    say(&format!("the screen changed ({change}); restarting Android to fit it"));
    restart(dirs)
}

pub fn owner() -> &'static str {
    if unit_active() { "systemd (sarab.service)" } else { "sarab start" }
}
