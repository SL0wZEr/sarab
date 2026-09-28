//! `sarab start --foreground` — the runtime's owner, and the boot ordering.
//!
//! It stays in the foreground for the whole life of Android: the systemd unit
//! runs it, and so does a detached `sarab start` (session.rs). The lifetime is
//! open-ended (no `timeout` unless `--lifetime` asks for one), and `Helpers`
//! kills only the pids we spawned, never a `pkill` by name, which would also
//! reach a second session's helpers. One runtime per user: two would fight over
//! the composer's surfaces, the binder service names and the freeze cgroup.
//!
//! The order in `start` is load-bearing. `sarab-hostd --wait` goes up before
//! the namespace, because system_server binds the host services exactly once
//! during startup; it gets `--allow-shutdown` because Android's power menu
//! otherwise sits at a black screen waiting for an answer. The kmsg FIFO is
//! held open read/write for the life of the process: a FIFO with no reader
//! gives init EPIPE on its first line. pasta attaches as soon as sarab-ns has
//! unshared, before init starts netd: sarab-ns says so on a pipe whose write
//! end only the runtime's side inherits (`--ready-fd`, net.rs), and we close
//! ours right after the spawn, so a sarab-ns that fails first is an end of
//! file. Signal handlers only set `INTERRUPTED`,
//! and they are in place before anything is spawned: a stop that arrived while
//! pasta attached used to end `sarab start` with the default action and leave
//! Android and hostd running with nobody to stop them. `Runtime` holds the
//! spawned runtime and stops it when dropped while it still runs, so an error
//! on the way out of `boot` never leaves it behind either. The runtime's
//! output is read by `filter_to_stdout` threads, which end when the last
//! process in the scope has closed it, so `boot` joins them before its closing
//! lines rather than sleeping and hoping they are done.
//!
//! `runtime_argv` puts Android in a systemd user scope of its own; that cgroup
//! (delegated by `user@<uid>.service`, no root) is what freezing writes and
//! what stats read. sarab-ns mounts binds in argv order, so the trees
//! (`/vendor`, `/data`) must come before the overlay files: an overlay file
//! under `vendor/` listed first is covered by the tree and silently reads back
//! as stock. `overlay_binds` merges the hand-written and the generated overlay,
//! one bind per target, the hand-written file winning, and uses
//! `symlink_metadata` to match `find -type f`, which does not follow links.
//! Every path is absolute, so nothing depends on the working directory.
//! `socket_binds` gives Android one socket each (Wayland, and Pulse when
//! present), never `/run/user/<uid>`: that directory also holds the session bus
//! and the GnuPG, SSH and keyring agents, and Android's root is the desktop
//! user, so binding it would let anything that became Android root run commands
//! as the user.
//!
//! The Wayland socket Android gets is not the compositor's but `relay_socket`,
//! a listening socket in our runtime directory that sarab-hostd accepts on and
//! relays to the compositor, fixing each app window at its size
//! (sarab-hostd's wayland.rs). We create it, not hostd, so it exists before
//! sarab-ns binds it, whenever hostd gets to it; hostd inherits it (the
//! `pre_exec` clears close-on-exec in hostd's child only) and we close our copy,
//! so a hostd that dies leaves a socket that refuses connections instead of one
//! that queues them forever. With `--no-hostd` Android gets the compositor's
//! socket, as before the relay, and its windows tile. `display` asks the
//! compositor for the screens before the prop file is written (screen.rs); the
//! answer becomes `persist.waydroid.width`, `height` and `ro.sf.lcd_density`,
//! and a compositor that does not answer gets the fallback size and a line on
//! stderr. The prop file goes into the vendor tree as `waydroid.prop`, the
//! name the image's init reads, so failing to write it is an error (Android
//! would boot with the last start's screen), and a copy stays at `prop_file`
//! (`android.prop` in the state folder); `measured` redoes the
//! measurement quietly, returning nothing when it fails, so session.rs can
//! compare what the running Android was started with against the screen now.
//!
//! The `#` lines in the text `props()` builds belong to the prop file (a `#`
//! line is a comment to Android's property loader too) and record why each
//! line exists; the tests check their key facts survive. The prop file does
//! not settle the window model, since persisted properties load after it, so
//! `guard_multi_windows` re-checks `MULTI_WINDOWS` after `sys.boot_completed`
//! and treats anything but the exact string `false` as multi-window. The
//! composer read the value when it started, so a fix only takes effect on the
//! next boot. The first-boot desktop policy runs after the guard.
//!
//! `wait_for_boot` calls `find_binder` on every poll: the pid owning the
//! private /dev exists only once sarab-ns has pivoted, and rsbinder caches a
//! successful connect but not a failed one. A boot that never completes is not
//! fatal; the runtime stays up to be looked at.
//!
//! How the runtime ended decides what `start` does next (`ending`). `sarab
//! stop` leaves no exit code, only a signal. Android's init calls reboot(2),
//! which in a pid namespace ends it by signal instead: SIGHUP for a restart
//! (129), SIGINT for a power-off (130). 137 is a SIGKILL: `--lifetime`'s
//! `timeout`, or hostd honouring a shutdown request. A restart used to end the
//! session, since the unit restarts only on failure; now `start` execs itself
//! again (`own_exe`, so an upgrade made meanwhile is the one that boots, and
//! hostd is not handed a " (deleted)" path), same pid and arguments, rather than looping
//! in-process: a process can bind to one runtime's binder device only, so a
//! second boot in the same process never saw its own `sys.boot_completed`. It
//! first waits for the old scope to be collected, since the new one takes the
//! same name. init also reboots after a fatal signal or a crash loop, so
//! `UNBOOTED` counts restarts in a row that never finished a boot, and at
//! `RESTARTS_BEFORE_BOOT` it stops trying. Any other exit code after a
//! completed boot is a failure (`Ending::Died`), so `Restart=on-failure`
//! brings Android back; before a completed boot it is not, or a setup that
//! cannot boot would retry every five seconds forever.
//!
//! Before any of that, `fit` checks what no retry can change (set up, the
//! overlay, an image of the tested Android, pasta, a GPU Android can use),
//! prints `image::behind`'s line when `sarab setup` has an upgrade to make,
//! and a failure exits with
//! `EXIT_UNFIT`, which the unit does not restart (session.rs); these used to
//! fail every five seconds, forever.
//!
//! `wait_for_exit` keeps pasta in step with the host's nameserver
//! (`net::Link::keep`), and also watches hostd, because Android's display runs through
//! it: a hostd that ends while Android runs has closed every window, and no new
//! one can open. hostd exits 0 when the runtime does (it follows init through a
//! pidfd), so only another status is a crash, and it is judged a quarter of a
//! second later, so a stop that reaches both (systemd signals the unit's whole
//! cgroup) is not mistaken for one. On a crash hostd's log is copied to
//! `hostd.crash.log`, since the next start truncates `hostd.log`,
//! `stop_runtime` stops Android as an interrupt would, and `start` fails:
//! sarab.service's `Restart=on-failure` boots both again, and without the unit
//! the next app opened does. Restarting hostd alone would bring the display
//! back but not the services, which system_server binds once.

