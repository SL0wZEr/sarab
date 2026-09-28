//! `sarab logs`: Android's logcat by default, like `docker logs` shows what
//! the container prints; `--kernel`, `--daemon` and `--hostd` for the three
//! logs on the host side.
//!
//! The kernel log is what Android init writes to its /dev/kmsg, which is a
//! FIFO into `kmsg.log` in the state directory. The daemon log is `sarab start
//! --foreground`'s own output: the unit's journal, or `sarab.log` beside
//! `kmsg.log` for a detached start. The
//! logcat dump skips the noisy radio buffer.

use crate::android::{self, User};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Android,
    Kernel,
    Daemon,
    Hostd,
}

pub struct Opts {
    pub source: Source,
    pub follow: bool,
    pub tail: Option<u32>,
    pub extra: Vec<String>,
}

fn logcat_argv(o: &Opts) -> Vec<String> {
    let mut a = vec!["logcat".to_string(), "-b".into(), "main,system,crash".into()];
    if !o.follow {
        a.push("-d".into());
    }
    if let Some(n) = o.tail {
        a.push("-T".into());
        a.push(n.to_string());
    }
    a.extend(o.extra.iter().cloned());
    a
}

fn tail_file(path: &Path, o: &Opts) -> Result<i32> {
    if !path.exists() {
        bail!("{} does not exist yet", path.display());
    }
    let mut c = Command::new("tail");
    c.arg("-n").arg(o.tail.map_or("+1".to_string(), |n| n.to_string()));
    if o.follow {
        c.arg("-F");
    }
    Ok(c.arg(path).status().context("run tail")?.code().unwrap_or(1))
}

pub fn run(dirs: &crate::paths::Dirs, o: Opts) -> Result<i32> {
    if !o.extra.is_empty() && o.source != Source::Android {
        bail!("extra arguments are for logcat, and only apply to the Android log");
    }
    match o.source {
        Source::Android => android::exec(User::Root, &[], &logcat_argv(&o)),
        Source::Kernel => tail_file(&dirs.state.join("kmsg.log"), &o),
        Source::Hostd => tail_file(&dirs.state.join("hostd.log"), &o),
        Source::Daemon if crate::session::unit_installed() => {
            let mut c = Command::new("journalctl");
            c.args(["--user", "-u", crate::session::UNIT, "--no-pager", "-o", "cat"]);
            if let Some(n) = o.tail {
                c.arg("-n").arg(n.to_string());
            }
            if o.follow {
                c.arg("-f");
            }
            Ok(c.status().context("run journalctl")?.code().unwrap_or(1))
        }
        Source::Daemon => tail_file(&crate::session::detached_log(dirs), &o),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(follow: bool, tail: Option<u32>, extra: &[&str]) -> Opts {
        Opts { source: Source::Android, follow, tail, extra: extra.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn dump_by_default_follow_on_request() {
        assert_eq!(logcat_argv(&o(false, None, &[])), ["logcat", "-b", "main,system,crash", "-d"]);
        assert_eq!(logcat_argv(&o(true, None, &[])), ["logcat", "-b", "main,system,crash"]);
    }

    #[test]
    fn tail_and_passthrough() {
        assert_eq!(
            logcat_argv(&o(true, Some(50), &["-s", "ActivityManager"])),
            ["logcat", "-b", "main,system,crash", "-T", "50", "-s", "ActivityManager"]
        );
    }
}
