# Security

Sarab runs Android as your user, with no root anywhere on the host. That
answers one question, "can Android take over the machine's root?" No, short
of a kernel bug. It does not answer "can one bad app take over my desktop?",
and this page is about the gap between the two.

**The short version:** a Sarab app is separated from other apps by
Android's uid model, and from Android's system by little more than that.
Android's root is your desktop user, fenced in by namespaces rather than by
permissions. Sarab is not a sandbox for apps you do not trust, and it is not
comparable to GrapheneOS or to a phone with verified boot. Install apps into
it the way you would install a native Linux program: from sources you trust.
This matters more than usual because the Android image is signed with AOSP's
public test keys, so an app can make itself part of Android's system
([below](#the-image-is-signed-with-public-test-keys)).

## Who runs as whom

| Inside Android | On the host | What runs there |
|---|---|---|
| root (uid 0) | **you** (your uid) | `init`, `ueventd`, `storaged`, the Wayland composer HAL, the audio HAL |
| system uids (1000 and up) | your subuids (100999 and up) | `system_server`, `servicemanager`, SurfaceFlinger, the other HALs |
| apps (10000 and up) | your subuids (109999 and up) | every app, Google Play services included |

A user namespace maps them this way (see `crates/sarab-ns`). The first row is
the one that matters: anything that becomes Android root runs as you.

## What Android root can reach

It runs as your uid, but inside the runtime's own user, mount, pid, network,
cgroup, IPC and UTS namespaces, with an environment of its own: nothing of
your session's (no `SSH_AUTH_SOCK`, no `DBUS_SESSION_BUS_ADDRESS`, no tokens
you exported) reaches Android. From there it sees:

- the Android image, Android's data and the generated overlay files, all in
  the data directory (`sarab info` lists it) and all writable, since you own
  them. The hand-written overlay files are writable too when Sarab is
  installed into your home directory, and read-only when root installed them;
- the host's `/usr`, read-only, since the host's root owns it;
- **your compositor, through sarab-hostd, and your Pulse socket, and nothing
  else of `/run/user/<uid>`.** The Wayland socket Android sees is a relay
  that passes every message on to the compositor and adds only the size of
  each app window, so it gives the same reach as the compositor's own socket.
  The D-Bus session bus, the GnuPG, SSH and keyring agents and any password
  manager's socket are not visible;
- these device nodes, with the host's permissions: all of `/dev/dri` (the
  render nodes and the card nodes), `/dev/snd`, `/dev/dma_heap` (root-only
  on most hosts, so no Android uid can open it), `/dev/net` (its `tun` also
  at `/dev/tun`, where VPN apps reach it through Android's VpnService; making
  an interface on it takes `CAP_NET_ADMIN`, which apps do not have), and
  `null`, `zero`, `full`, `random`, `urandom`, `tty` and `fuse`;
- the network, as an ordinary outbound client (below).

The two sockets are real reach into your session. A Wayland client can do
whatever your compositor lets any client do. Many compositors, Hyprland and
the other wlroots ones included, give every client screen capture, clipboard
management and virtual input unless it connects through the
`security-context` protocol, and Sarab does not do that yet. A Pulse client
can record from your microphone. So treat an Android root compromise as
"can watch the screen, read the clipboard, type and listen", not as "contained".

## Between apps and Android's system

The image is built without SELinux. What is left:

- **Uids and file permissions**, as on any Linux system, and Android's
  per-app seccomp filter.
- **Only root and the system uids can set system properties.** On stock
  Android, SELinux decides this per property. Here an ACL on the property
  service's socket admits root and uids 1000-1099, 2000 (shell), 9998 and
  9999, and refuses every app and every isolated process (WebView
  renderers). Anything with a system uid, system-uid apps such as the phone
  app included, can still set *any* property, `persist.*` and
  `ctl.start`/`ctl.stop` included. On a kernel without ACL support on tmpfs
  (`CONFIG_TMPFS_POSIX_ACL`), the socket stays open to every app, and
  sarab-ns says so in the runtime's log.
- **Any app can look up any binder service.** Most of Android's services check
  the caller's permissions themselves. A service that relies on SELinux
  alone is open to every app.

The Android-side seccomp filter that `sarab-ns` installs
(`crates/sarab-ns/src/seccomp.rs`) is not a sandbox. It makes four scheduling
and privilege syscalls report success so Android's init does not abort, and
what it refuses is a new user namespace (below), and with it every `clone3`
and every x32-ABI syscall, which get ENOSYS.

