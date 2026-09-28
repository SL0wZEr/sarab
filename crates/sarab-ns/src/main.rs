//! sarab-ns — rootless namespace harness.
//!
//! Enters fresh user + mount (+ optional pid/net) namespaces with a
//! *multi-uid* mapping: uid 0 → the caller, uids 1.. → the caller's
//! /etc/subuid range. That is what lets Android's init run services as
//! `system` (1000) and zygote fork apps as 10000+ without any real privilege.
//! Builds a private /dev on tmpfs with the host device nodes an Android
//! userspace needs and a private binderfs at /dev/binderfs, then execs the
//! given command. Every Android process we start later inherits this
//! namespace — this is the seed of the "no container, just namespaces" design.
//!
//!   sarab-ns [--pid] [--net] [--root DIR] [--kmsg FIFO] [--ready-fd FD] [--bind SRC:DST]... [--] CMD [ARGS...]
//!   sarab-ns --enter PID [--as UID:GID[:G1,G2...]] -- CMD [ARGS...]
//!
//! The second form runs a command in the namespaces of a running runtime
//! (enter.rs), on a terminal of the runtime's own when ours is one (pty.rs).
//!
//! With --root, DST paths in --bind are relative to DIR, /dev /proc /sys are
//! set up inside DIR, and we pivot_root into it before exec — the Android
//! image tree becomes "/", with host paths bound in wherever we want them.
//! --kmsg binds a FIFO at /dev/kmsg so init's log can be read rootless.
//! --ready-fd names an inherited pipe: once the namespaces exist (the network
//! one included, with its loopback up) our pid, as the host sees it, is
//! written to it as a line and it is closed, which is the moment `sarab start`
//! attaches pasta. The parent closes its copy at once, so a child that fails
//! earlier leaves the reader an end of file rather than a wait.
//!
//! Id maps. lib.rs holds them, and the check of the user's sub-id ranges,
//! shared with the `sarab` command; `map_ids` refuses a short range with the
//! fix before newuidmap runs, then writes `UID_MAP` and `GID_MAP` after the
//! 0 → caller extent (`map_args`).
//!
//! Ordering in `main`. Id maps can only be written from outside the new
//! userns, hence the fork and two-pipe handshake (child unshares, parent runs
//! newuidmap/newgidmap, child continues; the pipes are close-on-exec, so
//! neither newuidmap nor the command inherits them); the `fork()`s are sound because the
//! process is single-threaded. Under Ubuntu's user-namespace restriction the
//! unshare either fails or leaves the child in AppArmor's capability-less
//! `unprivileged_userns` profile; the child checks for both once it has its
//! maps, before anything that would fail with a bare EPERM, and says how to
//! allow sarab-ns (`sarab_ns::apparmor`). A child that fails before it is
//! ready closes the pipe unwritten, and the parent passes its exit on rather
//! than running newuidmap against it. `isolate_cgroup` moves us into
//! `<cgroup>/android` *before* `CLONE_NEWCGROUP`: Android's init chowns its
//! cgroup root to system, so with the extra level the host user keeps the
//! parent's cgroup.freeze and can freeze the whole tree with one write. Not
//! being able to make or join that cgroup is not fatal: it is said on stderr,
//! and only freezing is lost. The fresh /run tmpfs goes up
//! before the --bind mounts so host sockets bound under it land there, not as
//! empty files in the image. A fresh sysfs, and the namespace-rooted cgroup2
//! on it, needs --net (a userns may mount sysfs only if it owns the netns);
//! without it /sys is rbound and Android's cgroup setup fails. After
//! unshare(CLONE_NEWPID) the *next* child is pid 1. The seccomp shim goes on
//! last, right before exec, so all of Android inherits it.
//!
//! What Android does not get from the host. IPC and UTS are unshared with the
//! mount namespace: sharing them let Android's root, which is the desktop user,
//! attach to the desktop's SysV shared memory, and showed apps the host name.
//! Nothing inside may create a user namespace of its own (a phone never exposes
//! that kernel surface to apps): the seccomp filter refuses it (seccomp.rs),
//! and `max_user_namespaces` is also set to 0 for this namespace straight after
//! the id maps (`forbid_user_namespaces`), though Android's init holds
//! CAP_SYS_RESOURCE here too and could set it back, which is why the filter is
//! the rule that holds. pasta and `--enter` only join. /proc gets
//! `hidepid=invisible` with Android's readproc group (`AID_READPROC`, 3009,
//! whose members see every process), as stock first-stage init mounts it, so an
//! app sees only its own processes. With --root the command runs with the
//! environment the kernel gives init plus Android's default PATH (`INIT_ENV`:
//! the kernel's `envp_init` and bionic's `_PATH_DEFPATH`, what first-stage init
//! would have set before execing the second stage), never the caller's:
//! everything in Android, apps included, inherits init's environment, and the
//! caller's is the whole desktop session (`SSH_AUTH_SOCK`,
//! `DBUS_SESSION_BUS_ADDRESS`, tokens, a stray `LD_PRELOAD` for the bionic
//! linker). `init.environ.rc` sets the rest. Without --root (image extraction)
//! the caller's environment stays, since that command is a host binary. The
//! umask is fixed at 022 first thing: init inherits it, and under a desktop
//! umask of 077 `/dev/socket` and the property files come out unreadable to
//! everything but root.
//!
//! The property service. Without SELinux, init lets anything that can connect
//! to `/dev/socket/property_service` set any property. `socket_acl` gives the
//! staged `/dev/socket` a default ACL (`system.posix_acl_default`: version 2,
//! then (tag, perm, id) entries sorted by tag and id, as the kernel requires)
//! admitting only root and the system uids
//! (1000-1099, shell 2000, misc 9998, nobody 9999), so the property socket,
//! the first socket init creates there, inherits it; the image's init.waydroid
//! .rc, as Sarab generates it (overlay.rs), then drops the socket's mode for
//! everyone else, which is what makes the ACL bite, and removes the default so
//! no later socket inherits it. Without tmpfs ACLs this only warns: the
//! socket then has no ACL, the .rc leaves its mode alone, and it stays open.
//!
//! /dev and exec. The new /dev is staged on tmpfs while the host /dev is still
//! visible, then MS_MOVEd over in one step. `HOST_NODES` keep the host's
//! permissions (ueventd cannot chmod a node the host's root owns), so each is
//! open to every app: `kvm`, `uhid` and `sw_sync` are left out on purpose, and
//! `fuse` stays for vold since it grants nothing without a mount to serve.
//! The host's `net/tun` is bound a second time at `/dev/tun`, the path
//! Android's VpnService opens (every VPN app failed on it; apps/vpn-probe is
//! the test); the host's ueventd would have made that node, and Android's has
//! no uevents to act on.
//! Binder nodes are 0666 because system and app uids open them. `tty` opens
//! the caller's controlling terminal, so with --root the command starts a
//! session of its own first (`setsid`; we are a forked child, never a group
//! leader): Android's init has no controlling terminal, and nothing in Android
//! can reach the terminal `sarab start --foreground` was run from, to read it
//! or, where the kernel allows `TIOCSTI`, type into it.
//! `BinderfsDevice` mirrors the kernel's `struct binderfs_device` for the
//! BINDER_CTL_ADD ioctl. `pivot` binds the new root onto itself because
//! pivot_root needs a mount point. `exec` close_range()s every fd above 2:
//! zygote refuses to fork with unknown fd types (e.g. a FIFO) in its table.

