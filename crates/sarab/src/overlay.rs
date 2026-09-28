//! The overlay files generated from the image, rather than written by hand:
//! init .rc files stripped of directives a rootless init cannot honour and of
//! the ones that start adbd, and the hwcomposer with its fractional-scale bug
//! patched. `sarab setup` writes them to the data directory's `generated/`,
//! never to the hand-written overlay, and a path the hand-written overlay has
//! is not generated at all, so each file comes from exactly one place.
//!
//! Both overlays are bind-mounted over the same path in the image at boot
//! (start.rs), so the image is never modified, and deleting a generated file
//! restores stock behaviour until the next `sarab setup`.
//!
//! The .rc files: `needs_privilege` matches `priority N`, `rlimit
//! rtprio|nice|memlock ...` and `ioprio rt ...`, which need initial-namespace
//! scheduling privilege; init treats a failure to apply any of them as fatal
//! for the service, so they have to go. `start adbd` goes too: the image starts
//! adbd once boot completes and whenever `sys.usb.config` holds `adb`, and
//! nothing here uses it (`sarab exec` is the way in, pasta forwards no port to
//! it). `ro.adb.secure=1` is what keeps an app from using an adbd it starts
//! itself; this is only about not running it for nothing. `generate_rc` skips a
//! vendor .rc whose services are all defined by a /system .rc as well: init
//! parses /system/etc/init first and keeps the first definition of a service,
//! so an overlay of the vendor file would never be read.
//!
//! Sarab's own init rules (`SARAB_RC`) go at the end of the generated copy of
//! `APPEND_TO`, the image's own platform .rc, because a hand-written overlay can
//! only replace a file the image has, and the rules need an .rc that init
//! reads. The one rule so far is the second half of the property-service
//! lock (sarab-ns has the first, and the why). The socket inherits an ACL
//! from /dev/socket, but init creates it mode 0666, and "others" still get in
//! through that; `chmod 0660` takes them out, leaving root and the ACL's
//! system uids. The system uids have to stay: daemons such as logd, lmkd and
//! statsd set properties, and so do system-uid apps (the phone app, uid 1001,
//! crash-loops without its `cache_key.*` writes), which is also why a group
//! cannot do it (every zygote child has `everybody`). The chmod happens only if
//! the ACL is there, or it would shut out every daemon (toybox's getfattr
//! exits 0 for a missing attribute, hence the grep, and `$` is init's
//! property syntax, hence no `$(...)`); the default ACL goes
//! either way, before any other socket is created, as this runs on
//! `early-init`, before init starts anything with a socket, and apps start
//! long after. Isolated processes (WebView renderers) are covered too: they
//! are "others".
//!
//! Every generated .rc that this run did not write is deleted at the end
//! (`kept`): one whose source no longer needs stripping, one the hand-written
//! overlay now covers, a vendor file now shadowed, and one whose source left
//! the image. Otherwise a file generated from the previous image would still
//! be bind-mounted over the new image's own after an upgrade.
//!
//! `sarab start` regenerates everything on each boot, quietly (`refresh`), cheaply,
//! since a file is rewritten only when its content changes: a newer Sarab's
//! rules then reach a data directory set up by an older one without another
//! `sarab setup`.
//!
//! Modes. Android's init skips an .rc file that its group or others can write
//! ("Skipping insecure file"), and under a desktop umask of 002 (Ubuntu's, for
//! every user with a group of their own) that was every generated one,
//! zygote's included, so Android never booted. sarab now runs with umask 022
//! (main.rs), every generated .rc is set to `RC_MODE` whether or not it was
//! rewritten, which repairs a data directory an older Sarab generated, and
//! `secure_overlay` takes group and other write off the hand-written overlay,
//! which a `git clone` under that umask checks out group-writable.
//!
//! The composer: its `output_handle_scale`, the handler for the legacy integer
//! `wl_output.scale`, did `d->scale = max((int)d->scale, scale)`. A compositor
//! rounds that legacy scale up (2 for any output between 1x and 2x). The
//! `wp_fractional_scale_v1` handler sets the real value, but the truncating
//! `max` let the legacy value win anyway (1.25 truncates to 1, which always
//! loses to 2) and never let it come back down; everything downstream divides
//! by the scale, so the app sat in the top-left quarter of its own window with
//! the rest transparent. `PATCHED` replaces the 26 bytes of `ORIGINAL`
//! (cvttsd2si, cmp, cmovl, cvtsi2sd, movsd, ret on the double at 0xd8(%rdi))
//! with `if (d->scale == 0) d->scale = (double)scale;`: a `cmpq $0` on that
//! field, a `jne` to the `ret`, the conversion and the store, then three `int3`
//! to keep the footprint. This is safe because `struct display` is
//! value-initialised and a double +0.0 is all-zero bits, so the integer compare
//! is exactly "nothing has set this yet", and the fractional handler still wins
//! on the first window and every later change. `patch_hwcomposer` refuses
//! unless `ORIGINAL` occurs exactly once: the point is to touch one function,
//! and a different image build is a reason to stop, not to guess.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const RC_MODE: u32 = 0o644;

