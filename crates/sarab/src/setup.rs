//! `sarab setup`: from an install to a runtime that boots, in one command.
//!
//! The steps: check the host (binderfs, the tools, the subuid range, a GPU
//! Mesa drives, and on Ubuntu the AppArmor profile sarab-ns needs), fetch the
//! image if there is none, extract it, generate the overlay. Every host check
//! comes before the download: each of them otherwise fails only after it, at
//! the first extraction or at the black window of the first boot. The in-Android half (the desktop policy) happens on
//! the first boot by itself (policy.rs). `--vanilla` downloads the image's
//! build without Google apps. The build is the one pinned in image.rs, kept as
//! a zip named after it; `--latest` takes the newest in the listing instead.
//!
//! The data directory is printed with its free space before anything is
//! downloaded. `SPACE_NEEDED` is the peak while the system half extracts: its
//! zip (1.2 GB, kept), the unsparsed image (2.5 GB, deleted afterwards) and the
//! extracted tree (2.5 GB), with the vendor half after it; about 5 GB stay.
//! Less the zips setup will not download (`zips_on_disk`: given ones, and
//! pinned ones kept from before; with `--latest` the build is not known
//! offline), it is a hard limit for a first extraction, which would otherwise
//! stop at a full disk a minute in (`check_space`). When a tree, or what an
//! interrupted run left (`LEFTOVERS`), is already there it is only a warning,
//! since setup deletes those first and the free space understates the room.
//! An installed sarab saves the data directory it used, once it is known to be
//! writable, so the systemd unit and a shell agree on it (paths.rs);
//! `--data-dir` changes it, and data already in the old place is left there.
//!
//! `sarab start` on a machine where nothing is set up calls `first_run`, a
//! wizard (ui.rs): it checks the host, explains the two builds (`FLAVOURS`)
//! and asks which one, says where Android goes and how much it takes, asks
//! once, runs the same steps and boots Android. With Google apps it warns,
//! right after the choice, that an unregistered device plays Play services'
//! reminder sound again and again (`UNREGISTERED`, a `ui::alert` short
//! enough to survive skimming, and said again on the spinner that runs while
//! the beeps do), and has it acknowledged: Enter on "Got it", or a switch to
//! the build without Google apps. After the boot it waits
//! up to `GOOGLE_WAIT` for the Google ID and shows how to register it
//! (`google_step`, which also offers to open the page with xdg-open), since
//! nobody would guess that `sarab google-id` exists. Ctrl-C or Esc at a
//! question, or No, ends it with `NOT_NOW`. Only with a person at a terminal;
//! the unit, a launcher click and a script get the plain "not set up" error,
//! because a 1.4 GB download is not theirs to start. What `prepare` prints goes
//! through ui.rs too: in the wizard its bookkeeping lines (`ui::detail`) are
//! left out, and the rest shows as the wizard's own lines.
//!
//! Idempotent, and the way to upgrade: `plan` decides for each half before
//! anything is fetched, and `carry_out` does it. An extracted half is left
//! alone unless `--force`, or unless it is older than the build this sarab
//! pins (`image::standing`): then the pinned build of the same flavour is
//! fetched and swapped in, and Android's data, which lives apart from both
//! halves, is kept. A newer build (from `--latest`), another line, a zip the
//! user gave, or a GAPPS install asked for `--vanilla` is kept, with a line
//! saying `--force` replaces it. Replacing a half Android is running on is
//! refused before the download. So after updating sarab, `sarab setup` brings
//! the image along, and with nothing to do it only refreshes the generated
//! overlay.
//!
//! `sarab upgrade` is the command people are told about, since "setup" sounds
//! like something run once. It takes only the upgrades `due` (older halves;
//! it never downgrades or switches flavour), shows each from and to with the
//! download size, asks once (`--yes` without a terminal), stops a running
//! Android, runs `prepare`, and starts Android again whether or not that
//! worked: a failed upgrade leaves the old image in place, so Android comes
//! back on it. `--check` prints only `image::behind`'s lines, for install.sh. `zips_on_disk` counts the zips the plans need that are already
//! there. A sub-id
//! range shorter than sarab-ns's maps take is an error here, before the
//! download, with the same check and fix line sarab-ns gives
//! (`sarab_ns::check`); the default 65536 is enough. A missing tool gets the
//! install line for this distro.

