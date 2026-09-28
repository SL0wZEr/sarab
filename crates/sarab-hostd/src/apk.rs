//! Pull a launcher icon out of an installed app's APK.
//!
//! Android's /data is mode 0771 under mapped uids, so the host cannot list
//! it. We re-exec ourselves as a single-threaded helper that joins the
//! runtime's user and mount namespaces (the same move `nsenter -U -m`
//! makes), reads the APK there, and writes the PNG to stdout. `setns` with
//! CLONE_NEWUSER refuses a multi-threaded caller, which is why this is a
//! fresh process rather than a fork, and why it is `/proc/self/exe`: that
//! still runs this binary after an upgrade replaced the file, where
//! `current_exe()` names the new path with " (deleted)" on the end and every
//! icon fails until Android restarts. `enter_namespaces` joins the user
//! namespace first: that grants the capabilities that make the mount
//! namespace readable, and our uid maps to 0 inside, so no credential change
//! follows. The helper reports the APK path on stderr behind `PATH_MARKER`
//! even when it finds no icon, and the PNG (if any) on stdout.
//!
//! The APK is the app author's and a zip entry can inflate to far more than it
//! declares, so every read is capped (`MAX_MANIFEST`, `MAX_ICON`); the largest
//! real manifest seen is well under 1 MiB, and usermonitor refuses an icon over
//! 4 MiB anyway. `apk_declares` finds a system app's APK by the `package`
//! attribute of its manifest's root element (`manifest_package`), read from the
//! binary XML: the chunks in order, the string pool (UTF-8 or UTF-16, each with
//! its length prefix), and the first start element, which must be `<manifest>`;
//! the value is the attribute's raw string, or its typed value when that is a
//! string. A scan for the name anywhere in the file used to match a longer
//! package (`com.android.settings` inside `com.android.settings.intelligence`)
//! or a manifest that only mentions it, and gave that app the wrong icon. Every
//! offset is bounds-checked, since the manifest is the app author's. System
//! apps are looked for in every partition's app directories
//! (`SYSTEM_APP_DIRS`), system_ext's included, where Settings and SystemUI
//! live; the old scan hid that gap by answering with another APK that merely
//! mentioned the name. User apps live in `/data/app/*/<package>-<base64>/`, so
//! `find_apk` requires the `-` after the name, lest a package that is a prefix
//! of another match it. Adaptive icons are vector XML and cannot be extracted
//! without a resource compiler, so `icon_score` settles for a foreground layer
//! over nothing (background and monochrome layers never work alone), and
//! `extract_icon` falls back to the largest PNG in any mipmap bucket.

use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub struct AppFiles {
    pub apk: PathBuf,
    pub icon_png: Option<Vec<u8>>,
}

impl AppFiles {
    pub fn is_system(&self) -> bool {
        !self.apk.starts_with("/data/app")
    }
}

const PATH_MARKER: &str = "apk-path: ";

const SYSTEM_APP_DIRS: &[&str] = &[
    "/system/app",
    "/system/priv-app",
    "/system/product/app",
    "/system/product/priv-app",
    "/system/system_ext/app",
    "/system/system_ext/priv-app",
];

const MAX_MANIFEST: u64 = 8 << 20;
const MAX_ICON: u64 = 4 << 20;

fn read_capped(r: impl Read, cap: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(cap + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= cap).then_some(buf)
}

pub fn app_files(init_pid: u32, package: &str) -> Result<AppFiles> {
    let out = Command::new("/proc/self/exe")
        .arg("--icon-helper")
        .arg(init_pid.to_string())
        .arg(package)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawn icon helper")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("icon helper: {}", stderr.trim());
    }
    let apk = stderr
        .lines()
        .find_map(|l| l.strip_prefix(PATH_MARKER))
        .ok_or_else(|| anyhow!("icon helper did not report an APK path"))?;
    Ok(AppFiles { apk: PathBuf::from(apk), icon_png: (!out.stdout.is_empty()).then_some(out.stdout) })
}

