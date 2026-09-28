//! Desktop package policy: switch off the parts of the GAPPS image that have
//! no function when Android is a set of windows on a Linux desktop.
//!
//! What stays: Play Store, Play services, GSF, WebView, keyboard, contacts and
//! calendar providers -- anything an app may actually call into. What goes is
//! in PACKAGES with the reason. PackageManager persists the state in
//! /data/system/users/0/package-restrictions.xml, so it survives reboots and
//! `revert` is the exact inverse. Measured: 2036 -> 1654 MB PSS.
//!
//! Beyond packages:
//!  * home: org.sarab.home (apps/sarab-home), a code-less HOME activity, takes
//!    the HOME role so Launcher3 (73 MB) never starts. The APK is compiled into
//!    this binary, so the policy needs no file at run time. The
//!    role change must come from the shell uid -- RoleManager ignores it from
//!    uid 0 -- and with the role moved, Launcher3 can be disabled and SystemUI
//!    stops binding its recents service. Android 13's `cmd role` has no
//!    get-role-holders, so `status` falls back to `dumpsys role`.
//!  * nav: three-button navigation, so nothing needs a gesture-nav launcher.
//!  * cached: ActivityManager keeps up to 32 cached app processes; on a 31 GB
//!    box lmkd never trims them. 8 keeps recent apps warm and drops the rest.
//!    device_config sync is disabled so Play services cannot overwrite it.
//!  * verify: verifier_verify_adb_installs=0. `sarab install` runs `pm` as the
//!    shell uid, which Android treats as an adb install, and for an app it has
//!    not seen Play Protect then asks "Send app for a security check?" -- in
//!    a dialog nobody sees when no Android window is open (the composer's
//!    closed mode draws nothing, and a host install opens no window), so the
//!    install waited on it for ever. It also offers to upload the APK to
//!    Google, which is not a question to ask about a file somebody picked on
//!    their own machine -- client builds included. Play Store installs are still verified; this is
//!    the "verify apps over USB" switch, nothing wider. It is written through
//!    binder because the Settings provider refuses uid 0 (no calling package).
//!  * window: the composer's multi_windows property set to false through the
//!    property service, since the prop file alone does not settle it. The
//!    composer reads it at HAL start. `revert` leaves it alone: single window
//!    is the chosen window model, not a desktop policy tweak to undo.
//!
//! It used to be a script somebody had to know to run once per /data. It is
//! now applied by `sarab start` on the first boot of a /data, and the marker
//! that says so lives in that /data (a persist property) -- so a wiped /data
//! gets it again, and nothing on the host has to remember anything. The marker
//! holds `VERSION`, which is bumped whenever `apply` gains a step so an older
//! /data gets the new step on its next boot; `revert` writes "reverted" instead,
//! a decision rather than an absence, so the next boot does not quietly apply
//! it again. The marker is written only when every step that matters went
//! through: a home that could not be installed or given the role leaves it
//! unset, and `apply` ends with that error, so the next boot tries again
//! rather than leaving Launcher3 resident for good. A package that is not
//! there to disable (the image without Google apps has none of Google's) is a
//! "skip", not a failure. `first_boot` is never fatal (a runtime without the policy is a
//! heavier runtime, not a broken one), and since multi_windows and the
//! navigation overlay are read at start, the policy takes full effect only
//! from the next start.

use crate::android::{self, User};
use anyhow::{Context, Result, anyhow};
use sarab_runtime::settings;

const PACKAGES: &[(&str, &str)] = &[
    ("com.google.android.googlequicksearchbox", "Google app: hotword listener + voice interaction, 240 MB resident"),
    ("com.google.android.projection.gearhead", "Android Auto stub, three processes"),
    ("com.google.android.apps.messaging", "SMS/RCS client; there is no telephony"),
    ("com.google.android.ims", "carrier RCS services; no telephony"),
    ("com.android.smspush", "WAP push; no telephony"),
    ("com.google.android.setupwizard", "first-run wizard, already completed"),
    ("com.google.android.partnersetup", "OEM partner config, boot receiver only"),
    ("com.google.android.feedback", "crash-report uploader for Google apps"),
    ("com.stevesoltys.seedvault", "LineageOS backup transport; the host has backups"),
    ("org.calyxos.backup.contacts", "contacts backup for seedvault"),
    ("com.android.printspooler", "Android print stack; printing is the host's job"),
    ("com.google.android.tts", "speech synthesis engine, started by the Google app"),
];

