//! `sarab app ...`: the apps inside Android, from the host.
//!
//! Launching, listing and settings go over binder (IPlatform, sarab-runtime).
//! Installing and removing go through `pm` as the shell uid instead:
//! IPlatform's installApp takes a path *inside* Android, so the APK had to be
//! copied into a bind-mounted directory first, and its removeApp is broken on
//! this image (a PendingIntent without FLAG_IMMUTABLE, which Android 13
//! rejects). `pm` reads the APK from stdin, takes split bundles, and says *why*
//! an install failed -- which for most apps people want is "this is ARM code",
//! and that message is the difference between a bug report and an answer. `pm`
//! also exits 0 on some failures, so `check_pm` decides by the "Success" text,
//! not the exit status. A bundle holding only ARM native splits is refused up
//! front in `choose_splits`: installing its base alone would "succeed" and then
//! crash the first time the app loads its libraries. A single APK gets the same
//! check per library in `native_libs`, which compares the libraries under
//! `lib/<abi>/`: the ARM set, arm64 first as the most complete one (a library
//! only the old 32-bit ARM build carries is not held against it), against the
//! x86 ABI Android would pick for this APK. Android accepts one with *some*
//! x86_64 libraries and runs it as x86_64, so an APK built for ARM whose
//! plugins brought x86_64 copies of their own (a Flutter app built for arm64
//! alone is the common case) installs fine and dies on start without its
//! engine. Split installs use the same `install-create` / `install-write` /
//! `install-commit` session protocol as adb's `install-multiple`, fed straight
//! from the zip, each split written under its file name. A bundle's APKs are
//! its top-level ones, or those in `splits/`, where bundletool's `build-apks`
//! puts them (its `standalones/` are whole APKs for old devices and never mix
//! with splits). `split_abi` reads the ABI off both namings, Play's
//! `split_config.arm64_v8a.apk` and bundletool's `base-arm64_v8a.apk`; with an
//! x86_64 split present the 32-bit x86 one is dropped too, as Play would.
//!
//! Every uninstall (`rm`) restarts Android's system_server: the UID_REMOVED
//! broadcast reaches NetworkStatsService, which walks its BPF cookie-tag map --
//! null here, because bpffs cannot be mounted without root -- and the
//! NullPointerException takes system_server down (docs/TODO.md). Not fixable
//! from the host, so `rm` says so and waits for the restart to finish. Whether
//! it happened is asked, not guessed at from a time window: `cmd activity
//! wait-for-broadcast-idle` (hidden from `am help`, there since Android 11)
//! returns once every queued broadcast, UID_REMOVED included, has been
//! delivered, and fails if system_server dies delivering it; a changed
//! `sys.system_server.start_count` counts too. Waiting for the new
//! system_server has no such call, since nothing tells the host that Android's
//! services are back, so that part looks, like the boot wait does: the start
//! count, which init's property service answers even while system_server is
//! down, and a fresh `service check` each time, since our own binder handles
//! would still point at the dead process. init restarts zygote at most once per
//! 5 s, so a second uninstall in quick succession waits several seconds.
//!
//! `lazy` starts Android when a command implies it (a launcher click, a
//! double-clicked APK); `SARAB_NO_LAZY_START=1` restores the hard failure.
//! The commands that open a window (`launch`, `show`, `intent`) first let
//! `session::refit` restart an idle Android whose display no longer fits the
//! screen. With
//! no terminal on stderr, `install` reports through a desktop notification,
//! and `launch` and `intent` do when they fail (`notify_failure`): they are what
//! launcher entries run, and a launcher throws stderr away, so a failed click
//! used to do nothing at all. `launch` checks the app exists first: the
//! platform service's launch reports nothing for a package that is not there,
//! so `sarab app launch` exited 0 and had already made the missing package the
//! active app. `intent` needs no such check: the service answers with the
//! package that took the intent, and with nothing when none did, which is now
//! an error rather than a silent exit 0. It leaves `waydroid.active_apps`
//! alone; the activity it starts gets a window of its own even when it
//! belongs to another package (checked 2026-09-27 with App Settings and a
//! browser URL, both over a running Calculator).