const MARKER: &str = "# gen-overlay: scheduling-privilege directives and adbd starts removed (sarab setup)\n";

const APPEND_TO: &str = "system/etc/init/init.waydroid.rc";

const SARAB_RC: &str = r#"
# sarab: only root and the system uids may set properties (overlay.rs)
on early-init
    exec - root root -- /system/bin/sh -c "toybox getfattr -n system.posix_acl_access /dev/socket/property_service 2>/dev/null | grep -q posix_acl && chmod 0660 /dev/socket/property_service; toybox setfattr -x system.posix_acl_default /dev/socket"
"#;

fn needs_privilege(line: &str) -> bool {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.as_slice() {
        ["priority", n] => {
            let d = n.strip_prefix('-').unwrap_or(n);
            !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())
        }
        ["rlimit", what, ..] => matches!(*what, "rtprio" | "nice" | "memlock"),
        ["ioprio", "rt", ..] => true,
        _ => false,
    }
}

fn starts_adbd(line: &str) -> bool {
    line.split_whitespace().eq(["start", "adbd"])
}

fn dropped(line: &str) -> bool {
    needs_privilege(line) || starts_adbd(line)
}

fn services(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("service")).then(|| w.next().map(str::to_string)).flatten()
        })
        .collect()
}

fn rc_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rc_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rc") {
            out.push(p);
        }
    }
    out.sort();
}

fn read_lossy(p: &Path) -> Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(p).with_context(|| format!("read {}", p.display()))?).into_owned())
}

pub fn generate_rc(images: &Path, generated: &Path, overlay: &Path) -> Result<(usize, usize)> {
    let mut system_rc = Vec::new();
    rc_files(&images.join("system/system/etc/init"), &mut system_rc);
    let mut shadowed = BTreeSet::new();
    for f in &system_rc {
        shadowed.extend(services(&read_lossy(f)?));
    }
    if !images.join("system").join(APPEND_TO).is_file() {
        bail!("the image has no /{APPEND_TO}, where Sarab's init rules go; is this the image `sarab setup` downloads?");
    }
    let (mut written, mut skipped) = (0, 0);
    let mut kept = BTreeSet::new();
    for (part, sub) in [("system", "system/etc/init"), ("vendor", "etc/init")] {
        let part_root = images.join(part);
        let mut files = Vec::new();
        rc_files(&part_root.join(sub), &mut files);
        for src in files {
            let rel = src.strip_prefix(&part_root)?;
            let rel = if part == "vendor" { Path::new("vendor").join(rel) } else { rel.to_path_buf() };
            if overlay.join(&rel).exists() {
                continue;
            }
            let text = read_lossy(&src)?;
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let clean: Vec<&str> = lines.iter().copied().filter(|l| !dropped(l)).collect();
            let extra = if rel == Path::new(APPEND_TO) { SARAB_RC } else { "" };
            if clean.len() == lines.len() && extra.is_empty() {
                continue;
            }
            let defined = services(&text);
            if part == "vendor" && !defined.is_empty() && defined.is_subset(&shadowed) {
                skipped += 1;
                continue;
            }
            let dst = generated.join(&rel);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            let new = format!("{MARKER}{}{extra}", clean.concat());
            if std::fs::read_to_string(&dst).ok().as_deref() != Some(new.as_str()) {
                std::fs::write(&dst, new)?;
            }
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(RC_MODE))?;
            kept.insert(rel);
            written += 1;
        }
    }
    for sub in ["system/etc/init", "vendor/etc/init"] {
        let mut old = Vec::new();
        rc_files(&generated.join(sub), &mut old);
        for f in old {
            if !kept.contains(f.strip_prefix(generated)?) {
                std::fs::remove_file(&f).with_context(|| format!("remove the stale {}", f.display()))?;
            }
        }
    }
    Ok((written, skipped))
}

