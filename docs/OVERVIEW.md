# Sarab — architecture overview

How Sarab works and why, in one page. The [README](../README.md) is the
quick start; [`TODO.md`](TODO.md) is the backlog.

## Purpose

Run Android apps on a Linux Wayland desktop as ordinary user processes:
**rootless, containerless, no VM, no daemon running as root**, quick to
boot and frozen when unused. The Android framework itself is not
modified: we boot a stock LineageOS 20 (Android 13, x86_64, GAPPS) image,
downloaded from `ota.waydro.id`, unchanged, and rebuild only the *host side*
in Rust.

## How it works

```
host user (uid 1000)
└─ systemd-run --user --scope  "sarab-<pid>"           ← our own cgroup
   └─ sarab-ns  (userns + pidns + netns + mountns + cgroupns + ipcns + utsns)
      └─ /system/bin/init second_stage   (pid 1 inside, "root" = host uid)
         └─ servicemanager, zygote, system_server, SurfaceFlinger,
            the image's hwcomposer HAL → Wayland toplevels, via sarab-hostd
   sarab-hostd  (host binder services Android calls into; relays the display)
sarab start --foreground   (the runtime's owner: run by the systemd unit or a
                            detached `sarab start`; boots the above, freezes
                            the cgroup when no window is open, applies the
                            desktop policy on a /data's first boot)
sarab <command>            (everything a person types: see Daily use)
```

Crates (`crates/`):

- **sarab** — the one command (`setup`, `start`/`stop`/`status`, `app`,
  `exec`, `logs`, `stats`, `policy`, `google-id`, ...), shaped like
  `docker`/`podman`, and the runtime's owner (`start --foreground`). Image
  extraction, overlay generation and the desktop policy live here too.
- **sarab-ns** — the rootless harness. Maps the ids Android 13 uses, sparse,
  from `/etc/subuid` with `newuidmap` (ns uid 0 → us, so Android's
  `system`=1000 is host 100999): 40000 uids and 60000 gids, which the default
  65536 covers. The isolated ids end at 99999, and a one-id extent maps
  `INT_MAX`, the id ConnectivityService means by "every uid"; netd needs both
  ends, see Setup,
  builds a private `/dev` with its own **binderfs**, pivot_roots into the
  extracted image, binds `/usr` for host tools, exposes a FIFO as `/dev/kmsg`,
  and `--bind`/overlay-binds `/system`, `/vendor`, `/data`. Before entering the
  cgroup namespace it moves itself into `<scope>/android` so Android init's
  chown of "its" cgroup root does not take ours away from us. It hardens what
  Android gets: init's environment is the kernel's, not the desktop's;
  its seccomp filter refuses new user namespaces; its init has no
  controlling terminal; `/proc` has `hidepid`; and a default ACL
  on `/dev/socket`, finished by an `early-init` rule in the generated overlay,
  keeps apps off the property service (SECURITY.md). `sarab-ns --enter PID`
  is the way back in, nsenter with supplementary groups: `sarab exec` and
  every `pm`/`cmd` call go through it, and `-u shell` gets adbd's groups.
- **sarab-runtime** (library) — hand-rolled AIDL client for the
  image's platform service over raw binder (rsbinder),
  reached through `/proc/<init>/root/dev/binderfs/binder`. All 13 methods:
  launch, install, getprop/setprop, app list, settings. Also owns freeze /
  thaw / reclaim of the runtime cgroup; every inbound command thaws first.
- **sarab-hostd** — serves the four binder interfaces Android expects *from*
  the host (clipboard, notifications, user monitor, hardware). Must register
  before system_server binds them once at boot. Exits with the runtime via
  `pidfd_open` + poll, so no stale daemons. It also relays Android's
  Wayland connection to the compositor (Window size, below), which makes it
  display-critical: if hostd dies, every Android window goes with it, so
  `sarab start --foreground` stops Android when hostd crashes and exits
  non-zero, and sarab.service boots both again.

