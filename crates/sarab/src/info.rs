//! `sarab info` (what is installed where, and whether this host can run it --
//! the first thing to paste into a bug report) and `sarab stats` (what the
//! running Android costs right now).
//!
//! The sub-id rows come from sarab-ns's library (`sarab_ns`), which holds the
//! id maps: how many sub-ids each map takes, and the user's first range, the
//! one the maps are laid onto. Both fit the 65536 every distro hands out.
//!
//! The binderfs, gpu and apparmor rows are the same checks `sarab setup`
//! refuses on (`binderfs`, host.rs, apparmor.rs), each with its reason.
//! binderfs counts once it is in /proc/filesystems. Most distribution kernels
//! (Ubuntu's and Debian's among them) build it as a module that nothing loads,
//! so when it is missing `binder_module` looks through the running kernel's
//! modules.dep for a binder module under drivers/android, and the answer is
//! the modprobe that loads it plus the modules-load.d line that loads it at
//! every boot, not "this kernel has no binderfs".
//!
//! `install_hint` turns a package name into the install command for the
//! distro in /etc/os-release (`ID`, then `ID_LIKE`), for the messages that
//! say what is missing; `install_hint_named` does it for a package whose name
//! differs by family, given in the order Arch, Debian, Fedora, SUSE
//! (`GEOCLUE`).
//!
//! The location row only says whether GeoClue is installed, by its D-Bus
//! activation file (`geoclue_installed`): asking GeoClue itself would start it
//! and may send the machine's Wi-Fi networks or address to a location service,
//! which a status command must not do. Whether an agent lets it answer, and
//! how accurate its answer is, is in hostd's log, which sees every fix.

use crate::paths;
use anyhow::{Result, bail};
use sarab_runtime::{find_runtime, is_frozen, runtime_cgroup};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub fn subid_files() -> (String, String) {
    let read = |f: &str| std::fs::read_to_string(f).unwrap_or_default();
    (read("/etc/subuid"), read("/etc/subgid"))
}

pub const GEOCLUE: [&str; 4] = ["geoclue", "geoclue-2.0", "geoclue2", "geoclue2"];

pub fn install_hint(package: &str) -> String {
    install_hint_named([package; 4])
}

pub fn install_hint_named(names: [&str; 4]) -> String {
    let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    install_command(&os, names).unwrap_or_else(|| format!("install the {} package", names[0]))
}

pub fn geoclue_installed() -> bool {
    ["/usr/share/dbus-1/system-services", "/usr/local/share/dbus-1/system-services"]
        .iter()
        .any(|d| Path::new(d).join("org.freedesktop.GeoClue2.service").is_file())
}

fn install_command(os_release: &str, names: [&str; 4]) -> Option<String> {
    let field = |k: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .map(|v| v.trim_matches('"').to_ascii_lowercase())
            .unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID"), field("ID_LIKE"));
    let (tool, package) = ids.split_whitespace().find_map(|id| match id {
        "arch" => Some(("pacman -S", names[0])),
        "debian" | "ubuntu" => Some(("apt install", names[1])),
        "fedora" | "rhel" => Some(("dnf install", names[2])),
        "suse" | "opensuse" => Some(("zypper install", names[3])),
        _ => None,
    })?;
    Some(format!("sudo {tool} {package}"))
}

pub fn username() -> String {
    std::env::var("USER").unwrap_or_else(|_| {
        let pw = unsafe { libc::getpwuid(libc::getuid()) };
        if pw.is_null() {
            return String::new();
        }
        unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned()
    })
}

pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub const HOST_TOOLS: &[(&str, &str)] = &[
    ("newuidmap", "shadow, or uidmap on Debian/Ubuntu"),
    ("systemd-run", "systemd"),
    ("ip", "iproute2, or iproute on Fedora"),
    ("pasta", "passt"),
    ("debugfs", "e2fsprogs"),
    ("curl", "curl"),
    ("sha256sum", "coreutils"),
];

fn binderfs_available() -> bool {
    std::fs::read_to_string("/proc/filesystems").is_ok_and(|s| s.lines().any(|l| l.ends_with("\tbinder")))
}

