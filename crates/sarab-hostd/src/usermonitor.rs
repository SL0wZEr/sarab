//! The image's `IUserMonitor` binder service: Android tells us when its user
//! is unlocked and when packages come and go. We turn that into freedesktop
//! `.desktop` entries so Android apps appear in the host launcher and can be
//! started without knowing Android is involved.
//!
//! Android calls us synchronously, from system_server, so the binder side
//! (`Service`) only checks the caller, queues a `Job` and answers; one worker
//! thread (`serve`) owns the `UserMonitor` and does the work in order: the
//! sync with its icon extraction (a process per app, seconds on a first
//! boot) no longer holds system_server's thread, and two calls can no longer
//! write the same entry at once through the same temporary file. `serve`
//! queues a first sync when asked, for a hostd started while Android is
//! already up.
//!
//! Every entry we own carries `PREFIX`, so `sync_all` can delete stale ones
//! without touching anyone else's. System apps get `NoDisplay=true` rather
//! than no entry, because notifications need a desktop file to match.
//! `Categories` includes `Utility` only to keep `desktop-file-validate` quiet;
//! `X-Sarab-App` is the key we use. `StartupWMClass` matches the name the
//! composer HAL gives each toplevel (a fixed prefix plus the package).
//!
//! The unlock is also what theme.rs waits for before it sets Android's dark
//! mode, the first moment Android's window manager is sure to be ready, so it
//! is passed on to whoever asked (`tell_on_unlock`) straight from the binder
//! call.
//!
//! Everything here is app-controlled input written into the host's files.
//! `is_package_name` applies Android's own PackageParser.validateName rule
//! before a name becomes a path, so `../../.bashrc` cannot. `entry_value`
//! turns control characters into spaces so a label cannot end the `Name=`
//! line and add a key such as `Exec=`. `exec_arg` applies both layers of the
//! spec's quoting: Exec quoting, then the file's string escape (one backslash
//! becomes four), and `%` doubled. `is_plain_png` only lets through a real PNG
//! of icon size, since the host's image loaders parse whatever we write;
//! adaptive and vector icons cannot be extracted and get no icon. A package
//! update drops the cached icon, since the label or icon may have changed.
//!
//! `replace_file` writes each entry beside its final name and renames it into
//! place, and leaves an unchanged one alone. A launcher that watches the
//! directory (Omarchy's does) re-reads an entry only when the directory
//! changes, so one rewritten in place kept showing the old entry, without its
//! new icon, until the launcher restarted.

use crate::apk;
use crate::wire::Gate;
use crate::wire::logln;
use crate::wire::{ok, str_arg};
use anyhow::{Context, Result};
use rsbinder::*;
use sarab_runtime::{AppInfo, Platform};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};

pub const DESCRIPTOR: &str = "lineageos.waydroid.IUserMonitor";
pub const SERVICE_NAME: &str = "waydroidusermonitor";

const USER_UNLOCKED: TransactionCode = 1;
const PACKAGE_STATE_CHANGED: TransactionCode = 2;

const PACKAGE_REMOVED: i32 = 1;

const PREFIX: &str = "sarab.";
const LAUNCHER_CATEGORY: &str = "android.intent.category.LAUNCHER";

pub struct UserMonitor {
    binder_path: PathBuf,
    init_pid: u32,
    apps_dir: PathBuf,
    icons_dir: PathBuf,
    launcher: PathBuf,
}

enum Job {
    SyncAll,
    Package { removed: bool, package: String },
}

pub struct Service {
    gate: Gate,
    jobs: Sender<Job>,
    unlocked: Option<Sender<()>>,
    apps_dir: PathBuf,
}

pub fn serve(monitor: UserMonitor, sync_first: bool, gate: Gate) -> Result<Service> {
    let (jobs, queue) = channel();
    if sync_first {
        let _ = jobs.send(Job::SyncAll);
    }
    let apps_dir = monitor.apps_dir.clone();
    std::thread::Builder::new()
        .name("usermonitor".into())
        .spawn(move || {
            for job in queue {
                monitor.run(job);
            }
        })
        .context("spawn the usermonitor thread")?;
    Ok(Service { gate, jobs, unlocked: None, apps_dir })
}

impl Service {
    pub fn tell_on_unlock(&mut self, to: Sender<()>) {
        self.unlocked = Some(to);
    }
}

impl UserMonitor {
    pub fn new(binder_path: PathBuf, init_pid: u32, launcher: PathBuf, icons: Option<PathBuf>) -> Result<Self> {
        let data = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/share"));
        let apps_dir = data.join("applications");
        let icons_dir = icons.unwrap_or_else(|| data.join("sarab/icons"));
        std::fs::create_dir_all(&apps_dir).with_context(|| format!("create {}", apps_dir.display()))?;
        std::fs::create_dir_all(&icons_dir).with_context(|| format!("create {}", icons_dir.display()))?;
        Ok(Self { binder_path, init_pid, apps_dir, icons_dir, launcher })
    }

