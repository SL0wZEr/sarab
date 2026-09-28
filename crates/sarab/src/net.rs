//! Networking for the session: pasta attached to the namespace `sarab-ns --net`
//! already made. Without it, `--net` gives the Android tree an empty network
//! namespace with a loopback and nothing else: apps show "Connectivity error"
//! and the log fills with Firebase failing to reach its installations service.
//!
//! Why pasta: it is a user-mode TCP/IP stack that joins the target namespace,
//! puts a tap interface in it, and translates every flow to ordinary sockets it
//! opens in the host's namespace as *us*. So it needs no bridge, no veth pair,
//! no `dnsmasq`, no `ip netns`, no CAP_NET_ADMIN on the host side and no NAT
//! rule anywhere (the usual lxc-net path is all root). Our uid owns the user
//! namespace that owns the netns, which gives full capabilities inside it.
//!
//! The attach: pasta needs the namespace to exist before it can join, and
//! `sarab-ns` unshares only after it is spawned, so `sarab start` hands it a
//! pipe (`--ready-fd`) and `attach` reads one line from it: the pid, as the
//! host sees it, of the sarab-ns that owns the new network namespace, written
//! the moment its loopback is up. An end of file instead means sarab-ns ended
//! first, and Android boots without networking. Namespaces are compared by
//! the `net:[inode]` link text, which is the namespace identity. Attaching
//! before init starts netd means Android's first network syscall has a route.
//! The one hard failure is identity: `start` reads the pid's netns again and
//! refuses if it is ours, because pasta `--config-net` against the host netns
//! would rewrite the desktop's own addresses and routes.
//!
//! pasta runs without `-f`, so it forks into the background only once it has
//! joined the namespace and configured it, and its launcher's exit status is
//! the answer to "did the network come up": 0, or the failure its log
//! explains. `start` waits for that on a pidfd, for `PASTA_DEADLINE` at most,
//! then takes the daemon's pid from `--pid` and a pidfd for it (`Pasta`),
//! which is how it is watched and signalled from then on (`ended_within`,
//! `signal`). The daemon is not our child, so only the pidfd pins its pid: in
//! the moment between the launcher's exit and `pidfd_open`, a daemon that died
//! and had its pid taken by another process would be the one we hold, but
//! from the pidfd on no pid can be reused under us. A pidfile with no pid in
//! it, or a daemon already gone, leaves Android booting without networking,
//! as every other pasta failure does. A `Pasta` dropped stops its daemon:
//! SIGTERM, then SIGKILL half a second later. image.rs waits on curl with the
//! same `pidfd_open` and `ended_within`.
//!
//! pasta reads the host's nameserver once, when it starts, and has no way to
//! read it again (`pesto`, its control client, changes port forwarding only).
//! So `attach` returns a `Link`, which `start`'s wait loop calls `keep` on:
//! every `CHECK_EVERY` it reads the host's resolvers again (`Resolvers`), and
//! when the one pasta forwards to (`target`: the host nameserver, or the
//! upstream one on old pasta) is a different address, it ends pasta and starts
//! it again against the new one, with the same identity check. That is what
//! moving networks does where NetworkManager, dhcpcd or iwd write the router's
//! address into /etc/resolv.conf, and on old pasta anywhere; systemd-resolved's
//! 127.0.0.53 never changes. A host with no nameserver at all (offline) keeps
//! the pasta it has. An offline start still gets a pasta, in its "local mode"
//! (link-local address, a default route, no nameserver), and the first
//! nameserver the host gets restarts it properly. The restart takes the tap
//! down and up, so Android sees its network go and come back and open
//! connections end; measured 2026-09-27 on Ubuntu 24.04 (old pasta), names
//! resolved again 1.2 s after the restart, with the new server in Android's
//! lease. A pasta that dies is started again if it had run `RESPAWN_GAP`; one
//! that died sooner waits for the host's nameserver to change, so a pasta that
//! cannot start does not loop. `Link` ends its pasta when dropped.
//!
//! A missing pasta is refused before anything starts (`require_pasta`, from
//! `sarab start -F` and from `session::ensure_running`, which every command
//! that boots Android goes through), with the install line for this distro:
//! every app needs the network, so an Android that boots offline only looks
//! broken. `--no-network` is the explicit way to boot without it.
//!
//! `pasta_argv` was read off `pasta --help` and `man pasta` for version
//! 2026_07_28.f8df3f1; flag names have changed across releases.
//!  * `--userns` + `--netns` is what `pasta PID` expands to, spelled out so
//!    pasta joins exactly the netns we identity-checked; the userns is what
//!    gives it CAP_NET_ADMIN there for `--config-net` (addresses, routes, tap up).
//!  * `--ns-ifname eth0`: Android's `EthernetTracker` only considers interfaces
//!    matching `config_ethernet_iface_regex`, `eth\d` in AOSP.
//!  * `--dns-forward`, `--dns` and `--dhcp-dns` are all needed: the first makes
//!    pasta answer at `DNS`, the second puts it in the list, and in pasta mode
//!    only `--dhcp-dns` sends that list in the DHCP lease. Without the lease
//!    ConnectivityService hands netd no resolver and every lookup fails while
//!    raw sockets to a literal IP work.
//!  * `--dns-host` must then be explicit: `--dns` means "instead of reading
//!    /etc/resolv.conf", which empties the list pasta's default forwarding
//!    target comes from, so the forwarder answers nothing. `host_nameserver`
//!    takes the host's first nameserver (here systemd-resolved's 127.0.0.53,
//!    reachable only from pasta's host-side sockets, so split DNS and MagicDNS
//!    keep working inside Android).
//!  * `--log-file` logs *only* to that file (rotated at 1 MiB): pasta otherwise
//!    also writes to syslog, which journald files under the daemon's unit.
//!    stdout and stderr go to /dev/null for the same reason.
//!  * No `--no-netns-quit`, so pasta exits by itself when the runtime's netns
//!    goes away; a daemon of it can outlive us only as long as Android does.
//!  * `-t/-u/-T/-U none`: the default `auto` would publish every port Android
//!    binds (adb's 5555 included) onto the host. `--map-host-loopback none`
//!    keeps apps off whatever listens on the host's 127.0.0.1.
//!
//! pasta from before mid-2024 (Ubuntu 24.04 ships 2024_02_20) has neither
//! `--dns-host` nor `--map-host-loopback`, and an unknown flag makes it print
//! its usage and exit, so Android booted offline. `pasta_argv` spells the
//! flags the installed pasta has (`Flags`), as its `pasta --help` lists them.
//! That older pasta also copies its help text into the journal, once per
//! start. The loopback guard is then `--no-map-gw` (older pasta mapped
//! the gateway address to the host's loopback by default). Without
//! `--dns-host` that pasta cannot forward to systemd-resolved's 127.0.0.53
//! (with `--no-map-gw` it drops a loopback nameserver: "Couldn't get any
//! nameserver address"), so `--dns` names an upstream server instead
//! (`upstream_nameserver`: the first non-loopback one in /etc/resolv.conf, or
//! in systemd-resolved's own list, /run/systemd/resolve/resolv.conf), which
//! the lease then carries. Lookups work; split DNS is what is lost there.
//! Found on Ubuntu 24.04, where every lookup in Android failed.
//!
//! `DNS` is a fixed link-local address (the one podman uses for rootless pasta)
//! because `net.dns1` is set to it and must be the same on every boot, and a
//! host resolver on 127.0.0.53 is unreachable from inside the namespace.
//! `set_dns_props` sets only `net.dns1` (pasta forwards one IPv4 address) via
//! the property service over binder, the only resolver knob reachable from out
//! here. It is the legacy fallback for `ANDROID_DNS_MODE=local` callers: bionic
//! normally asks netd, whose resolver config comes from ConnectivityService.
//! Sockets working is not the same as Android's idea of connectivity. It is best
//! effort and must never take the boot down.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DNS: &str = "169.254.1.1";

