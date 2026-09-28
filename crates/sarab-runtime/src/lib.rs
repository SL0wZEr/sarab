//! The running Android, seen from the host: finding the runtime, freezing,
//! thawing and reclaiming its cgroup, and a proxy for the image's `IPlatform`
//! service, which its lineage-sdk framework patch runs inside Android to
//! expose getprop/setprop, app list/install/remove/launch and Settings.
//!
//! The wire format follows the AIDL of the image's lineage-sdk framework
//! patch, hand-rolled. `AppInfo` is an unstructured parcelable: six nullable
//! strings and a string list. `Platform::install` takes a path inside Android,
//! not on the host. We talk to Android 13's *own* servicemanager with AOSP
//! `IServiceManager` transactions against handle 0 rather than rsbinder's hub,
//! which speaks the Android 16 protocol (getService2) on Linux hosts:
//! `check_service` is checkService (2, non-blocking, valid for Android 11-15)
//! and `add_service` is addService (3: name, binder, allowIsolated = false,
//! dumpPriority = DEFAULT, 1 << 3). The servicemanager refuses app uids, but
//! inside the namespace we are uid 0, and SELinux is compiled out of the image
//! so `canAdd` always permits. `Platform::connect` and `connect` each
//! initialise the process's binder state, so a process calls one of them
//! once; the former asks for one binder thread, being a pure client.
//!
//! `find_runtime` locates a running `sarab-ns` from the host through
//! `/proc/<pid>/root`, where its private /dev is visible. It matches the
//! process name, not the command line (a shell whose command mentions sarab-ns
//! is not the runtime), and requires that the process's root is not the host's,
//! because another Android container on the host may mount a binderfs at the
//! host's /dev/binderfs. Of the sarab-ns processes a runtime starts, the outer
//! one stays in the host's namespaces, and the --pid child pivots and then
//! becomes Android's init, named `init` from then on; the one between them,
//! which set the namespaces up, shares the child's mount namespace, and
//! pivot_root moves the root of every process in it, so that one is the match,
//! and the pid everything else means by "the runtime" (the same pid sarab-ns
//! reports on `--ready-fd`). A `sarab-ns --enter` (`sarab exec`, and the
//! helpers that run commands inside) has joined that root too, and is younger,
//! so `enters` rules it out by its own arguments, up to a `--`: while a long
//! `sarab exec` ran, it was taken for the runtime, and `sarab status` showed
//! the terminal's cgroup as Android's.
//!
//! Freezing uses cgroup v2 on `runtime_cgroup`, the nearest ancestor of the
//! runtime's cgroup whose `cgroup.freeze` we may write: normally the scope
//! `sarab start` creates, since sarab-ns places Android one level below it.
//! Frozen costs no CPU or wakeups and keeps all memory. `set_frozen` waits for
//! `cgroup.events` to say the tree is in the asked state, so callers can
//! transact right after: the kernel notifies a change to that file as
//! `POLLPRI` on an open copy that has been read, so it reads, polls, and reads
//! again, and fails after `SETTLE` rather than reporting a state it never saw.
//! `thaw` is the "wake it if it sleeps" every caller about to make a binder
//! call wants, hostd's notification clicks included. `reclaim`
//! then pushes the frozen tree to swap with `memory.reclaim` instead of
//! waiting for pressure; it refuses a running tree (that would thrash), does
//! nothing without swap (only file pages could go, not worth the refaults),
//! and treats EAGAIN, the kernel's "nothing more can be reclaimed", as success.

use anyhow::{Context, Result, anyhow, bail};
use rsbinder::*;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const INTERFACE: &str = "lineageos.waydroid.IPlatform";
pub const SERVICE_NAME: &str = "waydroidplatform";

mod code {
    pub const GETPROP: u32 = 1;
    pub const SETPROP: u32 = 2;
    pub const GET_APPS_INFO: u32 = 3;
    pub const GET_APP_INFO: u32 = 4;
    pub const INSTALL_APP: u32 = 5;
    pub const REMOVE_APP: u32 = 6;
    pub const LAUNCH_APP: u32 = 7;
    pub const GET_APP_NAME: u32 = 8;
    pub const SETTINGS_PUT_STRING: u32 = 9;
    pub const SETTINGS_GET_STRING: u32 = 10;
    pub const SETTINGS_PUT_INT: u32 = 11;
    pub const SETTINGS_GET_INT: u32 = 12;
    pub const LAUNCH_INTENT: u32 = 13;
}

