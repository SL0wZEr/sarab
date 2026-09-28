# TODO

What is left, roughly in the order it should be done.

Every item carries a priority and a status:

- **Priority.** `P1` before the first public release; `next` for the
  milestone after it, our own vendor image; `P2` soon after; `P3` when there
  is time; `later` for the rest of the image work and the long game.
- **Status.** `confirmed`: reproduced on a live runtime. `from code`: found
  by reading the code, not reproduced. `unverified`: expected from upstream
  documentation or reasoning, not seen on any machine. `partly done`: some
  of it works already. `open`: a plan or a question, nothing to reproduce.
  `blocked`: waiting on something outside this repository.

## Before a first release

- [x] `P1` `confirmed` **Ubuntu 23.10 and later.** Tested 2026-09-27 on
      Ubuntu 24.04 with GNOME 46 (Wayland) in a qemu VM with virgl, as a
      stranger would: install.sh, then following each message. Now handled,
      each with a one-line fix before any download: binder is a module
      nothing loads (`binder_linux`); AppArmor restricts user namespaces
      (`sarab apparmor-profile`, and sarab-ns names the fix when confined);
      the render node is 0660 `render`, so SurfaceFlinger restarted forever
      (udev rule to upstream's 0666); the user's umask is 002, so init
      skipped every generated .rc as insecure and zygote never started
      (sarab sets 022 and repairs modes); pasta 2024_02_20 lacks
      `--dns-host` and `--map-host-loopback` (flags read from its binary;
      `--no-map-gw`, and an upstream nameserver for DNS). Boots in about 2 s
      with network and DNS.
- [ ] `P2` `confirmed` **What the Ubuntu run left.** The clipboard is off on
      GNOME: no `zwlr_data_control_manager_v1`, and the wl-clipboard fallback
      is not installed by default; say so in `sarab info` and the README, or
      find another route. The missing-tools error names `uidmap` but gives an
      install line only for pasta; one line for all of them. The gpu row
      shows `virtio-pci` for a virtio GPU (the PCI parent's driver). With
      old pasta, split DNS is lost (queries go to the upstream server, not
      systemd-resolved). A window on GNOME was not yet looked at by a
      person, nor any KDE session.
- [ ] `P2` `partly done` **Android's data cannot be moved.** `sarab purge
      [--data-only]` now deletes it through the namespace, and
      `install.sh uninstall --purge` calls it. `--data-dir` still only saves
      a new path, so the apps are left behind in the old one: add `--move`,
      renaming or copying inside the namespace.
- [ ] `P2` `open` **No GPU Mesa can drive.** Setup (before the download),
      start and `sarab info` now refuse with the reason, where a black window
      used to follow a 1.4 GB download. What is left is a software path, so
      those machines can run at all: two are in the image and need a boot
      test, `mesa` with `debug.mesa.android.no.kms.swrast=true` (llvmpipe), or
      `angle` with Vulkan `pastel` (SwiftShader Vulkan), with no `/dev/dri`
      bound.
- [ ] `P1` `partly done` **Tests beyond the unit tests.**
      `crates/sarab/tests/cli.rs` runs the built binary with no Android (help,
      completions, errors, exit codes) in CI, but nothing boots Android.
  - [ ] `P1` An end-to-end test, `#[ignore]`d: boot, install an APK, launch
        it, check the window, uninstall, stop. Run locally before each
        release; CI runners lack binderfs.
  - [ ] `P3` Replace the hand-rolled temp dirs in tests with `tempfile`: a
        failing test leaves its folder in /tmp.
  - [ ] `P3` `insta` snapshots for the prop file and `--help`, `proptest`
        for `exec_arg` quoting and the debugfs parsers.
- [ ] `P1` `partly done` **First run on a clean machine**, as a stranger
      would. Done 2026-09-26 on the development machine: a fresh clone,
      `install.sh --prefix`, `sarab start` with nothing downloaded, setup
      through a terminal, first boot in 2.9 s, Settings opened floating at
      its size, `uninstall --purge` leaving nothing. It found, and these are
      fixed: the mirror ran at about 260 KB/s behind a bare bar, dropped the
      connection twice (53% and 70% of the system half), and each drop
      failed setup; launching a missing package exited 0; a failed launcher
      click was silent. Still open from it: the first boot's policy "takes
      full effect from the next start", so a stranger's first session is not
      the finished one, and it wrote a launcher entry for the keyboard's
      settings (`com.android.inputmethod.latin`) that a settled install does
      not have. Not covered by that run: a cold cargo cache, and the
      automatic resume against the real mirror (it did not drop again once
      that build ran; a test covers it). Ubuntu with GNOME was covered later
      (the first entry above); other distros and compositors are not.
- [ ] `P1` `confirmed` **The image is signed with AOSP's public test
      keys.** Its platform certificate is the one whose private key is in
      the Android source, so an app signed with it and declaring
      `sharedUserId="android.uid.system"` installs as uid 1000 (confirmed
      2026-09-27 with a code-less app through `sarab install`), and an app
      signed with the test keys can update a preinstalled one and keep its
      privileges. Documented in SECURITY.md. The fix is an image signed with
      keys of our own: the system half, not only the vendor, since the
      platform key signs framework-res and the system apps. Decide before
      the release whether it ships documented, or waits for that image.

## Next milestone: a vendor image of our own

After the first release. Media decoding is blocked in the vendor half, the
composer that window resizing needs lives there too, and the vendor image is
189 MB against the system's 1.2 GB, so owning it is the cheaper half to own.
In order:

- [ ] `next` `open` **Watch the upstream vendor listing first.** When a vendor
      built after 2026-07-23 appears (`sarab setup --latest` reads the
      listing), move the pin in image.rs, run apps/media-probe with default
      settings only, and see what it unlocks before building anything.
- [ ] `next` `open` **Build the vendor half.** It comes out of the full
      LineageOS 20 tree with the image's device and vendor repositories: a
      sync of 100 GB or more and hours for the first build, then cheap
      rebuilds. Reproduce the published vendor first, byte for byte where it
      can be, so every later difference is ours.
- [ ] `next` `open` **Test why concurrent allocations hang the GPU.** The
      evidence (four allocator threads in one millisecond, then the fault)
      points at one Mesa context used from several threads in minigbm's
      gbm_mesa; a lock around its allocations is the first thing to try,
      with apps/media-probe `--parallel` on throwaway data and the kernel log
      watched. A newer Mesa may fix it as well.
- [ ] `next` `open` **Carry e5a7769a** (minigbm, "Fix YVU420 format") for
      video frames, then pin our own vendor build with its hash in image.rs.
- [ ] `next` `open` **Report the GPU hang to Mesa**, with the kernel log (gfx
      ring timeout, MODE2 reset, the faulting process `allocator@4.0-s`
      thread `cs0`), Mesa 26.0.5 in the image and 26.2.3 on the host, and
      the reproduction: several MediaCodec decoders with
      `debug.stagefright.c2-poolmask=851968`. First check whether it
      reproduces with a current Mesa in the image.
- [ ] `next` `blocked` **No media decoding.** Every Codec2 decoder fails with
      `NO_MEMORY`: `/dev/dma_heap/system` is the host's, root-only (0600),
      so no Android uid can open it ("DMABUFHEAPS: No ion heap of name
      system"), and the ueventd rule cannot change a node host root owns.
      Anything that plays audio or video through MediaCodec is affected.
      Reproduced 2026-09-26 with apps/media-probe (AAC, MP3, Vorbis, Opus,
      H.264, VP9). Tried and ruled out:
  - `debug.stagefright.c2-poolmask=851968` (0xd0000, the BLOB bit) makes
    Codec2 take linear buffers from minigbm. One decoder at a time works
    (AAC, MP3, Vorbis, Opus). Several at once hang the host GPU, desktop
    included: in the minigbm allocator, four threads allocating BLOB
    buffers in the same millisecond, then a gfx ring timeout and a full
    GPU reset (radeonsi, Mesa in the allocator service). Set in the prop
    file it happens during boot; set after `sys.boot_completed` it
    happened on the first test with eight parallel decodes. Never ship it.
  - `gralloc.gbm.legacy=true` keeps the older gbm gralloc: the image's init
    otherwise rewrites our `ro.hardware.gralloc=gbm` to `minigbm_gbm_mesa`.
    It cannot allocate BLOB (format 33) and crash-loops system_server and
    SystemUI.
  - A udev rule opening the heap. A uaccess ACL reaches only the desktop
    uid, which inside Android is root alone; the app and `mediacodec` are
    subuids. Reaching them takes a world-open heap (any local process pins
    unaccounted memory) or the media parser running as the desktop user.
    The world-open heap is what the image's own container tooling does: it
    runs `chmod -R 777` on `/dev/dma_heap/*` and the render nodes, host-wide,
    until the next reboot. With
    the heap left that way by the 2026-09-26 benchmark, AAC decoded under
    Sarab with default settings, which confirms the diagnosis.

  No host-side route is left for audio: it needs image work, a Codec2
  linear allocator backed by memfd, or a minigbm that serialises its Mesa
  context. Video needs that and one more fix: the software decoders write
  YV12 frames, which the pinned vendor's minigbm allocates as a 1D buffer
  it then cannot map ("Failed to map the buffer", `lockYCbCr` err 3).
  Fixed upstream in the image's minigbm, e5a7769a
  (2026-07-23), after the pinned vendor (2026-04-28): move the pin when a
  vendor build carries it, and re-run the decode probe.

- [ ] `next` `confirmed` **Mouse drags are not touches.** The composer
      hands Android the pointer as a mouse (`CURSOR` class, tool type mouse),
      so pull-to-refresh and drag-to-scroll fail in apps that want a finger.
      The image's `persist.waydroid.fake_touch` (package list, `*` for all)
      only relabels the event source in ViewRootImpl, which native views
      follow and Flutter does not: Flutter reads the tool type, and its
      default scroll behaviour ignores mouse drags. Checked 2026-09-27 with a
      Flutter app: the property logged "Faking touch inputs" and nothing
      changed. A touchpad's two-finger swipe arrives as wheel events, which
      no pull-to-refresh reacts to. The fix is in the composer: send
      click-drags, and finger-source scrolls (`axis_source`), as touches on
      its touch device. An app of one's own can instead add
      `PointerDeviceKind.mouse` and `trackpad` to its `dragDevices`.

With the vendor built, the composer's display hotplug for window resizing
(Windows, below) becomes a change of ours rather than image work.

## Security

`SECURITY.md` is the model as it stands.

- [ ] `P3` `open` **System uids can set any property.** The property
      socket's ACL (sarab-ns, overlay.rs) keeps apps and isolated processes
      out, but any system uid, system-uid apps such as the phone app
      included, can still set anything, `ctl.*` and `persist.*` included;
      SELinux would decide per property. Narrowing it needs a filter that
      reads the request, not just the caller.
- [ ] `P3` `open` **Nothing reports an open property service.** Without
      tmpfs ACLs (`CONFIG_TMPFS_POSIX_ACL`) the socket stays open to apps
      and only the runtime's log says so. `sarab info` should check the
      socket's mode on a running runtime.
- [x] `P2` `from code` **`sarab exec` hands the host terminal to Android.**
      `sarab-ns --enter` now takes a pseudo-terminal from the runtime's own
      devpts (`TIOCGPTPEER`) when any of stdin, stdout or stderr is a
      terminal, and relays, as lxc-attach does. Tested 2026-09-27: a shell,
      Ctrl-C, window size, exit codes, mixed pipes and terminals, `-u shell`
      owning its terminal, and a SIGTERM to the relay restoring the host
      terminal.
- [ ] `P2` `open` A licence policy for `cargo deny` (`[licenses]` in
      `deny.toml`). CI already checks advisories, bans and sources.
- [ ] `P3` `unverified` **The host session keyring is inherited.** sarab-ns
      never touches keyrings, and the unit sets no `KeyringMode`, so it gets
      the user manager's default, `inherit`. Confirm
      with `KEYCTL_GET_KEYRING_ID` on `@s`, then join a fresh anonymous
      session keyring before exec.
- [ ] `P3` `open` **The composer through `security-context`.** Connect it with
      `wp_security_context_v1`, so the compositor denies it screen capture,
      data-control and virtual input: a listening socket of ours, registered
      with the compositor. The display relay's upstream connection can go
      through that listener; neither needs the other. Then the
      composer and the audio HAL (both Android root, i.e. the desktop user)
      can move to a subuid: they need only the two sockets, whose modes we
      would then control.
- [ ] `P3` `open` **Landlock for sarab-hostd.** It parses Android-supplied
      strings and the APKs apps ship. It needs to read the runtime's
      `/proc/<pid>`, write `~/.local/share/applications` and
      `~/.local/share/sarab/icons`, talk to the session bus, and connect to
      the Wayland socket, for the clipboard and for every connection it
      relays for Android; nothing else.

## Setup and images

- [x] `P2` `from code` **No upgrade path.** `sarab setup` now upgrades a
      half older than the pin (same flavour), extracted beside the old tree
      and swapped in only when complete, keeping `/data`; it refuses while
      Android runs on it, and leaves newer, unknown and user-given builds
      alone. `sarab upgrade` does it with a confirmation and stops and starts
      Android around it; install.sh, `sarab start` and `sarab info` say when
      it is due. Tested
      2026-09-27 in the Ubuntu VM with the vendor pin moved by a second:
      refused while running, then extracted and swapped in 3 s, booted with
      the test app still installed. The old zip stays (see below).
- [x] `P2` `from code` **Refuse an image outside the tested range.** Both
      halves must be API 33 (`image::check_api`): checked on the extracted
      tree before the swap, and by `sarab start`.
- [ ] `P2` `partly done` **The generated overlay outlives the image.**
      `sarab start` now regenerates it before every boot, so an edited file
      is always current, but nothing clears `generated/`: an rc file that no
      longer needs edits, or one the image dropped, keeps its old copy.
      Generate into `generated.new` and swap it whole.
- [ ] `P2` `partly done` **Audio.** The Pulse socket is bound when it exists
      and accepts a connection from Android, but no sound has been played
      through it end to end, and nothing says so when it is missing: add a
      row to `sarab info` and a line to the README's requirements
      (pipewire-pulse or PulseAudio).
- [ ] `P3` `from code` **The zips stay after extraction**, 1.4 GB, and a zip
      from an older pin stays forever, one more with every upgrade. Delete
      them on success unless `--keep-zips`.
- [ ] `P3` `partly done` **An enabled unit before setup** failed every 5 s,
      forever. It now exits with a status the unit does not retry (5, with
      the other host refusals), so it fails once; waiting quietly for setup
      would still be nicer.
- [ ] `P3` `unverified` **No CPU or architecture check.** An x86_64 CPU
      without SSSE3, SSE4.2 or POPCNT, or a non-x86_64 host, fails only as
      "runtime exited before it finished booting", after the download.
      Check both in `check_host` and show them in `sarab info`; refuse
      rather than fall back to the x86 image.
- [ ] `P3` `open` **No property overrides.** The prop file is regenerated
      every start, so `ro.*` values such as the density cannot be changed.
      Read `$XDG_CONFIG_HOME/sarab/props` after the generated lines.
- [ ] `P3` `open` **Preinstalled zips.** Look for the pinned zips in
      `/usr/share/sarab/images` before downloading, for
      distro packages.

## Session and networking

- [ ] `P2` `partly done` **A compositor restart strands Android.** Android's
      socket is now hostd's relay, which stays alive and connects to the
      compositor's path anew for each connection, so a new compositor on the
      same socket name is reachable. Untested: whether the composer, whose
      connection dies with the old compositor, reconnects at all (that means
      SurfaceFlinger restarting the HAL); if not, watch the socket from
      `start --foreground` and restart the runtime. hostd's native
      clipboard still stops for good: give it a reconnect loop.
- [ ] `P2` `from code` **Network failures.** An error in `net::attach` after
      the runtime is spawned leaves Android running with no owner, no host
      services and no freeze watcher. The warnings land in a log nobody
      reads. Kill the child on every error path, and add a network line
      (with the address) to `sarab status`. A pasta that dies is now started
      again (see DNS below).
- [x] `P2` `from code` **DNS is a snapshot.** `sarab start` now reads the
      host's nameserver every 2 s and restarts pasta when the one it forwards
      to changes (pasta cannot re-read it), and restarts a pasta that died
      after running 10 s. Tested 2026-09-27 on Ubuntu 24.04, old pasta:
      `resolvectl dns` to 1.1.1.1 and back, Android resolving 3.1 s after the
      change with the new server in its lease. An offline start gets pasta's
      local mode (link-local address, default route, no nameserver) and a
      proper pasta with the first nameserver.
- [ ] `P3` `from code` **Each start wipes the previous boot's logs.**
      `kmsg.log` and `hostd.log` are truncated, so a crash's log is gone
      after the restart; `sarab.log` grows forever. Keep one previous copy;
      cap `sarab.log`.
- [ ] `P3` `from code` `host::runtime_dir` hardcodes `/run/user/<uid>`,
      while paths.rs honours `XDG_RUNTIME_DIR`.
- [ ] `P3` `open` Treat Android's suspend request as a hint to freeze at once,
      not after the idle minute.

## Graphics and devices

- [ ] `P2` `from code` **All of `/dev/dri` is bound.** Mesa's EGL picks the
      first render node that probes, so with `SARAB_RENDER_NODE` on a
      two-GPU machine, gralloc allocates on one GPU and EGL renders on the
      other. Bind only the chosen render node and its card node, set
      `drm.gpu.vendor_name`, and warn when the node is not world-rw, since
      only Android root has the desktop user's ACL.
- [ ] `P3` `from code` **Vulkan where the image has no driver for the GPU.**
      The image always declares Vulkan. Fall back to lavapipe (`lvp`) when
      no HAL matches, map the legacy `radeon` driver there instead of RADV,
      and pick `intel_hasvk` for Intel before Gen9 (by PCI id).

## Shell and logs

- [ ] `P3` `from code` `sarab logs -- -b radio` still shows the default
      buffers: skip the default `-b` when the user passes one.
- [ ] `P3` `confirmed` `/system/bin/monkey` has no shebang, so `sarab exec
      monkey` fails with "Exec format error"; `sh /system/bin/monkey` works.

## Desktop integration

- [ ] `P2` `partly done` **Location on every desktop.** hostd serves
      Android's GNSS HAL from GeoClue (location.rs). Works on Hyprland with
      GeoClue's demo agent (2026-09-28): an IP-only fix (25 km) reached
      Android but Google Play services dropped it, a 50 m one from
      `/etc/geolocation` reached the app. Test GNOME's built-in agent, and
      KDE, which may have none. `sarab info` says whether GeoClue is
      installed; whether an agent runs is only in the hostd log.
- [ ] `P2` `from code` **A late notification daemon disables notifications
      for the boot.** hostd gives up if the daemon is not on the bus when it
      starts. Register the service anyway and connect on the first
      notification.
- [ ] `P2` `open` **A first run on the desktop.** Setup is `install.sh`, then
      `sarab start`, both in a terminal. Nothing on the desktop says
      "register this device" when the Play Store is clicked.
- [ ] `P3` `from code` **User edits to launcher entries are overwritten**,
      read-only or not, at every boot, and the prune deletes any file named
      `sarab.*`. Only rewrite or prune a file whose content is still what
      hostd wrote.
- [ ] `P3` `from code` **An app that loses its launcher activity** keeps its
      entry until the next boot. Remove it on the package-change event.
- [ ] `P3` `open` **A menu folder for Android apps** on menu-based desktops
      (KDE, XFCE): an `applications-merged` `.menu` and a `.directory`.
- [ ] `P3` `open` **A `market://` handler**, hidden, default only where there
      is none, running `sarab app intent`.
- [ ] `P3` `from code` **Entry details.** No `Comment`, so an Android app and a
      host app of the same name look the same; an empty label gives an
      empty `Name`; names are always in Android's locale.
- [ ] `P3` `from code` hostd applies its own `XDG_DATA_HOME` rule, which takes
      an empty or relative value paths.rs ignores. Pass the directory from
      `sarab start`.
- [ ] `P3` `from code` Icons are never pruned, and an updated icon keeps its
      path, so caching launchers show the old one.
- [ ] `P3` `from code` **Notification details.** Log once when the daemon
      lacks `actions`; take the activation token and the action from one
      ordered stream; ignore action ids that are not ours.
- [ ] `P3` `from code` The APK handler's `Exec` path in install.sh is not
      escaped per the Desktop Entry spec.
- [ ] `P3` `open` `sarab info` does not say which host services (clipboard,
      notifications, location) are off, or why.

## Apps

- [ ] `P2` `open` **ARM translation.** Most apps people want are ARM-only.
      libhoudini and libndk_translation make them run on x86_64, but neither
      is redistributable. `sarab install` refuses an ARM-only APK, or one
      only partly built for x86_64, with a clear message; decide whether to
      document a way to add a translation layer, or to do something better.
- [ ] `P2` `unverified` **Clear Play's data after registration**, or say when
      to: the steps `sarab google-id` prints end in `sarab restart`, which
      has not been verified end to end on a fresh registration.
- [ ] `P2` `confirmed` **Uninstalling any app restarts Android's
      system_server.** The UID_REMOVED broadcast reaches
      NetworkStatsService, whose `deleteKernelTagData` walks a BPF map that
      is null (no bpffs rootless): a NullPointerException, and every open
      app goes with system_server. `sarab app rm` says so and waits it out.
      The fix is a null check in `service-connectivity.jar` (the tethering
      APEX), i.e. image work, or an overlay of that one jar.
- [x] `P2` `confirmed` **Dialogs from other packages are invisible in
      single-window mode.** Not so with the pinned vendor (2026-04-28): its
      composer reads `waydroid.active_apps` only to pick a mode, and
      single-window mode gives the topmost task one window with every layer
      in it, whatever the package. Checked 2026-09-27: Gallery's media
      permission prompt (permissioncontroller, in Gallery's task) drew in
      Gallery's window, as a test app's did on Ubuntu, and an activity of another
      package started as a task of its own got its own window. What was
      invisible was the closed mode (`none`, how Android boots): a
      notification's activity with no window up, fixed by `focus_app_for`,
      and Play Protect's host-install dialog, turned off by the policy.
- [ ] `P3` `confirmed` **Nothing puts the previous app back.** After a
      notification opens an activity of another package, `active_apps`
      stays on that package, so when the activity closes the screen is
      empty. The next launch from the desktop entry heals it; the right
      behaviour is to restore the previous app when the notification's task
      finishes, which needs a way to know it finished.

## Performance

- [ ] `P3` `open` **Partial reclaim.** Reclaim writes `memory.current` to
      `memory.reclaim` for the whole tree after ten minutes frozen, so the
      first launch afterwards spends about 2.5 s faulting pages back.
      Leaving system_server's working set in place would cut that.
- [ ] `P3` `confirmed` First boot after a package-state change costs about
      5 s in zygote. Find out why.
- [ ] `P3` `open` A perfetto/ftrace trace of a cold app start.
- [ ] `later` `open` SystemUI (116 MB) is the largest thing never shown, and
      it cannot be pm-disabled. Image work, or a platform-signed RRO, since
      the image is signed with AOSP's public test keys (the P1 item above).
- [ ] `later` `confirmed` The composer HAL's vsync thread free-runs at 60 Hz
      and causes most idle wakeups while Android is awake. A composer of our
      own would fix it.

## Windows

- [ ] `P3` `from code` **Windows cannot be resized.** The relay fixes each
      app window at the display's size, so the compositor holds it there
      rather than growing a frame around content that stays put (before the
      relay, confirmed: `wm size` and the task bounds never followed): the shipped
      composer reads its size once, at calibration. A real resize is image
      work; the published composer source does not match the shipped binary,
      so the next step is a matching source or a disassembly of the real
      configure handler.
- [ ] `P3` `partly done` **The display size is measured once**, when
      Android starts, from the shortest output. Opening an app after the
      screen changed restarts an idle Android to fit (`session::refit`), but
      a window already open keeps its size until it closes. Resizing it live
      means the relay rescaling the window and every input coordinate
      (pointer, touch, tablet, relative motion, text-input rectangles); do
      it together with drag-resize, if people ask for either. There is no
      way to choose the size (see property overrides above).
- [ ] `next` `confirmed` **No window frame on GNOME.** Seen 2026-09-27 on
      Ubuntu 24.04 with GNOME 46: an Android window has no title bar, so it
      cannot be dragged or closed with the mouse (Super+drag, Alt+F4 and
      Super+H work). The composer creates a bare `xdg_toplevel` and never
      asks for decorations. The fix belongs in the composer: request
      server-side decorations with `xdg-decoration` where the compositor
      offers it (KDE, sway), and draw its own with libdecor where it does not
      (GNOME offers none). The relay cannot do it properly: it would have to
      create objects on Android's connection, whose ids only Android's
      libwayland may hand out. Part of the vendor image and the composer below.
- [ ] `later` `open` Single-window mode composites one app at a time.
      Per-app windows are image work.
- [ ] `P3` `open` **Adaptive icons.** System apps ship vector icons, so the
      APK extractor finds no bitmap. They are hidden anyway
      (`NoDisplay=true`). A proper fix needs Android to render the icon,
      which means an image-side helper.

## Upstream

- [ ] `P3` `open` **rsbinder's servicemanager client is Android 16 only on
      Linux** (it calls `getService2`). We hand-roll
      `checkService`/`addService` against handle 0 instead. Worth a patch
      upstream.

## The long game

These change the image rather than the host side.

- [ ] `later` `open` **Build our own image** on top of LineageOS 20. After
      that we own the patch set: window resize that hotplugs the display, a
      better window model, and fewer services.
- [ ] `later` `open` **The desktop's own cursor.** The composer sends
      Android's pointer icon as a bitmap, so the cursor over an Android window
      is Android's arrow, not the desktop's theme; the relay only makes
      Hyprland show it at the size Android meant (a buffer scale in place of
      the viewport, which Hyprland ignores on cursors). A composer that maps Android's pointer types (arrow,
      hand, text, ...) to `wp_cursor_shape_v1` shapes lets the compositor draw
      the user's own cursor. Composer work, with the item below.
- [ ] `later` `open` **A Rust hwcomposer.** The composer HAL is AIDL
      (`composer3`) in Android 13 and AOSP supports Rust HALs, so a Rust
      Wayland bridge is feasible. It decides how native the windows feel.
- [ ] `later` `open` An audio HAL bridge to PipeWire. Still HIDL in Android
      13; Android 14 moves it to AIDL, which is when a Rust HAL becomes the
      obvious move.