use crate::host::Gpu;
use crate::screen::Display;
use anyhow::{Context, Result, bail};
use sarab_runtime::Platform;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub lifetime: Option<u64>,
    pub hostd: bool,
    pub idle_freeze: u64,
    pub network: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { lifetime: None, hostd: true, idle_freeze: 60, network: true }
    }
}

use crate::session::BOOT_DEADLINE;

const XDG: &str = "/run/xdg";

pub fn props(wayland: &str, gpu: &Gpu, display: &Display) -> String {
    let Display { width, height, density } = display;
    let density = density.map(|d| format!("ro.sf.lcd_density={d}\n")).unwrap_or_default();
    let Gpu { node, driver } = gpu;
    let mut graphics = format!("ro.hardware.gralloc=gbm\nro.hardware.egl=mesa\ngralloc.gbm.device={node}\n");
    if let Some(v) = crate::host::vulkan_driver(driver) {
        graphics += &format!("ro.hardware.vulkan={v}\n");
    }
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "\
sys.use_memfd=true
{graphics}ro.opengles.version=196610
ro.vndk.lite=true
# adb stays shut. Nothing on the host can reach it (pasta forwards no ports;
# `sarab exec` is the way in), and inside, adbd on 5555 with no key check and
# `adb root` honoured was a way for any app to become Android root, which is
# the desktop user. Without SELinux any app can set any property, `ctl.start
# adbd` included, so not starting adbd is not enough on its own: a key check an
# app cannot switch off is, because an ro. property is set once, here.
ro.adb.secure=1
persist.sys.usb.config=none
# The image is a userdebug build. Debuggable makes every app debuggable and
# honours debugging hooks that any app, being able to set properties, could
# aim at another app. This file loads after the image's build.prop, and an ro.
# property is set once, so this wins; `sarab exec` does not need it.
ro.debuggable=0
waydroid.xdg_runtime_dir={XDG}
waydroid.wayland_display={wayland}
waydroid.pulse_runtime_path={XDG}/pulse
waydroid.tools_version=sarab-{version}
waydroid.host.uid=0
waydroid.stub_sensors_hal=1
waydroid.active_apps=none
# Single-window model. This line alone does not decide it: init loads
# /data/property/persistent_properties after the vendor prop file, and a
# persisted `true` (setprop from the 2026-09-07 experiment) overrides it —
# that is why the Play Store opened its own toplevel during boot 18.
# The desktop policy (`sarab policy apply`, automatic on the first boot of a
# /data) writes false through the property service, and `sarab start`
# re-asserts it after sys.boot_completed if it ever reads otherwise.
persist.waydroid.multi_windows=false
# The composer never creates a toplevel for apps in this colon-separated list
# (Launcher3 and FallbackHome are built in). Our code-less home replaced
# Launcher3, so it must be listed too, or closing the last app window brings
# up an empty black window: Android showing HOME.
waydroid.blacklist_apps=org.sarab.home
# The display, in logical pixels: sized to the shortest screen when Android
# started (screen.rs), and every app window is fixed at it (sarab-hostd). The
# density makes one dp one logical pixel of that screen; without it the
# composer picks 180 per unit of scale and everything looks zoomed.
persist.waydroid.width={width}
persist.waydroid.height={height}
{density}# The stock boot animation plays parts 2-4 with playUntilComplete=true, and
# WindowManager will not call finishBooting() until it exits. Measured: the
# system is fully up at ~4.4 s and then waits ~5.6 s for an animation nobody
# can see (we do not show Android's display during boot). bootanimation reads
# this prop and exits immediately; WM then proceeds at once.
debug.sf.nobootanimation=1
"
    )
}

