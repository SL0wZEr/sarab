//! Running things inside the Android namespace: `sarab exec`, and every
//! command that needs `pm`, `cmd`, `logcat` or `sqlite3` rather than binder.
//!
//! Two details were each paid for once. The environment is Android's own
//! (`ENV`: init.environ.rc plus ANDROID_RUNTIME_ROOT), without which `sqlite3`
//! aborts on its first query and looked, for a while, like it simply did not
//! exist. And the caller can be the shell uid: `pm uninstall` and `cmd role`
//! fail from uid 0 with an AppOps NullPointerException (they need a *calling
//! package*, and root has none), which is the only reason the scripts ever went
//! through adb. uid 2000 is `com.android.shell`, which is what adb would have
//! given us anyway.
//!
//! `enter_argv` goes through `sarab-ns --enter`, not nsenter: it is nsenter
//! plus supplementary groups, and each user gets the ones Android gives it
//! (`User::groups`): shell has adbd's (`SHELL_GROUPS`, from adbd's
//! `drop_privileges`: adb, log, input, inet, net_bt, net_bt_admin, sdcard_r,
//! sdcard_rw, net_bw_stats, readproc, uhid, ext_data_rw, ext_obb_rw), without
//! which the resolver refuses it (no `inet`) and hidepid hides every other
//! process (no `readproc`); system has system_server's (`SYSTEM_GROUPS`, its
//! `--setgroups` in ZygoteInit.forkSystemServer). Root gets none beyond its own
//! gid, since it holds every capability, and never the host's, which would show
//! up unmapped. The command runs under `env -i`: the host's PATH, LD_* and
//! XDG_* have no business in there, and a stray LD_PRELOAD would be loaded by
//! the bionic linker. Only TERM is passed through, or an interactive `sarab
//! exec` gets a shell that cannot redraw its prompt. HOME and TMPDIR are
//! /data/local/tmp because /tmp in the image is not writable by shell. The
//! classpaths are not in `ENV`: init loads them at boot from what
//! derive_classpath writes, so they differ between images, and `zygote_env`
//! copies them from zygote's environment (`inherited`: every `*CLASSPATH`, and
//! `STANDALONE_SYSTEMSERVER_JARS`). Without them `svc`, `content`,
//! `uiautomator`, `am instrument` and every other app_process tool printed
//! nothing and exited 0. Before zygote runs there are none to copy, and the
//! command runs without them. `init_pid` targets Android's init (the sarab-ns
//! child running `/system/bin/init second_stage`) because the sarab-ns above it
//! is not in the pid namespace. `command` thaws first: a process started in a
//! frozen tree blocks forever on its first binder call. In `run_with_input` a
//! write error is ignored on purpose: it is the child exiting early (a bad APK
//! is rejected before it is read in full), and its stderr says why.

use anyhow::{Context, Result, anyhow, bail};
use std::io::Write;
use std::process::{Command, Output, Stdio};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum User {
    Root,
    Shell,
    Uid(u32),
}

impl User {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "root" | "0" => User::Root,
            "shell" | "2000" => User::Shell,
            "system" => User::Uid(1000),
            n => User::Uid(n.parse().map_err(|_| anyhow!("unknown user {n:?} (root, shell, system or a uid)"))?),
        })
    }

    fn uid(self) -> u32 {
        match self {
            User::Root => 0,
            User::Shell => 2000,
            User::Uid(n) => n,
        }
    }

    fn groups(self) -> &'static [u32] {
        match self.uid() {
            2000 => SHELL_GROUPS,
            1000 => SYSTEM_GROUPS,
            _ => &[],
        }
    }
}

const SHELL_GROUPS: &[u32] = &[1011, 1007, 1004, 3003, 3002, 3001, 1028, 1015, 3006, 3009, 3011, 1078, 1079];
const SYSTEM_GROUPS: &[u32] = &[
    1001, 1002, 1003, 1004, 1005, 1006, 1007, 1008, 1009, 1010, 1018, 1021, 1023, 1024, 1032, 1065, 3001, 3002, 3003,
    3005, 3006, 3007, 3009, 3010, 3011, 3012,
];