const IFNAME: &str = "eth0";

const PASTA_DEADLINE: Duration = Duration::from_secs(10);

const CHECK_EVERY: Duration = Duration::from_secs(2);

const RESPAWN_GAP: Duration = Duration::from_secs(10);

const RESOLV: &str = "/etc/resolv.conf";

const RESOLVED: &str = "/run/systemd/resolve/resolv.conf";

fn ns_net(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/ns/net")).ok().map(|p| p.display().to_string())
}

fn find_pasta() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("pasta")).find(|p| p.is_file())
}

pub fn require_pasta() -> Result<PathBuf> {
    find_pasta().ok_or_else(|| {
        anyhow::anyhow!(
            "pasta is not installed, and without it Android has no network. Install it:\n  {}\n\
             (or boot offline on purpose: sarab start -F --no-network)",
            crate::info::install_hint("passt")
        )
    })
}

fn nameservers(resolv: &str) -> impl Iterator<Item = &str> {
    resolv.lines().filter_map(|l| l.trim().strip_prefix("nameserver")).filter_map(|rest| {
        let addr = rest.strip_prefix(|c: char| c.is_ascii_whitespace())?.trim();
        (!addr.is_empty()).then_some(addr)
    })
}

fn host_nameserver(resolv: &str) -> Option<String> {
    nameservers(resolv).next().map(str::to_string)
}