    fn run(&self, job: Job) {
        match job {
            Job::SyncAll => {
                if let Err(e) = self.sync_all() {
                    logln!("usermonitor: sync failed: {e}");
                }
            }
            Job::Package { removed: true, package } => {
                let _ = std::fs::remove_file(self.desktop_path(&package));
                let _ = std::fs::remove_file(self.icons_dir.join(format!("{package}.png")));
                logln!("usermonitor: {package} removed");
            }
            Job::Package { removed: false, package } => {
                let _ = std::fs::remove_file(self.icons_dir.join(format!("{package}.png")));
                match self.platform().and_then(|p| p.app(&package)) {
                    Ok(Some(app)) => {
                        self.sync_app(&app);
                        logln!("usermonitor: {package} added/updated");
                    }
                    Ok(None) => logln!("usermonitor: {package} not known to Android"),
                    Err(e) => logln!("usermonitor: {package}: {e}"),
                }
            }
        }
    }

    fn platform(&self) -> Result<Platform> {
        Platform::connect(&self.binder_path)
    }

    fn desktop_path(&self, package: &str) -> PathBuf {
        self.apps_dir.join(format!("{PREFIX}{package}.desktop"))
    }

    fn sync_app(&self, app: &AppInfo) {
        if !is_package_name(&app.package_name) {
            logln!("usermonitor: not a package name, no entry: {:?}", app.package_name);
            return;
        }
        let path = self.desktop_path(&app.package_name);
        let launchable = app.categories.iter().any(|c| c.trim() == LAUNCHER_CATEGORY);
        if !launchable {
            let _ = std::fs::remove_file(&path);
            return;
        }

        let (icon, is_system) = self.ensure_icon(&app.package_name);
        let exec = exec_arg(&self.launcher.to_string_lossy());
        let pkg = &app.package_name;
        let mut entry = String::from("[Desktop Entry]\nType=Application\n");
        entry.push_str(&format!("Name={}\n", entry_value(&app.name)));
        entry.push_str(&format!("Exec={exec} app launch {pkg}\n"));
        if let Some(icon) = &icon {
            entry.push_str(&format!("Icon={}\n", icon.display()));
        }
        entry.push_str("Terminal=false\n");
        if is_system {
            entry.push_str("NoDisplay=true\n");
        }
        entry.push_str("Categories=Utility;X-Sarab-App;\n");
        entry.push_str(&format!("StartupWMClass=waydroid.{pkg}\n"));
        entry.push_str(&format!("X-Sarab-Package={pkg}\n"));
        entry.push_str("Actions=app-settings;\n\n");
        entry.push_str("[Desktop Action app-settings]\nName=App Settings\n");
        entry
            .push_str(&format!("Exec={exec} app intent android.settings.APPLICATION_DETAILS_SETTINGS package:{pkg}\n"));

        if let Err(e) = replace_file(&path, &entry) {
            logln!("usermonitor: write {}: {e}", path.display());
        }
    }

    fn ensure_icon(&self, package: &str) -> (Option<PathBuf>, bool) {
        let path = self.icons_dir.join(format!("{package}.png"));
        let cached = path.exists().then(|| path.clone());
        match apk::app_files(self.init_pid, package) {
            Ok(files) => {
                let system = files.is_system();
                if cached.is_some() {
                    return (cached, system);
                }
                match files.icon_png.filter(|png| is_plain_png(png)) {
                    Some(png) => match std::fs::write(&path, png) {
                        Ok(()) => (Some(path), system),
                        Err(e) => {
                            logln!("usermonitor: write icon {}: {e}", path.display());
                            (None, system)
                        }
                    },
                    None => (None, system),
                }
            }
            Err(e) => {
                logln!("usermonitor: {package}: {e}");
                (cached, false)
            }
        }
    }

    fn sync_all(&self) -> Result<()> {
        let platform = self.platform()?;
        let apps = platform.apps()?;
        for app in &apps {
            self.sync_app(app);
        }
        let keep: Vec<String> = apps.iter().map(|a| format!("{PREFIX}{}.desktop", a.package_name)).collect();
        if let Ok(entries) = std::fs::read_dir(&self.apps_dir) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(PREFIX) && name.ends_with(".desktop") && !keep.iter().any(|k| *k == *name) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        logln!("usermonitor: {} app entries in {}", apps.len(), self.apps_dir.display());
        Ok(())
    }
}

fn replace_file(path: &Path, content: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))
}