mod enter;
mod pty;
mod seccomp;

use anyhow::{Context, Result, anyhow, bail};
use nix::fcntl::OFlag;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::stat::{Mode, umask};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Pid, User, execve, execvp, fork, getgid, getuid, pipe2, read, sethostname, setsid, write,
};
use sarab_ns::{GID_MAP, Map, UID_MAP, apparmor, first_range};
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;

#[repr(C)]
struct BinderfsDevice {
    name: [u8; 256],
    major: u32,
    minor: u32,
}
nix::ioctl_readwrite!(binder_ctl_add, b'b', 1, BinderfsDevice);

unsafe extern "C" {
    #[link_name = "close_range"]
    fn libc_close_range(first: u32, last: u32, flags: u32) -> i32;
}

const HOST_NODES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty", "fuse"];
const HOST_DIRS: &[&str] = &["dri", "snd", "dma_heap", "net"];
const ANDROID_DIRS: &[&str] = &["socket", "__properties__", "shm", "cpuctl", "binderfs"];
const INIT_ENV: &[&str] = &[
    "HOME=/",
    "TERM=linux",
    "PATH=/product/bin:/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system_ext/bin:/system/bin:/system/xbin:/odm/bin:/vendor/bin:/vendor/xbin",
];
const AID_READPROC: u32 = 3009;