use crate::android::{self, User};
use anyhow::{Context, Result, anyhow, bail};
use sarab_runtime::{Platform, find_binder, settings};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn connect() -> Result<Platform> {
    let dev = find_binder().map_err(|_| anyhow!("Android is not running (sarab start)"))?;
    sarab_runtime::thaw()?;
    Platform::connect(&dev)
}

pub fn lazy(dirs: &crate::paths::Dirs) -> Result<Platform> {
    if std::env::var_os("SARAB_NO_LAZY_START").is_some_and(|v| !v.is_empty() && v != "0") {
        return connect();
    }
    let dev = crate::session::ensure_running(dirs, &|s| eprintln!("{s}"))?;
    Platform::connect(&dev)
}

fn packages(third_party_only: bool) -> Result<Vec<String>> {
    let mut argv = vec!["pm", "list", "packages"];
    if third_party_only {
        argv.push("-3");
    }
    let out = android::run(User::Shell, &argv)?;
    Ok(parse_packages(&out))
}

fn parse_packages(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_string).collect()
}

pub fn ls(json: bool, quiet: bool) -> Result<()> {
    let p = connect()?;
    let mut apps = p.apps()?;
    apps.sort_by_key(|a| a.name.to_lowercase());
    let user: Vec<String> = packages(true)?;
    if quiet {
        for a in &apps {
            println!("{}", a.package_name);
        }
    } else if json {
        let v: Vec<_> = apps
            .iter()
            .map(|a| serde_json::json!({ "package": a.package_name, "name": a.name, "system": !user.contains(&a.package_name) }))
            .collect();
        println!("{}", serde_json::Value::Array(v));
    } else {
        let w = apps.iter().map(|a| a.package_name.len()).max().unwrap_or(0).max("PACKAGE".len());
        println!("{:<w$}  {:<7}  NAME", "PACKAGE", "SOURCE");
        for a in &apps {
            let src = if user.contains(&a.package_name) { "user" } else { "system" };
            println!("{:<w$}  {src:<7}  {}", a.package_name, a.name);
        }
    }
    Ok(())
}

enum Payload {
    Single { size: u64 },
    Bundle { splits: Vec<(String, u64)> },
}

fn split_abi(split: &str) -> &str {
    let file = split.rsplit('/').next().unwrap_or(split);
    let stem = file.strip_suffix(".apk").unwrap_or(file);
    stem.rsplit(['.', '-']).next().unwrap_or(stem)
}

fn foreign_abi(split: &str) -> bool {
    matches!(split_abi(split), "arm64_v8a" | "armeabi_v7a" | "armeabi" | "mips" | "mips64")
}

fn native_abi(split: &str) -> bool {
    matches!(split_abi(split), "x86_64" | "x86")
}

fn bundle_entry(name: &str) -> bool {
    let rel = name.strip_prefix("splits/").unwrap_or(name);
    name.ends_with(".apk") && !rel.contains('/')
}

fn choose_splits(entries: &[(String, u64)]) -> Result<Vec<(String, u64)>> {
    let names = || entries.iter().map(|(n, _)| n.as_str());
    let has_foreign = names().any(foreign_abi);
    let has_native = names().any(native_abi);
    if has_foreign && !has_native {
        bail!("{}", arm_only());
    }
    let has_64 = names().any(|n| split_abi(n) == "x86_64");
    Ok(entries.iter().filter(|(n, _)| !foreign_abi(n) && !(has_64 && split_abi(n) == "x86")).cloned().collect())
}

enum Native {
    Fine,
    ArmOnly,
    Missing { abi: &'static str, libs: Vec<String> },
}

fn native_libs<'a>(names: impl IntoIterator<Item = &'a str>) -> Native {
    let mut by_abi: std::collections::BTreeMap<&str, std::collections::BTreeSet<&str>> = Default::default();
    for n in names {
        if let Some((abi, file)) = n.strip_prefix("lib/").and_then(|r| r.split_once('/'))
            && file.ends_with(".so")
            && !file.contains('/')
        {
            by_abi.entry(abi).or_default().insert(file);
        }
    }
    let Some(arm) = ["arm64-v8a", "armeabi-v7a", "armeabi"].iter().find_map(|a| by_abi.get(a)) else {
        return Native::Fine;
    };
    let Some((abi, ours)) = ["x86_64", "x86"].into_iter().find_map(|a| by_abi.get(a).map(|l| (a, l))) else {
        return Native::ArmOnly;
    };
    let libs: Vec<String> = arm.difference(ours).map(|l| l.to_string()).collect();
    if libs.is_empty() { Native::Fine } else { Native::Missing { abi, libs } }
}