pub fn overlay_binds(generated: &Path, overlay: &Path) -> Result<Vec<String>> {
    let mut by_target = std::collections::BTreeMap::new();
    for dir in [generated, overlay] {
        if dir.is_dir() {
            walk(dir, dir, &mut by_target)?;
        }
    }
    Ok(by_target
        .into_iter()
        .map(|(dst, src): (PathBuf, PathBuf)| format!("{}:/{}", src.display(), dst.display()))
        .collect())
}

fn walk(dir: &Path, top: &Path, out: &mut std::collections::BTreeMap<PathBuf, PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let p = e?.path();
        let md = std::fs::symlink_metadata(&p)?;
        if md.is_dir() {
            walk(&p, top, out)?;
        } else if md.is_file() {
            out.insert(p.strip_prefix(top)?.to_path_buf(), p);
        }
    }
    Ok(())
}

pub fn print_props() -> Result<()> {
    let runtime_dir = crate::host::runtime_dir(unsafe { libc::getuid() });
    let wayland = crate::host::wayland_display(&runtime_dir)?;
    print!("{}", props(&wayland, &crate::host::require_gpu()?, &display(&runtime_dir.join(&wayland))));
    Ok(())
}

pub fn prop_file(dirs: &crate::paths::Dirs) -> PathBuf {
    dirs.state.join("android.prop")
}

pub fn measured() -> Option<Display> {
    let runtime_dir = crate::host::runtime_dir(unsafe { libc::getuid() });
    let wayland = crate::host::wayland_display(&runtime_dir).ok()?;
    let screens = crate::screen::screens(&runtime_dir.join(wayland)).ok()?;
    (!screens.is_empty()).then(|| crate::screen::display_for(&screens))
}

fn display(compositor: &Path) -> Display {
    let screens = crate::screen::screens(compositor).unwrap_or_else(|e| {
        eprintln!("could not ask the compositor for the screen size ({e:#}); using the default");
        Vec::new()
    });
    crate::screen::display_for(&screens)
}

pub fn print_overlay_binds() -> Result<()> {
    let d = crate::paths::dirs()?;
    for line in bind_lines(&overlay_binds(&d.generated(), &d.overlay)?) {
        println!("{line}");
    }
    Ok(())
}

fn bind_lines(binds: &[String]) -> Vec<String> {
    binds.iter().flat_map(|b| ["--bind".to_string(), b.clone()]).collect()
}

struct Layout {
    ns: PathBuf,
    system: PathBuf,
    vendor: PathBuf,
    data: PathBuf,
    kmsg: PathBuf,
    ready: Option<i32>,
    sockets: Vec<String>,
}

fn socket_binds(runtime_dir: &Path, wayland: &str, relay: Option<&Path>) -> Vec<String> {
    let display = relay.map_or_else(|| runtime_dir.join(wayland), Path::to_path_buf);
    let mut v = vec![format!("{}:{XDG}/{wayland}", display.display())];
    let pulse = runtime_dir.join("pulse/native");
    if pulse.exists() {
        v.push(format!("{}:{XDG}/pulse/native", pulse.display()));
    }
    v
}

fn runtime_argv(l: &Layout, o: &Opts, overlay: &[String], unit: u32) -> Vec<String> {
    let p = |x: PathBuf| x.display().to_string();
    let mut a: Vec<String> = ["systemd-run", "--user", "--scope", "--quiet", "--unit"].map(String::from).into();
    a.push(format!("sarab-{unit}"));
    a.push("--collect".into());
    if let Some(secs) = o.lifetime {
        a.extend(["timeout", "--foreground", "-s", "KILL"].map(String::from));
        a.push(secs.to_string());
    }
    a.push(p(l.ns.clone()));
    a.extend(["--pid", "--net", "--root"].map(String::from));
    a.push(p(l.system.clone()));
    a.push("--kmsg".into());
    a.push(p(l.kmsg.clone()));
    if let Some(fd) = l.ready {
        a.push("--ready-fd".into());
        a.push(fd.to_string());
    }
    let own =
        [format!("{}:/vendor", l.vendor.display()), format!("{}:/data", l.data.display()), "/usr:/usr".to_string()];
    for b in own.into_iter().chain(l.sockets.iter().cloned()).chain(overlay.iter().cloned()) {
        a.push("--bind".into());
        a.push(b);
    }
    a.extend(["--", "/system/bin/init", "second_stage"].map(String::from));
    a
}