const ENV: &[&str] = &[
    "PATH=/product/bin:/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system_ext/bin:/system/bin:/system/xbin:/odm/bin:/vendor/bin:/vendor/xbin",
    "ANDROID_ROOT=/system",
    "ANDROID_ASSETS=/system/app",
    "ANDROID_DATA=/data",
    "ANDROID_STORAGE=/storage",
    "ANDROID_ART_ROOT=/apex/com.android.art",
    "ANDROID_I18N_ROOT=/apex/com.android.i18n",
    "ANDROID_TZDATA_ROOT=/apex/com.android.tzdata",
    "ANDROID_RUNTIME_ROOT=/apex/com.android.runtime",
    "EXTERNAL_STORAGE=/sdcard",
    "ASEC_MOUNTPOINT=/mnt/asec",
    "HOME=/data/local/tmp",
    "TMPDIR=/data/local/tmp",
];

pub fn init_pid() -> Result<u32> {
    let (rt, _) = sarab_runtime::find_runtime().map_err(|_| anyhow!("Android is not running (sarab start)"))?;
    for e in std::fs::read_dir("/proc")? {
        let Some(pid) = e?.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        let ppid = status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse::<u32>().ok());
        if ppid != Some(rt) {
            continue;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if cmdline.starts_with(b"/system/bin/init\0second_stage") {
            return Ok(pid);
        }
    }
    bail!("the runtime (pid {rt}) has no init process yet; is it still starting?")
}

fn inherited(environ: &[u8]) -> Vec<String> {
    environ
        .split(|&b| b == 0)
        .filter_map(|v| std::str::from_utf8(v).ok())
        .filter(|v| {
            v.split_once('=').is_some_and(|(k, _)| k.ends_with("CLASSPATH") || k == "STANDALONE_SYSTEMSERVER_JARS")
        })
        .map(str::to_string)
        .collect()
}

fn zygote_env(init: u32) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        let ppid = status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse::<u32>().ok());
        if ppid != Some(init)
            || !std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default().starts_with(b"zygote")
        {
            continue;
        }
        if let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) {
            return inherited(&env);
        }
    }
    Vec::new()
}

fn enter_argv(init: u32, user: User, extra: &[u32], zygote: &[String], argv: &[String]) -> Vec<String> {
    let id = user.uid();
    let mut groups: Vec<u32> = user.groups().iter().chain(extra).copied().filter(|&g| g != id).collect();
    groups.dedup();
    let groups: Vec<String> = groups.iter().map(u32::to_string).collect();
    let mut a: Vec<String> = vec!["--enter".into(), init.to_string(), "--as".into()];
    a.push(format!("{id}:{id}:{}", groups.join(",")));
    a.push("--".into());
    a.push("/system/bin/env".into());
    a.push("-i".into());
    if let Ok(t) = std::env::var("TERM") {
        a.push(format!("TERM={t}"));
    }
    a.extend(ENV.iter().map(|s| s.to_string()));
    a.extend(zygote.iter().cloned());
    a.extend(argv.iter().cloned());
    a
}

pub fn command(user: User, argv: &[String]) -> Result<Command> {
    command_with(user, &[], argv)
}

fn command_with(user: User, extra: &[u32], argv: &[String]) -> Result<Command> {
    let ns = crate::paths::dirs()?.helper("sarab-ns")?;
    let init = init_pid()?;
    sarab_runtime::thaw()?;
    let mut c = Command::new(ns);
    c.args(enter_argv(init, user, extra, &zygote_env(init), argv));
    Ok(c)
}

pub fn output(user: User, argv: &[&str]) -> Result<Output> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    command(user, &argv)?.stdin(Stdio::null()).output().context("run sarab-ns")
}

pub fn run(user: User, argv: &[&str]) -> Result<String> {
    let o = output(user, argv)?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        let msg = if err.trim().is_empty() { out.trim() } else { err.trim() };
        bail!("{} failed: {msg}", argv.join(" "));
    }
    Ok(out)
}