fn upstream_nameserver(resolvs: &[&str]) -> Option<String> {
    let loopback = |a: &str| a.starts_with("127.") || a == "::1";
    resolvs.iter().find_map(|r| nameservers(r).find(|a| !loopback(a))).map(str::to_string)
}

struct Flags {
    dns_host: bool,
    map_host_loopback: bool,
}

impl Flags {
    fn from_help(help: &str) -> Self {
        let has = |flag: &str| help.split_whitespace().any(|w| w == flag);
        Self { dns_host: has("--dns-host"), map_host_loopback: has("--map-host-loopback") }
    }

    fn of(pasta: &Path) -> Self {
        let out = Command::new(pasta).arg("--help").stdin(Stdio::null()).output();
        let help = out.map(|o| [o.stdout, o.stderr].concat()).unwrap_or_default();
        Self::from_help(&String::from_utf8_lossy(&help))
    }
}

#[derive(Debug, Default, PartialEq)]
struct Resolvers {
    host: Option<String>,
    upstream: Option<String>,
}

impl Resolvers {
    fn parse(resolv: &str, resolved: &str) -> Self {
        Self { host: host_nameserver(resolv), upstream: upstream_nameserver(&[resolv, resolved]) }
    }

    fn read() -> Self {
        let read = |f: &str| std::fs::read_to_string(f).unwrap_or_default();
        Self::parse(&read(RESOLV), &read(RESOLVED))
    }

    fn target(&self, flags: &Flags) -> Option<&str> {
        if flags.dns_host { self.host.as_deref() } else { self.upstream.as_deref() }
    }
}

fn pasta_argv(pasta: &Path, flags: &Flags, ns_pid: u32, dns: &Resolvers, log: &Path, pidfile: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![pasta.display().to_string()];
    a.push("-q".into());
    a.push("--pid".into());
    a.push(pidfile.display().to_string());
    a.push("--userns".into());
    a.push(format!("/proc/{ns_pid}/ns/user"));
    a.push("--netns".into());
    a.push(format!("/proc/{ns_pid}/ns/net"));
    a.push("--config-net".into());
    a.push("--ns-ifname".into());
    a.push(IFNAME.into());
    a.push("--dns-forward".into());
    a.push(DNS.into());
    a.push("--dns".into());
    a.push(if flags.dns_host { DNS } else { dns.target(flags).unwrap_or(DNS) }.into());
    a.push("--dhcp-dns".into());
    a.push("--log-file".into());
    a.push(log.display().to_string());
    if let Some(h) = dns.target(flags).filter(|_| flags.dns_host) {
        a.push("--dns-host".into());
        a.push(h.into());
    }
    for f in ["-t", "-u", "-T", "-U"] {
        a.push(f.into());
        a.push("none".into());
    }
    if flags.map_host_loopback {
        a.extend(["--map-host-loopback", "none"].map(String::from));
    } else {
        a.push("--no-map-gw".into());
    }
    a
}