Kernel side: binder with binderfs (the Rust binder driver on Arch's kernel,
the `binder_linux` module on Ubuntu's); both work inside an unprivileged user
namespace, which is what makes rootless possible.

## Key technical facts

- **Boot gating.** The image's WindowManager waits for the boot animation to
  stop before enabling the screen. `debug.sf.nobootanimation=1` skips it,
  which is most of what makes the boot take seconds rather than ten.
- **netd and iptables.** The host's legacy xtables modules are not loaded and
  an unprivileged userns cannot autoload them, so the real `iptables-restore`
  fails and netd respawns it in a loop. `overlay/system/bin/iptables` is a
  shell stub over the image's multi-call iptables (the `-restore` tools are
  symlinks to it) that only echoes the `#PING` ack lines netd waits for. Networking itself works: `pasta` on the netns, below.
- **Networking.** `pasta` joins the netns as us — no bridge, no dnsmasq, no
  root — and puts a tap called `eth0` in it. The name is the trick: the stock
  image's `EthernetTracker` filter is `eth\d`, so EthernetService claims it,
  runs DhcpClient against pasta's own DHCP server and publishes a real
  NetworkAgent, which is how Android reaches **VALIDATED** with no image work
  and no NetworkAgent of ours. pasta forwards DNS at `169.254.1.1` to the
  host's first resolver (systemd-resolved's stub here, so host split DNS and
  MagicDNS come along), and needs `--dns-forward` *and* `--dns` *and*
  `--dhcp-dns` *and* `--dns-host` to do it — each missing one fails
  differently and none of them looks like DNS. `sarab start -F --no-network`
  opts out (it needs `--foreground`).
- **Idle = frozen.** The composer HAL maintains `waydroid.open_windows`.
  When it stays 0 for 60 s the watcher writes `cgroup.freeze`; idle CPU is
  exactly 0 % / 0 wakeups. A frozen runtime answers a binder call 13 ms
  after thaw. After 10 min frozen, `memory.reclaim` pushes it to zram
  or whatever swap the host has (skipped when there is none). Physical footprint while idle: ~172 MB.
- **Policy (`sarab policy`, applied by itself on the first boot of a
  `/data`).** Disables Google Quick Search, Gearhead,
  Messaging, IMS, SetupWizard, Seedvault, etc.; installs a code-less HOME
  activity (`apps/sarab-home.apk`, package `org.sarab.home`, 8.5 KB) and
  moves the HOME role to it so Launcher3 never starts (−48 MB net); sets
  `device_config` `max_cached_processes=8`, `max_phantom_processes=8`, and
  the post-boot no-kill grace to 0 (default is 10 min, which hides the
  effect); turns off Play Protect's check of *host* installs, whose "send
  this app to Google?" dialog a host install, with no window open, cannot
  show. Role changes,
  `pm uninstall` and `settings put` refuse uid 0 (AppOps wants a calling
  package), so those run as the shell uid, 2000 -- which `sarab exec -u
  shell` gives without adb.
- **Window model.** Single window: `persist.waydroid.multi_windows=false`.
  `waydroid.active_apps` only picks the composer's mode on each frame:
  `none` draws nothing, `Waydroid` is the full UI, and any other value (we
  write the package) is single-window mode, where the topmost task gets one
  toplevel with every layer on screen in it, so a permission prompt or a
  share sheet inside the app's task is drawn in the app's window, and a task
  of another package gets a window of its own. The
  `false` in the vendor prop file is not enough on its own, because init
  loads `/data/property/persistent_properties` after the vendor file and a
  persisted `true` wins; the policy writes `false` through the property
  service (once per `/data`), and `sarab start` runs a post-boot guard that
  re-sets it and warns when it reads anything else. The composer reads the
  prop at HAL start, so a correction lands on the next boot. The prop file
  lists our home activity in `waydroid.blacklist_apps`: when HOME is the
  topmost task (the last app closed), the composer would otherwise give it an
  empty black window. Launcher3 is hardcoded in the HAL's blacklist.