pub mod settings {
    pub const SYSTEM: i32 = 0;
    pub const SECURE: i32 = 1;
    pub const GLOBAL: i32 = 2;
}

#[derive(Debug, Clone, Default)]
pub struct AppInfo {
    pub name: String,
    pub package_name: String,
    pub action: String,
    pub launch_intent: String,
    pub component_package_name: String,
    pub component_class_name: String,
    pub categories: Vec<String>,
}

impl AppInfo {
    fn read(p: &mut Parcel) -> Result<Self> {
        let s = |p: &mut Parcel| -> Result<String> { Ok(p.read::<Option<String>>()?.unwrap_or_default()) };
        let mut a = AppInfo {
            name: s(p)?,
            package_name: s(p)?,
            action: s(p)?,
            launch_intent: s(p)?,
            component_package_name: s(p)?,
            component_class_name: s(p)?,
            categories: Vec::new(),
        };
        let n = p.read::<i32>()?;
        for _ in 0..n.max(0) {
            a.categories.push(s(p)?);
        }
        Ok(a)
    }
}

pub fn find_binder() -> Result<PathBuf> {
    Ok(find_runtime()?.1)
}

pub fn find_runtime() -> Result<(u32, PathBuf)> {
    let mut found = None;
    for e in std::fs::read_dir("/proc")? {
        let e = e?;
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "sarab-ns" || enters(&std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default()) {
            continue;
        }
        let pivoted = match (std::fs::metadata(format!("/proc/{pid}/root")), std::fs::metadata("/")) {
            (Ok(a), Ok(b)) => (a.dev(), a.ino()) != (b.dev(), b.ino()),
            _ => false,
        };
        let dev = PathBuf::from(format!("/proc/{pid}/root/dev/binderfs/binder"));
        if pivoted && dev.exists() {
            found = Some((pid, dev));
        }
    }
    found.ok_or_else(|| anyhow!("no running sarab-ns with a binderfs found"))
}

fn enters(cmdline: &[u8]) -> bool {
    cmdline.split(|&b| b == 0).take_while(|a| *a != b"--").any(|a| a == b"--enter")
}

pub fn runtime_cgroup() -> Option<PathBuf> {
    let (pid, _) = find_runtime().ok()?;
    let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim().trim_start_matches('/');
    let mut dir = PathBuf::from("/sys/fs/cgroup").join(rel);
    loop {
        let f = dir.join("cgroup.freeze");
        if std::fs::OpenOptions::new().write(true).open(&f).is_ok() {
            return Some(dir);
        }
        if !dir.pop() || dir == Path::new("/sys/fs/cgroup") {
            return None;
        }
    }
}

pub fn is_frozen() -> bool {
    runtime_cgroup()
        .and_then(|d| std::fs::read_to_string(d.join("cgroup.freeze")).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

pub fn set_frozen(frozen: bool) -> Result<()> {
    use std::io::{Read, Seek};
    use std::os::fd::AsRawFd;
    let dir = runtime_cgroup()
        .ok_or_else(|| anyhow!("runtime has no host-owned cgroup (started outside `sarab start`'s scope?)"))?;
    let events_path = dir.join("cgroup.events");
    let mut events = std::fs::File::open(&events_path).with_context(|| format!("open {}", events_path.display()))?;
    std::fs::write(dir.join("cgroup.freeze"), if frozen { "1" } else { "0" })
        .with_context(|| format!("write {}/cgroup.freeze", dir.display()))?;
    let want = if frozen { "frozen 1" } else { "frozen 0" };
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        let mut text = String::new();
        events.rewind()?;
        events.read_to_string(&mut text)?;
        if text.lines().any(|l| l.trim() == want) {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            bail!("the runtime is still not {want:?} {} s after asking", SETTLE.as_secs());
        }
        let mut p = libc::pollfd { fd: events.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut p, 1, ms) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e).context("poll cgroup.events");
            }
        }
    }
}

pub fn thaw() -> Result<()> {
    if is_frozen() {
        set_frozen(false)?;
    }
    Ok(())
}

const SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

pub fn reclaim() -> Result<Option<u64>> {
    if !is_frozen() {
        bail!("runtime is not frozen; refusing to reclaim a running tree");
    }
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    if swaps.lines().count() < 2 {
        return Ok(None);
    }
    let dir = runtime_cgroup().ok_or_else(|| anyhow!("runtime has no host-owned cgroup"))?;
    let current: u64 = std::fs::read_to_string(dir.join("memory.current"))?.trim().parse()?;
    match std::fs::write(dir.join("memory.reclaim"), current.to_string()) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {}
        Err(e) => return Err(e).with_context(|| format!("write {}/memory.reclaim", dir.display())),
    }
    Ok(Some(std::fs::read_to_string(dir.join("memory.current"))?.trim().parse()?))
}

pub struct Platform {
    remote: SIBinder,
}

impl Platform {
    pub fn connect(binder: &Path) -> Result<Self> {
        let path = binder.to_str().context("binder path is not UTF-8")?;
        let ps = ProcessState::init(path, 1).map_err(|e| anyhow!("open {path}: {e}"))?;
        let sm = ps.context_object().map_err(|e| anyhow!("servicemanager handle 0: {e:?}"))?;
        let remote = check_service(&sm, SERVICE_NAME)?
            .ok_or_else(|| anyhow!("service {SERVICE_NAME} not registered (Android not booted yet?)"))?;
        Ok(Self { remote })
    }

    fn call(&self, code: u32, fill: impl FnOnce(&mut Parcel) -> Result<()>) -> Result<Parcel> {
        let proxy = self.remote.as_proxy().context("platform binder is not a proxy")?;
        let mut data = proxy.prepare_transact(true).map_err(|e| anyhow!("prepare: {e:?}"))?;
        fill(&mut data)?;
        let mut reply = proxy
            .submit_transact(code, &data, 0)
            .map_err(|e| anyhow!("transact {code}: {e:?}"))?
            .context("no reply parcel")?;
        let status = reply.read::<Status>().map_err(|e| anyhow!("reply status: {e:?}"))?;
        if status.exception_code() != ExceptionCode::None {
            bail!("IPlatform transaction {code} failed: {status}");
        }
        Ok(reply)
    }

    pub fn getprop(&self, prop: &str, default: &str) -> Result<String> {
        let mut r = self.call(code::GETPROP, |d| Ok(d.write(prop).and(d.write(default))?))?;
        Ok(r.read::<Option<String>>()?.unwrap_or_default())
    }

    pub fn setprop(&self, prop: &str, value: &str) -> Result<()> {
        self.call(code::SETPROP, |d| Ok(d.write(prop).and(d.write(value))?)).map(drop)
    }

    pub fn apps(&self) -> Result<Vec<AppInfo>> {
        let mut r = self.call(code::GET_APPS_INFO, |_| Ok(()))?;
        let n = r.read::<i32>()?;
        let mut v = Vec::new();
        for _ in 0..n.max(0) {
            if r.read::<i32>()? == 1 {
                v.push(AppInfo::read(&mut r)?);
            }
        }
        Ok(v)
    }

    pub fn app(&self, package: &str) -> Result<Option<AppInfo>> {
        let mut r = self.call(code::GET_APP_INFO, |d| Ok(d.write(package)?))?;
        if r.read::<i32>()? == 1 { Ok(Some(AppInfo::read(&mut r)?)) } else { Ok(None) }
    }

    pub fn install(&self, path: &str) -> Result<i32> {
        let mut r = self.call(code::INSTALL_APP, |d| Ok(d.write(path)?))?;
        Ok(r.read::<i32>()?)
    }

    pub fn remove(&self, package: &str) -> Result<i32> {
        let mut r = self.call(code::REMOVE_APP, |d| Ok(d.write(package)?))?;
        Ok(r.read::<i32>()?)
    }

    pub fn launch(&self, package: &str) -> Result<()> {
        self.call(code::LAUNCH_APP, |d| Ok(d.write(package)?)).map(drop)
    }

    pub fn app_name(&self, package: &str) -> Result<String> {
        let mut r = self.call(code::GET_APP_NAME, |d| Ok(d.write(package)?))?;
        Ok(r.read::<Option<String>>()?.unwrap_or_default())
    }