pub struct Link {
    pasta: PathBuf,
    flags: Flags,
    ns_pid: u32,
    mine: String,
    log: PathBuf,
    serving: Resolvers,
    running: Option<Pasta>,
    started: Instant,
    checked: Instant,
}

struct Pasta {
    pidfd: OwnedFd,
}

pub(crate) fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

pub(crate) fn ended_within(pidfd: &OwnedFd, wait: Duration) -> bool {
    let mut p = libc::pollfd { fd: pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ms = wait.as_millis().min(i32::MAX as u128) as i32;
    loop {
        match unsafe { libc::poll(&mut p, 1, ms) } {
            n if n > 0 => return true,
            0 => return false,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return true,
        }
    }
}

fn signal(pidfd: &OwnedFd, sig: libc::c_int) {
    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), sig, std::ptr::null::<()>(), 0) };
}

impl Pasta {
    fn ended(&self) -> bool {
        ended_within(&self.pidfd, Duration::ZERO)
    }
}

impl Drop for Pasta {
    fn drop(&mut self) {
        signal(&self.pidfd, libc::SIGTERM);
        if !ended_within(&self.pidfd, Duration::from_millis(500)) {
            signal(&self.pidfd, libc::SIGKILL);
            ended_within(&self.pidfd, Duration::from_millis(500));
        }
    }
}

pub fn attach(pasta: &Path, ready: OwnedFd, run: &Path) -> Result<Option<Link>> {
    let mine = ns_net(std::process::id()).context("read our own /proc/self/ns/net")?;
    let mut line = String::new();
    let _ = BufReader::new(std::fs::File::from(ready)).read_line(&mut line);
    let Ok(ns_pid) = line.trim().parse::<u32>() else {
        eprintln!("networking: sarab-ns ended before its network namespace was up; continuing without it");
        return Ok(None);
    };
    let mut link = Link {
        pasta: pasta.to_path_buf(),
        flags: Flags::of(pasta),
        ns_pid,
        mine,
        log: run.join("pasta.log"),
        serving: Resolvers::default(),
        running: None,
        started: Instant::now(),
        checked: Instant::now(),
    };
    link.start()?;
    if link.running.is_none() {
        eprintln!("booting without networking for now; pasta starts again when the host's nameserver changes");
    }
    Ok(Some(link))
}