fn relay_socket(path: &Path) -> Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).with_context(|| format!("listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn cloexec_pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2");
    }
    Ok(unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) })
}

fn mkfifo(path: &Path) -> Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("mkfifo {}", path.display()));
    }
    Ok(())
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[derive(Default)]
struct Helpers {
    hostd: Option<Child>,
    pasta: Option<crate::net::Link>,
}

impl Drop for Helpers {
    fn drop(&mut self) {
        self.pasta = None;
        if let Some(c) = self.hostd.as_mut() {
            end(c);
        }
    }
}

pub(crate) fn end(c: &mut Child) {
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    for _ in 0..50 {
        if matches!(c.try_wait(), Ok(Some(_)) | Err(_)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = c.kill();
    let _ = c.wait();
}

struct Runtime(Child);

impl Drop for Runtime {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = stop_runtime(&mut self.0);
        }
    }
}

fn filter_to_stdout(s: impl Read + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(s).lines().map_while(Result::ok) {
            if !line.starts_with("sarab-ns:") {
                println!("{line}");
            }
        }
    })
}

fn fit(o: &Opts, dirs: &crate::paths::Dirs) -> Result<()> {
    if !dirs.is_set_up() {
        bail!("Sarab is not set up yet: run `sarab start` in a terminal, or `sarab setup`");
    }
    if !dirs.overlay.is_dir() {
        bail!("the hand-written overlay is missing ({}); reinstall sarab", dirs.overlay.display());
    }
    crate::image::check_api(&dirs.system(), "system")?;
    crate::image::check_api(&dirs.vendor(), "vendor")?;
    if o.network {
        crate::net::require_pasta()?;
    }
    crate::host::require_gpu()?;
    for note in crate::image::behind(&dirs.data) {
        println!("{note}");
    }
    Ok(())
}

pub fn start(o: Opts) -> Result<()> {
    let dirs = crate::paths::dirs()?;
    if let Ok((pid, _)) = sarab_runtime::find_runtime() {
        eprintln!("Android is already running (pid {pid}); `sarab stop` first");
        std::process::exit(crate::session::EXIT_ALREADY_RUNNING);
    }
    if let Err(e) = fit(&o, &dirs) {
        eprintln!("sarab: {e:#}");
        std::process::exit(crate::session::EXIT_UNFIT);
    }
    let (status, booted) = boot(&o, &dirs)?;
    match ending(status.code(), status.signal(), booted) {
        Ending::Stopped => Ok(()),
        Ending::Died(c) => bail!("the runtime exited with status {c} after it had booted (sarab logs --kernel)"),
        Ending::Restart => {
            let before: u32 = std::env::var(UNBOOTED).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            let unbooted = if booted { 0 } else { before + 1 };
            if unbooted == RESTARTS_BEFORE_BOOT {
                eprintln!("Android restarted {unbooted} times without finishing a boot; not trying again");
                return Ok(());
            }
            println!("Android asked to restart; booting again");
            let scope = format!("sarab-{}.scope", std::process::id());
            for _ in 0..50 {
                let out = Command::new("systemctl").args(["--user", "show", "-P", "LoadState", &scope]).output();
                if out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() != "loaded") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let mut args = std::env::args_os();
            let err = Command::new(crate::paths::own_exe()?)
                .arg0(args.next().unwrap_or_default())
                .args(args)
                .env(UNBOOTED, unbooted.to_string())
                .exec();
            Err(err).context("run sarab again to boot Android again")
        }
    }
}

const UNBOOTED: &str = "SARAB_RESTARTS_UNBOOTED";
const RESTARTS_BEFORE_BOOT: u32 = 3;

#[derive(Debug, PartialEq)]
enum Ending {
    Stopped,
    Restart,
    Died(i32),
}

fn ending(code: Option<i32>, signal: Option<i32>, booted: bool) -> Ending {
    match (code, signal) {
        (Some(129), _) => Ending::Restart,
        (Some(0 | 130 | 137), _) | (None, _) => Ending::Stopped,
        (Some(c), _) if booted => Ending::Died(c),
        (Some(_), _) => Ending::Stopped,
    }
}