fn partly_arm(abi: &str, libs: &[String]) -> String {
    let flutter = if libs.iter().any(|l| l == "libflutter.so") {
        " (for Flutter: flutter build apk --target-platform android-arm64,android-x64)"
    } else {
        ""
    };
    format!(
        "this APK is only partly built for {abi}: it has ARM builds of {} and no {abi} ones, so the app \
         would crash as it starts. Install a build that includes {abi}{flutter}",
        libs.join(", ")
    )
}

fn arm_only() -> &'static str {
    "this app is built for ARM processors only, and Sarab cannot run ARM code yet \
     (there is no translation layer)"
}

fn inspect_file(path: &Path) -> Result<Payload> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let size = f.metadata()?.len();
    let mut z = zip::ZipArchive::new(f).map_err(|_| anyhow!("{} is not an APK (not a zip file)", file_name(path)))?;
    if z.by_name("AndroidManifest.xml").is_ok() {
        return match native_libs(z.file_names()) {
            Native::Fine => Ok(Payload::Single { size }),
            Native::ArmOnly => bail!("{}", arm_only()),
            Native::Missing { abi, libs } => bail!("{}", partly_arm(abi, &libs)),
        };
    }
    let mut entries = Vec::new();
    for i in 0..z.len() {
        let e = z.by_index(i)?;
        let name = e.name().to_string();
        if bundle_entry(&name) {
            entries.push((name, e.size()));
        }
    }
    if entries.is_empty() {
        bail!(
            "{} has neither an AndroidManifest.xml nor any APKs inside (encrypted .apkm files are not supported)",
            path.display()
        );
    }
    Ok(Payload::Bundle { splits: choose_splits(&entries)? })
}

fn explain_failure(pm: &str) -> String {
    let code = pm
        .split(|c: char| c == '[' || c == ']' || c == ':' || c.is_whitespace())
        .find(|w| w.starts_with("INSTALL_"))
        .unwrap_or("");
    let why = match code {
        "INSTALL_FAILED_NO_MATCHING_ABIS" => arm_only(),
        "INSTALL_FAILED_UPDATE_INCOMPATIBLE" => {
            "a copy of this app signed by someone else is already installed; remove it first (sarab app rm)"
        }
        "INSTALL_FAILED_VERSION_DOWNGRADE" => "a newer version of this app is already installed",
        "INSTALL_FAILED_OLDER_SDK" => "this app needs a newer Android than the one Sarab runs (13)",
        "INSTALL_FAILED_MISSING_SPLIT" => {
            "this app comes in several pieces; install the whole bundle (.apks/.xapk) rather than one APK from it"
        }
        "INSTALL_FAILED_INSUFFICIENT_STORAGE" => "not enough space on the disk that holds Android's data (sarab info)",
        c if c.starts_with("INSTALL_PARSE_FAILED") || c == "INSTALL_FAILED_INVALID_APK" => {
            "the file is not a valid APK"
        }
        _ => "",
    };
    let raw = pm.trim();
    if why.is_empty() { raw.to_string() } else { format!("{why} ({code})") }
}

fn check_pm(out: &str) -> Result<()> {
    if out.lines().map(str::trim).any(|l| l == "Success" || l.starts_with("Success:")) {
        Ok(())
    } else {
        bail!("{}", explain_failure(out))
    }
}