pub fn icon_helper_main(init_pid: u32, package: &str) -> Result<()> {
    enter_namespaces(init_pid)?;
    let apk = find_apk(package)?;
    eprintln!("{PATH_MARKER}{}", apk.display());
    if let Ok(png) = extract_icon(&apk) {
        use std::io::Write;
        std::io::stdout().write_all(&png)?;
    }
    Ok(())
}

fn enter_namespaces(pid: u32) -> Result<()> {
    use nix::sched::{CloneFlags, setns};
    for (ns, flag) in [("user", CloneFlags::CLONE_NEWUSER), ("mnt", CloneFlags::CLONE_NEWNS)] {
        let path = format!("/proc/{pid}/ns/{ns}");
        let f = File::open(&path).with_context(|| format!("open {path}"))?;
        setns(f.as_fd(), flag).with_context(|| format!("setns {ns}"))?;
    }
    Ok(())
}

fn find_apk(package: &str) -> Result<PathBuf> {
    if let Ok(entries) = std::fs::read_dir("/data/app") {
        for outer in entries.flatten() {
            let Ok(inner) = std::fs::read_dir(outer.path()) else { continue };
            for d in inner.flatten() {
                let name = d.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(package) && name[package.len()..].starts_with('-') {
                    let apk = d.path().join("base.apk");
                    if apk.exists() {
                        return Ok(apk);
                    }
                }
            }
        }
    }
    for dir in SYSTEM_APP_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for d in entries.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == "apk") && apk_declares(&p, package) {
                    return Ok(p);
                }
            }
        }
    }
    Err(anyhow!("no APK found for {package}"))
}

fn apk_declares(apk: &std::path::Path, package: &str) -> bool {
    let Ok(file) = File::open(apk) else { return false };
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return false };
    let Ok(manifest) = zip.by_name("AndroidManifest.xml") else { return false };
    let Some(buf) = read_capped(manifest, MAX_MANIFEST) else { return false };
    manifest_package(&buf).as_deref() == Some(package)
}

const RES_STRING_POOL: u16 = 0x0001;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const UTF8_FLAG: u32 = 1 << 8;
const TYPE_STRING: u8 = 0x03;
const NO_INDEX: u32 = u32::MAX;

fn manifest_package(axml: &[u8]) -> Option<String> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(axml.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(axml.get(o..o + 4)?.try_into().ok()?));
    let mut pool: Option<usize> = None;
    let mut at = u16_at(2)? as usize;
    while at < axml.len() {
        let (kind, header, size) = (u16_at(at)?, u16_at(at + 2)? as usize, u32_at(at + 4)? as usize);
        if size < 8 || header > size {
            return None;
        }
        match kind {
            RES_STRING_POOL => pool = Some(at),
            RES_XML_START_ELEMENT => {
                let pool = pool?;
                let string = |i: u32| pool_string(axml, pool, i);
                let ext = at + header;
                if string(u32_at(ext + 4)?)? != "manifest" {
                    return None;
                }
                let (first, each, count) =
                    (u16_at(ext + 8)? as usize, u16_at(ext + 10)? as usize, u16_at(ext + 12)? as usize);
                return (0..count).find_map(|n| {
                    let a = ext + first + n * each;
                    if u32_at(a)? != NO_INDEX || string(u32_at(a + 4)?)? != "package" {
                        return None;
                    }
                    match (u32_at(a + 8)?, *axml.get(a + 15)?) {
                        (raw, _) if raw != NO_INDEX => string(raw),
                        (_, TYPE_STRING) => string(u32_at(a + 16)?),
                        _ => None,
                    }
                });
            }
            _ => {}
        }
        at = at.checked_add(size)?;
    }
    None
}