impl Remotable for Service {
    fn descriptor() -> &'static str
    where
        Self: Sized,
    {
        DESCRIPTOR
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> rsbinder::Result<()> {
        self.gate.check(SERVICE_NAME)?;
        match code {
            USER_UNLOCKED => {
                let uid: i32 = data.read()?;
                logln!("usermonitor: user {uid} unlocked");
                if let Some(to) = &self.unlocked {
                    let _ = to.send(());
                }
                let _ = self.jobs.send(Job::SyncAll);
                ok(reply)
            }
            PACKAGE_STATE_CHANGED => {
                let mode: i32 = data.read()?;
                let package = str_arg(data)?;
                let _uid: i32 = data.read()?;
                if is_package_name(&package) {
                    let _ = self.jobs.send(Job::Package { removed: mode == PACKAGE_REMOVED, package });
                } else {
                    logln!("usermonitor: not a package name, ignored: {package:?}");
                }
                ok(reply)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
        let _ = writeln!(w, "sarab usermonitor: entries in {}", self.apps_dir.display());
        Ok(())
    }
}

fn exec_arg(s: &str) -> String {
    let s = s.replace('%', "%%");
    if !s.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        return s;
    }
    let mut q = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '`' | '$' => {
                q.push_str("\\\\");
                q.push(c);
            }
            '\\' => q.push_str("\\\\\\\\"),
            _ => q.push(c),
        }
    }
    q.push('"');
    q
}

fn is_package_name(s: &str) -> bool {
    s.len() <= 255
        && s.split('.').all(|seg| {
            seg.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn entry_value(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().replace('\\', "\\\\")
}

fn is_plain_png(b: &[u8]) -> bool {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    const MAX_BYTES: usize = 4 << 20;
    const MAX_SIDE: u32 = 1024;
    if b.len() > MAX_BYTES || !b.starts_with(SIGNATURE) || b.get(12..16) != Some(b"IHDR") {
        return false;
    }
    let side = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
    b.len() >= 24 && (1..=MAX_SIDE).contains(&side(16)) && (1..=MAX_SIDE).contains(&side(20))
}

#[cfg(test)]
mod tests {
    use super::{entry_value, exec_arg, is_package_name, is_plain_png, replace_file};

    #[test]
    fn an_entry_is_replaced_by_a_rename_and_only_when_it_changed() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("sarab-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("sarab.a.desktop");
        std::fs::write(&f, "old").unwrap();
        let first = std::fs::metadata(&f).unwrap().ino();
        replace_file(&f, "new").unwrap();
        let second = std::fs::metadata(&f).unwrap().ino();
        assert_ne!(first, second, "rewritten in place");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        replace_file(&f, "new").unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().ino(), second, "rewritten though unchanged");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["sarab.a.desktop"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_package_names_become_file_names() {
        for ok in ["com.example.app", "org.sarab.home", "android", "com.x_y.Z9"] {
            assert!(is_package_name(ok), "{ok}");
        }
        for bad in ["", "../../.bashrc", "com..x", ".com.x", "com.x.", "com.9x", "com.x/y", "com.x y", "com.é"] {
            assert!(!is_package_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_label_cannot_add_a_key() {
        assert_eq!(entry_value("Foo\nExec=sh -c evil"), "Foo Exec=sh -c evil");
        assert_eq!(entry_value("a\rb\tc\u{0}d"), "a b c d");
        assert_eq!(entry_value("C:\\x"), "C:\\\\x");
        assert_eq!(entry_value("Café ☕"), "Café ☕");
    }

    #[test]
    fn icons_are_sniffed_before_they_are_written() {
        let png = |w: u32, h: u32| {
            let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
            v.extend(w.to_be_bytes());
            v.extend(h.to_be_bytes());
            v.extend([8, 6, 0, 0, 0]);
            v
        };
        assert!(is_plain_png(&png(192, 192)));
        assert!(!is_plain_png(&png(0, 192)));
        assert!(!is_plain_png(&png(65_535, 65_535)));
        assert!(!is_plain_png(b"<svg/>"));
        assert!(!is_plain_png(&png(192, 192)[..20]));
    }

    #[test]
    fn exec_paths_are_quoted_only_when_they_need_it() {
        assert_eq!(exec_arg("/home/u/.local/bin/sarab"), "/home/u/.local/bin/sarab");
        assert_eq!(exec_arg("/home/u/My Code/sarab"), "\"/home/u/My Code/sarab\"");
        assert_eq!(exec_arg("/a/$x/b c"), "\"/a/\\\\$x/b c\"");
        assert_eq!(exec_arg("/a/100%/b"), "/a/100%%/b");
        assert_eq!(exec_arg("/a\\b"), "\"/a\\\\\\\\b\"");
    }
}