pub fn install(dirs: &crate::paths::Dirs, path: &Path) -> Result<()> {
    let result = install_inner(dirs, path);
    if !std::io::stderr().is_terminal() {
        let (title, body) = match &result {
            Ok(msg) => ("App installed".to_string(), msg.clone()),
            Err(e) => (format!("Could not install {}", file_name(path)), format!("{e:#}")),
        };
        let _ = std::process::Command::new("notify-send").args(["-a", "Sarab", &title, &body]).status();
    }
    println!("{}", result?);
    Ok(())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn install_inner(dirs: &crate::paths::Dirs, path: &Path) -> Result<String> {
    let payload = inspect_file(path)?;
    let p = lazy(dirs)?;
    let before = packages(false)?;
    eprintln!("installing {} ...", file_name(path));
    match payload {
        Payload::Single { size } => {
            let mut f = std::fs::File::open(path)?;
            let out = android::run_with_input(User::Shell, &["pm", "install", "-S", &size.to_string()], &mut f)
                .map_err(|e| anyhow!("{}", explain_failure(&e.to_string())))?;
            check_pm(&out)?;
        }
        Payload::Bundle { splits } => install_bundle(path, &splits)?,
    }
    let after = packages(false)?;
    Ok(match after.iter().find(|pkg| !before.contains(pkg)) {
        Some(pkg) => {
            let name = p.app_name(pkg).unwrap_or_default();
            if name.is_empty() || name == *pkg {
                format!("installed {pkg}")
            } else {
                format!("installed {name} ({pkg})")
            }
        }
        None => format!("updated the installed app from {}", file_name(path)),
    })
}

fn install_bundle(path: &Path, splits: &[(String, u64)]) -> Result<()> {
    let total: u64 = splits.iter().map(|(_, s)| s).sum();
    let out = android::run(User::Shell, &["pm", "install-create", "-S", &total.to_string()])?;
    let session = out
        .split(['[', ']'])
        .nth(1)
        .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| anyhow!("pm install-create: {}", out.trim()))?
        .to_string();
    let written = (|| -> Result<()> {
        let mut z = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        for (name, size) in splits {
            let mut entry = z.by_name(name)?;
            let file = name.rsplit('/').next().unwrap_or(name);
            let out = android::run_with_input(
                User::Shell,
                &["pm", "install-write", "-S", &size.to_string(), &session, file, "-"],
                &mut entry as &mut dyn Read,
            )?;
            check_pm(&out).with_context(|| format!("writing {name}"))?;
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = android::run(User::Shell, &["pm", "install-abandon", &session]);
        return Err(e);
    }
    let out = android::output(User::Shell, &["pm", "install-commit", &session])?;
    check_pm(&(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)))
}

fn system_server_starts() -> Option<u64> {
    android::run(User::Root, &["getprop", "sys.system_server.start_count"]).ok()?.trim().parse().ok()
}

fn services_up() -> bool {
    ["waydroidplatform", "package"]
        .iter()
        .all(|s| android::run(User::Root, &["service", "check", s]).is_ok_and(|o| o.contains(": found")))
}

pub fn rm(pkg: &str) -> Result<()> {
    connect()?;
    if !packages(false)?.iter().any(|p| p == pkg) {
        bail!("{pkg} is not installed");
    }
    if !packages(true)?.iter().any(|p| p == pkg) {
        bail!("{pkg} is part of the Android image and cannot be removed (`sarab policy` can switch it off)");
    }
    let before = system_server_starts();
    let out = android::run(User::Shell, &["pm", "uninstall", pkg])?;
    check_pm(&out)?;
    println!("removed {pkg}");
    let delivered =
        android::output(User::Shell, &["cmd", "activity", "wait-for-broadcast-idle"]).is_ok_and(|o| o.status.success());
    if !delivered || system_server_starts() != before {
        eprintln!(
            "note: Android restarts its system services after an uninstall (a known issue); open Android apps were closed"
        );
        let t0 = Instant::now();
        while !(services_up() && system_server_starts() != before) && t0.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}

fn notify_failure(title: &str, result: Result<()>) -> Result<()> {
    if let Err(e) = &result
        && !std::io::stderr().is_terminal()
    {
        let _ = std::process::Command::new("notify-send").args(["-a", "Sarab", title, &format!("{e:#}")]).status();
    }
    result
}

pub fn launch(dirs: &crate::paths::Dirs, pkg: &str) -> Result<()> {
    notify_failure(&format!("Could not open {pkg}"), launch_inner(dirs, pkg))
}

fn launch_inner(dirs: &crate::paths::Dirs, pkg: &str) -> Result<()> {
    crate::session::refit(dirs, &|s| eprintln!("{s}"))?;
    let p = lazy(dirs)?;
    if p.app(pkg)?.is_none() {
        bail!("{pkg}: not installed, or has no launcher activity");
    }
    p.settings_put_string(settings::GLOBAL, "policy_control", "immersive.full=*")?;
    p.setprop("waydroid.active_apps", pkg)?;
    p.launch(pkg)
}

pub fn show(dirs: &crate::paths::Dirs) -> Result<()> {
    crate::session::refit(dirs, &|s| eprintln!("{s}"))?;
    let p = lazy(dirs)?;
    p.settings_put_string(settings::GLOBAL, "policy_control", "")?;
    p.setprop("waydroid.active_apps", "Waydroid")
}

pub fn intent(dirs: &crate::paths::Dirs, action: &str, uri: &str) -> Result<()> {
    notify_failure(&format!("Could not open {uri}"), intent_inner(dirs, action, uri))
}

fn intent_inner(dirs: &crate::paths::Dirs, action: &str, uri: &str) -> Result<()> {
    crate::session::refit(dirs, &|s| eprintln!("{s}"))?;
    let pkg = lazy(dirs)?.launch_intent(action, uri)?;
    if pkg.is_empty() {
        bail!("nothing in Android opens {uri} ({action})");
    }
    println!("{pkg}");
    Ok(())
}

pub fn inspect(pkg: &str, json: bool) -> Result<()> {
    let p = connect()?;
    let app = p.app(pkg)?.ok_or_else(|| anyhow!("{pkg}: not installed, or has no launcher activity"))?;
    let dump = android::run(User::Shell, &["dumpsys", "package", pkg])?;
    let field = |k: &str| {
        dump.lines()
            .find_map(|l| l.trim().strip_prefix(k).map(|v| v.split_whitespace().next().unwrap_or("").to_string()))
    };
    let version = field("versionName=");
    let code = field("versionCode=");
    let abi = field("primaryCpuAbi=").filter(|a| a != "null");
    let user = packages(true)?.contains(&pkg.to_string());
    let paths: Vec<String> = parse_packages(&android::run(User::Shell, &["pm", "path", pkg])?);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "package": app.package_name, "name": app.name, "system": !user,
                "version": version, "version_code": code, "abi": abi,
                "activity": format!("{}/{}", app.component_package_name, app.component_class_name),
                "categories": app.categories, "apks": paths,
            })
        );
        return Ok(());
    }
    let row = |k: &str, v: &str| println!("{k:<10} {v}");
    row("package", &app.package_name);
    row("name", &app.name);
    row("source", if user { "user" } else { "system" });
    row("version", &format!("{} ({})", version.as_deref().unwrap_or("?"), code.as_deref().unwrap_or("?")));
    row("abi", abi.as_deref().unwrap_or("none (no native code)"));
    row("activity", &format!("{}/{}", app.component_package_name, app.component_class_name));
    for (i, p) in paths.iter().enumerate() {
        row(if i == 0 { "apk" } else { "" }, p);
    }
    Ok(())
}