- **Window size.** The composer shows Android's display at the size of
  `persist.waydroid.width`/`height`, in logical pixels, and nothing told the
  compositor that this is the only size the content has, so a tiling
  compositor tiled the window around it. Two changes. `sarab start` asks the
  compositor for its outputs' logical sizes (`xdg_output`) and writes a
  phone-shaped size, 92% of the shortest screen's height and at most
  480x1000, into the prop file (`screen.rs`): 422x882 for a screen 960
  logical pixels tall. It sets the density too, 160 per unit of the screen's
  scale, so one dp is one logical pixel and Android's text and controls are
  the size of the desktop's; the composer's own choice, 180 per unit, made
  everything 12% larger and the display 346 dp wide. And Android's Wayland socket is a listening socket of
  `sarab start`'s, served by sarab-hostd, which relays every connection to the
  compositor and, before the commit that maps an app window (`waydroid.*` app
  id), adds `set_min_size` and `set_max_size` equal to the composer's own
  `wp_viewport` destination. A fixed-size window is one Hyprland and sway
  float by themselves, centred, at that size. The size is mirrored, not
  computed, so it follows whatever the composer draws. The composer scales
  by the scale it booted with, not the window's output: a window moved to a
  scale-1 output, or opened on one, keeps the same logical size, as every
  other window does (measured on Hyprland, booted at scale 2). Floating fixed-size windows is the compositor's heuristic, not
  protocol, so the README gives a window rule for compositors without it.
  `--no-hostd` binds the compositor's own socket, and windows tile.
- **Trust boundary.** Android's root is the host user, so what keeps an
  Android compromise out of the desktop session is what the namespace can
  see: the image, `/data`, `/usr`, the Pulse socket from `/run/user/<uid>`
  and the display relay's socket in its place of the Wayland one, bound one by
  one onto a tmpfs `/run` as `/run/xdg`.
  adbd is never started and needs a key (`ro.adb.secure=1`), and
  `sarab-hostd` answers only Android's `system` uid, which the image's
  public test keys let any app signed with them take. `SECURITY.md` has the
  whole model, including what is not enforced.
- **Runtime detection.** The runtime is found from the host as the process
  whose comm is `sarab-ns`, whose root is pivoted, which has
  `dev/binderfs/binder`, and which is not a `sarab-ns --enter`: the one that
  set the namespaces up, since its child has become `init` by then. Anything looser matched shells, and other Android
  containers running as root on the same host.
- **Zygote floor.** Any app process costs ~25 MB PSS even with no code; it is
  the fork-from-zygote baseline, not something we can trim from the host.
- **Measurement.** Everything is read from `/proc` on the host; since we own
  the userns we can read `smaps_rollup` of the whole Android tree without
  root. `sarab stats` shows it live.

## Setup

```
packaging/install.sh --prefix ~/.local
                         # builds and installs `sarab`, the systemd user unit,
                         # the .apk handler, shell completion; prints what is left
sarab start              # the first time: asks, then downloads and prepares the
                         # image (~1.4 GB down, ~5 GB on disk); returns when
                         # Android is up (~1.5 s; first boot longer)
sarab google-id          # the one step the Play Store needs, explained
```

The first `sarab start` runs `sarab setup` with its defaults, after saying
where the data goes and asking; only at a terminal, since the unit and a
launcher click have nobody to ask and a 1.4 GB download is not theirs to
start. `sarab setup` itself takes the options: `--data-dir`, `--vanilla` (no
Google apps), `--system`/`--vendor` for zips already on disk, `--latest` (the
newest build rather than the tested one), `--force`.

Setup checks the host first (binderfs, the tools, the sub-id range; `sarab
info` shows these and more) and says what is missing, with the install line
for this distro. Android's ids for one user run to 99999, but sparsely, so
sarab-ns maps only the ranges Android 13 uses, in a fixed layout: 40000 uids
and 60000 gids out of the 65536 that `useradd` hands every user, so the
default needs no root at all. The layout is one constant, never chosen per
machine, because the host owner of every file in Android's /data follows
from it. The ends of ranges matter as much as the ids in them: netd installs
routing rules for uid ranges ending at 99999 and at `INT_MAX`, init writes
`0 2147483647` to `ping_group_range`, and the kernel refuses a range whose
end is unmapped (EINVAL). Without the uid end the runtime boots perfectly with
no networking at all; without the gid end, every ping fails. So both maps
carry a one-id extent for `INT_MAX`. `pasta` is the one host tool whose absence Android would
survive, offline; `sarab start` refuses instead, unless `sarab start -F
--no-network` asks for exactly that.