pub fn binderfs() -> Result<(), String> {
    if binderfs_available() {
        return Ok(());
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let dep = std::fs::read_to_string(format!("/lib/modules/{}/modules.dep", release.trim())).unwrap_or_default();
    Err(match binder_module(&dep) {
        Some(m) => format!(
            "binderfs is in this kernel's {m} module, which is not loaded. Load it now and at every boot:\n  \
             sudo modprobe {m} && echo {m} | sudo tee /etc/modules-load.d/sarab.conf"
        ),
        None => "this kernel has no binderfs (CONFIG_ANDROID_BINDERFS), and Android cannot run without it".into(),
    })
}

fn binder_module(dep: &str) -> Option<String> {
    let path = dep.lines().filter_map(|l| l.split(':').next()).find(|p| p.contains("/drivers/android/"))?;
    let name = path.rsplit('/').next()?;
    let name = name.split(".ko").next()?;
    name.contains("binder").then(|| name.to_string())
}

pub fn info(dirs: &paths::Dirs) -> Result<()> {
    let row = |k: &str, v: &str| println!("{k:<14} {v}");
    let shown = |p: &Path| p.display().to_string();
    println!("Sarab {}", env!("CARGO_PKG_VERSION"));
    if dirs.is_set_up() {
        let sys = dirs.system().join("system/build.prop");
        let old = |part: &str| {
            let behind = crate::image::build_of(&dirs.data.join(part), part)
                .is_some_and(|b| crate::image::standing(part, &b) == crate::image::Standing::Older);
            if behind { " (older than the tested build: `sarab upgrade`)" } else { "" }
        };
        row("image", &format!("{}{}", crate::image::describe(&dirs.system(), "system"), old("system")));
        row(
            "android",
            &format!(
                "{} (API {})",
                paths::build_prop(&sys, "ro.build.version.release").unwrap_or_default(),
                paths::build_prop(&sys, "ro.build.version.sdk").unwrap_or_default()
            ),
        );
        row("vendor", &format!("{}{}", crate::image::describe(&dirs.vendor(), "vendor"), old("vendor")));
    } else {
        row("image", "not set up (sarab start sets it up)");
    }
    let rt = match find_runtime() {
        Ok((pid, _)) => {
            format!("{} (pid {pid}, {})", if is_frozen() { "paused" } else { "running" }, crate::session::owner())
        }
        Err(_) => "stopped".into(),
    };
    row("runtime", &rt);
    row(
        "unit",
        if crate::session::unit_installed() { "sarab.service installed" } else { "none (packaging/install.sh)" },
    );
    println!("\nDirectories");
    row(
        "mode",
        &match &dirs.mode {
            paths::Mode::Checkout(root) => format!("checkout ({})", root.display()),
            paths::Mode::Installed => "installed".into(),
        },
    );
    row("data", &shown(&dirs.data));
    row("overlay", &format!("{} ({})", shown(&dirs.overlay), if dirs.overlay.is_dir() { "ok" } else { "MISSING" }));
    row("state", &shown(&dirs.state));
    row("runtime", &shown(&dirs.runtime));
    row("icons", &shown(&dirs.icons));
    for name in ["sarab-ns", "sarab-hostd"] {
        row(name, &dirs.helper(name).map_or_else(|e| format!("MISSING: {e}"), |p| shown(&p)));
    }
    println!("\nHost");
    let uname = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    row("kernel", uname.trim());
    row("binderfs", &binderfs().map_or_else(|why| format!("MISSING ({})", why.replace("\n  ", " ")), |()| "ok".into()));
    let user = username();
    let (subuid, subgid) = subid_files();
    for (name, text, need) in
        [("subuid", &subuid, sarab_ns::SUBUIDS_NEEDED), ("subgid", &subgid, sarab_ns::SUBGIDS_NEEDED)]
    {
        let n = sarab_ns::first_range(text, &user).map_or(0, |r| r.1);
        let verdict = if n >= need as u64 { "ok".to_string() } else { format!("need {need}: Android cannot start") };
        row(name, &format!("{n} ({verdict})"));
    }
    let gpu = match crate::host::gpu() {
        Ok(g) => format!("{} ({})", g.node, g.driver),
        Err(e) => format!("MISSING ({}): Android cannot start. {}", e.why, e.fix.replace("\n  ", " ")),
    };
    row("gpu", &gpu);
    row(
        "apparmor",
        &crate::apparmor::status(dirs)
            .unwrap_or_else(|why| format!("MISSING ({why}): Android cannot start; fix: {}", sarab_ns::apparmor::FIX)),
    );
    let uid = unsafe { libc::getuid() };
    row(
        "wayland",
        &crate::host::wayland_display(&crate::host::runtime_dir(uid)).unwrap_or_else(|e| format!("MISSING ({e})")),
    );
    let missing: Vec<String> =
        HOST_TOOLS.iter().filter(|(b, _)| !on_path(b)).map(|(b, pkg)| format!("{b} ({pkg})")).collect();
    row("tools", &if missing.is_empty() { "ok".to_string() } else { format!("MISSING {}", missing.join(", ")) });
    row(
        "location",
        &if geoclue_installed() {
            "GeoClue (the hostd log says how accurate its location is)".to_string()
        } else {
            format!("MISSING: apps get no location; fix: {}", install_hint_named(GEOCLUE))
        },
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Reading {
    at: Instant,
    cpu_usec: u64,
    memory: u64,
    swap: u64,
    pids: u64,
}

fn read_u64(dir: &Path, file: &str) -> u64 {
    std::fs::read_to_string(dir.join(file)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn cpu_usage(stat: &str) -> u64 {
    stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

fn read(dir: &Path) -> Reading {
    Reading {
        at: Instant::now(),
        cpu_usec: cpu_usage(&std::fs::read_to_string(dir.join("cpu.stat")).unwrap_or_default()),
        memory: read_u64(dir, "memory.current"),
        swap: read_u64(dir, "memory.swap.current"),
        pids: read_u64(dir, "pids.current"),
    }
}

fn cpu_percent(a: &Reading, b: &Reading) -> f64 {
    let wall = b.at.duration_since(a.at).as_micros() as f64;
    if wall <= 0.0 { 0.0 } else { (b.cpu_usec.saturating_sub(a.cpu_usec)) as f64 * 100.0 / wall }
}

fn mb(b: u64) -> String {
    format!("{} MB", b / 1_048_576)
}

pub fn stats(stream: bool, json: bool) -> Result<()> {
    if find_runtime().is_err() {
        bail!("Android is not running (sarab start)");
    }
    let dir = runtime_cgroup().ok_or_else(|| anyhow::anyhow!("the runtime has no cgroup of its own"))?;
    let mut prev = read(&dir);
    let tty = std::io::stdout().is_terminal();
    let header = format!("{:<8} {:>9} {:>9} {:>7} {:>6}", "STATE", "MEMORY", "SWAP", "CPU", "PIDS");
    if !json {
        println!("{header}");
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if find_runtime().is_err() {
            bail!("Android stopped");
        }
        let now = read(&dir);
        let state = if is_frozen() { "paused" } else { "running" };
        let cpu = cpu_percent(&prev, &now);
        if json {
            println!(
                "{}",
                serde_json::json!({ "state": state, "memory_bytes": now.memory, "swap_bytes": now.swap,
                                    "cpu_percent": (cpu * 100.0).round() / 100.0, "pids": now.pids })
            );
        } else {
            let line = format!("{state:<8} {:>9} {:>9} {:>6.2}% {:>6}", mb(now.memory), mb(now.swap), cpu, now.pids);
            if tty && stream {
                print!("\x1b[1A\x1b[2K{header}\n\x1b[2K{line}\r");
                std::io::stdout().flush()?;
            } else {
                println!("{line}");
            }
        }
        if !stream {
            return Ok(());
        }
        prev = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binder_module_is_found_in_modules_dep() {
        let ubuntu =
            "kernel/fs/fuse/fuse.ko.zst:\nkernel/drivers/android/binder_linux.ko.zst:\nkernel/net/tun.ko.zst:\n";
        assert_eq!(binder_module(ubuntu).as_deref(), Some("binder_linux"));
        assert_eq!(binder_module("kernel/drivers/android/binder_linux.ko:\n").as_deref(), Some("binder_linux"));
        assert_eq!(binder_module("kernel/fs/fuse/fuse.ko.zst:\n"), None);
        assert_eq!(binder_module(""), None);
    }

    #[test]
    fn the_install_line_follows_the_distro() {
        let hint = |os: &str| install_command(os, ["passt"; 4]);
        assert_eq!(hint("NAME=\"Arch Linux\"\nID=arch\n").as_deref(), Some("sudo pacman -S passt"));
        assert_eq!(hint("ID=ubuntu\nID_LIKE=debian\n").as_deref(), Some("sudo apt install passt"));
        assert_eq!(hint("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n").as_deref(), Some("sudo apt install passt"));
        assert_eq!(hint("ID=fedora\n").as_deref(), Some("sudo dnf install passt"));
        assert_eq!(
            hint("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n").as_deref(),
            Some("sudo zypper install passt")
        );
        assert_eq!(hint("ID=endeavouros\nID_LIKE=arch\n").as_deref(), Some("sudo pacman -S passt"));
        assert_eq!(hint("ID=nixos\n"), None);
        assert_eq!(hint(""), None);
        let geoclue = |os: &str| install_command(os, GEOCLUE);
        assert_eq!(geoclue("ID=arch\n").as_deref(), Some("sudo pacman -S geoclue"));
        assert_eq!(geoclue("ID=ubuntu\nID_LIKE=debian\n").as_deref(), Some("sudo apt install geoclue-2.0"));
        assert_eq!(geoclue("ID=fedora\n").as_deref(), Some("sudo dnf install geoclue2"));
    }

    #[test]
    fn cpu_is_a_share_of_one_core() {
        let t = Instant::now();
        let a = Reading { at: t, cpu_usec: 1_000, memory: 0, swap: 0, pids: 0 };
        let b = Reading { at: t + Duration::from_secs(1), cpu_usec: 501_000, ..a };
        assert!((cpu_percent(&a, &b) - 50.0).abs() < 0.01);
        assert_eq!(cpu_usage("usage_usec 1234\nuser_usec 1000\n"), 1234);
    }
}
