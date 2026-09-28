//! What Android needs to know about this host's display and GPU: which
//! Wayland socket to draw on, which render node to allocate buffers from, and
//! which Mesa Vulkan driver matches it. Read at every start, never assumed:
//! a Radeon box on Hyprland (renderD128, wayland-1) is one machine, not all of
//! them, and a wrong guess here is a black window with no message.
//!
//! `gpu` fails, with the reason and the fix (`NoGpu`), when there is nothing
//! Mesa can drive: no render node, or only NVIDIA's proprietary driver
//! (`UNSUPPORTED`), where the fix is only `NEEDS_MESA`'s explanation. The image
//! ships no software renderer, so booting anyway gave a black window after a
//! 1.4 GB download and no error. It also fails when the node is not open to
//! every user (`open_to_all`): SurfaceFlinger, the other system services and
//! every app draw through it as Android uids, which are sub-uids, not the
//! desktop user whose ACL logind adds. Debian and Ubuntu build systemd with
//! render nodes at 0660 (upstream, Arch and Fedora use 0666), so there Android
//! restarted SurfaceFlinger forever and never booted; the fix is `RENDER_RULE`,
//! a udev rule with upstream's mode for render nodes only (SECURITY.md).
//! `require_gpu` turns either into the refusal setup gives before the download
//! and start before the boot. `SARAB_RENDER_NODE` picks a node explicitly on a
//! multi-GPU machine. `vulkan_driver` returning None means no Vulkan, which
//! Android handles by not offering it.
//!
//! `wayland_display` returns a socket *name*: the composer joins it to the
//! runtime dir itself, and Android only ever sees the Wayland and Pulse
//! sockets, never the directory (start.rs). An absolute WAYLAND_DISPLAY is
//! accepted only when it points directly into the runtime dir, so the name is
//! also the one Android sees it under. A socket counts only if a connect to it
//! succeeds (`answers`): a compositor that crashed leaves its socket file
//! behind, and a dead one picked from WAYLAND_DISPLAY or a scan would give
//! Android a display it can never draw on. A scan skips dead sockets, and
//! names them when nothing else is left.

use anyhow::{Result, anyhow, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub node: String,
    pub driver: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoGpu {
    pub why: String,
    pub fix: &'static str,
}

const UNSUPPORTED: &[&str] = &["nvidia"];
const NEEDS_MESA: &str = "Android needs a GPU that Mesa drives (AMD, Intel, or NVIDIA with nouveau): this image \
     has no software renderer, so it would start to a black window. On a machine with several GPUs, \
     SARAB_RENDER_NODE=/dev/dri/renderD* picks one";
const RENDER_RULE: &str = "Android's system services and apps draw through it as uids of their own. Open render \
     nodes to every user, as systemd upstream, Arch and Fedora do:\n  \
     echo 'SUBSYSTEM==\"drm\", KERNEL==\"renderD*\", MODE=\"0666\"' | sudo tee /etc/udev/rules.d/70-sarab-render.rules \
     && sudo udevadm control --reload && sudo udevadm trigger -s drm";

pub fn vulkan_driver(kernel: &str) -> Option<&'static str> {
    Some(match kernel {
        "amdgpu" | "radeon" => "radeon",
        "i915" | "xe" => "intel",
        "nouveau" => "nouveau",
        "panfrost" => "panfrost",
        "msm" | "msm_dpu" | "msm_drm" => "freedreno",
        "vc4" | "v3d" => "broadcom",
        _ => return None,
    })
}

fn kernel_driver(sys: &Path, node: &str) -> Option<String> {
    let uevent = std::fs::read_to_string(sys.join(node).join("device/uevent")).ok()?;
    uevent.lines().find_map(|l| l.strip_prefix("DRIVER=")).map(str::to_string)
}

pub fn gpu() -> Result<Gpu, NoGpu> {
    let g = pick_gpu(Path::new("/sys/class/drm"), std::env::var("SARAB_RENDER_NODE").ok().as_deref())
        .map_err(|why| NoGpu { why, fix: NEEDS_MESA })?;
    open_to_all(Path::new(&g.node)).map_err(|why| NoGpu { why, fix: RENDER_RULE })?;
    Ok(g)
}

pub fn require_gpu() -> Result<Gpu> {
    gpu().map_err(|e| anyhow!("{}. {}", e.why, e.fix))
}

fn open_to_all(node: &Path) -> Result<(), String> {
    let mode = std::fs::metadata(node).map_err(|e| format!("{}: {e}", node.display()))?.permissions().mode();
    if mode & 0o006 == 0o006 {
        return Ok(());
    }
    Err(format!("{} is mode {:o}, open only to you and its group", node.display(), mode & 0o777))
}

fn pick_gpu(sys: &Path, wanted: Option<&str>) -> Result<Gpu, String> {
    let mut nodes: Vec<String> = std::fs::read_dir(sys)
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("renderD")).collect()
        })
        .unwrap_or_default();
    nodes.sort();
    if let Some(w) = wanted {
        let name = w.trim_start_matches("/dev/dri/");
        nodes.retain(|n| n == name);
        if nodes.is_empty() {
            return Err(format!("SARAB_RENDER_NODE={w} is not a render node"));
        }
    }
    let mut refused = Vec::new();
    for n in &nodes {
        match kernel_driver(sys, n) {
            Some(d) if !UNSUPPORTED.contains(&d.as_str()) => {
                return Ok(Gpu { node: format!("/dev/dri/{n}"), driver: d });
            }
            Some(d) => refused.push(format!("{n} ({d})")),
            None => refused.push(format!("{n} (no driver)")),
        }
    }
    Err(if refused.is_empty() {
        "no GPU render node in /dev/dri".into()
    } else {
        format!("no render node Mesa can drive: {}", refused.join(", "))
    })
}