    pub fn launch_intent(&self, action: &str, uri: &str) -> Result<String> {
        let mut r = self.call(code::LAUNCH_INTENT, |d| Ok(d.write(action).and(d.write(uri))?))?;
        Ok(r.read::<Option<String>>()?.unwrap_or_default())
    }

    pub fn settings_put_string(&self, mode: i32, key: &str, value: &str) -> Result<()> {
        self.call(code::SETTINGS_PUT_STRING, |d| Ok(d.write(&mode).and(d.write(key)).and(d.write(value))?)).map(drop)
    }

    pub fn settings_get_string(&self, mode: i32, key: &str) -> Result<String> {
        let mut r = self.call(code::SETTINGS_GET_STRING, |d| Ok(d.write(&mode).and(d.write(key))?))?;
        Ok(r.read::<Option<String>>()?.unwrap_or_default())
    }

    pub fn settings_put_int(&self, mode: i32, key: &str, value: i32) -> Result<()> {
        self.call(code::SETTINGS_PUT_INT, |d| Ok(d.write(&mode).and(d.write(key)).and(d.write(&value))?)).map(drop)
    }

    pub fn settings_get_int(&self, mode: i32, key: &str) -> Result<i32> {
        let mut r = self.call(code::SETTINGS_GET_INT, |d| Ok(d.write(&mode).and(d.write(key))?))?;
        Ok(r.read::<i32>()?)
    }
}

pub fn add_service(sm: &SIBinder, name: &str, service: &SIBinder) -> Result<()> {
    const ADD_SERVICE_TRANSACTION: u32 = 3;
    const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 1 << 3;
    let proxy = sm.as_proxy().context("handle 0 is not a proxy")?;
    let mut data = proxy.prepare_transact(true).map_err(|e| anyhow!("prepare: {e:?}"))?;
    data.write(name)?;
    data.write(service)?;
    data.write(&0i32)?;
    data.write(&DUMP_FLAG_PRIORITY_DEFAULT)?;
    let mut reply = proxy
        .submit_transact(ADD_SERVICE_TRANSACTION, &data, 0)
        .map_err(|e| anyhow!("servicemanager addService: {e:?}"))?
        .context("no reply from servicemanager")?;
    let status = reply.read::<Status>().map_err(|e| anyhow!("reply status: {e:?}"))?;
    if status.exception_code() != ExceptionCode::None {
        bail!("addService({name}): {status}");
    }
    Ok(())
}

pub fn connect(binder: &Path) -> Result<SIBinder> {
    let path = binder.to_str().context("binder path is not UTF-8")?;
    let ps = ProcessState::init(path, 4).map_err(|e| anyhow!("open {path}: {e}"))?;
    ps.context_object().map_err(|e| anyhow!("servicemanager handle 0: {e:?}"))
}

pub fn check_service(sm: &SIBinder, name: &str) -> Result<Option<SIBinder>> {
    const CHECK_SERVICE_TRANSACTION: u32 = 2;
    let proxy = sm.as_proxy().context("handle 0 is not a proxy")?;
    let mut data = proxy.prepare_transact(true).map_err(|e| anyhow!("prepare: {e:?}"))?;
    data.write(name)?;
    let mut reply = proxy
        .submit_transact(CHECK_SERVICE_TRANSACTION, &data, 0)
        .map_err(|e| anyhow!("servicemanager checkService: {e:?}"))?
        .context("no reply from servicemanager")?;
    let status = reply.read::<Status>().map_err(|e| anyhow!("reply status: {e:?}"))?;
    if status.exception_code() != ExceptionCode::None {
        bail!("checkService({name}): {status}");
    }
    Ok(reply.read::<Option<SIBinder>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sarab_exec_is_not_the_runtime() {
        assert!(enters(b"sarab-ns\0--enter\x001234\0--as\x000:0\0--\0sh\0"));
        assert!(!enters(b"sarab-ns\0--pid\0--net\0--root\0/images/system\0--\0/system/bin/init\0second_stage\0"));
        assert!(!enters(b"sarab-ns\0--pid\0--\0/system/bin/sh\0--enter\0"), "an --enter after -- is the command's");
        assert!(!enters(b""));
    }
}