use crate::image::Standing;
use crate::info::{self, HOST_TOOLS};
use crate::paths::{Dirs, Mode};
use crate::{image, overlay, paths, ui};
use anyhow::{Context, Result, bail};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Opts {
    pub system: Option<PathBuf>,
    pub vendor: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub vanilla: bool,
    pub latest: bool,
    pub force: bool,
}

const GB: u64 = 1_000_000_000;
const SPACE_NEEDED: u64 = 7 * GB;
const LEFTOVERS: &[&str] = &[
    "system",
    "vendor",
    "system.partial",
    "vendor.partial",
    "system.old",
    "vendor.old",
    "system.raw.img",
    "vendor.raw.img",
];

fn free_bytes(p: &Path) -> Option<u64> {
    let existing = p.ancestors().find(|a| a.exists())?;
    let c = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail * st.f_frsize)
}

fn check_host(dirs: &Dirs) -> Result<crate::host::Gpu> {
    info::binderfs().map_err(|why| anyhow::anyhow!(why))?;
    let missing: Vec<String> = HOST_TOOLS
        .iter()
        .filter(|(b, _)| *b != "curl" && !info::on_path(b))
        .map(|(b, pkg)| format!("{b} (package: {pkg})"))
        .collect();
    if !missing.is_empty() {
        let pasta =
            if info::on_path("pasta") { String::new() } else { format!("\npasta: {}", info::install_hint("passt")) };
        bail!("missing host tools: {}{pasta}", missing.join(", "));
    }
    let (subuid, subgid) = info::subid_files();
    sarab_ns::check(&info::username(), &subuid, &subgid).map_err(|m| anyhow::anyhow!(m))?;
    let gpu = crate::host::require_gpu()?;
    crate::apparmor::check(dirs)?;
    Ok(gpu)
}

