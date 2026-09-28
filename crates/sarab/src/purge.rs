//! `sarab purge`: deleting what Sarab downloaded and what Android stored.
//!
//! Android's /data, and the extracted image trees too, belong to our subuids
//! (sarab-ns maps them), so a plain `rm -rf` from the host fails with EACCES
//! halfway through. `delete` runs `rm` inside a user namespace instead
//! (`sarab-ns` with no `--root`, as image.rs extracts), where those ids are
//! mapped and uid 0 owns them. `--one-file-system` keeps it from walking into
//! anything mounted below.
//!
//! It never deletes the data directory itself: `sarab setup --data-dir` may
//! have pointed it at a directory that holds other things. `targets` lists only
//! the names setup creates there (`OURS`: the image halves, their `.partial`
//! and `.raw.img` leftovers, Android's `data`, the `generated` overlay) and the
//! downloaded zips, `lineage-*.zip` and their `.zip.part` (`is_download`);
//! with `--data-only`, Android's `data` alone, so apps, accounts and settings
//! go and the image stays. An empty data directory is removed afterwards.
//!
//! The launcher entries hostd wrote (`sarab.*.desktop` carrying
//! `X-Sarab-Package=`, in the applications directory beside the icons one,
//! `applications`) and the icons go too, since they would open apps that no
//! longer exist; a data-only purge's next boot writes the system apps' back.
//! A full purge also removes Sarab's state directory (the logs and the record
//! of the last start's display) and the file that remembers `--data-dir`
//! (`paths::data_dir_file`), then its directory if that is left empty, so
//! nothing of the data is left behind to point at.
//! It refuses while Android runs, and asks first unless `--yes`; without a
//! terminal to ask on, it wants `--yes`.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OURS: &[&str] =
    &["system", "vendor", "system.partial", "vendor.partial", "system.raw.img", "vendor.raw.img", "data", "generated"];

fn is_download(name: &str) -> bool {
    name.starts_with("lineage-") && (name.ends_with(".zip") || name.ends_with(".zip.part"))
}

pub fn targets(data: &Path, data_only: bool) -> Vec<PathBuf> {
    if data_only {
        return [data.join("data")].into_iter().filter(|p| p.symlink_metadata().is_ok()).collect();
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(data)
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| OURS.contains(&n.as_str()) || is_download(n))
                .map(|n| data.join(n))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn applications(icons: &Path) -> Option<PathBuf> {
    Some(icons.parent()?.parent()?.join("applications"))
}

fn launcher_entries(apps: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(apps) else { return Vec::new() };
    let mut found: Vec<PathBuf> = rd
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("sarab.") && n.ends_with(".desktop"))
        })
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|s| s.lines().any(|l| l.starts_with("X-Sarab-Package="))))
        .collect();
    found.sort();
    found
}

fn delete(dirs: &crate::paths::Dirs, paths: &[PathBuf]) -> Result<()> {
    let ns = dirs.helper("sarab-ns")?;
    let out = Command::new(&ns)
        .args(["--", "rm", "-rf", "--one-file-system", "--"])
        .args(paths)
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {}", ns.display()))?;
    for line in String::from_utf8_lossy(&out.stderr).lines().filter(|l| !l.starts_with("sarab-ns:")) {
        eprintln!("{line}");
    }
    if !out.status.success() {
        bail!("deleting inside the namespace failed ({})", out.status);
    }
    Ok(())
}

fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("nothing to ask on; pass --yes to delete without asking");
    }
    print!("Delete? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub fn run(data_only: bool, yes: bool) -> Result<()> {
    let dirs = crate::paths::dirs()?;
    if sarab_runtime::find_runtime().is_ok() || crate::session::unit_active() {
        bail!("Android is running; `sarab stop` first");
    }
    let paths = targets(&dirs.data, data_only);
    let entries = applications(&dirs.icons).map(|a| launcher_entries(&a)).unwrap_or_default();
    let choice = crate::paths::data_dir_file(&|k| std::env::var_os(k));
    let records: Vec<PathBuf> = if data_only {
        Vec::new()
    } else {
        [dirs.state.clone(), choice.clone()].into_iter().filter(|p| p.symlink_metadata().is_ok()).collect()
    };
    if paths.is_empty() && entries.is_empty() && records.is_empty() {
        println!("nothing to delete in {}", dirs.data.display());
        return Ok(());
    }
    if data_only {
        println!("This deletes Android's data: every app, account and setting.");
    } else {
        println!("This deletes the Android image, its downloads, and Android's data.");
    }
    for p in &paths {
        println!("  {}", p.display());
    }
    if !entries.is_empty() {
        let n = entries.len();
        println!("  {n} launcher {}, and {}", if n == 1 { "entry" } else { "entries" }, dirs.icons.display());
    }
    for r in &records {
        println!("  {}", r.display());
    }
    if !confirm(yes)? {
        println!("nothing deleted");
        return Ok(());
    }
    if !paths.is_empty() {
        delete(&dirs, &paths)?;
    }
    for e in &entries {
        std::fs::remove_file(e).with_context(|| format!("remove {}", e.display()))?;
    }
    let _ = std::fs::remove_dir_all(&dirs.icons);
    for r in &records {
        let gone = if r.is_dir() { std::fs::remove_dir_all(r) } else { std::fs::remove_file(r) };
        gone.with_context(|| format!("remove {}", r.display()))?;
    }
    if let Some(config) = records.iter().find(|r| **r == choice).and_then(|r| r.parent()) {
        let _ = std::fs::remove_dir(config);
    }
    let _ = std::fs::remove_dir(&dirs.data);
    println!("deleted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sarab-purge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_what_setup_made_is_deleted() {
        let d = tmp("targets");
        for n in ["system", "vendor", "data", "generated", "vendor.partial"] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        for n in [
            "lineage-20.0-20260403-GAPPS-waydroid_x86_64-system.zip",
            "lineage-20.0-20260428-MAINLINE-waydroid_x86_64-vendor.zip.part",
            "notes.txt",
            "system.zip",
        ] {
            std::fs::write(d.join(n), "").unwrap();
        }
        std::fs::create_dir_all(d.join("photos")).unwrap();
        let names: Vec<String> =
            targets(&d, false).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(
            names,
            [
                "data",
                "generated",
                "lineage-20.0-20260403-GAPPS-waydroid_x86_64-system.zip",
                "lineage-20.0-20260428-MAINLINE-waydroid_x86_64-vendor.zip.part",
                "system",
                "vendor",
                "vendor.partial"
            ]
        );
        assert_eq!(targets(&d, true), [d.join("data")]);
        std::fs::remove_dir_all(d.join("data")).unwrap();
        assert!(targets(&d, true).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_hostds_launcher_entries_are_deleted() {
        let d = tmp("entries");
        let apps = d.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("sarab.com.example.desktop"), "[Desktop Entry]\nX-Sarab-Package=com.example\n")
            .unwrap();
        std::fs::write(apps.join("sarab.mine.desktop"), "[Desktop Entry]\nName=mine\n").unwrap();
        std::fs::write(apps.join("sarab-install.desktop"), "X-Sarab-Package=x\n").unwrap();
        std::fs::write(apps.join("firefox.desktop"), "X-Sarab-Package=x\n").unwrap();
        assert_eq!(launcher_entries(&apps), [apps.join("sarab.com.example.desktop")]);
        assert_eq!(applications(&d.join("sarab/icons")), Some(apps));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