const HOME: &str = "org.sarab.home";
const LAUNCHER3: &str = "com.android.launcher3";
const HOME_APK: &[u8] = include_bytes!("../../../apps/sarab-home.apk");
const THREE_BUTTON: &str = "com.android.internal.systemui.navbar.threebutton";
const VERIFY_ADB: &str = "verifier_verify_adb_installs";

const VERSION: &str = "2";
const MARKER: &str = "persist.sarab.policy";

fn root(argv: &[&str]) -> bool {
    android::output(User::Root, argv).is_ok_and(|o| o.status.success())
}

fn shell(argv: &[&str]) -> bool {
    android::output(User::Shell, argv).is_ok_and(|o| o.status.success())
}

fn line(what: &str, detail: &str) {
    println!("{what:<10}{detail}");
}

pub fn apply() -> Result<()> {
    for (p, _) in PACKAGES {
        line(if root(&["pm", "disable-user", "--user", "0", p]) { "disabled" } else { "skip" }, p);
    }
    let installed =
        android::run_with_input(User::Shell, &["pm", "install", "-S", &HOME_APK.len().to_string()], &mut &HOME_APK[..])
            .is_ok_and(|o| o.contains("Success"));
    let home = if !installed {
        Err(anyhow!("could not install {HOME}"))
    } else if !shell(&["cmd", "role", "add-role-holder", "--user", "0", "android.app.role.HOME", HOME]) {
        Err(anyhow!("could not give {HOME} the HOME role; Launcher3 stays"))
    } else {
        line("home", HOME);
        if root(&["pm", "disable-user", "--user", "0", LAUNCHER3]) {
            line("disabled", LAUNCHER3);
        }
        Ok(())
    };
    if root(&["cmd", "overlay", "enable", "--user", "0", THREE_BUTTON]) {
        line("nav", "three-button");
    }
    root(&["device_config", "set_sync_disabled_for_tests", "persistent"]);
    root(&["device_config", "put", "activity_manager", "max_cached_processes", "8"]);
    root(&["device_config", "put", "activity_manager", "max_phantom_processes", "8"]);
    if root(&[
        "device_config",
        "put",
        "activity_manager",
        "no_kill_cached_processes_post_boot_completed_duration_millis",
        "0",
    ]) {
        line("cached", "max 8 processes, no post-boot grace");
    }
    let p = crate::app::connect()?;
    p.settings_put_string(settings::GLOBAL, VERIFY_ADB, "0")?;
    line("verify", "host installs are not sent to Play Protect");
    p.setprop("persist.waydroid.multi_windows", "false")?;
    line("window", "single window");
    home.context("the desktop policy is not complete; the next start tries again")?;
    p.setprop(MARKER, VERSION)?;
    Ok(())
}

pub fn revert() -> Result<()> {
    for (p, _) in PACKAGES {
        line(if root(&["pm", "enable", "--user", "0", p]) { "enabled" } else { "skip" }, p);
    }
    if root(&["pm", "enable", "--user", "0", LAUNCHER3]) {
        line("enabled", LAUNCHER3);
    }
    if shell(&["cmd", "role", "add-role-holder", "--user", "0", "android.app.role.HOME", LAUNCHER3]) {
        line("home", LAUNCHER3);
    }
    if root(&["cmd", "overlay", "disable", "--user", "0", THREE_BUTTON]) {
        line("nav", "default");
    }
    root(&["device_config", "delete", "activity_manager", "max_cached_processes"]);
    root(&["device_config", "delete", "activity_manager", "max_phantom_processes"]);
    if root(&["device_config", "set_sync_disabled_for_tests", "none"]) {
        line("cached", "default");
    }
    let p = crate::app::connect()?;
    p.settings_put_string(settings::GLOBAL, VERIFY_ADB, "1")?;
    line("verify", "host installs go through Play Protect (it may ask, in a dialog you cannot see)");
    p.setprop(MARKER, "reverted")?;
    Ok(())
}