fn boot(o: &Opts, dirs: &crate::paths::Dirs) -> Result<(std::process::ExitStatus, bool)> {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let pasta = if o.network { Some(crate::net::require_pasta()?) } else { None };
    let run = &dirs.state;
    for d in [run, &dirs.runtime, &dirs.android_data()] {
        std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }
    crate::overlay::refresh(&dirs.data, &dirs.generated(), &dirs.overlay).context("refresh the generated overlay")?;
    let ns = dirs.helper("sarab-ns")?;
    let uid = unsafe { libc::getuid() };

    let vendor = if dirs.vendor().join("etc").is_dir() {
        dirs.vendor()
    } else {
        let v = run.join("vendor-stub");
        std::fs::create_dir_all(v.join("etc/init"))?;
        v
    };
    let wayland = crate::host::wayland_display(&crate::host::runtime_dir(uid))?;
    let gpu = crate::host::require_gpu()?;
    println!("display {wayland}, gpu {} ({})", gpu.node, gpu.driver);
    let display = display(&crate::host::runtime_dir(uid).join(&wayland));
    match display.density {
        Some(d) => println!("android display {}x{}, density {d}", display.width, display.height),
        None => println!("android display {}x{}", display.width, display.height),
    }
    let props = props(&wayland, &gpu, &display);
    let for_init = vendor.join("waydroid.prop");
    std::fs::write(&for_init, &props).with_context(|| format!("write {}", for_init.display()))?;
    std::fs::write(prop_file(dirs), &props).context("write the prop file")?;

    let kmsg = dirs.runtime.join("kmsg");
    let _ = std::fs::remove_file(&kmsg);
    mkfifo(&kmsg)?;
    let fifo = std::fs::OpenOptions::new().read(true).write(true).open(&kmsg).context("open the kmsg FIFO")?;
    let kmsg_log = run.join("kmsg.log");
    let mut log = std::fs::File::create(&kmsg_log).with_context(|| format!("create {}", kmsg_log.display()))?;
    std::thread::spawn(move || {
        let mut fifo = fifo;
        let _ = std::io::copy(&mut fifo, &mut log);
    });

    let mut helpers = Helpers::default();
    let runtime_dir = crate::host::runtime_dir(uid);
    let relay = o.hostd.then(|| dirs.runtime.join("wayland"));

    if let Some(relay) = &relay {
        let hostd = dirs.helper("sarab-hostd").context("--no-hostd skips it")?;
        let launcher = crate::paths::own_exe()?;
        let hostd_log = run.join("hostd.log");
        let log = std::fs::File::create(&hostd_log)?;
        let listener = relay_socket(relay)?;
        let fd = listener.as_raw_fd();
        let mut cmd = Command::new(&hostd);
        cmd.args(["--wait", "180", "--allow-shutdown"])
            .arg("--launcher")
            .arg(&launcher)
            .arg("--icons")
            .arg(&dirs.icons)
            .arg("--wayland-fd")
            .arg(fd.to_string())
            .arg("--wayland-upstream")
            .arg(runtime_dir.join(&wayland))
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
        helpers.hostd = Some(cmd.spawn().with_context(|| format!("spawn {}", hostd.display()))?);
        drop(listener);
        println!("host services starting (log: {})", hostd_log.display());
    }

    let ready = pasta.as_ref().map(|_| cloexec_pipe()).transpose()?;
    let argv = runtime_argv(
        &Layout {
            ns,
            system: dirs.system(),
            vendor,
            data: dirs.android_data(),
            kmsg,
            ready: ready.as_ref().map(|(_, w)| w.as_raw_fd()),
            sockets: socket_binds(&runtime_dir, &wayland, relay.as_deref()),
        },
        o,
        &overlay_binds(&dirs.generated(), &dirs.overlay)?,
        std::process::id(),
    );
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir("/").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some((_, w)) = &ready {
        let fd = w.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
    }

    match o.lifetime {
        Some(s) => println!("booting init second_stage for {s}s ... (log: {})", kmsg_log.display()),
        None => println!("booting init second_stage ... (log: {})", kmsg_log.display()),
    }
    let mut runtime = Runtime(cmd.spawn().context("spawn systemd-run (is this a systemd user session?)")?);
    let child = &mut runtime.0;
    let output: Vec<_> = [child.stdout.take().map(filter_to_stdout), child.stderr.take().map(filter_to_stdout)]
        .into_iter()
        .flatten()
        .collect();

    if let (Some(p), Some((r, w))) = (&pasta, ready) {
        drop(w);
        helpers.pasta = crate::net::attach(p, r, run)?;
    }

    if o.idle_freeze != 0 {
        crate::freeze::spawn(Duration::from_secs(o.idle_freeze));
    }

    let booted = wait_for_boot(child)?;
    if helpers.pasta.is_some() {
        crate::net::set_dns_props();
    }
    if booted {
        guard_multi_windows();
        crate::policy::first_boot();
    }
    let status = wait_for_exit(child, helpers.hostd.as_mut(), helpers.pasta.as_mut(), &run.join("hostd.log"))?;
    for t in output {
        let _ = t.join();
    }
    println!("{}", exit_summary(status.code(), status.signal()));
    let lines = std::fs::read_to_string(run.join("kmsg.log")).map(|s| s.lines().count()).unwrap_or(0);
    println!("=== kmsg.log: {lines} lines ===");
    Ok((status, booted))
}

fn exit_summary(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(0), _) => "=== exit=0 (the runtime stopped by itself) ===".into(),
        (Some(129), _) => "=== exit=129 (Android asked to restart) ===".into(),
        (Some(130), _) => "=== exit=130 (Android powered off) ===".into(),
        (Some(137), _) => "=== exit=137 (killed: --lifetime expired, or hostd honoured a shutdown request) ===".into(),
        (Some(c), _) => format!("=== exit={c} ==="),
        (None, Some(s)) => format!("=== killed by signal {s} (`sarab stop`, or the scope going away) ==="),
        (None, None) => "=== gone, with neither an exit code nor a signal ===".into(),
    }
}

