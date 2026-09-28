//! Where things are: one `Dirs`, resolved once per command, in one of two
//! modes decided by the binary alone, never by the working directory.
//!
//! Checkout mode is for development and for installs that link `sarab` into
//! `target/release`: `/proc/self/exe` resolves through the symlink, and the
//! walk up from it finds a workspace Cargo.toml next to overlay/ (a crate's
//! own Cargo.toml has none, so the walk goes past it). `SARAB_ROOT` forces it.
//! Everything then lives in the checkout: the image, Android's /data and the
//! generated overlay under images/, logs and runtime files under run/.
//!
//! Installed mode is everything else, the layout `packaging/install.sh
//! --prefix` and a distro package produce: `bin/sarab`, the hand-written
//! overlay in `share/sarab/overlay`, and the rest in the XDG base directories.
//! The data directory is `sarab/android` rather than `sarab` itself, because
//! with `--prefix ~/.local` the overlay also lands in `~/.local/share/sarab`,
//! and deleting the data must not delete the install. It holds several
//! gigabytes, so `sarab setup --data-dir` can put it elsewhere, and setup
//! saves whichever directory it used in `$XDG_CONFIG_HOME/sarab`: an
//! environment variable set in a shell would not reach the systemd unit.
//! `XDG_RUNTIME_DIR` is emptied when the last session of the user ends. Only
//! the kmsg FIFO and the start lock live there, both recreated by the command
//! that needs them; a runtime that outlives the directory keeps its open FIFO.
//!
//! `own_exe` is the path to ourselves that every other file uses, for the
//! helpers here, the launcher entries hostd writes, and the commands that run
//! `sarab` again. A binary replaced while it runs (an upgrade, a rebuild)
//! reads back from `/proc/self/exe` as the old path plus " (deleted)", so
//! `undeleted` drops that suffix when a file is back at the path: the launcher
//! entries written after a rebuild all pointed at "sarab (deleted)", and every
//! click did nothing.
//!
//! The helpers, `sarab-ns` and `sarab-hostd`, are programs nobody types, so an
//! install keeps them off PATH: `SARAB_LIBEXECDIR` at build time for distros
//! that use `/usr/libexec`, else `lib/sarab/` beside `bin/`, else next to the
//! binary. A checkout uses the ones cargo built next to it, or `SARAB_BIN`,
//! relative to the checkout.

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Checkout(PathBuf),
    Installed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dirs {
    pub mode: Mode,
    pub data: PathBuf,
    pub overlay: PathBuf,
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub icons: PathBuf,
    pub helpers: Vec<PathBuf>,
}

impl Dirs {
    pub fn system(&self) -> PathBuf {
        self.data.join("system")
    }

    pub fn vendor(&self) -> PathBuf {
        self.data.join("vendor")
    }

    pub fn android_data(&self) -> PathBuf {
        self.data.join("data")
    }

    pub fn generated(&self) -> PathBuf {
        self.data.join("generated")
    }

    pub fn is_set_up(&self) -> bool {
        self.system().join("system/build.prop").is_file()
            && self.vendor().join("build.prop").is_file()
            && self.generated().join(crate::overlay::HWC).is_file()
    }

    pub fn helper(&self, name: &str) -> Result<PathBuf> {
        if let Some(p) = self.helpers.iter().map(|d| d.join(name)).find(|p| p.is_file()) {
            return Ok(p);
        }
        let looked: Vec<String> = self.helpers.iter().map(|d| d.display().to_string()).collect();
        bail!("{name} not found in {}", looked.join(" or "))
    }
}

fn undeleted(exe: PathBuf, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let Some(name) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) else { return exe };
    let replaced = PathBuf::from(name);
    if exists(&replaced) { replaced } else { exe }
}

pub fn own_exe() -> Result<PathBuf> {
    Ok(undeleted(std::env::current_exe().context("current_exe")?, |p| p.is_file()))
}

pub fn dirs() -> Result<Dirs> {
    let exe = own_exe()?;
    let var = |k: &str| std::env::var_os(k);
    if let Some(r) = var("SARAB_ROOT") {
        let r = PathBuf::from(r);
        if !is_checkout(&r) {
            bail!("SARAB_ROOT={} is not a sarab checkout (no Cargo.toml next to overlay/)", r.display());
        }
        return Ok(checkout(&r, &exe, &var));
    }
    if let Some(r) = exe.parent().and_then(find_root) {
        return Ok(checkout(&r, &exe, &var));
    }
    let chosen = std::fs::read_to_string(data_dir_file(&var)).ok().map(|s| PathBuf::from(s.trim()));
    Ok(installed(&exe, &var, chosen, unsafe { libc::getuid() }, option_env!("SARAB_LIBEXECDIR")))
}

