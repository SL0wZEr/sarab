//! Stopping the runtime directly, and `sarab status`, found by process
//! identity rather than by guessing at command lines. `sarab stop` itself goes
//! through session.rs first, which hands a unit-owned runtime to systemd.
//!
//! find_runtime() reports the `sarab-ns` that set the namespaces up (the one
//! whose root is Android's, sarab-runtime explains); above it is the harness
//! parent, still in the host's namespaces, and `outermost_ns` walks up (by
//! comm, which the kernel truncates at 15 characters; `sarab-ns` fits) to
//! that parent, bounded so a pid reused
//! mid-walk cannot loop. `stop` SIGKILLs it: init does not handle signals for
//! us, and every descendant has PR_SET_PDEATHSIG back to it. The tree is
//! thawed first (one write, about 13 ms) so it is not torn down mid-freeze.
//! Stopping what is already stopped succeeds, as `docker stop` and
//! `systemctl stop` do.
//!
//! `status` exits 3 when nothing runs (the LSB / `systemctl status`
//! convention), and never thaws just to ask: a binder call into a frozen tree
//! blocks, and "paused" is already the answer. In the active-apps property,
//! "none" is the prop file's placeholder and the composer writes its own fixed
//! name for the whole Android UI; neither is an app.

use anyhow::{Result, bail};
use sarab_runtime::{Platform, find_runtime, is_frozen, runtime_cgroup, set_frozen};
use std::time::Duration;

fn parse_status(s: &str) -> Option<(String, u32)> {
    let mut name = None;
    let mut ppid = None;
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("Name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("PPid:") {
            ppid = v.trim().parse().ok();
        }
    }
    Some((name?, ppid?))
}

fn status_of(pid: u32) -> Option<(String, u32)> {
    parse_status(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
}

fn outermost_ns(pid: u32, look: impl Fn(u32) -> Option<(String, u32)>) -> u32 {
    let mut cur = pid;
    for _ in 0..64 {
        let Some((_, ppid)) = look(cur) else { break };
        if ppid <= 1 {
            break;
        }
        match look(ppid) {
            Some((comm, _)) if comm == "sarab-ns" => cur = ppid,
            _ => break,
        }
    }
    cur
}

pub fn stop() -> Result<()> {
    let Ok((pid, _)) = find_runtime() else {
        println!("not running");
        return Ok(());
    };
    if is_frozen() {
        set_frozen(false)?;
    }
    let outer = outermost_ns(pid, status_of);
    if unsafe { libc::kill(outer as i32, libc::SIGKILL) } != 0 {
        bail!("kill {outer}: {}", std::io::Error::last_os_error());
    }
    for _ in 0..100 {
        if find_runtime().is_err() {
            println!("stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("killed {outer} but a runtime is still present 5 s later");
}

pub fn status(json: bool) -> Result<()> {
    let Ok((pid, dev)) = find_runtime() else {
        if json {
            println!("{}", serde_json::json!({ "running": false }));
        } else {
            println!("not running");
        }
        std::process::exit(3)
    };
    let frozen = is_frozen();
    let cgroup = runtime_cgroup().map(|c| c.display().to_string());
    let mut props: Vec<(&str, Option<String>)> = Vec::new();
    let mut unreachable = None;
    if !frozen {
        match Platform::connect(&dev) {
            Ok(p) => {
                for prop in ["sys.boot_completed", "waydroid.open_windows", "waydroid.active_apps"] {
                    props.push((prop, p.getprop(prop, "?").ok()));
                }
            }
            Err(e) => unreachable = Some(e.to_string()),
        }
    }
    let prop = |k: &str| props.iter().find(|(n, _)| *n == k).and_then(|(_, v)| v.clone());
    let state = if frozen {
        "paused"
    } else if prop("sys.boot_completed").as_deref() == Some("1") {
        "running"
    } else {
        "booting"
    };
    let owner = crate::session::owner();
    let active_app = prop("waydroid.active_apps").filter(|a| !matches!(a.as_str(), "none" | "" | "Waydroid"));
    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": true,
                "state": state,
                "pid": pid,
                "owner": owner,
                "cgroup": cgroup,
                "open_windows": prop("waydroid.open_windows").and_then(|v| v.parse::<u32>().ok()),
                "active_app": active_app,
            })
        );
        return Ok(());
    }
    println!("{:<14} {state}", "state");
    println!("{:<14} {pid}", "pid");
    println!("{:<14} {owner}", "started by");
    println!("{:<14} {}", "cgroup", cgroup.as_deref().unwrap_or("-"));
    if let Some(n) = prop("waydroid.open_windows") {
        println!("{:<14} {n}", "open windows");
    }
    if let Some(a) = prop("waydroid.active_apps") {
        let a = match a.as_str() {
            "Waydroid" => "(full Android UI)",
            "none" | "" => "-",
            other => other,
        };
        println!("{:<14} {a}", "active app");
    }
    if let Some(e) = unreachable {
        println!("{:<14} unreachable ({e})", "platform");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const STATUS: &str = "\
Name:\tsarab-ns
Umask:\t0022
State:\tS (sleeping)
Tgid:\t4242
Ngid:\t0
Pid:\t4242
PPid:\t4211
TracerPid:\t0
";

    #[test]
    fn reads_name_and_ppid() {
        assert_eq!(parse_status(STATUS), Some(("sarab-ns".to_string(), 4211)));
        assert_eq!(parse_status("Name:\tinit\nPPid:\t0\n"), Some(("init".to_string(), 0)));
        assert_eq!(parse_status("Name:\tsarab-ns\n"), None);
        assert_eq!(parse_status(""), None);
    }

    fn tree() -> HashMap<u32, (String, u32)> {
        HashMap::from([
            (900, ("sarab".to_string(), 1)),
            (901, ("sarab-ns".to_string(), 900)),
            (902, ("sarab-ns".to_string(), 901)),
            (903, ("sarab-ns".to_string(), 902)),
        ])
    }

    #[test]
    fn walks_up_to_the_outermost_sarab_ns() {
        let t = tree();
        let look = |p: u32| t.get(&p).cloned();
        assert_eq!(outermost_ns(903, look), 901);
        assert_eq!(outermost_ns(901, look), 901);
        assert_eq!(outermost_ns(999, look), 999);
    }

    #[test]
    fn a_cycle_cannot_hang_the_walk() {
        let t = HashMap::from([(10, ("sarab-ns".to_string(), 11)), (11, ("sarab-ns".to_string(), 10))]);
        let asked = std::cell::Cell::new(0);
        let found = outermost_ns(10, |p: u32| {
            asked.set(asked.get() + 1);
            t.get(&p).cloned()
        });
        assert!(found == 10 || found == 11);
        assert!(asked.get() > 64, "the walk went round the cycle until its bound, {} steps", asked.get());
    }
}