fn wait_for_boot(child: &mut Child) -> Result<bool> {
    let t0 = Instant::now();
    while t0.elapsed() < BOOT_DEADLINE {
        if let Ok(Some(_)) = child.try_wait() {
            eprintln!("runtime exited before it finished booting (sarab logs --kernel)");
            return Ok(false);
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let booted = sarab_runtime::find_binder()
            .and_then(|dev| Platform::connect(&dev))
            .and_then(|p| p.getprop("sys.boot_completed", "0"))
            .map(|v| v == "1")
            .unwrap_or(false);
        if booted {
            println!("boot completed in {:.1} s", t0.elapsed().as_secs_f64());
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("boot did not complete within {} s (sarab logs --kernel)", BOOT_DEADLINE.as_secs());
    Ok(false)
}

const MULTI_WINDOWS: &str = "persist.waydroid.multi_windows";

#[derive(Debug, PartialEq, Clone, Copy)]
enum Guard {
    Nothing,
    SetAndWarn,
}

fn guard_action(current: &str) -> Guard {
    if current == "false" { Guard::Nothing } else { Guard::SetAndWarn }
}

fn guard_multi_windows() {
    let p = match sarab_runtime::find_binder().and_then(|dev| Platform::connect(&dev)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("window model: no binder connection ({e:#}); {MULTI_WINDOWS} not checked");
            return;
        }
    };
    let current = match p.getprop(MULTI_WINDOWS, "") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("window model: could not read {MULTI_WINDOWS} ({e:#}); not checked");
            return;
        }
    };
    if guard_action(&current) == Guard::Nothing {
        return;
    }
    if let Err(e) = p.setprop(MULTI_WINDOWS, "false") {
        eprintln!(
            "window model: {MULTI_WINDOWS} is '{current}' and could not be set false ({e:#}); this boot and the next are multi-window"
        );
        return;
    }
    eprintln!(
        "!!! {MULTI_WINDOWS} was '{current}': set false, but THIS boot is still multi-window (composer read it at start); the next boot will be single-window"
    );
}