pub fn run_with_input(user: User, argv: &[&str], input: &mut dyn std::io::Read) -> Result<String> {
    let argv_s: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let mut child = command(user, &argv_s)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run sarab-ns")?;
    let mut stdin = child.stdin.take().expect("piped");
    let _ = std::io::copy(input, &mut stdin).and_then(|_| stdin.flush());
    drop(stdin);
    let o = child.wait_with_output()?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        let msg = if err.trim().is_empty() { out.trim() } else { err.trim() };
        bail!("{msg}");
    }
    Ok(out)
}

pub fn exec(user: User, extra: &[u32], argv: &[String]) -> Result<i32> {
    let argv = if argv.is_empty() { vec!["/system/bin/sh".to_string()] } else { argv.to_vec() };
    let st = command_with(user, extra, &argv)?.status().context("run sarab-ns")?;
    Ok(st.code().unwrap_or(128 + std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_parse_by_name_and_number() {
        assert_eq!(User::parse("root").unwrap(), User::Root);
        assert_eq!(User::parse("shell").unwrap(), User::Shell);
        assert_eq!(User::parse("2000").unwrap(), User::Shell);
        assert_eq!(User::parse("system").unwrap(), User::Uid(1000));
        assert_eq!(User::parse("10145").unwrap(), User::Uid(10145));
        assert!(User::parse("bob").is_err());
    }

    #[test]
    fn root_enters_as_plain_root_with_no_host_groups() {
        let a = enter_argv(42, User::Root, &[], &[], &["getprop".into(), "sys.boot_completed".into()]);
        assert_eq!(&a[..5], ["--enter", "42", "--as", "0:0:", "--"]);
        assert_eq!(&a[a.len() - 2..], ["getprop", "sys.boot_completed"]);
        let env = a.iter().position(|x| x == "/system/bin/env").unwrap();
        assert_eq!(a[env + 1], "-i", "the host environment stays out");
        assert!(a.iter().any(|x| x == "ANDROID_RUNTIME_ROOT=/apex/com.android.runtime"));
        assert!(a.iter().any(|x| x.starts_with("PATH=") && x.contains("/system/bin")));
    }

    #[test]
    fn shell_gets_adbds_groups_and_extra_ones_append() {
        let a = enter_argv(42, User::Shell, &[], &[], &["pm".into(), "uninstall".into(), "x".into()]);
        assert_eq!(&a[2..4], ["--as", "2000:2000:1011,1007,1004,3003,3002,3001,1028,1015,3006,3009,3011,1078,1079"]);
        let a = enter_argv(42, User::Uid(10145), &[3003, 10145], &[], &["id".into()]);
        assert_eq!(a[3], "10145:10145:3003", "extra groups, less the primary gid");
        let a = enter_argv(42, User::parse("system").unwrap(), &[], &[], &["id".into()]);
        assert!(a[3].starts_with("1000:1000:1001,") && a[3].contains(",3003,"));
    }

    #[test]
    fn the_classpaths_come_from_zygote_and_nothing_else_does() {
        let env = b"HOME=/\0BOOTCLASSPATH=/a.jar:/b.jar\0ANDROID_SOCKET_zygote=17\0SYSTEMSERVERCLASSPATH=/s.jar\0\
DEX2OATBOOTCLASSPATH=/a.jar\0STANDALONE_SYSTEMSERVER_JARS=/j.jar\0NOTCLASSPATHX=1\0";
        let v = inherited(env);
        assert_eq!(
            v,
            [
                "BOOTCLASSPATH=/a.jar:/b.jar",
                "SYSTEMSERVERCLASSPATH=/s.jar",
                "DEX2OATBOOTCLASSPATH=/a.jar",
                "STANDALONE_SYSTEMSERVER_JARS=/j.jar"
            ]
        );
        let a = enter_argv(42, User::Shell, &[], &v, &["svc".into(), "power".into()]);
        let boot = a.iter().position(|x| x.starts_with("BOOTCLASSPATH=")).unwrap();
        assert!(boot > a.iter().position(|x| x == "-i").unwrap() && boot < a.len() - 2);
    }
}