pub fn refresh(images: &Path, generated: &Path, overlay: &Path) -> Result<()> {
    secure_overlay(overlay)?;
    generate_rc(images, generated, overlay)?;
    patch_hwcomposer(images, generated)?;
    Ok(())
}

fn secure_overlay(dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in entries {
        let p = e?.path();
        let md = std::fs::symlink_metadata(&p)?;
        if md.is_dir() {
            secure_overlay(&p)?;
        } else if md.is_file() && md.permissions().mode() & 0o022 != 0 {
            let mode = md.permissions().mode() & 0o7755;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).with_context(|| {
                format!("{} is writable by others, and Android's init skips such files; chmod go-w it", p.display())
            })?;
        }
    }
    Ok(())
}

pub const HWC: &str = "vendor/lib64/hw/hwcomposer.waydroid.so";

const ORIGINAL: [u8; 26] = [
    0xf2, 0x0f, 0x2c, 0x87, 0xd8, 0x00, 0x00, 0x00, 0x39, 0xd0, 0x0f, 0x4c, 0xc2, 0xf2, 0x0f, 0x2a, 0xc0, 0xf2, 0x0f,
    0x11, 0x87, 0xd8, 0x00, 0x00, 0x00, 0xc3,
];

const PATCHED: [u8; 26] = [
    0x48, 0x83, 0xbf, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x75, 0x0c, 0xf2, 0x0f, 0x2a, 0xc2, 0xf2, 0x0f, 0x11, 0x87, 0xd8,
    0x00, 0x00, 0x00, 0xc3, 0xcc, 0xcc, 0xcc,
];

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect()
}