pub fn is_checkout(d: &Path) -> bool {
    d.join("Cargo.toml").is_file() && d.join("overlay").is_dir()
}

pub fn find_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| is_checkout(d)).map(Path::to_path_buf)
}

fn home(var: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    var("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &dyn Fn(&str) -> Option<OsString>, key: &str, default: &str) -> PathBuf {
    match var(key).map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => home(var).join(default),
    }
}

pub fn data_dir_file(var: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    xdg(var, "XDG_CONFIG_HOME", ".config").join("sarab/data-dir")
}

fn icons(var: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    xdg(var, "XDG_DATA_HOME", ".local/share").join("sarab/icons")
}

pub(crate) fn checkout(root: &Path, exe: &Path, var: &dyn Fn(&str) -> Option<OsString>) -> Dirs {
    let beside = exe.parent().map_or_else(|| root.join("target/release"), Path::to_path_buf);
    Dirs {
        mode: Mode::Checkout(root.to_path_buf()),
        data: root.join("images"),
        overlay: root.join("overlay"),
        state: root.join("run"),
        runtime: root.join("run"),
        icons: icons(var),
        helpers: vec![var("SARAB_BIN").map_or(beside, |b| root.join(PathBuf::from(b)))],
    }
}

fn installed(
    exe: &Path,
    var: &dyn Fn(&str) -> Option<OsString>,
    chosen: Option<PathBuf>,
    uid: u32,
    libexec: Option<&str>,
) -> Dirs {
    let bin = exe.parent().unwrap_or(Path::new("/"));
    let prefix = bin.parent().unwrap_or(Path::new("/"));
    let runtime = match var("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => PathBuf::from(format!("/run/user/{uid}")),
    };
    let mut helpers: Vec<PathBuf> = libexec.map(|d| prefix.join(d)).into_iter().collect();
    helpers.extend([prefix.join("lib/sarab"), bin.to_path_buf()]);
    Dirs {
        mode: Mode::Installed,
        data: chosen
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| xdg(var, "XDG_DATA_HOME", ".local/share").join("sarab/android")),
        overlay: prefix.join("share/sarab/overlay"),
        state: xdg(var, "XDG_STATE_HOME", ".local/state").join("sarab"),
        runtime: runtime.join("sarab"),
        icons: icons(var),
        helpers,
    }
}