fn wait_for_exit(
    child: &mut Child,
    mut hostd: Option<&mut Child>,
    mut net: Option<&mut crate::net::Link>,
    log: &Path,
) -> Result<std::process::ExitStatus> {
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        if let Some(n) = net.as_deref_mut() {
            n.keep();
        }
        if let Some(st) = hostd.as_mut().and_then(|h| h.try_wait().ok().flatten()) {
            hostd = None;
            std::thread::sleep(Duration::from_millis(250));
            if !st.success() && child.try_wait()?.is_none() && !INTERRUPTED.load(Ordering::SeqCst) {
                let kept = log.with_extension("crash.log");
                let _ = std::fs::copy(log, &kept);
                eprintln!("sarab-hostd ended ({st}), and every Android window with it; stopping Android");
                let _ = stop_runtime(child);
                bail!("sarab-hostd ended ({st}); its log is kept in {}", kept.display());
            }
        }
        if INTERRUPTED.swap(false, Ordering::SeqCst) {
            println!("stopping ...");
            return stop_runtime(child);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn stop_runtime(child: &mut Child) -> Result<std::process::ExitStatus> {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    for _ in 0..100 {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    Ok(child.wait()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sarab-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn props_carry_every_key() {
        let p = props("wayland-1", &amd(), &Display { width: 480, height: 1000, density: Some(320) });
        for key in [
            "sys.use_memfd=true",
            "ro.hardware.gralloc=gbm",
            "ro.hardware.egl=mesa",
            "gralloc.gbm.device=/dev/dri/renderD128",
            "ro.hardware.vulkan=radeon",
            "ro.opengles.version=196610",
            "ro.vndk.lite=true",
            "ro.adb.secure=1",
            "persist.sys.usb.config=none",
            "ro.debuggable=0",
            "waydroid.xdg_runtime_dir=/run/xdg",
            "waydroid.wayland_display=wayland-1",
            "waydroid.pulse_runtime_path=/run/xdg/pulse",
            concat!("waydroid.tools_version=sarab-", env!("CARGO_PKG_VERSION")),
            "waydroid.host.uid=0",
            "waydroid.stub_sensors_hal=1",
            "waydroid.active_apps=none",
            "persist.waydroid.multi_windows=false",
            "waydroid.blacklist_apps=org.sarab.home",
            "persist.waydroid.width=480",
            "persist.waydroid.height=1000",
            "ro.sf.lcd_density=320",
            "debug.sf.nobootanimation=1",
        ] {
            assert!(p.contains(key), "prop file is missing {key}");
        }
        for l in p.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            assert!(l.contains('='), "not a property: {l}");
        }
        assert!(!p.contains("/run/user"));
        let intel = props(
            "wayland-0",
            &Gpu { node: "/dev/dri/renderD129".into(), driver: "i915".into() },
            &Display { width: 390, height: 816, density: None },
        );
        for key in
            ["gralloc.gbm.device=/dev/dri/renderD129", "ro.hardware.vulkan=intel", "waydroid.wayland_display=wayland-0"]
        {
            assert!(intel.contains(key), "missing {key}");
        }
        let virt = props(
            "wayland-0",
            &Gpu { node: "/dev/dri/renderD128".into(), driver: "virtio_gpu".into() },
            &Display { width: 390, height: 816, density: None },
        );
        assert!(!virt.contains("ro.hardware.vulkan"));
        assert!(!virt.contains("lcd_density"), "no density without a screen to take it from");
    }

    fn amd() -> Gpu {
        Gpu { node: "/dev/dri/renderD128".into(), driver: "amdgpu".into() }
    }

    #[test]
    fn subcommand_output_is_machine_readable() {
        let p = props("wayland-1", &amd(), &Display { width: 480, height: 1000, density: Some(320) });
        assert!(p.ends_with('\n'), "prop file must end in a newline");
        for l in p.lines() {
            assert!(
                l.starts_with('#') || l.split_once('=').is_some_and(|(k, _)| !k.is_empty()),
                "neither comment nor key=value: {l}"
            );
        }
        assert!(p.contains("\npersist.waydroid.multi_windows=false\n"));
        assert!(p.contains("\ndebug.sf.nobootanimation=1\n"));
        for fact in ["persistent_properties", "colon-separated list", "playUntilComplete=true"] {
            assert!(p.contains(fact), "the prop comments lost {fact:?}");
        }
        let binds = vec![
            "/w/overlay/system/bin/ip:/system/bin/ip".to_string(),
            "/w/overlay/system/etc/x.rc:/system/etc/x.rc".to_string(),
        ];
        let lines = bind_lines(&binds);
        assert_eq!(lines.len(), binds.len() * 2);
        let mut back = Vec::new();
        for c in lines.chunks(2) {
            assert_eq!(c[0], "--bind");
            assert!(c[1].contains(":/"), "not a bind spec: {}", c[1]);
            back.push(c[1].clone());
        }
        assert_eq!(back, binds);
        assert!(bind_lines(&[]).is_empty());
    }

    #[test]
    fn overlay_binds_merge_both_overlays_and_the_hand_written_file_wins() {
        let d = tmpdir("overlay");
        let (o, g) = (d.join("overlay"), d.join("generated"));
        for f in [
            o.join("system/bin/iptables"),
            o.join("system/etc/ueventd.rc"),
            g.join("system/etc/init/logd.rc"),
            g.join("system/etc/ueventd.rc"),
        ] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        assert_eq!(
            overlay_binds(&g, &o).unwrap(),
            vec![
                format!("{}/overlay/system/bin/iptables:/system/bin/iptables", d.display()),
                format!("{}/generated/system/etc/init/logd.rc:/system/etc/init/logd.rc", d.display()),
                format!("{}/overlay/system/etc/ueventd.rc:/system/etc/ueventd.rc", d.display()),
            ]
        );
        assert!(overlay_binds(&d.join("none"), &d.join("nothing-here")).unwrap().is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn runtime_argv_is_the_expected_command_line() {
        let dirs = crate::paths::checkout(Path::new("/w/sarab"), Path::new("/w/sarab/target/release/sarab"), &|_| None);
        let l = Layout {
            ns: dirs.helpers[0].join("sarab-ns"),
            system: dirs.system(),
            vendor: dirs.state.join("vendor-stub"),
            data: dirs.android_data(),
            kmsg: dirs.runtime.join("kmsg"),
            ready: Some(7),
            sockets: vec!["/run/user/1000/wayland-1:/run/xdg/wayland-1".to_string()],
        };
        let overlay = vec!["/w/sarab/overlay/system/etc/init/logd.rc:/system/etc/init/logd.rc".to_string()];
        let a = runtime_argv(&l, &Opts::default(), &overlay, 4242);
        assert_eq!(
            a.join(" "),
            "systemd-run --user --scope --quiet --unit sarab-4242 --collect \
             /w/sarab/target/release/sarab-ns --pid --net --root /w/sarab/images/system --kmsg /w/sarab/run/kmsg \
             --ready-fd 7 --bind /w/sarab/run/vendor-stub:/vendor --bind /w/sarab/images/data:/data \
             --bind /usr:/usr --bind /run/user/1000/wayland-1:/run/xdg/wayland-1 \
             --bind /w/sarab/overlay/system/etc/init/logd.rc:/system/etc/init/logd.rc \
             -- /system/bin/init second_stage"
        );
        let deep = vec![
            "/w/sarab/overlay/vendor/lib64/hw/hwcomposer.waydroid.so:/vendor/lib64/hw/hwcomposer.waydroid.so"
                .to_string(),
        ];
        let d = runtime_argv(&l, &Opts::default(), &deep, 1);
        let tree = d.iter().position(|x| x.ends_with(":/vendor")).expect("the /vendor bind");
        let file = d.iter().position(|x| x.contains("hwcomposer")).expect("the overlay bind");
        assert!(tree < file, "/vendor must be mounted before anything that shadows a file inside it");

        let t = runtime_argv(&l, &Opts { lifetime: Some(45), ..Opts::default() }, &[], 4242);
        assert_eq!(&t[6..12], &["--collect", "timeout", "--foreground", "-s", "KILL", "45"]);
        assert!(!a.contains(&"timeout".to_string()));
    }

    #[test]
    fn android_gets_the_display_and_audio_sockets_and_nothing_else() {
        let d = tmpdir("xdg");
        for f in ["wayland-1", "wayland-1.lock", "bus", "gnupg/S.gpg-agent", "pulse/native", "pulse/pid"] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        assert_eq!(
            socket_binds(&d, "wayland-1", None),
            [
                format!("{}/wayland-1:/run/xdg/wayland-1", d.display()),
                format!("{}/pulse/native:/run/xdg/pulse/native", d.display()),
            ]
        );
        std::fs::remove_dir_all(d.join("pulse")).unwrap();
        assert_eq!(socket_binds(&d, "wayland-1", None), [format!("{}/wayland-1:/run/xdg/wayland-1", d.display())]);
        assert_eq!(
            socket_binds(&d, "wayland-1", Some(Path::new("/w/sarab/run/wayland"))),
            ["/w/sarab/run/wayland:/run/xdg/wayland-1"],
            "with the relay, Android finds hostd's socket under the compositor's name"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_relay_socket_listens_replaces_a_stale_one_and_is_the_users_alone() {
        let d = tmpdir("relay");
        let p = d.join("wayland");
        std::fs::write(&p, "stale").unwrap();
        let l = relay_socket(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        std::os::unix::net::UnixStream::connect(&p).unwrap();
        assert!(l.accept().is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_window_model_guard_trusts_only_the_exact_string_false() {
        assert_eq!(guard_action("false"), Guard::Nothing);
        assert_eq!(guard_action("true"), Guard::SetAndWarn);
        assert_eq!(guard_action(""), Guard::SetAndWarn);
        for garbage in ["False", "FALSE", "0", "1", "no", " false"] {
            assert_eq!(guard_action(garbage), Guard::SetAndWarn, "{garbage:?} is not `false`");
        }
    }

    #[test]
    fn a_crashed_hostd_takes_android_down_and_keeps_its_log() {
        let dir = tmpdir("hostd-crash");
        let log = dir.join("hostd.log");
        std::fs::write(&log, "panicked at wayland.rs\n").unwrap();
        let mut android = Command::new("sleep").arg("30").spawn().unwrap();
        let mut hostd = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let e = wait_for_exit(&mut android, Some(&mut hostd), None, &log).unwrap_err().to_string();
        assert!(e.contains("sarab-hostd ended (exit status: 3)"), "{e}");
        assert!(android.try_wait().unwrap().is_some(), "Android was stopped");
        assert_eq!(std::fs::read_to_string(dir.join("hostd.crash.log")).unwrap(), "panicked at wayland.rs\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hostd_that_follows_the_runtime_out_is_no_crash() {
        let dir = tmpdir("hostd-clean");
        let mut android = Command::new("sleep").arg("0.6").spawn().unwrap();
        let mut hostd = Command::new("true").spawn().unwrap();
        let st = wait_for_exit(&mut android, Some(&mut hostd), None, &dir.join("hostd.log")).unwrap();
        assert!(st.success());
        assert!(!dir.join("hostd.crash.log").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_restart_boots_again_and_only_a_booted_android_that_dies_is_a_failure() {
        assert_eq!(ending(Some(129), None, true), Ending::Restart);
        assert_eq!(ending(Some(129), None, false), Ending::Restart);
        for c in [0, 130, 137] {
            assert_eq!(ending(Some(c), None, true), Ending::Stopped, "exit {c}");
        }
        assert_eq!(ending(None, Some(9), true), Ending::Stopped);
        assert_eq!(ending(Some(1), None, true), Ending::Died(1));
        assert_eq!(ending(Some(134), None, true), Ending::Died(134));
        assert_eq!(ending(Some(1), None, false), Ending::Stopped);
    }

    #[test]
    fn exit_summary_says_what_actually_happened() {
        assert_eq!(exit_summary(None, Some(9)), "=== killed by signal 9 (`sarab stop`, or the scope going away) ===");
        assert_eq!(exit_summary(None, Some(15)), "=== killed by signal 15 (`sarab stop`, or the scope going away) ===");
        assert!(exit_summary(Some(137), None).contains("--lifetime expired"));
        assert_eq!(exit_summary(Some(0), None), "=== exit=0 (the runtime stopped by itself) ===");
        assert_eq!(exit_summary(Some(1), None), "=== exit=1 ===");
        assert!(exit_summary(None, None).contains("neither an exit code nor a signal"));
        assert!(exit_summary(Some(129), None).contains("restart"));
        assert!(exit_summary(Some(130), None).contains("powered off"));
    }
}