pub fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).with_context(|| format!("{}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(names: &[&str]) -> Vec<(String, u64)> {
        names.iter().map(|n| (n.to_string(), 1)).collect()
    }

    #[test]
    fn a_bundle_keeps_our_abi_and_drops_arm() {
        let s = choose_splits(&e(&[
            "base.apk",
            "split_config.arm64_v8a.apk",
            "split_config.x86_64.apk",
            "split_config.en.apk",
            "split_config.xxhdpi.apk",
        ]))
        .unwrap();
        let names: Vec<&str> = s.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["base.apk", "split_config.x86_64.apk", "split_config.en.apk", "split_config.xxhdpi.apk"]);
    }

    #[test]
    fn a_bundletool_set_keeps_its_splits_and_our_64_bit_abi() {
        let names = [
            "toc.pb",
            "splits/base-master.apk",
            "splits/base-arm64_v8a.apk",
            "splits/base-armeabi_v7a.apk",
            "splits/base-x86.apk",
            "splits/base-x86_64.apk",
            "splits/base-xxhdpi.apk",
            "splits/base-en.apk",
            "standalones/standalone-x86_64_xxhdpi.apk",
            "asset-slices/pack-master.apk",
        ];
        let entries: Vec<&str> = names.into_iter().filter(|n| bundle_entry(n)).collect();
        let s = choose_splits(&e(&entries)).unwrap();
        let kept: Vec<&str> = s.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            kept,
            ["splits/base-master.apk", "splits/base-x86_64.apk", "splits/base-xxhdpi.apk", "splits/base-en.apk"]
        );
        assert!(choose_splits(&e(&["splits/base-master.apk", "splits/base-arm64_v8a.apk"])).is_err());
        let s = choose_splits(&e(&["base.apk", "split_config.x86.apk", "split_config.armeabi_v7a.apk"])).unwrap();
        assert_eq!(s.len(), 2, "a 32-bit x86 split stays when it is the only one");
        assert_eq!(split_abi("split_config.arm64_v8a.apk"), "arm64_v8a");
        assert_eq!(split_abi("splits/feature-x86_64.apk"), "x86_64");
        assert!(!native_abi("splits/base-xxhdpi.apk") && !foreign_abi("base.apk"));
    }

    #[test]
    fn an_arm_only_bundle_is_refused_up_front() {
        let err =
            choose_splits(&e(&["base.apk", "split_config.arm64_v8a.apk", "split_config.armeabi_v7a.apk"])).unwrap_err();
        assert!(err.to_string().contains("ARM"), "{err}");
        assert_eq!(choose_splits(&e(&["base.apk", "split_config.en.apk"])).unwrap().len(), 2);
    }

    fn libs(names: &[&str]) -> Native {
        native_libs(names.iter().copied())
    }

    #[test]
    fn an_apk_missing_x86_64_builds_of_arm_libraries_is_refused() {
        let Native::Missing { abi, libs } = libs(&[
            "classes.dex",
            "lib/arm64-v8a/libapp.so",
            "lib/arm64-v8a/libflutter.so",
            "lib/arm64-v8a/libsentry.so",
            "lib/armeabi-v7a/libsentry.so",
            "lib/x86_64/libsentry.so",
        ]) else {
            panic!("not refused")
        };
        assert_eq!((abi, libs.as_slice()), ("x86_64", ["libapp.so".to_string(), "libflutter.so".into()].as_slice()));
        assert!(partly_arm(abi, &libs).contains("android-x64"));
    }

    #[test]
    fn complete_or_library_free_apks_pass_and_arm_only_ones_do_not() {
        assert!(matches!(libs(&["classes.dex", "res/x.png"]), Native::Fine));
        assert!(matches!(
            libs(&["lib/arm64-v8a/liba.so", "lib/x86_64/liba.so", "lib/x86_64/libextra.so"]),
            Native::Fine
        ));
        assert!(matches!(libs(&["lib/armeabi-v7a/liba.so", "lib/x86/liba.so"]), Native::Fine));
        assert!(matches!(
            libs(&["lib/arm64-v8a/liba.so", "lib/armeabi-v7a/libold.so", "lib/x86_64/liba.so"]),
            Native::Fine
        ));
        assert!(matches!(libs(&["lib/arm64-v8a/liba.so", "lib/armeabi-v7a/liba.so"]), Native::ArmOnly));
    }

    #[test]
    fn pm_failures_come_back_in_words_with_the_code() {
        let m =
            explain_failure("Failure [INSTALL_FAILED_NO_MATCHING_ABIS: Failed to extract native libraries, res=-113]");
        assert!(m.contains("ARM") && m.ends_with("(INSTALL_FAILED_NO_MATCHING_ABIS)"), "{m}");
        let m = explain_failure("Failure [INSTALL_PARSE_FAILED_NOT_APK: Failed to parse /data/app/vmdl.tmp]");
        assert!(m.starts_with("the file is not a valid APK"), "{m}");
        assert!(explain_failure("Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: ...]").contains("sarab app rm"));
        assert_eq!(explain_failure("Failure [INSTALL_FAILED_WEIRD]\n"), "Failure [INSTALL_FAILED_WEIRD]");
    }

    #[test]
    fn success_is_the_word_not_the_exit_status() {
        assert!(check_pm("Success\n").is_ok());
        assert!(check_pm("Performing Streamed Install\nSuccess\n").is_ok());
        assert!(check_pm("Success: streamed 8538 bytes\n").is_ok());
        assert!(check_pm("Success: created install session [42]").is_ok());
        assert!(check_pm("Successful? no: Failure [X]").is_err());
        assert!(check_pm("Failure [INSTALL_FAILED_OLDER_SDK]").is_err());
        assert!(check_pm("").is_err());
    }

    #[test]
    fn package_lines_parse() {
        assert_eq!(parse_packages("package:com.a\npackage:org.b\n\nnoise\n"), ["com.a", "org.b"]);
    }
}