fn settle_data_dir(dirs: &mut Dirs, chosen: Option<&Path>) -> Result<()> {
    if let Mode::Checkout(root) = &dirs.mode {
        if chosen.is_some() {
            bail!("a checkout keeps its data in {}/images; --data-dir is for an installed sarab", root.display());
        }
        return Ok(());
    }
    let dir = chosen.map_or_else(|| dirs.data.clone(), Path::to_path_buf);
    if dir != dirs.data && sarab_runtime::find_runtime().is_ok() {
        bail!("Android is running; `sarab stop` before moving its data");
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let probe = dir.join(".sarab-write-test");
    std::fs::write(&probe, "").with_context(|| format!("{} is not writable", dir.display()))?;
    std::fs::remove_file(&probe)?;
    let file = crate::paths::data_dir_file(&|k| std::env::var_os(k));
    if std::fs::read_to_string(&file).is_ok_and(|s| Path::new(s.trim()) == dir) {
        return Ok(());
    }
    if dir != dirs.data && dirs.system().exists() {
        ui::info(format!(
            "data: the image in {} stays where it is; delete it, or move it to {}",
            dirs.data.display(),
            dir.display()
        ));
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, format!("{}\n", dir.display())).with_context(|| format!("write {}", file.display()))?;
    ui::detail(format!("data: {} is the data directory now (saved in {})", dir.display(), file.display()));
    dirs.data = dir;
    Ok(())
}

fn done(dirs: &Dirs, part: &str) -> PathBuf {
    match part {
        "system" => dirs.system().join("system/build.prop"),
        _ => dirs.vendor().join("build.prop"),
    }
}

enum Plan {
    Keep,
    Zip(PathBuf),
    Fetch(image::Release),
}

fn plan(dirs: &Dirs, part: &str, given: Option<&Path>, o: &Opts) -> Result<Plan> {
    let tree = dirs.data.join(part);
    if let Some(build) = image::build_of(&tree, part).filter(|_| !o.force) {
        let what = image::describe(&tree, part);
        let keep = |why: &str| {
            ui::info(format!("{part}: already extracted ({what}){why}"));
            Ok(Plan::Keep)
        };
        if given.is_some() || o.latest {
            return keep("; --force to redo");
        }
        if part == "system" && o.vanilla && image::gapps(&build) {
            return keep(", not the VANILLA build you asked for; --force replaces it");
        }
        return match image::standing(part, &build) {
            Standing::Pinned => keep(""),
            Standing::Newer => keep(", newer than the build this sarab is tested with; --force puts that one back"),
            Standing::Other => {
                keep(", not a build this sarab knows; --force replaces it with the one it is tested with")
            }
            Standing::Older => {
                let pin = image::pinned(part, part != "system" || image::gapps(&build));
                ui::info(format!(
                    "{part}: {what} is older than the build this sarab is tested with; upgrading it to {} \
                     (Android's data is kept)",
                    pin.filename
                ));
                Ok(Plan::Fetch(pin))
            }
        };
    }
    Ok(match given {
        Some(z) => Plan::Zip(z.to_path_buf()),
        None => Plan::Fetch(image::release(part, !o.vanilla, o.latest)?),
    })
}

fn zip_of(dirs: &Dirs, plan: &Plan) -> Option<PathBuf> {
    match plan {
        Plan::Keep => None,
        Plan::Zip(z) => Some(z.clone()),
        Plan::Fetch(r) => Some(dirs.data.join(&r.filename)),
    }
}

fn carry_out(dirs: &Dirs, part: &str, plan: Plan) -> Result<()> {
    let Some(zip) = zip_of(dirs, &plan) else { return Ok(()) };
    if let Plan::Fetch(r) = &plan
        && !zip.is_file()
    {
        if !info::on_path("curl") {
            bail!("no {} and no curl to download it; pass --{part} FILE", zip.display());
        }
        std::fs::create_dir_all(&dirs.data)?;
        image::download(part, r, &zip)?;
    }
    image::extract(dirs, part, &zip)
}

fn zips_on_disk(dirs: &Dirs, plans: &[(&str, Plan)]) -> u64 {
    plans.iter().filter_map(|(_, p)| zip_of(dirs, p)).filter_map(|z| std::fs::metadata(z).ok()).map(|m| m.len()).sum()
}

fn check_space(free: Option<u64>, on_disk: u64, fresh: bool) -> Result<Option<String>, String> {
    let needed = SPACE_NEEDED.saturating_sub(on_disk);
    match free {
        Some(b) if b < needed => {
            let msg = format!(
                "setup needs about {:.1} GB there while it extracts, and keeps about 5 GB; {:.1} GB are free",
                needed as f64 / GB as f64,
                b as f64 / GB as f64
            );
            if fresh { Err(msg) } else { Ok(Some(msg)) }
        }
        _ => Ok(None),
    }
}

fn free_text(p: &Path) -> String {
    free_bytes(p).map_or("free space unknown".into(), |b| format!("{:.1} GB free", b as f64 / GB as f64))
}

fn yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

const NOT_NOW: &str = "Android is not set up; `sarab start` asks again";

const FLAVOURS: &str = "\
With Google apps
  The Play Store, Google sign-in, and the many apps that need Google:
  maps, notifications, most banking and chat apps. Google asks you to
  register the device once; the last step here shows how.

Without Google apps
  No Google account, and nothing sent to Google. Install apps from .apk
  files or an open store such as F-Droid. Apps that need Google will not
  work.";

const UNREGISTERED: (&str, &str) = (
    "Expect notification sounds, many of them",
    "Google keeps beeping until you register this device, the last step here.",
);

pub fn first_run(dirs: &Dirs) -> Result<()> {
    use std::io::IsTerminal;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!("Sarab is not set up yet: run `sarab start` in a terminal, or `sarab setup`");
    }
    ui::begin("Sarab");
    let checking = ui::Task::spin("Checking this computer");
    let gpu = check_host(dirs)?;
    checking.done(format!("This computer can run Android (graphics: {})", gpu.driver));
    let _ = cliclack::note("Google apps or not", FLAVOURS);
    let mut gapps = cliclack::select("Which Android do you want?")
        .item(true, "With Google apps", "recommended")
        .item(false, "Without Google apps", "more private")
        .interact()
        .map_err(|_| anyhow::anyhow!(NOT_NOW))?;
    if gapps {
        ui::alert(UNREGISTERED.0, UNREGISTERED.1);
        gapps = cliclack::select("Continue with Google apps?")
            .item(true, "Got it, continue", "")
            .item(false, "Switch to Android without Google apps", "")
            .interact()
            .map_err(|_| anyhow::anyhow!(NOT_NOW))?;
    }
    let size = image::pinned("system", gapps).size + image::pinned("vendor", true).size;
    ui::info(format!(
        "Android 13 (LineageOS 20) goes into {}\n{:.1} GB to download, about 5 GB on disk ({})",
        dirs.data.display(),
        size as f64 / GB as f64,
        free_text(&dirs.data)
    ));
    if dirs.mode == Mode::Installed {
        let _ = cliclack::log::remark("To put it elsewhere, answer No and run `sarab setup --data-dir DIR`.");
    }
    let go = cliclack::confirm("Download and set up Android now?")
        .initial_value(true)
        .interact()
        .map_err(|_| anyhow::anyhow!(NOT_NOW))?;
    if !go {
        bail!(NOT_NOW);
    }
    prepare(&Opts { system: None, vendor: None, data_dir: None, vanilla: !gapps, latest: false, force: false })?;
    let booting = ui::Task::spin("Starting Android for the first time");
    let t0 = std::time::Instant::now();
    crate::session::ensure_running(&paths::dirs()?, &|_| {})?;
    booting.done(format!("Android is up ({:.1} s)", t0.elapsed().as_secs_f64()));
    if gapps {
        google_step();
    }
    ui::end("Done. Install an app with `sarab install app.apk`, or double-click an .apk file.");
    Ok(())
}