pub fn status() -> Result<()> {
    let disabled = android::run(User::Root, &["pm", "list", "packages", "-d", "--user", "0"])?;
    let is_disabled = |p: &str| disabled.lines().any(|l| l.trim().strip_prefix("package:") == Some(p));
    for (p, why) in PACKAGES {
        println!("{:<10}{p:<42} {why}", if is_disabled(p) { "disabled" } else { "enabled" });
    }
    println!("{:<10}{LAUNCHER3}", if is_disabled(LAUNCHER3) { "disabled" } else { "enabled" });
    let holders =
        android::run(User::Shell, &["cmd", "role", "get-role-holders", "--user", "0", "android.app.role.HOME"])
            .ok()
            .or_else(|| {
                let d = android::run(User::Root, &["dumpsys", "role"]).ok()?;
                let at = d.find("name=android.app.role.HOME")?;
                d[at..].lines().take(3).find_map(|l| l.trim().strip_prefix("holders=").map(str::to_string))
            })
            .unwrap_or_else(|| "?".into());
    println!("{:<10}{}", "home", holders.trim());
    let cached = android::run(User::Root, &["device_config", "get", "activity_manager", "max_cached_processes"])?;
    println!("{:<10}max_cached_processes={}", "cached", cached.trim());
    let p = crate::app::connect()?;
    println!("{:<10}{VERIFY_ADB}={}", "verify", p.settings_get_string(settings::GLOBAL, VERIFY_ADB)?);
    println!("{:<10}multi_windows={}", "window", p.getprop("persist.waydroid.multi_windows", "")?);
    println!("{:<10}{MARKER}={}", "applied", p.getprop(MARKER, "")?);
    Ok(())
}

#[derive(Debug, PartialEq)]
enum FirstBoot {
    Apply,
    Nothing,
}

fn first_boot_action(marker: &str) -> FirstBoot {
    if marker == VERSION || marker == "reverted" { FirstBoot::Nothing } else { FirstBoot::Apply }
}

pub fn first_boot() {
    let marker = match crate::app::connect().and_then(|p| p.getprop(MARKER, "")) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("policy: cannot read {MARKER} ({e:#}); not applied");
            return;
        }
    };
    if first_boot_action(&marker) == FirstBoot::Nothing {
        return;
    }
    println!("first boot of this /data: applying the desktop policy (sarab policy status)");
    if let Err(e) = apply() {
        eprintln!("policy: {e:#}");
        return;
    }
    println!("policy applied; it takes full effect from the next start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_boot_applies_once_and_respects_a_revert() {
        assert_eq!(first_boot_action(""), FirstBoot::Apply);
        assert_eq!(first_boot_action("0"), FirstBoot::Apply);
        assert_eq!(first_boot_action("1"), FirstBoot::Apply);
        assert_eq!(first_boot_action(VERSION), FirstBoot::Nothing);
        assert_eq!(first_boot_action("reverted"), FirstBoot::Nothing);
    }

    #[test]
    fn the_embedded_home_apk_is_an_apk() {
        assert_eq!(&HOME_APK[..4], b"PK\x03\x04");
        let z = zip::ZipArchive::new(std::io::Cursor::new(HOME_APK)).unwrap();
        assert!(z.file_names().any(|n| n == "AndroidManifest.xml"));
    }

    #[test]
    fn nothing_the_apps_need_is_on_the_list() {
        for keep in [
            "com.android.vending",
            "com.google.android.gms",
            "com.google.android.gsf",
            "com.android.webview",
            "com.google.android.webview",
        ] {
            assert!(!PACKAGES.iter().any(|(p, _)| *p == keep), "{keep} must never be disabled");
        }
    }
}