## The image is signed with public test keys

The LineageOS build Sarab downloads is signed with AOSP's test keys
(`ro.build.tags=test-keys`; its platform certificate has SHA-1
`27:19:6E:38:6B:87:5E:76:AD:F7:00:E7:EA:84:E4:C6:EE:E3:3D:FA`). The private
halves of those keys are published in the Android source tree. Android
decides who may join the `system` uid, and who may update a preinstalled
app, by comparing signing certificates. So anyone can sign an app that:

- declares `android:sharedUserId="android.uid.system"` and is installed as
  uid 1000, Android's `system` user: it can then set any system property,
  call the host services below as if it were `system_server`, and use every
  permission the platform grants itself. This was confirmed on this image: a
  code-less test app signed with the published key installed as uid 1000;
- replaces a preinstalled app signed with one of those keys and keeps that
  app's privileged permissions.

Nothing in Sarab can fix this from the host: the fix is an image signed with
keys that are not public, which is the vendor and system image work in
[`docs/TODO.md`](docs/TODO.md). Until then, an app from an untrusted source
can become Android's system, which here is as good as Android root (it can
start init's root services with `ctl.start`), and so reach what Android root
reaches (above).

## What Sarab does about it

- **adb is off, and keyed.** adbd is never started, and `ro.adb.secure=1`
  makes it require an authorised key, so an adbd that something with a
  system uid starts refuses its clients (`unauthorized`); no key is
  authorised. An `ro.` property cannot be changed once it is set. No port is
  forwarded to adbd from the host; `sarab exec` is the way in.
- **Your terminal stays out.** `sarab exec` from a terminal gives the
  command a terminal of Android's own and relays to yours, as `lxc-attach`
  and `podman exec -t` do, so nothing inside Android ever holds your
  terminal: it cannot read what you type after it ends, change your
  terminal's modes, or, on kernels that still allow `TIOCSTI`, type
  commands into your shell. A killed `sarab exec` puts your terminal back
  first. Android's init, and a `sarab exec` with no terminal at all, start
  in a session of their own, so `/dev/tty` inside Android never opens the
  terminal `sarab start` or `sarab exec` was run from.
- **Not debuggable.** The image is a userdebug build; Sarab sets
  `ro.debuggable=0`, so apps are not debuggable unless they say so
  themselves, and the debugging hooks a debuggable build honours for every
  app are off.
- **No user namespaces inside.** The seccomp filter every Android process
  inherits refuses `unshare` and `clone` with `CLONE_NEWUSER` (and `clone3`,
  whose flags it cannot see, so libc falls back to `clone`), and no process
  can remove it, Android root included. So nothing in Android can create a
  user namespace, and the kernel surface behind them (nf_tables and the
  rest), which a phone never exposes to apps, stays out of reach.
  `user.max_user_namespaces` is also set to 0 for the runtime's namespace,
  but Android root could set that back; the filter is what holds.
- **Apps see only their own processes.** `/proc` is mounted with
  `hidepid=invisible`, as on stock Android; members of Android's `readproc`
  group (system_server, shell) see all of Android's processes. None of the
  host's are visible to anything in Android.
- **Only Android's `system` uid can call the host services.**
  `sarab-hostd` checks the kernel-reported caller of every binder
  transaction. An ordinary app that calls the clipboard, notification,
  launcher or power service directly is refused, and the refusal is logged
  (`sarab logs --hostd`). The `system` uid is `system_server` and every app
  that shares its uid: Android's own platform-signed apps (Settings among
  them) and, with this image, any app signed with the public test key
  (above). A check on the calling process would not narrow it, since those
  processes can name themselves anything. For every other app, the host
  clipboard is read only through Android's own clipboard service, with
  Android's focus rule, and only Android's power menu can stop the runtime.
- **Desktop entries are built from checked input.** Package names must be
  package names before they are used in a file name. Control characters in an
  app's label are replaced, so a label cannot add lines to the `.desktop`
  file. The `Exec` path is quoted. An icon is written only if it is a PNG of
  sane dimensions. Reads from an APK are capped in size.
- **Notification images must match their own dimensions** before they reach
  the notification daemon. Body text is escaped when the daemon renders
  markup.