impl Link {
    fn start(&mut self) -> Result<()> {
        match ns_net(self.ns_pid) {
            Some(n) if n != self.mine => {}
            other => bail!(
                "refusing to attach pasta: pid {} netns is {other:?}, ours is {} (host netns — pasta --config-net would reconfigure the desktop)",
                self.ns_pid,
                self.mine
            ),
        }
        self.serving = Resolvers::read();
        let pidfile = self.log.with_file_name("pasta.pid");
        let _ = std::fs::remove_file(&pidfile);
        let argv = pasta_argv(&self.pasta, &self.flags, self.ns_pid, &self.serving, &self.log, &pidfile);
        let mut launcher = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {}", argv[0]))?;
        self.started = Instant::now();
        let launched = pidfd_open(launcher.id()).context("pidfd_open pasta")?;
        if !ended_within(&launched, PASTA_DEADLINE) {
            let _ = launcher.kill();
            let _ = launcher.wait();
            eprintln!(
                "pasta did not finish setting up the network within {} s (see {})",
                PASTA_DEADLINE.as_secs(),
                self.log.display()
            );
            return Ok(());
        }
        let st = launcher.wait()?;
        if !st.success() {
            eprintln!("pasta could not set up the network ({st}) (see {})", self.log.display());
            return Ok(());
        }
        let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|t| t.trim().parse::<u32>().ok()) else {
            eprintln!("pasta wrote no pid to {} (see {})", pidfile.display(), self.log.display());
            return Ok(());
        };
        let pidfd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("pasta (pid {pid}) ended as soon as it started: {e} (see {})", self.log.display());
                return Ok(());
            }
        };
        let upstream = self.serving.target(&self.flags).unwrap_or("none");
        println!(
            "network: pasta on ns pid {} ({IFNAME}, dns {DNS} -> {upstream}; log: {})",
            self.ns_pid,
            self.log.display()
        );
        self.running = Some(Pasta { pidfd });
        Ok(())
    }

    pub fn keep(&mut self) {
        if self.checked.elapsed() < CHECK_EVERY {
            return;
        }
        self.checked = Instant::now();
        if self.running.as_ref().is_some_and(Pasta::ended) {
            self.running = None;
            let lasted = self.started.elapsed().as_secs();
            if self.started.elapsed() >= RESPAWN_GAP {
                eprintln!("network: pasta ended after {lasted} s; starting it again");
                return self.restart();
            }
            eprintln!(
                "network: pasta ended {lasted} s after it started; it starts again when the host's nameserver changes (see {})",
                self.log.display()
            );
        }
        let now = Resolvers::read();
        if let Some(new) = now.target(&self.flags)
            && Some(new) != self.serving.target(&self.flags)
        {
            let old = self.serving.target(&self.flags).unwrap_or("none");
            println!("network: the host's nameserver is now {new} (was {old}); restarting pasta");
            self.restart();
        }
    }

    fn restart(&mut self) {
        self.running = None;
        if ns_net(self.ns_pid).is_none() {
            return;
        }
        if let Err(e) = self.start() {
            eprintln!("network: {e:#}");
        }
    }
}