fn socket_acl() -> Vec<u8> {
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const RWX: u16 = 7;
    const NONE: u32 = u32::MAX;
    let mut e: Vec<(u16, u16, u32)> = vec![(USER_OBJ, RWX, NONE)];
    e.extend((1000..=1099).chain([2000, 9998, 9999]).map(|uid| (USER, RWX, uid)));
    e.extend([(GROUP_OBJ, RWX, NONE), (MASK, RWX, NONE), (OTHER, RWX, NONE)]);
    let mut b = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in e {
        b.extend(tag.to_le_bytes());
        b.extend(perm.to_le_bytes());
        b.extend(id.to_le_bytes());
    }
    b
}

fn set_socket_acl(dir: &Path) {
    let path = CString::new(dir.as_os_str().as_encoded_bytes()).expect("no NUL in a path we built");
    let acl = socket_acl();
    let name = c"system.posix_acl_default";
    if unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), acl.as_ptr().cast(), acl.len(), 0) } != 0 {
        eprintln!(
            "sarab-ns: WARNING: no ACL on /dev/socket ({}); the property service stays open to every app",
            std::io::Error::last_os_error()
        );
    }
}

struct Opts {
    pid_ns: bool,
    net_ns: bool,
    binds: Vec<(PathBuf, PathBuf)>,
    root: Option<PathBuf>,
    kmsg: Option<PathBuf>,
    ready: Option<i32>,
    enter: Option<u32>,
    ids: Option<enter::Ids>,
    argv: Vec<String>,
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts {
        pid_ns: false,
        net_ns: false,
        binds: vec![],
        root: None,
        kmsg: None,
        ready: None,
        enter: None,
        ids: None,
        argv: vec![],
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pid" => o.pid_ns = true,
            "--net" => o.net_ns = true,
            "--bind" => {
                let v = it.next().context("--bind needs SRC:DST")?;
                let (s, d) = v.split_once(':').context("--bind format is SRC:DST")?;
                o.binds.push((PathBuf::from(s), PathBuf::from(d)));
            }
            "--root" => o.root = Some(PathBuf::from(it.next().context("--root needs DIR")?)),
            "--kmsg" => o.kmsg = Some(PathBuf::from(it.next().context("--kmsg needs FIFO")?)),
            "--ready-fd" => {
                let fd: i32 = it.next().context("--ready-fd needs FD")?.parse().context("--ready-fd FD")?;
                if fd < 3 {
                    bail!("--ready-fd {fd}: not one of stdin, stdout or stderr");
                }
                o.ready = Some(fd);
            }
            "--enter" => o.enter = Some(it.next().context("--enter needs PID")?.parse().context("--enter PID")?),
            "--as" => o.ids = Some(enter::parse_ids(&it.next().context("--as needs UID:GID")?)?),
            "--" => {
                o.argv.extend(it.by_ref());
                break;
            }
            _ => {
                o.argv.push(a);
                o.argv.extend(it.by_ref());
                break;
            }
        }
    }
    if o.ids.is_some() && o.enter.is_none() {
        bail!("--as goes with --enter");
    }
    if o.argv.is_empty() && o.enter.is_none() {
        o.argv.push(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    }
    Ok(o)
}

fn map_args(pid: &str, id: &str, start: u64, map: Map) -> Vec<String> {
    let mut a: Vec<String> = [pid, "0", id, "1"].iter().map(|s| s.to_string()).collect();
    for &(inside, offset, count) in map {
        a.extend([inside as u64, start + offset as u64, count as u64].map(|n| n.to_string()));
    }
    a
}