pub fn build_prop(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_binary_is_found_at_its_path_again() {
        let there = |_: &Path| true;
        let gone = |_: &Path| false;
        assert_eq!(undeleted("/x/sarab (deleted)".into(), there), PathBuf::from("/x/sarab"));
        assert_eq!(undeleted("/x/sarab (deleted)".into(), gone), PathBuf::from("/x/sarab (deleted)"));
        assert_eq!(undeleted("/x/sarab".into(), gone), PathBuf::from("/x/sarab"));
    }
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, OsString> = pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn root_is_the_cargo_toml_next_to_overlay() {
        let d = std::env::temp_dir().join(format!("sarab-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("overlay/system")).unwrap();
        std::fs::create_dir_all(d.join("crates/sarab/src")).unwrap();
        std::fs::create_dir_all(d.join("target/release")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(d.join("crates/sarab/Cargo.toml"), "[package]").unwrap();
        assert_eq!(find_root(&d.join("crates/sarab/src")).unwrap(), d);
        assert_eq!(find_root(&d.join("target/release")).unwrap(), d);
        assert!(!checkout(&d, &d.join("target/release/sarab"), &env(&[])).is_set_up());
        assert_eq!(find_root(Path::new("/nonexistent/x")), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_checkout_keeps_everything_inside_it() {
        let d = checkout(Path::new("/w/sarab"), Path::new("/w/sarab/target/release/sarab"), &env(&[("HOME", "/h")]));
        assert_eq!(d.mode, Mode::Checkout("/w/sarab".into()));
        assert_eq!(d.system(), Path::new("/w/sarab/images/system"));
        assert_eq!(d.vendor(), Path::new("/w/sarab/images/vendor"));
        assert_eq!(d.android_data(), Path::new("/w/sarab/images/data"));
        assert_eq!(d.generated(), Path::new("/w/sarab/images/generated"));
        assert_eq!(d.overlay, Path::new("/w/sarab/overlay"));
        assert_eq!((d.state.as_path(), d.runtime.as_path()), (Path::new("/w/sarab/run"), Path::new("/w/sarab/run")));
        assert_eq!(d.icons, Path::new("/h/.local/share/sarab/icons"));
        assert_eq!(d.helpers, [PathBuf::from("/w/sarab/target/release")]);
        let o = checkout(Path::new("/w/sarab"), Path::new("/x/sarab"), &env(&[("SARAB_BIN", "target/debug")]));
        assert_eq!(o.helpers, [PathBuf::from("/w/sarab/target/debug")]);
    }

    #[test]
    fn an_install_uses_the_prefix_and_the_xdg_directories() {
        let var = env(&[("HOME", "/h"), ("XDG_RUNTIME_DIR", "/run/user/1000"), ("XDG_STATE_HOME", "relative")]);
        let d = installed(Path::new("/usr/bin/sarab"), &var, None, 1000, None);
        assert_eq!(d.mode, Mode::Installed);
        assert_eq!(d.data, Path::new("/h/.local/share/sarab/android"));
        assert_eq!(d.generated(), Path::new("/h/.local/share/sarab/android/generated"));
        assert_eq!(d.overlay, Path::new("/usr/share/sarab/overlay"));
        assert_eq!(d.state, Path::new("/h/.local/state/sarab"));
        assert_eq!(d.runtime, Path::new("/run/user/1000/sarab"));
        assert_eq!(d.icons, Path::new("/h/.local/share/sarab/icons"));
        assert_eq!(d.helpers, [PathBuf::from("/usr/lib/sarab"), PathBuf::from("/usr/bin")]);
        let d = installed(Path::new("/usr/bin/sarab"), &var, None, 1000, Some("/usr/libexec/sarab"));
        assert_eq!(
            d.helpers,
            [PathBuf::from("/usr/libexec/sarab"), PathBuf::from("/usr/lib/sarab"), PathBuf::from("/usr/bin")]
        );

        let local = installed(Path::new("/h/.local/bin/sarab"), &var, None, 1000, None);
        assert_eq!(local.overlay, Path::new("/h/.local/share/sarab/overlay"));
        assert!(!local.overlay.starts_with(&local.data) && !local.data.starts_with(&local.overlay));

        let var = env(&[("HOME", "/h"), ("XDG_DATA_HOME", "/d"), ("SARAB_BIN", "/ignored")]);
        let d = installed(Path::new("/opt/s/bin/sarab"), &var, Some("/big/sarab".into()), 1000, None);
        assert_eq!(d.data, Path::new("/big/sarab"));
        assert_eq!(d.icons, Path::new("/d/sarab/icons"));
        assert_eq!(d.runtime, Path::new("/run/user/1000/sarab"));
        assert_eq!(d.overlay, Path::new("/opt/s/share/sarab/overlay"));
        assert!(!d.helpers.contains(&PathBuf::from("/ignored")));
        let d = installed(Path::new("/usr/bin/sarab"), &env(&[("HOME", "/h")]), Some("relative".into()), 1000, None);
        assert_eq!(d.data, Path::new("/h/.local/share/sarab/android"));
    }

    #[test]
    fn the_data_dir_choice_lives_in_the_config_directory() {
        assert_eq!(data_dir_file(&env(&[("HOME", "/h")])), Path::new("/h/.config/sarab/data-dir"));
        assert_eq!(data_dir_file(&env(&[("XDG_CONFIG_HOME", "/c")])), Path::new("/c/sarab/data-dir"));
    }

    #[test]
    fn build_prop_reads_one_key_exactly() {
        let d = std::env::temp_dir().join(format!("sarab-prop-{}", std::process::id()));
        std::fs::write(&d, "# c\nro.lineage.version=20.0-x\nro.lineage.version.extra=no\n").unwrap();
        assert_eq!(build_prop(&d, "ro.lineage.version").as_deref(), Some("20.0-x"));
        assert_eq!(build_prop(&d, "ro.missing"), None);
        std::fs::remove_file(&d).unwrap();
    }
}