fn google_step() {
    let waiting = ui::Task::spin("Getting your Google ID (the beeps are Google's reminder; registering stops them)");
    match crate::google::wait_for_id(GOOGLE_WAIT) {
        Ok(Some(id)) => {
            waiting.done("Google Play services checked in");
            let _ = cliclack::note("Register this device with Google", crate::google::steps(&id));
            let open = info::on_path("xdg-open")
                && cliclack::confirm("Open the registration page in your browser?")
                    .initial_value(true)
                    .interact()
                    .unwrap_or(false);
            if open {
                let _ = std::process::Command::new("xdg-open")
                    .arg(crate::google::REGISTER_URL)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }
        Ok(None) => waiting.failed("Google Play services has not checked in yet; `sarab google-id` in a few minutes"),
        Err(e) => waiting.failed(format!("could not read the Google ID ({e:#}); `sarab google-id` tries again")),
    }
}

const GOOGLE_WAIT: std::time::Duration = std::time::Duration::from_secs(150);

pub fn run(o: Opts) -> Result<()> {
    prepare(&o)?;
    println!("\nSarab is set up.");
    println!("  sarab start            boot Android (about 2 s; the first boot takes longer)");
    println!("  sarab install app.apk  install an app, or double-click an .apk");
    println!("  sarab google-id        register this device so the Play Store works");
    Ok(())
}

fn due(dirs: &Dirs) -> Vec<(&'static str, String, image::Release)> {
    ["system", "vendor"]
        .into_iter()
        .filter_map(|part| {
            let tree = dirs.data.join(part);
            let build = image::build_of(&tree, part)?;
            (image::standing(part, &build) == Standing::Older).then(|| {
                (part, image::describe(&tree, part), image::pinned(part, part != "system" || image::gapps(&build)))
            })
        })
        .collect()
}

pub fn upgrade(assume_yes: bool, check: bool) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let dirs = paths::dirs()?;
    if check {
        for note in image::behind(&dirs.data) {
            println!("{note}");
        }
        return Ok(());
    }
    if !dirs.is_set_up() {
        bail!("Sarab is not set up yet; `sarab start` sets it up with the image it is tested with");
    }
    let due = due(&dirs);
    if due.is_empty() {
        println!(
            "Android's image is the one this Sarab is tested with ({}); nothing to upgrade",
            image::describe(&dirs.system(), "system")
        );
        return Ok(());
    }
    for (part, from, to) in &due {
        println!("{part}: {from} -> {}", to.filename);
    }
    let download: u64 =
        due.iter().filter(|(_, _, r)| !dirs.data.join(&r.filename).is_file()).map(|(_, _, r)| r.size).sum();
    if download > 0 {
        println!("download: {:.1} GB", download as f64 / GB as f64);
    }
    let running = sarab_runtime::find_runtime().is_ok();
    if !assume_yes {
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            bail!("an upgrade asks first; without a terminal, `sarab upgrade --yes`");
        }
        let restart = if running { "Android restarts, and your" } else { "Your" };
        print!("{restart} apps and data are kept. Upgrade now? [Y/n] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        if std::io::stdin().lock().read_line(&mut answer)? == 0 || !yes(&answer) {
            bail!("not upgraded");
        }
    }
    if running {
        crate::session::stop(false)?;
    }
    let done =
        prepare(&Opts { system: None, vendor: None, data_dir: None, vanilla: false, latest: false, force: false });
    let back = running.then(|| {
        if done.is_err() {
            eprintln!("the upgrade failed; starting Android again on the image it had");
        }
        crate::session::ensure_running(&dirs, &|s| println!("{s}"))
    });
    done?;
    if let Some(back) = back {
        back?;
    }
    println!("upgraded; Android's data is kept");
    Ok(())
}