fn map_ids(child: Pid) -> Result<()> {
    let me = User::from_uid(getuid())?.context("current user")?;
    let read = |f: &str| fs::read_to_string(f).with_context(|| format!("reading {f}"));
    let (subuid, subgid) = (read("/etc/subuid")?, read("/etc/subgid")?);
    sarab_ns::check(&me.name, &subuid, &subgid).map_err(|m| anyhow!(m))?;
    let (us, _) = first_range(&subuid, &me.name).context("no sub-uid range")?;
    let (gs, _) = first_range(&subgid, &me.name).context("no sub-gid range")?;
    let uid = getuid().as_raw().to_string();
    let gid = getgid().as_raw().to_string();
    let pid = child.as_raw().to_string();
    let run = |tool: &str, id: &str, start: u64, map: Map| -> Result<()> {
        let st = Command::new(tool)
            .args(map_args(&pid, id, start, map))
            .status()
            .with_context(|| format!("running {tool}"))?;
        if !st.success() {
            bail!("{tool} failed: {st}");
        }
        Ok(())
    };
    run("newuidmap", &uid, us, UID_MAP)?;
    run("newgidmap", &gid, gs, GID_MAP)?;
    Ok(())
}

fn bind(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
    } else {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        if !dst.exists() {
            fs::File::create(dst)?;
        }
    }
    mount(Some(src), dst, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>)
        .with_context(|| format!("bind {} -> {}", src.display(), dst.display()))
}