pub fn wayland_display(runtime_dir: &Path) -> Result<String> {
    pick_wayland(runtime_dir, std::env::var("WAYLAND_DISPLAY").ok().filter(|s| !s.is_empty()).as_deref())
}

fn pick_wayland(runtime_dir: &Path, env: Option<&str>) -> Result<String> {
    if let Some(d) = env {
        let path = runtime_dir.join(d);
        let name = match path.strip_prefix(runtime_dir) {
            Ok(n) if n.components().count() == 1 => n.display().to_string(),
            _ => bail!("WAYLAND_DISPLAY={d} is not a socket directly in {}", runtime_dir.display()),
        };
        if !path.exists() {
            bail!("WAYLAND_DISPLAY={d}, but there is no {}", path.display());
        }
        if !answers(&path) {
            bail!("WAYLAND_DISPLAY={d}, but nothing answers on {}: its compositor is gone", path.display());
        }
        return Ok(name);
    }
    let mut socks: Vec<String> = std::fs::read_dir(runtime_dir)
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                .collect()
        })
        .unwrap_or_default();
    socks.sort();
    let (live, dead): (Vec<String>, Vec<String>) = socks.into_iter().partition(|n| answers(&runtime_dir.join(n)));
    match live.as_slice() {
        [one] => Ok(one.clone()),
        [] if !dead.is_empty() => bail!(
            "no Wayland session: WAYLAND_DISPLAY is unset and nothing answers on {} in {} (left by a compositor that exited)",
            dead.join(", "),
            runtime_dir.display()
        ),
        [] => {
            bail!("no Wayland session: WAYLAND_DISPLAY is unset and {} has no wayland-* socket", runtime_dir.display())
        }
        many => bail!("WAYLAND_DISPLAY is unset and there are several Wayland sockets ({}); set it", many.join(", ")),
    }
}

fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

pub fn runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sarab-host-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn node(sys: &Path, n: &str, driver: &str) {
        std::fs::create_dir_all(sys.join(n).join("device")).unwrap();
        std::fs::write(sys.join(n).join("device/uevent"), format!("DRIVER={driver}\nPCI_CLASS=30000\n")).unwrap();
    }

    #[test]
    fn the_gpu_is_read_from_sysfs_and_nvidia_is_skipped() {
        let sys = tmp("drm");
        std::fs::create_dir_all(sys.join("card0")).unwrap();
        assert_eq!(pick_gpu(&sys, None), Err("no GPU render node in /dev/dri".into()));
        node(&sys, "renderD128", "nvidia");
        let why = pick_gpu(&sys, None).expect_err("nvidia is not a Mesa GPU");
        assert!(why.contains("renderD128 (nvidia)"), "{why}");
        node(&sys, "renderD129", "i915");
        assert_eq!(pick_gpu(&sys, None), Ok(Gpu { node: "/dev/dri/renderD129".into(), driver: "i915".into() }));
        assert!(pick_gpu(&sys, Some("/dev/dri/renderD128")).is_err());
        assert!(pick_gpu(&sys, Some("renderD200")).unwrap_err().contains("not a render node"));
        std::fs::remove_dir_all(&sys).unwrap();
    }

    #[test]
    fn a_render_node_must_be_open_to_every_user() {
        let d = tmp("mode");
        let node = d.join("renderD128");
        std::fs::write(&node, "").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o660)).unwrap();
        assert!(open_to_all(&node).unwrap_err().ends_with("is mode 660, open only to you and its group"));
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(open_to_all(&node), Ok(()));
        assert!(RENDER_RULE.contains(r#"KERNEL=="renderD*", MODE="0666""#));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn vulkan_follows_the_kernel_driver() {
        assert_eq!(vulkan_driver("amdgpu"), Some("radeon"));
        assert_eq!(vulkan_driver("i915"), Some("intel"));
        assert_eq!(vulkan_driver("xe"), Some("intel"));
        assert_eq!(vulkan_driver("virtio_gpu"), None);
    }

    #[test]
    fn the_wayland_socket_comes_from_the_environment_or_the_only_one_there() {
        let rt = tmp("rt");
        let listen = |n: &str| std::os::unix::net::UnixListener::bind(rt.join(n)).unwrap();
        assert!(pick_wayland(&rt, None).is_err());
        let zero = listen("wayland-0");
        std::fs::write(rt.join("wayland-0.lock"), "").unwrap();
        assert_eq!(pick_wayland(&rt, None).unwrap(), "wayland-0");
        drop(listen("wayland-2"));
        assert_eq!(pick_wayland(&rt, None).unwrap(), "wayland-0", "a socket nobody answers on is skipped");
        let one = listen("wayland-1");
        assert!(pick_wayland(&rt, None).unwrap_err().to_string().contains("several"));
        assert_eq!(pick_wayland(&rt, Some("wayland-1")).unwrap(), "wayland-1");
        let abs = rt.join("wayland-1");
        assert_eq!(pick_wayland(&rt, abs.to_str()).unwrap(), "wayland-1");
        assert!(pick_wayland(&rt, Some("wayland-7")).is_err());
        assert!(pick_wayland(&rt, Some("/tmp/elsewhere")).is_err());
        assert!(pick_wayland(&rt, Some("wayland-2")).unwrap_err().to_string().contains("nothing answers"));
        drop((zero, one));
        assert!(
            pick_wayland(&rt, None)
                .unwrap_err()
                .to_string()
                .contains("nothing answers on wayland-0, wayland-1, wayland-2")
        );
        std::fs::remove_dir_all(&rt).unwrap();
    }
}