fn prepare(o: &Opts) -> Result<()> {
    let mut dirs = paths::dirs()?;
    check_host(&dirs)?;
    settle_data_dir(&mut dirs, o.data_dir.as_deref())?;
    if !dirs.overlay.is_dir() {
        bail!("the hand-written overlay is missing ({}); reinstall sarab", dirs.overlay.display());
    }
    for bin in ["sarab-ns", "sarab-hostd"] {
        dirs.helper(bin)?;
    }
    ui::detail(format!("data: {} ({})", dirs.data.display(), free_text(&dirs.data)));
    let plans = [
        ("system", plan(&dirs, "system", o.system.as_deref(), o)?),
        ("vendor", plan(&dirs, "vendor", o.vendor.as_deref(), o)?),
    ];
    let extracting = plans.iter().any(|(_, p)| !matches!(p, Plan::Keep));
    let replacing = plans.iter().any(|(part, p)| !matches!(p, Plan::Keep) && done(&dirs, part).is_file());
    if replacing && sarab_runtime::find_runtime().is_ok() {
        bail!("Android is running on the image this replaces; `sarab upgrade` stops and starts it for you");
    }
    let fresh = !LEFTOVERS.iter().any(|l| dirs.data.join(l).exists());
    let elsewhere = if dirs.mode == Mode::Installed { " `sarab setup --data-dir DIR` puts it elsewhere." } else { "" };
    if extracting {
        match check_space(free_bytes(&dirs.data), zips_on_disk(&dirs, &plans), fresh) {
            Err(msg) => bail!("not enough space in {}: {msg}.{elsewhere}", dirs.data.display()),
            Ok(Some(msg)) => ui::warn(format!("{msg}.{elsewhere}")),
            Ok(None) => {}
        }
    }
    for (part, p) in plans {
        carry_out(&dirs, part, p)?;
    }
    let (written, skipped) = overlay::generate_rc(&dirs.data, &dirs.generated(), &dirs.overlay)?;
    ui::detail(format!(
        "overlay: {written} init files rewritten for a rootless runtime, {skipped} shadowed by /system"
    ));
    ui::detail(format!("overlay: hwcomposer scale fix {}", overlay::patch_hwcomposer(&dirs.data, &dirs.generated())?));
    std::fs::create_dir_all(dirs.android_data())?;
    std::fs::create_dir_all(&dirs.state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_little_space_stops_only_a_first_extraction() {
        assert_eq!(check_space(Some(8 * GB), 0, true), Ok(None));
        assert_eq!(check_space(None, 0, true), Ok(None));
        assert!(check_space(Some(3 * GB), 0, true).unwrap_err().contains("7.0 GB there"));
        assert!(check_space(Some(3 * GB), 0, false).unwrap().unwrap().contains("3.0 GB are free"));
        assert_eq!(check_space(Some(6 * GB), 2 * GB, true), Ok(None));
        assert!(check_space(Some(4 * GB), 2 * GB, true).is_err());
        assert_eq!(check_space(Some(0), 9 * GB, true), Ok(None));
    }

    #[test]
    fn enter_means_yes_and_anything_else_but_y_means_no() {
        for a in ["\n", "y\n", "Y", " yes \n"] {
            assert!(super::yes(a), "{a:?}");
        }
        for a in ["n\n", "no", "q", "yess"] {
            assert!(!super::yes(a), "{a:?}");
        }
    }
}