fn build_dev(dev_target: &Path, kmsg: Option<&Path>) -> Result<()> {
    let stage = std::env::temp_dir().join(format!("sarab-dev-{}", std::process::id()));
    fs::create_dir_all(&stage)?;
    mount(Some("tmpfs"), &stage, Some("tmpfs"), MsFlags::MS_NOSUID, Some("mode=0755"))
        .context("tmpfs on staging /dev")?;

    let host = Path::new("/dev");
    for n in HOST_NODES {
        let src = host.join(n);
        if src.exists() {
            bind(&src, &stage.join(n))?;
        }
    }
    for d in HOST_DIRS {
        let src = host.join(d);
        if src.is_dir() {
            bind(&src, &stage.join(d))?;
        }
    }
    let tun = host.join("net/tun");
    if tun.exists() {
        bind(&tun, &stage.join("tun"))?;
    }
    for d in ANDROID_DIRS {
        fs::create_dir_all(stage.join(d))?;
    }
    set_socket_acl(&stage.join("socket"));
    fs::create_dir_all(stage.join("pts"))?;
    mount(
        Some("devpts"),
        &stage.join("pts"),
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )
    .context("mount devpts")?;
    std::os::unix::fs::symlink("pts/ptmx", stage.join("ptmx"))?;
    mount(Some("tmpfs"), &stage.join("shm"), Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;

    let bfs = stage.join("binderfs");
    mount(Some("binder"), &bfs, Some("binder"), MsFlags::empty(), None::<&str>)
        .context("mount binderfs (kernel needs CONFIG_ANDROID_BINDERFS)")?;
    let ctl = fs::File::open(bfs.join("binder-control"))?;
    for name in ["binder", "hwbinder", "vndbinder"] {
        let mut dev = BinderfsDevice { name: [0; 256], major: 0, minor: 0 };
        dev.name[..name.len()].copy_from_slice(name.as_bytes());
        unsafe { binder_ctl_add(ctl.as_raw_fd(), &mut dev) }.with_context(|| format!("BINDER_CTL_ADD {name}"))?;
        fs::set_permissions(bfs.join(name), std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
        std::os::unix::fs::symlink(format!("binderfs/{name}"), stage.join(name))?;
    }

    if let Some(fifo) = kmsg {
        bind(fifo, &stage.join("kmsg"))?;
    }

    fs::create_dir_all(dev_target)?;
    mount(Some(&stage), dev_target, None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .with_context(|| format!("move staged /dev over {}", dev_target.display()))?;
    let _ = fs::remove_dir(&stage);
    Ok(())
}

fn pivot(newroot: &Path) -> Result<()> {
    mount(Some(newroot), newroot, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>)
        .context("bind newroot onto itself")?;
    let old = newroot.join("mnt");
    fs::create_dir_all(&old)?;
    nix::unistd::pivot_root(newroot, &old).context("pivot_root")?;
    std::env::set_current_dir("/")?;
    nix::mount::umount2("/mnt", nix::mount::MntFlags::MNT_DETACH).context("detach old root")?;
    Ok(())
}

fn exec(argv: &[String], env: Option<&[&str]>) -> Result<()> {
    unsafe { libc_close_range(3, u32::MAX, 0) };
    let cargs: Vec<CString> = argv.iter().map(|a| CString::new(a.as_str())).collect::<Result<_, _>>()?;
    match env {
        Some(env) => {
            let cenv: Vec<CString> = env.iter().map(|e| CString::new(*e)).collect::<Result<_, _>>()?;
            execve(&cargs[0], &cargs, &cenv).with_context(|| format!("execve {}", argv[0]))?;
        }
        None => {
            execvp(&cargs[0], &cargs).context("execvp")?;
        }
    }
    unreachable!()
}

fn forbid_user_namespaces() -> Result<()> {
    fs::write("/proc/sys/user/max_user_namespaces", "0").context("set user.max_user_namespaces to 0")
}

fn isolate_cgroup() {
    let Ok(cg) = fs::read_to_string("/proc/self/cgroup") else { return };
    let Some(rel) = cg.lines().find_map(|l| l.strip_prefix("0::")) else { return };
    let dir = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/')).join("android");
    if fs::create_dir(&dir)
        .or_else(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { Ok(()) } else { Err(e) })
        .is_err()
    {
        eprintln!("sarab-ns: no writable cgroup for the runtime; freeze will be unavailable");
        return;
    }
    if let Err(e) = fs::write(dir.join("cgroup.procs"), std::process::id().to_string()) {
        eprintln!("sarab-ns: could not join {} ({e}); freeze will be unavailable", dir.display());
    }
}

fn wait_and_exit(child: Pid) -> ! {
    let code = match waitpid(child, None) {
        Ok(WaitStatus::Exited(_, c)) => c,
        Ok(WaitStatus::Signaled(_, s, _)) => 128 + s as i32,
        _ => 1,
    };
    std::process::exit(code)
}

fn main() -> Result<()> {
    umask(Mode::from_bits_truncate(0o022));
    let opts = parse_args()?;
    if let Some(pid) = opts.enter {
        return enter::run(pid, opts.ids, &opts.argv);
    }
    if !Path::new("/proc/self/ns/user").exists() {
        bail!("no user namespace support");
    }

    let (ready_r, ready_w): (OwnedFd, OwnedFd) = pipe2(OFlag::O_CLOEXEC)?;
    let (go_r, go_w): (OwnedFd, OwnedFd) = pipe2(OFlag::O_CLOEXEC)?;

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(go_r);
            if let Some(fd) = opts.ready {
                unsafe { libc::close(fd) };
            }
            let mut b = [0u8; 1];
            if read(&ready_r, &mut b)? == 0 {
                wait_and_exit(child);
            }
            if let Err(e) = map_ids(child) {
                eprintln!("sarab-ns: {e:#}");
                let _ = write(&go_w, b"x");
                wait_and_exit(child);
            }
            write(&go_w, b"g")?;
            wait_and_exit(child);
        }
        ForkResult::Child => {
            drop(ready_r);
            drop(go_w);
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::SIGKILL)?;
            if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
                if apparmor::restricted() {
                    bail!("{}", apparmor::blocked(&format!("unshare(user): {e}")));
                }
                return Err(anyhow!(e).context("unshare(user)"));
            }
            write(&ready_w, b"r")?;
            let mut b = [0u8; 1];
            read(&go_r, &mut b)?;
            if b[0] != b'g' {
                std::process::exit(1);
            }
            if apparmor::confined() {
                bail!("{}", apparmor::blocked("AppArmor confined sarab-ns to its unprivileged_userns profile"));
            }
        }
    }

    forbid_user_namespaces()?;
    isolate_cgroup();
    unshare(
        CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWCGROUP | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS,
    )
    .context("unshare(mount|cgroup|ipc|uts)")?;
    sethostname("localhost").context("sethostname")?;
    if opts.net_ns {
        unshare(CloneFlags::CLONE_NEWNET).context("unshare(net)")?;
        let st = Command::new("ip").args(["link", "set", "lo", "up"]).status().context("ip link set lo up")?;
        if !st.success() {
            bail!("ip link set lo up: {st}");
        }
    }
    if let Some(fd) = opts.ready {
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let _ = write(&fd, format!("{}\n", std::process::id()).as_bytes());
    }
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>)
        .context("make / rprivate")?;
    let root = opts.root.clone().map(fs::canonicalize).transpose()?;
    let base = root.clone().unwrap_or_else(|| PathBuf::from("/"));
    let under = |p: &str| base.join(p.trim_start_matches('/'));

    build_dev(&under("dev"), opts.kmsg.as_deref())?;
    if root.is_some() {
        let run = under("run");
        fs::create_dir_all(&run)?;
        mount(Some("tmpfs"), &run, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=0755"))
            .context("tmpfs on /run")?;
    }
    for (s, d) in &opts.binds {
        bind(s, &base.join(d.strip_prefix("/").unwrap_or(d)))?;
    }
    if root.is_some() {
        let sys = under("sys");
        fs::create_dir_all(&sys)?;
        if opts.net_ns {
            mount(
                Some("sysfs"),
                &sys,
                Some("sysfs"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
                None::<&str>,
            )
            .context("mount sysfs")?;
        } else {
            mount(Some("/sys"), &sys, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>)
                .context("rbind /sys")?;
        }
        let cg = under("sys/fs/cgroup");
        mount(
            Some("cgroup2"),
            &cg,
            Some("cgroup2"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None::<&str>,
        )
        .context("mount cgroup2 (namespace-rooted view)")?;
        for t in ["mnt", "apex", "linkerconfig", "debug_ramdisk", "second_stage_resources", "tmp"] {
            let p = under(t);
            fs::create_dir_all(&p)?;
            mount(Some("tmpfs"), &p, Some("tmpfs"), MsFlags::MS_NOSUID, Some("mode=0755"))
                .with_context(|| format!("tmpfs on /{t}"))?;
        }
    }

    if opts.pid_ns {
        unshare(CloneFlags::CLONE_NEWPID).context("unshare(pid)")?;
        if let ForkResult::Parent { child } = unsafe { fork() }? {
            wait_and_exit(child);
        }
        nix::sys::prctl::set_pdeathsig(nix::sys::signal::SIGKILL)?;
        let proc_dir = under("proc");
        fs::create_dir_all(&proc_dir)?;
        mount(
            Some("proc"),
            &proc_dir,
            Some("proc"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some(format!("hidepid=invisible,gid={AID_READPROC}").as_str()),
        )
        .context("mount /proc in pid ns")?;
    }

    if let Some(r) = &root {
        pivot(r)?;
        setsid().context("setsid")?;
    }
    seccomp::install()?;

    eprintln!(
        "sarab-ns: uid={} pid={} binderfs=/dev/binderfs{} — exec {:?}",
        getuid(),
        std::process::id(),
        if opts.pid_ns { " pidns" } else { "" },
        opts.argv
    );
    exec(&opts.argv, root.is_some().then_some(INIT_ENV))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_acl_is_sorted_and_admits_only_system_uids() {
        let b = socket_acl();
        assert_eq!(&b[..4], &[2, 0, 0, 0]);
        let e: Vec<(u16, u32)> = b[4..]
            .chunks(8)
            .map(|c| (u16::from_le_bytes([c[0], c[1]]), u32::from_le_bytes([c[4], c[5], c[6], c[7]])))
            .collect();
        assert!(e.is_sorted(), "the kernel rejects an unsorted ACL");
        let users: Vec<u32> = e.iter().filter(|x| x.0 == 0x02).map(|x| x.1).collect();
        assert!(users.contains(&1001) && users.contains(&2000) && users.iter().all(|&u| u < 10000));
    }

    #[test]
    fn map_args_put_the_caller_at_0_and_each_extent_after_it() {
        let a = map_args("4242", "1000", 100_000, UID_MAP).join(" ");
        assert_eq!(a, "4242 0 1000 1 1 100000 29999 90000 129999 10000 2147483647 139999 1");
        let a = map_args("4242", "1000", 100_000, GID_MAP).join(" ");
        assert_eq!(
            a,
            "4242 0 1000 1 1 100000 29999 90000 129999 10000 30000 139999 10000 50000 149999 10000 2147483647 159999 1"
        );
    }
}