pub fn set_dns_props() {
    let r = sarab_runtime::find_binder()
        .and_then(|dev| sarab_runtime::Platform::connect(&dev))
        .and_then(|p| p.setprop("net.dns1", DNS));
    match r {
        Ok(()) => println!("network: net.dns1={DNS}"),
        Err(e) => eprintln!("network: could not set net.dns1 ({e:#}); resolution inside Android may not work"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_netns_is_named_by_its_link_and_a_gone_pid_has_none() {
        let a = ns_net(std::process::id()).expect("own netns link");
        assert!(a.starts_with("net:["), "unexpected link target {a}");
        assert_eq!(Some(a), ns_net(std::process::id()));
        assert_eq!(ns_net(0), None);
    }

    #[test]
    fn nameserver_is_read_off_resolv_conf() {
        let stub = "# Managed by systemd-resolved\n\nnameserver 127.0.0.53\n                    options edns0 trust-ad\nsearch example.ts.net\n";
        assert_eq!(host_nameserver(stub).as_deref(), Some("127.0.0.53"));
        assert_eq!(host_nameserver("nameserver 1.1.1.1\nnameserver 8.8.8.8\n").as_deref(), Some("1.1.1.1"));
        assert_eq!(host_nameserver("search example.com\n"), None);
        assert_eq!(host_nameserver("nameserverfoo\n"), None);
        assert_eq!(host_nameserver("nameserver\n"), None);
        assert_eq!(host_nameserver(""), None);
    }

    #[test]
    fn argv_is_the_flags_read_off_pasta_2026_07_28() {
        let flags = Flags::from_help("  --dns-host ADDR\tHost nameserver\n  --map-host-loopback ADDR\tTranslate\n");
        let dns = Resolvers { host: Some("127.0.0.53".into()), upstream: Some("10.0.2.3".into()) };
        let a = pasta_argv(
            Path::new("/usr/bin/pasta"),
            &flags,
            4242,
            &dns,
            Path::new("/w/run/pasta.log"),
            Path::new("/w/run/pasta.pid"),
        );
        assert_eq!(
            a.join(" "),
            "/usr/bin/pasta -q --pid /w/run/pasta.pid --userns /proc/4242/ns/user --netns /proc/4242/ns/net \
             --config-net --ns-ifname eth0 --dns-forward 169.254.1.1 --dns 169.254.1.1 \
             --dhcp-dns --log-file /w/run/pasta.log --dns-host 127.0.0.53 \
             -t none -u none -T none -U none --map-host-loopback none"
        );
        assert_eq!(a.iter().filter(|x| *x == DNS).count(), 2);
        assert!(a.windows(2).any(|w| w[0] == "--dns-host" && w[1] == "127.0.0.53"));
        assert!(!a.iter().any(|x| x == "-f" || x == "--foreground"), "pasta must fork once configured");
        assert!(!a.iter().any(|x| x == "4242"));
    }

    #[test]
    fn pasta_2024_02_gets_the_flags_it_has() {
        let flags = Flags::from_help(
            "  --dns-forward ADDR\tForward DNS queries\n  --no-map-gw\t\tDon't map gateway address to host\n",
        );
        let dns = Resolvers { host: Some("127.0.0.53".into()), upstream: Some("10.0.2.3".into()) };
        let a = pasta_argv(Path::new("/usr/bin/pasta"), &flags, 4242, &dns, Path::new("/l"), Path::new("/p"));
        assert!(a.ends_with(&["-U".into(), "none".into(), "--no-map-gw".into()]), "{a:?}");
        assert!(!a.iter().any(|x| x == "--dns-host" || x == "--map-host-loopback" || x == "127.0.0.53"));
        assert!(a.windows(2).any(|w| w[0] == "--dns-forward" && w[1] == DNS));
        assert!(a.windows(2).any(|w| w[0] == "--dns" && w[1] == "10.0.2.3"));
        let none = pasta_argv(Path::new("/p"), &flags, 1, &Resolvers::default(), Path::new("/l"), Path::new("/p"));
        assert!(none.windows(2).any(|w| w[0] == "--dns" && w[1] == DNS));
    }

    #[test]
    fn the_upstream_skips_loopback_resolvers() {
        let stub = "nameserver 127.0.0.53\noptions edns0\n";
        let resolved = "# resolved\nnameserver 10.0.2.3\nnameserver 1.1.1.1\n";
        assert_eq!(upstream_nameserver(&[stub, resolved]).as_deref(), Some("10.0.2.3"));
        assert_eq!(upstream_nameserver(&["nameserver 192.168.1.1\n", resolved]).as_deref(), Some("192.168.1.1"));
        assert_eq!(upstream_nameserver(&[stub, ""]), None);
        assert_eq!(upstream_nameserver(&["nameserver ::1\n"]), None);
    }

    #[test]
    fn the_target_is_what_pasta_was_told_to_forward_to() {
        let modern = Flags { dns_host: true, map_host_loopback: true };
        let old = Flags { dns_host: false, map_host_loopback: false };
        let resolved = Resolvers::parse("nameserver 127.0.0.53\n", "nameserver 192.168.1.1\n");
        assert_eq!(resolved.target(&modern), Some("127.0.0.53"));
        assert_eq!(resolved.target(&old), Some("192.168.1.1"));
        let home = Resolvers::parse("nameserver 192.168.1.1\n", "");
        let cafe = Resolvers::parse("nameserver 10.8.0.1\n", "");
        assert_ne!(home.target(&modern), cafe.target(&modern));
        let moved = Resolvers::parse("nameserver 127.0.0.53\n", "nameserver 10.8.0.1\n");
        assert_eq!(moved.target(&modern), resolved.target(&modern));
        assert_ne!(moved.target(&old), resolved.target(&old));
        assert_eq!(Resolvers::parse("", "").target(&modern), None);
    }
}