Setup is idempotent: an extracted image is left alone (`--force` redoes it),
so running it again after an upgrade only regenerates the overlay. It prints
the data directory and its free space before it downloads anything, and
`--data-dir DIR` moves the data of an installed Sarab to a bigger disk; a
first extraction that would not fit stops before it starts. The images are
the builds Sarab is tested with, pinned by SHA-256 in `image.rs`, and each
zip is kept under its release's name. It extracts with `debugfs rdump`
*inside* the user namespace, which is what gives Android's files their real
owners without root, into `<part>.partial`, renamed only when the whole
extraction succeeded; `--system`/`--vendor ZIP` use zips you already have.

`packaging/install.sh` writes the unit but never calls systemctl: enabling
it boots Android at every login, so that stays a deliberate act. Whenever
the unit is installed, enabled or not, `sarab start`/`stop`/`restart`, a
click on an app and a double-clicked `.apk` go through systemd (starting a
unit does not enable it); enabling adds only the start at login. Without the
unit (a checkout used without install.sh), `sarab start` starts a detached
runtime instead (log: `sarab logs --daemon`). Either way there is one
runtime: `sarab start --foreground`, which both routes end
in, refuses to start a second. `packaging/install.sh uninstall` removes
exactly what it created.

Where things live depends only on the `sarab` binary (paths.rs), and `sarab
info` prints it:

| | installed (`--prefix`) | checkout |
|---|---|---|
| image, Android's `/data`, generated overlay | `$XDG_DATA_HOME/sarab/android`, or `--data-dir` | `images/` |
| logs | `$XDG_STATE_HOME/sarab` | `run/` |
| kmsg FIFO, start lock | `$XDG_RUNTIME_DIR/sarab` | `run/` |
| hand-written overlay | `<prefix>/share/sarab/overlay` | `overlay/` |
| `sarab-ns`, `sarab-hostd` | `<prefix>/lib/sarab`, or `SARAB_LIBEXECDIR` at build time | `target/release` |

A binary inside a checkout (the `packaging/install.sh` link without
`--prefix`, `cargo run`) uses the checkout; anything else is installed. The
working directory never decides. The two do not share data: an install
reaches a checkout's image only through `sarab setup --data-dir
<checkout>/images`.

## Daily use

```
sarab status                         # running / paused / booting, and who owns it
sarab install app.apk                # also .apks/.xapk bundles; or double-click
sarab app ls                         # PACKAGE  SOURCE  NAME   (--json, -q)
sarab app launch com.example.app     # what the launcher entries run
sarab app rm com.example.app         # see the note in TODO.md: restarts Android
sarab exec [-u shell] [cmd ...]      # a command, or a shell, inside Android
sarab logs [-f] [--kernel|--daemon|--hostd] [-- logcat args]
sarab stats                          # live memory / CPU / pids of the runtime
sarab pause | unpause                # the cgroup freeze by hand (idle does it anyway)
sarab prop get|set|ls, sarab settings get|put, sarab policy status
sarab info                           # versions, directories, host checks
```

Every command that talks to Android thaws a paused runtime first, and a
`sarab exec` session counts as activity, so Android does not freeze under an
open shell. `sarab completion bash|zsh|fish` prints completions (install.sh
installs bash's, and fish's when fish is installed).

## Open work (see `TODO.md`)

1. **Apps.** Installing works (`sarab install`, the `.apk` handler, split
   bundles, the Play Store once `sarab google-id` is registered), but most
   apps people want are ARM-only and there is no translation layer yet.
2. **Uninstall restarts Android's system services** (NetworkStatsService's
   BPF map is null without bpffs). Needs a patched connectivity module.
3. SystemUI (116 MB) is image work; first boot after a package change costs
   ~5 s; reclaim could be partial rather than all at once.