- **Only the device nodes listed above are bound in.** `/dev/kvm`,
  `/dev/uhid` and `/dev/sw_sync` are not.
- **The network is outbound only.** pasta forwards no inbound port, and the
  host's loopback (a dev server, a database, a local agent) is not reachable
  from Android.

## What is not enforced

- **The INTERNET permission, only partly.** An app without it cannot resolve
  names, because Android's resolver checks the permission, but it can open a
  connection to any IP address. Android enforces the rest with BPF programs
  and iptables chains, and neither runs here. Per-app network restrictions,
  Data Saver, background data limits and VPN lockdown have no effect.
- **The LAN.** Android sees your local network the way your user does.
- **Data at rest.** Android's data is plain files in the data directory,
  readable by anything that runs as you. Keymaster and gatekeeper are software implementations, so an
  app's "hardware-backed" keys are files on your disk.
- **Verified boot.** The image is a set of writable files in the data
  directory.

## Trust in what Sarab downloads

`sarab setup`, which the first `sarab start` runs, downloads the LineageOS
images Sarab is tested with from SourceForge, where `ota.waydro.id` points,
over HTTPS on every hop, and checks each one against a SHA-256 kept in Sarab's
own source (`image.rs`). A changed file on the server or a mirror is refused.
You are still trusting whoever built those images, since there is no
signature, with code that runs as you. `sarab setup --latest` takes the newest
build in the `ota.waydro.id` listing instead, checked only against the SHA-256
that same server publishes: that catches a corrupt download, not a
compromised server.

The GAPPS image ships Google Play services as privileged system apps. The
desktop policy (`sarab policy`) turns off Play Protect's check of apps
installed from the host: an install from the host opens no Android window,
so the dialog it raises would wait unseen, and it offers to upload the APK to
Google. Apps from the Play Store are still checked.

## The kernel

Every app talks to the host kernel directly: through binder (on the kernels
Sarab was built on, the new Rust binder driver), the GPU driver, user
namespaces and ordinary syscalls. A bug in any of these that an app can reach
is a compromise of the host, for every user of the machine, not just yours.
That is true of any container, but it is the real boundary here.

The GPU is shared with your desktop, and so is its failure. Anything in
Android that can hang the GPU takes your screen down with it while the kernel
recovers. This has happened: with a Codec2 setting since removed, several
decoders allocating buffers at once made the image's graphics allocator
(minigbm, with Mesa 26.0.5's radeonsi) fault on the GPU, and amdgpu answered
with a ring timeout and a full GPU reset, the desktop included. Nothing needed
privileges: ordinary buffer allocations from an unprivileged process did it.
The kernel did its job, so this is availability, not escape, but any app that
drives the GPU hard enough could do the same.

Ubuntu 23.10 and later restrict unprivileged user namespaces through AppArmor
(`kernel.apparmor_restrict_unprivileged_userns`), to keep that kernel surface
away from programs that have no need for it. Sarab needs it, so on those
systems you install a profile, `/etc/apparmor.d/sarab-ns`, which lets one
path, sarab-ns, create user namespaces with their capabilities, and confines
it no further. Whoever can replace the file at that path gets the same
exemption. With the default install under your home that is anything running
as you, which for your user undoes what the restriction was for. Installed
somewhere only root can write, it stays limited to sarab-ns.

Android's services and apps use the GPU through its render node
(`/dev/dri/renderD*`) as uids of their own, which the desktop user's ACL does
not cover, so Sarab needs the node open to every local user. That is upstream
systemd's default, and Arch's and Fedora's; Debian and Ubuntu restrict it to
the `render` group, and there Sarab asks for a udev rule with the upstream
mode. It covers render nodes only, which cannot drive a display, not the
`card*` nodes that can. What it opens is the GPU driver's kernel interface,
to every local account, the same exposure those other distributions ship.

## Still to do

Tracked in [`docs/TODO.md`](docs/TODO.md):

- Connect the composer through Wayland's `security-context` protocol, so the
  compositor denies it screen capture, clipboard management and virtual input,
  and run the composer and the audio HAL as something other than Android root.
- Confine `sarab-hostd` with Landlock. It is the host process that parses what
  Android sends and the APKs apps ship.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting ("Report a vulnerability"
on the repository's Security tab) rather than a public issue. Include the
commit you tested and how to reproduce. This is a one-person project; expect
an answer within a few days, not hours.