fn pool_string(axml: &[u8], pool: usize, i: u32) -> Option<String> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(axml.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(axml.get(o..o + 4)?.try_into().ok()?));
    let (count, flags, strings) = (u32_at(pool + 8)?, u32_at(pool + 16)?, u32_at(pool + 20)? as usize);
    if i >= count {
        return None;
    }
    let at =
        pool.checked_add(strings)?.checked_add(u32_at(pool + u16_at(pool + 2)? as usize + 4 * i as usize)? as usize)?;
    if flags & UTF8_FLAG != 0 {
        let skip = |o: usize| Some(if axml.get(o)? & 0x80 != 0 { o + 2 } else { o + 1 });
        let after_chars = skip(at)?;
        let len = if axml.get(after_chars)? & 0x80 != 0 {
            ((*axml.get(after_chars)? as usize & 0x7f) << 8) | *axml.get(after_chars + 1)? as usize
        } else {
            *axml.get(after_chars)? as usize
        };
        let start = skip(after_chars)?;
        String::from_utf8(axml.get(start..start + len)?.to_vec()).ok()
    } else {
        let first = u16_at(at)? as usize;
        let (len, start) = if first & 0x8000 != 0 {
            (((first & 0x7fff) << 16) | u16_at(at + 2)? as usize, at + 4)
        } else {
            (first, at + 2)
        };
        let units: Vec<u16> =
            axml.get(start..start + 2 * len)?.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&units).ok()
    }
}

fn icon_score(name: &str) -> Option<u32> {
    if !name.ends_with(".png") || !name.starts_with("res/") {
        return None;
    }
    let base = name.rsplit('/').next()?;
    let base_score = match base {
        "ic_launcher.png" | "ic_launcher_round.png" => 300,
        "ic_launcher_foreground.png" => 200,
        b if b.contains("launcher") && !b.contains("background") && !b.contains("monochrome") => 100,
        b if b == "icon.png" || b == "app_icon.png" => 100,
        _ => return None,
    };
    let density = match () {
        _ if name.contains("xxxhdpi") => 6,
        _ if name.contains("xxhdpi") => 5,
        _ if name.contains("xhdpi") => 4,
        _ if name.contains("hdpi") => 3,
        _ if name.contains("mdpi") => 2,
        _ => 1,
    };
    Some(base_score + density)
}

fn extract_icon(apk: &std::path::Path) -> Result<Vec<u8>> {
    let file = File::open(apk).with_context(|| format!("open {}", apk.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("read APK as zip")?;

    let mut best: Option<(u32, u64, String)> = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        let name = entry.name().to_string();
        let Some(score) = icon_score(&name) else { continue };
        let size = entry.size();
        if best.as_ref().is_none_or(|(s, sz, _)| (score, size) > (*s, *sz)) {
            best = Some((score, size, name));
        }
    }
    if best.is_none() {
        for i in 0..zip.len() {
            let Ok(entry) = zip.by_index(i) else { continue };
            let name = entry.name().to_string();
            if !name.starts_with("res/mipmap") || !name.ends_with(".png") {
                continue;
            }
            let size = entry.size();
            if best.as_ref().is_none_or(|(_, sz, _)| size > *sz) {
                best = Some((0, size, name));
            }
        }
    }
    let (_, _, name) = best.ok_or_else(|| anyhow!("APK has no recognisable launcher icon"))?;
    let entry = zip.by_name(&name)?;
    read_capped(entry, MAX_ICON).ok_or_else(|| anyhow!("{name} is over {MAX_ICON} bytes"))
}

#[cfg(test)]
mod tests {
    use super::{manifest_package, read_capped};

    #[test]
    fn reads_stop_at_the_cap() {
        assert_eq!(read_capped(&b"abcd"[..], 4).as_deref(), Some(&b"abcd"[..]));
        assert_eq!(read_capped(&b"abcde"[..], 4), None);
        assert_eq!(read_capped(std::io::repeat(0), 1 << 10), None);
    }

    #[test]
    fn the_package_is_the_manifest_attribute_not_a_substring() {
        let apk = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/sarab-home.apk");
        let mut zip = zip::ZipArchive::new(std::fs::File::open(apk).unwrap()).unwrap();
        let mut axml = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name("AndroidManifest.xml").unwrap(), &mut axml).unwrap();
        assert_eq!(manifest_package(&axml).as_deref(), Some("org.sarab.home"));
        for cut in [0, 7, 8, 100, axml.len() / 2] {
            let _ = manifest_package(&axml[..cut]);
        }
        assert_eq!(manifest_package(b"not binary xml at all"), None);
    }
}
