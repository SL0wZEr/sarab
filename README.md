# Sarab

Run Android apps on your Linux desktop, each in its own window. No root, no
container, no virtual machine.

> **Alpha.** Built and used daily on x86_64 Arch Linux with Hyprland and an
> AMD GPU, and tested on Ubuntu 24.04 with GNOME. Other setups are untested.
> Please report what you find.

## Install

You need:

- Linux on x86_64, with a Wayland desktop
- A GPU that Mesa drives: AMD, Intel, or NVIDIA with nouveau (NVIDIA's proprietary driver does not work)
- Rust 1.88 or newer, to build
- For apps that ask where you are: GeoClue (see [Location](#location))

```sh
git clone https://github.com/SL0wZEr/sarab && cd sarab
packaging/install.sh --prefix ~/.local
sarab start
```

The first `sarab start` asks before it downloads Android (1.4 GB, about 5 GB
on disk). If your system needs a change first, it prints the exact command to
run. `sarab info` checks everything and tells you what is missing.

## Use

```sh
sarab install app.apk     # or double-click the .apk
```

Installed apps appear in your app launcher. Clicking one starts Android if it
is not running. A minute after the last app window closes, Android pauses and
uses no CPU until you open an app again.

- **Start Android at login:** `systemctl --user enable --now sarab`
- **Play Store:** run `sarab google-id` once and register the ID it prints.

## Commands

```
sarab start | stop | restart      start or stop Android
sarab status                      running, paused or stopped
sarab install app.apk             install an app
sarab app ls                      list installed apps
sarab app launch <package>        open an app
sarab app rm <package>            remove an app
sarab exec [cmd]                  run a command, or open a shell, inside Android
sarab logs [-f]                   show Android's log
sarab stats                       memory and CPU use
sarab info                        versions and system checks (include it in bug reports)
sarab upgrade                     move to the Android image this version is tested with
sarab purge                       delete Android and its data
```

Every command has `--help`.

## Update

Pull, then run `packaging/install.sh --prefix ~/.local` again. If the new
version expects a newer Android image, it tells you, and `sarab upgrade`
installs it. Your apps and data are kept.

## Uninstall

```sh
packaging/install.sh uninstall --prefix ~/.local           # keeps Android and its data
packaging/install.sh uninstall --prefix ~/.local --purge   # deletes them too
```

## Location

Apps get your computer's location, from GeoClue, the desktop's location
service. Install it (`geoclue` on Arch, `geoclue-2.0` on Debian and Ubuntu,
`geoclue2` on Fedora). GeoClue answers only while an agent allows it: GNOME
has one built in, and its location switch turns it on and off. On other
desktops GeoClue's demo agent does it, and most packages start it at login.
Android asks only while an app wants your location, and only apps you gave
the location permission get it.

How good the location is depends on what GeoClue can use, and apps are told
the real accuracy. Your IP address alone is only good to the nearest few
kilometres, and apps that ask Google Play services for a precise location
(delivery and ride apps) ignore that. `sarab logs --hostd` shows how
accurate the last one was. To do better:

- **A laptop with Wi-Fi** is located from the networks around it, where
  GeoClue's service, [BeaconDB](https://beacondb.net), knows them.
- **A computer that stays put** can be given its location once, in
  `/etc/geolocation`: latitude, longitude, altitude, and how far off it may
  be, in metres, one per line.

  ```
  printf '%s\n' 51.5007 -0.1246 0 30 | sudo tee /etc/geolocation
  ```

  Remember it is there if the computer moves.
- **A GPS receiver**, or a phone sharing its GPS on your network, is read by
  GeoClue itself.

## Limitations

- **x86 apps only.** Apps built only for ARM phones will not install.
- **One app on screen at a time.**
- **No audio or video playback** inside apps yet.
- **No title bar on GNOME.** Move a window with Super+drag, close it with
  Alt+F4. KDE is untested.
- **Removing an app takes a few seconds** while Android restarts some of its
  services.
- **Tiling window managers.** Hyprland and sway float app windows on their
  own. Others may need a float rule for app ids starting with `waydroid.`.
- **Location is the computer's, not a GPS fix.** See [Location](#location).

The full list is in [docs/TODO.md](docs/TODO.md).

## Security

Nothing in Sarab runs as root. Android runs as your user, walled off with
Linux namespaces, but it can reach your display and audio. It is not a
sandbox: install only apps you trust. The Android image is signed with
publicly known test keys, so a malicious app can make itself part of
Android's system and take full control of Android. See
[SECURITY.md](SECURITY.md).

## How it works

Sarab runs an unmodified LineageOS 20 (Android 13) image as ordinary
processes under your user. A small host side, written in Rust, connects it to
your desktop: windows, clipboard, notifications, launcher entries, dark
mode and location. See [docs/OVERVIEW.md](docs/OVERVIEW.md).

## Credits

The Android image is the LineageOS 20 build published by the Waydroid
project. Sarab is not affiliated with it. Binder support comes from
[rsbinder](https://github.com/hiking90/rsbinder).

## License

Apache-2.0, see [LICENSE](LICENSE). `overlay/system/etc/ueventd.rc` is
derived from the Android Open Source Project's file of the same name, also
Apache-2.0, and its header says what changed. The Android image Sarab
downloads is not part of this repository and comes under its own licenses.