pub fn patch_hwcomposer(images: &Path, generated: &Path) -> Result<&'static str> {
    let src = images.join(HWC);
    let dst = generated.join(HWC);
    let mut blob = std::fs::read(&src).with_context(|| format!("read {}", src.display()))?;
    let hits = find_all(&blob, &ORIGINAL);
    if hits.len() != 1 {
        bail!(
            "expected output_handle_scale exactly once in {}, found it {} times; \
             this image's composer differs from the one the patch was written for",
            src.display(),
            hits.len()
        );
    }
    let off = hits[0];
    blob[off..off + PATCHED.len()].copy_from_slice(&PATCHED);
    if std::fs::read(&dst).ok().as_deref() == Some(&blob[..]) {
        return Ok("already patched");
    }
    std::fs::create_dir_all(dst.parent().unwrap())?;
    std::fs::write(&dst, &blob)?;
    std::fs::set_permissions(&dst, std::fs::metadata(&src)?.permissions())?;
    Ok("patched")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_writable_files_are_made_safe_for_init() {
        let d = std::env::temp_dir().join(format!("sarab-overlay-modes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("system/etc/init")).unwrap();
        let rc = d.join("system/etc/init/x.rc");
        let bin = d.join("system/ip");
        for (f, m) in [(&rc, 0o664), (&bin, 0o775)] {
            std::fs::write(f, "").unwrap();
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(m)).unwrap();
        }
        secure_overlay(&d).unwrap();
        let mode = |f: &Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o7777;
        assert_eq!((mode(&rc), mode(&bin)), (0o644, 0o755));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_the_privileged_directives_are_stripped() {
        for l in [
            "    priority -20\n",
            "priority 5",
            "    rlimit rtprio 10 10\n",
            "rlimit nice 40 40",
            "rlimit memlock unlimited unlimited",
            "    ioprio rt 4\n",
        ] {
            assert!(needs_privilege(l), "{l:?} should go");
        }
        for l in [
            "    rlimit nofile 1024 4096\n",
            "rlimit rtprio_x 1 1",
            "    ioprio be 2\n",
            "priority",
            "priority -20 extra",
            "    class main\n",
            "# priority -20\n",
        ] {
            assert!(!needs_privilege(l), "{l:?} must stay");
        }
    }

    #[test]
    fn adbd_is_never_started() {
        for l in ["    start adbd\n", "start adbd", "\tstart  adbd\n"] {
            assert!(dropped(l), "{l:?} should go");
        }
        for l in ["    stop adbd\n", "    start adbd_x\n", "# start adbd\n", "    start logd\n"] {
            assert!(!dropped(l), "{l:?} must stay");
        }
    }

    #[test]
    fn services_are_the_second_word_of_service_lines() {
        let rc = "service audioserver /system/bin/audioserver\n    class core\n  service vendor.x /v/x\non boot\n";
        assert_eq!(services(rc), BTreeSet::from(["audioserver".to_string(), "vendor.x".to_string()]));
    }

    #[test]
    fn the_patch_keeps_its_footprint_and_is_found_once() {
        assert_eq!(ORIGINAL.len(), PATCHED.len());
        let mut blob = vec![0x90u8; 100];
        blob[40..66].copy_from_slice(&ORIGINAL);
        assert_eq!(find_all(&blob, &ORIGINAL), [40]);
        assert!(find_all(&blob, &PATCHED).is_empty());
    }

    #[test]
    fn a_hand_written_file_is_never_generated_too() {
        let d = std::env::temp_dir().join(format!("sarab-overlay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (images, generated, overlay) = (d.join("images"), d.join("generated"), d.join("overlay"));
        let rc = "service a /system/bin/a\n    priority -20\n";
        let files = ["system/system/etc/init/a.rc", "system/system/etc/init/b.rc", "vendor/etc/init/c.rc"];
        assert!(generate_rc(&images, &generated, &overlay).is_err(), "not the tested image");
        std::fs::create_dir_all(images.join("system/system/etc/init")).unwrap();
        std::fs::write(images.join("system").join(APPEND_TO), "on boot\n    start x\n").unwrap();
        for f in files {
            std::fs::create_dir_all(images.join(f).parent().unwrap()).unwrap();
            std::fs::write(images.join(f), rc.replace("a /", &format!("{f} /"))).unwrap();
        }
        std::fs::create_dir_all(overlay.join("system/etc/init")).unwrap();
        std::fs::write(overlay.join("system/etc/init/b.rc"), "hand-written").unwrap();
        std::fs::create_dir_all(generated.join("system/etc/init")).unwrap();
        std::fs::create_dir_all(generated.join("vendor/etc/init")).unwrap();
        for stale in ["system/etc/init/b.rc", "system/etc/init/gone.rc", "vendor/etc/init/d.rc"] {
            std::fs::write(generated.join(stale), "from the previous image").unwrap();
        }
        std::fs::write(images.join("vendor/etc/init/d.rc"), "service d /vendor/bin/d\n").unwrap();
        assert_eq!(generate_rc(&images, &generated, &overlay).unwrap(), (3, 0));
        assert!(!generated.join("system/etc/init/gone.rc").exists(), "its source left the image");
        assert!(!generated.join("vendor/etc/init/d.rc").exists(), "its source needs nothing stripped");
        let appended = read_lossy(&generated.join(APPEND_TO)).unwrap();
        assert!(appended.contains("    start x\n") && appended.ends_with(SARAB_RC), "{appended}");
        assert!(generated.join("system/etc/init/a.rc").is_file());
        assert!(generated.join("vendor/etc/init/c.rc").is_file());
        assert!(!generated.join("system/etc/init/b.rc").exists());
        assert!(!read_lossy(&generated.join("system/etc/init/a.rc")).unwrap().contains("priority"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn no_hand_written_rc_keeps_what_generation_strips() {
        let overlay = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../overlay");
        let mut files = Vec::new();
        rc_files(&overlay, &mut files);
        assert!(!files.is_empty(), "the overlay has .rc files");
        for f in files {
            let text = read_lossy(&f).unwrap();
            let kept: Vec<&str> = text.lines().filter(|l| dropped(l)).collect();
            assert!(kept.is_empty(), "{} keeps {kept:?}", f.display());
        }
    }
}
