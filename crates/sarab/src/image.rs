//! Getting the Android image onto disk, rootless: download, unsparse, extract.
//!
//! Each half of the image (system, vendor) is published as a zip holding one
//! ext4 image, usually in Android's sparse format, in a channel of its own
//! (GAPPS for the Play Store build, VANILLA without Google, MAINLINE for the
//! vendor half); `channel` gives both the channel's name and the directory
//! its builds live in, on the listing server and on SourceForge alike.
//!
//! `PINS` holds the builds this version of sarab was tested with, one per
//! channel, as (channel, file, SHA-256, size, build): the GAPPS system and the
//! vendor half are the ones booted on the development machine
//! (`ro.lineage.version` 20.0-20260403-GAPPS, vendor built 2026-04-28), and
//! VANILLA is the same build without Google apps. One of video decoding's two
//! blockers is in the pinned vendor: its minigbm cannot map the YV12 frames the
//! software decoders write, fixed upstream in the image's minigbm (e5a7769a,
//! 2026-07-23), so a vendor built after that date is the one to move to, and
//! apps/media-probe the test to re-run (docs/TODO.md has the other blocker).
//! The servicemanager codes, the rc rewriting and the composer patch are all
//! written against one build, and an upstream rebuild, or a move to a newer
//! Android, would break every fresh install only after the download. So setup
//! fetches exactly those, checked against hashes from this repository rather
//! than from the server that serves the file; moving a pin is a change to test
//! like any other. With `--latest`, `release` reads the channel's listing
//! instead, and `parse_ota` takes the newest release by datetime rather than
//! trusting the listing's order, and refuses one whose address is not https,
//! whose name is not a plain .zip, or whose checksum is not a SHA-256. Its hash
//! then comes from the same server, and a warning says so unless it is the
//! pinned build after all.
//!
//! A pin's build is how an extracted tree names itself (`build_of`): the
//! system half's `ro.lineage.version`, which is its zip's name without
//! `lineage-` and `-system.zip`, and the vendor half's
//! `ro.vendor.build.date.utc`, since nothing in it names its zip. `standing`
//! places an installed build against the pin of its own flavour (a VANILLA
//! install is compared with the VANILLA pin): the same build, an older or a
//! newer one by `stamp` (the date in the Lineage version, the vendor's build
//! time), or another line altogether, which is never called older. `behind`
//! turns the older ones into the line `sarab start` and `sarab info` show, since
//! a newer sarab moving its pins otherwise leaves every install on the build
//! it was set up with. `describe` is the readable form. Only Android 13 is
//! accepted (`check_api`, `TESTED_API`), on both halves: the servicemanager
//! codes, the rc rewriting and the composer patch are written against it, and
//! Android's data, which a newer Android would upgrade for good and an older
//! one cannot read. `--latest` or a zip of one's own is how a different
//! Android could arrive, so `extract` checks before the swap and `sarab start`
//! checks the tree it boots.
//!
//! Downloads use curl, not a Rust HTTP stack: it is on every machine, follows
//! SourceForge's mirror redirects, and resumes. `download` shows the speed and
//! the time left, not a bare bar: a first run on 2026-09-26 got about 260 KB/s
//! from the mirror, over an hour for the system half, and nothing said so.
//! Outside the first-run wizard that is curl's own meter, and curl's exit is
//! simply waited for; in it (ui.rs) curl runs silent, with its errors captured,
//! while a `ui::Task` follows the partial file's size, read each time a poll on
//! curl's pidfd (net.rs's `pidfd_open` and `ended_within`) times out after
//! `PROGRESS_EVERY`, so curl's exit ends the wait at once. A download that
//! gives up fails with curl's own reason (captured in the wizard, where
//! nothing else shows it; its exit status outside, after curl printed the
//! reason itself), and the checksum and each unpacking get a spinner
//! named by `plain_name` ("Android", "Android's drivers") instead of the
//! partition. `in_namespace` then sends the extraction's progress lines
//! nowhere and keeps what it says on stderr for the error, so nothing is
//! written across a spinner. A mirror that stalls below 1 KiB/s for a minute
//! counts as failed. curl's own `--retry` starts a download over from its first
//! byte, so `fetch_resuming` loops instead: it reruns curl with `-C -`, which asks
//! for the rest of the partial file, until the file is whole, and gives up after
//! `STALLED_TRIES` attempts in a row that added nothing (a mirror that dropped the
//! connection at 53% of the system half is what showed it). A partial file that
//! is already the full size goes straight to the checksum. Every call goes through
//! `CURL`, which fails on HTTP errors and keeps every hop, mirror redirects
//! included, on HTTPS. A zip is kept under its release's file name, so the
//! build it holds is never in doubt. `unsparse` reads strictly forward, so the
//! image streams straight out of the zip. It reads a file somebody may have
//! handed to `sarab setup`, so it trusts no field: header sizes below the
//! format's own, a block size that is zero, not a multiple of four or over
//! `MAX_SPARSE_BLOCK`, a chunk whose data length is not what its type
//! implies, and chunks that run past the declared size are all refused
//! before anything is sliced, subtracted or allocated.
//!
//! Mounting the image would need root or a loop device; instead `debugfs rdump`
//! copies the tree out of the image file, and it does so *inside the user
//! namespace* (`sarab-ns` with no `--root`): rdump restores each file's owner
//! and mode as it goes, and in there uid 1000 is Android's `system`, mapped
//! into our subuid range, rather than an EPERM. That replaces what used to take
//! four steps -- extract as ourselves, dump the owners from a fakeroot fuse2fs
//! mount, and chown the tree back inside the namespace -- and one of those
//! steps was never written down at all.
//!
//! rdump leaves two things behind, both fixed here from the image itself:
//! symlinks keep our uid (it never lchowns them), and file capabilities are
//! not copied (run-as and simpleperf_app_runner carry them). One more debugfs
//! pass reads both, and we apply them, still inside the namespace -- a
//! capability set there is stored as a v3 xattr rooted at the namespace's
//! uid 0, which is the only form that means anything to the runtime. debugfs
//! parses its own command lines with double quotes, so a path containing a
//! quote cannot be expressed; there are none in an Android image, and `quoted`
//! skips rather than guesses. An existing extracted tree is Android-owned
//! (subuids), so only the namespace can delete it (`remove_inner`), and only
//! an extracted half (it has a `build.prop`) or a `system.partial` or
//! `vendor.partial` is ever deleted that way (`removable`).
//!
//! debugfs exits 0 whatever goes wrong inside a command, and a full disk
//! leaves a tree with an empty `build.prop` in it. So `extract_inner` treats
//! any line on its stderr but the version banner as a failure
//! (`debugfs_errors`), and `extract`
//! works in `<part>.partial`, renamed to `<part>` only once everything
//! succeeded and `check_api` passed: a tree under the real name is always
//! complete, and one an interrupted run left behind is deleted by the next.
//! The tree it replaces stays in place until then, so a failed upgrade (a full
//! disk, a wrong Android) leaves the old image booting; it is renamed to
//! `<part>.old` for the swap and deleted after it, or by the next run when
//! that fails (`LEFTOVER_TREES` are the names `removable` accepts without a
//! `build.prop`, which a half-deleted tree may have lost).

use crate::{net, ui};
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::CString;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MAX_SPARSE_BLOCK: u64 = 1 << 20;

const SPARSE_MAGIC: u32 = 0xED26_FF3A;
const MIB: u64 = 1_048_576;
const CURL: &[&str] = &["-fL", "--proto", "=https", "--proto-redir", "=https"];

fn channel(part: &str, gapps: bool) -> (&'static str, &'static str) {
    match part {
        "system" => ("system/lineage/waydroid_x86_64", if gapps { "GAPPS" } else { "VANILLA" }),
        _ => ("vendor/waydroid_x86_64", "MAINLINE"),
    }
}

pub fn ota_url(part: &str, gapps: bool) -> String {
    let (dir, name) = channel(part, gapps);
    format!("https://ota.waydro.id/{dir}/{name}.json")
}

const PINS: &[(&str, &str, &str, u64, &str)] = &[
    (
        "GAPPS",
        "lineage-20.0-20260403-GAPPS-waydroid_x86_64-system.zip",
        "811ab2dd7ad1b0b4964bddf020fa450275ea1af2d5b0ac10d5ceced0ac1908a3",
        1_190_930_612,
        "20.0-20260403-GAPPS-waydroid_x86_64",
    ),
    (
        "VANILLA",
        "lineage-20.0-20260403-VANILLA-waydroid_x86_64-system.zip",
        "2e343b14c649a685853e7957ac34feb1ca110425d78a47ebad764caae24116a8",
        838_353_482,
        "20.0-20260403-VANILLA-waydroid_x86_64",
    ),
    (
        "MAINLINE",
        "lineage-20.0-20260428-MAINLINE-waydroid_x86_64-vendor.zip",
        "cba35433ffca73ed349e096b1daec495b3204e01c0e541c4203671e6d648e874",
        189_192_029,
        "1777409843",
    ),
];

pub const TESTED_API: &str = "33";

const LEFTOVER_TREES: &[&str] = &["system.partial", "vendor.partial", "system.old", "vendor.old"];

#[derive(Debug, PartialEq)]
pub enum Standing {
    Pinned,
    Older,
    Newer,
    Other,
}

fn pin(part: &str, gapps: bool) -> &'static (&'static str, &'static str, &'static str, u64, &'static str) {
    let name = channel(part, gapps).1;
    PINS.iter().find(|p| p.0 == name).expect("every channel is pinned")
}

fn prop_file(tree: &Path, part: &str) -> PathBuf {
    match part {
        "system" => tree.join("system/build.prop"),
        _ => tree.join("build.prop"),
    }
}

fn prop(tree: &Path, part: &str, system_key: &str, vendor_key: &str) -> Option<String> {
    let key = if part == "system" { system_key } else { vendor_key };
    crate::paths::build_prop(&prop_file(tree, part), key).filter(|v| !v.is_empty())
}

pub fn build_of(tree: &Path, part: &str) -> Option<String> {
    prop(tree, part, "ro.lineage.version", "ro.vendor.build.date.utc")
}

pub fn describe(tree: &Path, part: &str) -> String {
    prop(tree, part, "ro.lineage.version", "ro.vendor.build.date").unwrap_or_else(|| "?".into())
}

pub fn gapps(build: &str) -> bool {
    !build.contains("-VANILLA-")
}

fn stamp(part: &str, build: &str) -> Option<u64> {
    match part {
        "system" => build.split('-').nth(1)?.parse().ok(),
        _ => build.parse().ok(),
    }
}

pub fn standing(part: &str, build: &str) -> Standing {
    let pinned = pin(part, part != "system" || gapps(build)).4;
    if build == pinned {
        return Standing::Pinned;
    }
    let same_line = part != "system" || build.split('-').skip(2).eq(pinned.split('-').skip(2));
    match (stamp(part, build), stamp(part, pinned)) {
        (Some(a), Some(b)) if same_line && a < b => Standing::Older,
        (Some(a), Some(b)) if same_line && a > b => Standing::Newer,
        _ => Standing::Other,
    }
}

pub fn behind(data: &Path) -> Vec<String> {
    ["system", "vendor"]
        .into_iter()
        .filter(|part| build_of(&data.join(part), part).is_some_and(|b| standing(part, &b) == Standing::Older))
        .map(|part| {
            format!(
                "{part}: {} is older than the build this sarab is tested with; `sarab upgrade` moves it \
                 there (your apps and data are kept)",
                describe(&data.join(part), part)
            )
        })
        .collect()
}

pub fn check_api(tree: &Path, part: &str) -> Result<()> {
    let api = prop(tree, part, "ro.build.version.sdk", "ro.vendor.build.version.sdk");
    if api.as_deref() == Some(TESTED_API) {
        return Ok(());
    }
    let release = prop(tree, part, "ro.build.version.release", "ro.vendor.build.version.release");
    bail!(
        "the {part} image in {} is Android {} (API {}); Sarab is written and tested against Android 13 (API {TESTED_API}) \
         only: its binder calls, init rewriting and composer patch, and Android's data, which a newer Android \
         would upgrade and an older one cannot read. `sarab setup --force` puts the tested build back",
        tree.display(),
        release.as_deref().unwrap_or("?"),
        api.as_deref().unwrap_or("?")
    )
}

#[derive(Debug, PartialEq)]
pub struct Release {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

pub fn pinned(part: &str, gapps: bool) -> Release {
    let dir = channel(part, gapps).0;
    let &(_, filename, sha256, size, _) = pin(part, gapps);
    Release {
        filename: filename.into(),
        url: format!("https://sourceforge.net/projects/waydroid/files/images/{dir}/{filename}/download"),
        sha256: sha256.into(),
        size,
    }
}

pub fn parse_ota(json: &str) -> Result<Release> {
    let v: serde_json::Value = serde_json::from_str(json).context("it is not JSON")?;
    let newest = v["response"]
        .as_array()
        .and_then(|a| a.iter().max_by_key(|r| r["datetime"].as_u64().unwrap_or(0)))
        .ok_or_else(|| anyhow!("it has no releases"))?;
    let s = |k: &str| newest[k].as_str().map(str::to_string).ok_or_else(|| anyhow!("its newest release has no {k}"));
    let r = Release {
        filename: s("filename")?,
        url: s("url")?,
        sha256: s("id")?,
        size: newest["size"].as_u64().unwrap_or(0),
    };
    if !r.url.starts_with("https://") {
        bail!("its download address is not https: {}", r.url);
    }
    if r.filename.contains('/') || !r.filename.ends_with(".zip") {
        bail!("its file name is not a plain .zip: {}", r.filename);
    }
    if r.sha256.len() != 64 || !r.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("its checksum is not a SHA-256: {}", r.sha256);
    }
    Ok(r)
}

pub fn release(part: &str, gapps: bool, latest: bool) -> Result<Release> {
    let pin = pinned(part, gapps);
    if !latest {
        return Ok(pin);
    }
    let url = ota_url(part, gapps);
    let listing = Command::new("curl").args(CURL).args(["-sS", "--"]).arg(&url).output().context("run curl")?;
    if !listing.status.success() {
        bail!("could not fetch the {part} image listing ({url})");
    }
    let r = parse_ota(&String::from_utf8_lossy(&listing.stdout)).with_context(|| format!("malformed listing {url}"))?;
    if r.sha256.eq_ignore_ascii_case(&pin.sha256) {
        println!("{part}: the newest build is the tested one");
    } else {
        eprintln!(
            "WARNING: {part}: {} is newer than the build sarab is tested with ({}), and its checksum comes \
             from the same server. If Android does not boot, `sarab setup --force` goes back to the tested one.",
            r.filename, pin.filename
        );
    }
    Ok(r)
}

fn sha256(path: &Path) -> Result<String> {
    let o = Command::new("sha256sum").arg(path).output().context("run sha256sum")?;
    let out = String::from_utf8_lossy(&o.stdout);
    out.split_whitespace().next().map(str::to_string).ok_or_else(|| anyhow!("sha256sum printed nothing"))
}

fn plain_name(part: &str) -> &'static str {
    if part == "system" { "Android" } else { "Android's drivers" }
}

pub fn download(part: &str, r: &Release, dest: &Path) -> Result<()> {
    if !ui::wizard() {
        println!("{part}: downloading {} ({} MiB)", r.filename, r.size / MIB);
        println!("{part}: mirrors can be slow; Ctrl-C is safe, and the next `sarab start` resumes here");
    }
    let part_file = dest.with_extension("zip.part");
    let task = ui::Task::bytes(r.size, format!("Downloading {}", plain_name(part)));
    fetch_resuming(CURL, &r.url, &part_file, r.size, &task)
        .with_context(|| format!("{part}: download failed (run `sarab setup` again to resume)"))?;
    task.done(format!("Downloaded {} ({:.1} GB)", plain_name(part), r.size as f64 / 1e9));
    let check = ui::wizard().then(|| ui::Task::spin("Checking the download"));
    let got = sha256(&part_file)?;
    if !got.eq_ignore_ascii_case(&r.sha256) {
        let _ = std::fs::remove_file(&part_file);
        bail!("{part}: checksum mismatch (got {got}, expected {}); the partial file was removed", r.sha256);
    }
    std::fs::rename(&part_file, dest)?;
    if let Some(c) = check {
        c.done("Download checked");
    }
    Ok(())
}

const STALLED_TRIES: u32 = 3;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

fn fetch_resuming(curl: &[&str], url: &str, part_file: &Path, size: u64, task: &ui::Task) -> Result<()> {
    let len = || std::fs::metadata(part_file).map(|m| m.len()).unwrap_or(0);
    let mut stalled = 0;
    loop {
        if len() == size {
            return Ok(());
        }
        let before = len();
        let mut curl_cmd = Command::new("curl");
        curl_cmd.args(curl);
        if ui::wizard() {
            curl_cmd.arg("-sS").stderr(Stdio::piped());
        }
        let mut child = curl_cmd
            .args(["--speed-limit", "1024", "--speed-time", "60", "-C", "-", "-o"])
            .arg(part_file)
            .arg("--")
            .arg(url)
            .spawn()
            .context("run curl")?;
        if ui::wizard() {
            let pidfd = net::pidfd_open(child.id()).context("pidfd_open curl")?;
            while !net::ended_within(&pidfd, PROGRESS_EVERY) {
                task.set(len());
            }
        }
        let st = child.wait().context("wait for curl")?;
        if st.success() {
            return Ok(());
        }
        let mut why = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut why);
        }
        stalled = if len() > before { 0 } else { stalled + 1 };
        if stalled >= STALLED_TRIES {
            match why.trim() {
                "" => bail!("curl {st}"),
                why => bail!("{why}"),
            }
        }
        task.message(format!("the download broke off at {} MiB; resuming", len() / MIB));
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn unsparse(mut input: impl Read, output: &Path) -> Result<bool> {
    let mut out = BufWriter::with_capacity(1 << 20, std::fs::File::create(output)?);
    let mut hdr = [0u8; 28];
    input.read_exact(&mut hdr).context("image is shorter than a header")?;
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as usize;
    if u32_at(&hdr, 0) != SPARSE_MAGIC {
        out.write_all(&hdr)?;
        std::io::copy(&mut input, &mut out)?;
        out.flush()?;
        return Ok(false);
    }
    let (file_hdr, chunk_hdr) = (u16_at(&hdr, 8), u16_at(&hdr, 10));
    let blk = u32_at(&hdr, 12) as u64;
    let total_blks = u32_at(&hdr, 16) as u64;
    let chunks = u32_at(&hdr, 20);
    if file_hdr < 28 || chunk_hdr < 12 || blk == 0 || !blk.is_multiple_of(4) || blk > MAX_SPARSE_BLOCK {
        bail!("not a sparse image this can read (header {file_hdr}, chunk header {chunk_hdr}, block size {blk})");
    }
    std::io::copy(&mut (&mut input).take(file_hdr as u64 - 28), &mut std::io::sink())?;
    let mut ch = vec![0u8; chunk_hdr];
    let mut at: u64 = 0;
    for n in 0..chunks {
        input.read_exact(&mut ch).with_context(|| format!("sparse chunk {n} is cut short"))?;
        let kind = u16_at(&ch, 0);
        let blocks = u32_at(&ch, 4) as u64;
        let data = (u32_at(&ch, 8) as u64)
            .checked_sub(chunk_hdr as u64)
            .ok_or_else(|| anyhow!("sparse chunk {n} is smaller than its own header"))?;
        let bytes = blocks * blk;
        at += blocks;
        if at > total_blks {
            bail!("sparse chunk {n} runs past the {total_blks} blocks the image declares");
        }
        let want = match kind {
            0xCAC1 => bytes,
            0xCAC2 | 0xCAC4 => 4,
            0xCAC3 => 0,
            k => bail!("unknown sparse chunk type {k:#x}"),
        };
        if data != want {
            bail!("sparse chunk {n} ({kind:#x}) carries {data} bytes, not {want}");
        }
        match kind {
            0xCAC1 => {
                let copied = std::io::copy(&mut (&mut input).take(data), &mut out)?;
                if copied != data {
                    bail!("sparse chunk {n} is cut short");
                }
            }
            0xCAC2 => {
                let mut pat = [0u8; 4];
                input.read_exact(&mut pat)?;
                let buf: Vec<u8> = pat.iter().copied().cycle().take(blk as usize).collect();
                for _ in 0..blocks {
                    out.write_all(&buf)?;
                }
            }
            0xCAC3 => {
                out.flush()?;
                out.get_mut().seek(SeekFrom::Current(bytes as i64))?;
            }
            _ => {
                std::io::copy(&mut (&mut input).take(data), &mut std::io::sink())?;
            }
        }
    }
    out.flush()?;
    out.get_mut().set_len(total_blks * blk)?;
    Ok(true)
}

pub fn unzip_image(zip: &Path, raw: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(zip).with_context(|| format!("open {}", zip.display()))?)
        .with_context(|| format!("{} is not a zip", zip.display()))?;
    let name = z
        .file_names()
        .find(|n| n.ends_with(".img"))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} holds no .img", zip.display()))?;
    let sparse = unsparse(z.by_name(&name)?, raw)?;
    ui::detail(format!("  {name} -> {} ({})", raw.display(), if sparse { "unsparsed" } else { "raw" }));
    Ok(())
}

pub fn in_namespace(dirs: &crate::paths::Dirs, args: &[&str]) -> Result<()> {
    let ns = dirs.helper("sarab-ns")?;
    let exe = crate::paths::own_exe()?;
    let quiet = ui::wizard();
    let mut child = Command::new(&ns)
        .arg("--")
        .arg(&exe)
        .args(args)
        .current_dir("/")
        .stdout(if quiet { Stdio::null() } else { Stdio::inherit() })
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", ns.display()))?;
    let err = child.stderr.take().expect("piped");
    let mut said = Vec::new();
    for line in std::io::BufRead::lines(std::io::BufReader::new(err)).map_while(Result::ok) {
        if line.starts_with("sarab-ns:") {
            continue;
        }
        if quiet {
            said.push(line);
        } else {
            eprintln!("{line}");
        }
    }
    let st = child.wait()?;
    if !st.success() {
        if said.is_empty() {
            bail!("namespace step failed ({st})");
        }
        bail!("namespace step failed ({st}): {}", said.join("\n"));
    }
    Ok(())
}

pub fn extract(dirs: &crate::paths::Dirs, part: &str, zip: &Path) -> Result<()> {
    let images = &dirs.data;
    std::fs::create_dir_all(images).with_context(|| format!("create {}", images.display()))?;
    let raw = images.join(format!("{part}.raw.img"));
    let task = if ui::wizard() {
        Some(ui::Task::spin(format!("Unpacking {} (about a minute)", plain_name(part))))
    } else {
        println!("{part}: unpacking {}", zip.display());
        None
    };
    let done = unzip_image(zip, &raw).and_then(|()| extract_tree(dirs, part, &raw));
    let _ = std::fs::remove_file(&raw);
    if let (Some(t), Ok(())) = (task, &done) {
        t.done(format!("Unpacked {}", plain_name(part)));
    }
    done
}

fn extract_tree(dirs: &crate::paths::Dirs, part: &str, raw: &Path) -> Result<()> {
    let tree = dirs.data.join(part);
    let partial = dirs.data.join(format!("{part}.partial"));
    let replaced = dirs.data.join(format!("{part}.old"));
    let remove = |p: &Path| in_namespace(dirs, &["internal", "remove", &p.display().to_string()]);
    for stale in [&partial, &replaced] {
        if stale.exists() {
            remove(stale)?;
        }
    }
    std::fs::create_dir_all(&partial)?;
    ui::detail(format!("{part}: extracting (about a minute)"));
    in_namespace(dirs, &["internal", "extract", &raw.display().to_string(), &partial.display().to_string()])?;
    if let Err(e) = check_api(&partial, part) {
        remove(&partial)?;
        return Err(e);
    }
    if tree.exists() {
        std::fs::rename(&tree, &replaced)
            .with_context(|| format!("rename {} to {}", tree.display(), replaced.display()))?;
    }
    std::fs::rename(&partial, &tree).with_context(|| format!("rename {} to {}", partial.display(), tree.display()))?;
    if replaced.exists()
        && let Err(e) = remove(&replaced)
    {
        eprintln!(
            "{part}: could not delete the replaced tree {} ({e:#}); the next `sarab setup` does",
            replaced.display()
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    name: String,
}

fn parse_ls_p(line: &str) -> Option<Entry> {
    let f: Vec<&str> = line.strip_prefix('/')?.strip_suffix('/')?.split('/').collect();
    if f.len() != 6 {
        return None;
    }
    Some(Entry {
        mode: u32::from_str_radix(f[1], 8).ok()?,
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        name: f[4].to_string(),
    })
}

fn parse_capability(line: &str) -> Option<Vec<u8>> {
    let hex = line.trim().strip_prefix("security.capability (")?.split_once(") = ")?.1;
    hex.split_whitespace().map(|b| u8::from_str_radix(b, 16).ok()).collect()
}

fn quoted(image_path: &str) -> Option<String> {
    (!image_path.contains('"')).then(|| format!("\"{image_path}\""))
}

fn debugfs_batch(image: &Path, commands: &str) -> Result<String> {
    let mut f = tempfile_in(image.parent().unwrap_or(Path::new(".")))?;
    f.1.write_all(commands.as_bytes())?;
    drop(f.1);
    let o =
        Command::new("debugfs").arg("-f").arg(&f.0).arg(image).stderr(Stdio::null()).output().context("run debugfs")?;
    let _ = std::fs::remove_file(&f.0);
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn tempfile_in(dir: &Path) -> Result<(PathBuf, std::fs::File)> {
    let p = dir.join(format!(".sarab-debugfs-{}", std::process::id()));
    Ok((p.clone(), std::fs::File::create(&p)?))
}

fn walk(dir: &Path, top: &Path, dirs: &mut Vec<String>, execs: &mut Vec<String>) -> Result<()> {
    let rel = |p: &Path| format!("/{}", p.strip_prefix(top).unwrap().display()).replace("//", "/");
    dirs.push(rel(dir));
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let md = e.path().symlink_metadata()?;
        if md.is_dir() {
            walk(&e.path(), top, dirs, execs)?;
        } else if md.is_file() && std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o111 != 0 {
            execs.push(rel(&e.path()));
        }
    }
    Ok(())
}

fn lchown(p: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_capability(p: &Path, value: &[u8]) -> std::io::Result<()> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    let name = c"security.capability";
    if unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn extract_inner(raw: &Path, tree: &Path) -> Result<()> {
    if unsafe { libc::getuid() } != 0 {
        bail!("internal extract must run inside sarab-ns (uid 0 in the namespace)");
    }
    let o = Command::new("debugfs")
        .arg("-R")
        .arg(format!("rdump / {}", tree.display()))
        .arg(raw)
        .stdout(Stdio::null())
        .output()
        .context("run debugfs (e2fsprogs)")?;
    let stderr = String::from_utf8_lossy(&o.stderr);
    let errors = debugfs_errors(&stderr);
    if !o.status.success() || !errors.is_empty() {
        let first: Vec<&str> = errors.iter().take(3).copied().collect();
        bail!(
            "debugfs rdump failed ({}, {} error lines): {}",
            o.status,
            errors.len(),
            if first.is_empty() { "no message".into() } else { first.join("; ") }
        );
    }
    let (mut dirs, mut execs) = (Vec::new(), Vec::new());
    walk(tree, tree, &mut dirs, &mut execs)?;
    let mut cmds = String::new();
    for d in &dirs {
        if let Some(q) = quoted(d) {
            cmds.push_str(&format!("ls -p {q}\n"));
        }
    }
    for f in &execs {
        if let Some(q) = quoted(f) {
            cmds.push_str(&format!("ea_get {q} security.capability\n"));
        }
    }
    let out = debugfs_batch(raw, &cmds)?;
    let (mut links, mut caps) = (0, 0);
    let mut current: Option<(bool, String)> = None;
    for line in out.lines() {
        if let Some(cmd) = line.strip_prefix("debugfs: ") {
            current = cmd.strip_prefix("ls -p ").map(|p| (true, p.trim_matches('"').to_string())).or_else(|| {
                cmd.strip_prefix("ea_get ")
                    .and_then(|r| r.strip_suffix(" security.capability"))
                    .map(|p| (false, p.trim_matches('"').to_string()))
            });
            continue;
        }
        match &current {
            Some((true, dir)) => {
                if let Some(e) = parse_ls_p(line)
                    && e.mode & 0o170000 == 0o120000
                {
                    let p = tree.join(dir.trim_start_matches('/')).join(&e.name);
                    lchown(&p, e.uid, e.gid).with_context(|| format!("lchown {}", p.display()))?;
                    links += 1;
                }
            }
            Some((false, file)) => {
                if let Some(v) = parse_capability(line) {
                    let p = tree.join(file.trim_start_matches('/'));
                    set_capability(&p, &v).with_context(|| format!("setxattr {}", p.display()))?;
                    caps += 1;
                    println!("  capability restored: {file}");
                }
            }
            None => {}
        }
    }
    println!("  {} directories, {links} symlink owners, {caps} file capabilities", dirs.len());
    Ok(())
}

fn debugfs_errors(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("debugfs ")).collect()
}

fn removable(tree: &Path) -> bool {
    let named = tree.file_name().and_then(|n| n.to_str()).is_some_and(|n| LEFTOVER_TREES.contains(&n));
    named || tree.join("build.prop").exists() || tree.join("system/build.prop").exists()
}

pub fn remove_inner(tree: &Path) -> Result<()> {
    if !removable(tree) {
        bail!("{} does not look like an extracted image; not deleting it", tree.display());
    }
    std::fs::remove_dir_all(tree).with_context(|| format!("remove {}", tree.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_download_resumes_where_it_broke_off() {
        use std::io::{BufRead, BufReader, Write};
        let data: Vec<u8> = (0..400_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/f.zip", listener.local_addr().unwrap());
        let served = data.clone();
        let server = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            for (n, conn) in listener.incoming().take(2).enumerate() {
                let mut conn = conn.unwrap();
                let mut range = None;
                for line in BufReader::new(conn.try_clone().unwrap()).lines().map_while(Result::ok) {
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Range: bytes=") {
                        range = v.trim_end_matches('-').parse::<usize>().ok();
                    }
                }
                ranges.push(range);
                let from = range.unwrap_or(0);
                let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                    served.len() - from,
                    served.len() - 1,
                    served.len()
                );
                conn.write_all(head.as_bytes()).unwrap();
                let body = &served[from..];
                conn.write_all(if n == 0 { &body[..body.len() / 2] } else { body }).unwrap();
            }
            ranges
        });
        let dir = std::env::temp_dir().join(format!("sarab-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("f.zip.part");
        let _ = std::fs::remove_file(&part);
        fetch_resuming(&["-fsS"], &url, &part, data.len() as u64, &ui::Task::bytes(0, "")).unwrap();
        assert_eq!(server.join().unwrap(), [None, Some(data.len() / 2)], "the second request asks for the rest");
        assert_eq!(std::fs::read(&part).unwrap(), data);
        assert!(
            fetch_resuming(&["-fsS"], "http://127.0.0.1:9/never", &part, data.len() as u64, &ui::Task::bytes(0, ""))
                .is_ok(),
            "whole already"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn listing(releases: &[(u64, &str, &str, &str)]) -> String {
        let r: Vec<String> = releases
            .iter()
            .map(|(t, f, id, url)| {
                format!(r#"{{"datetime": {t}, "filename": "{f}", "id": "{id}", "size": {t}, "url": "{url}"}}"#)
            })
            .collect();
        format!(r#"{{"response":[{}]}}"#, r.join(","))
    }

    #[test]
    fn ota_takes_the_newest_release_and_only_a_sane_one() {
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "C".repeat(64));
        let j = listing(&[
            (1, "old.zip", &a, "https://x/1"),
            (3, "new.zip", &c, "https://x/3"),
            (2, "mid.zip", &b, "https://x/2"),
        ]);
        assert_eq!(
            parse_ota(&j).unwrap(),
            Release { filename: "new.zip".into(), url: "https://x/3".into(), sha256: c.clone(), size: 3 }
        );
        assert!(parse_ota(r#"{"response":[]}"#).is_err());
        assert!(parse_ota("<html>").is_err());
        assert!(parse_ota(&listing(&[(1, "new.zip", &c, "http://x/3")])).is_err());
        assert!(parse_ota(&listing(&[(1, "new.zip", &c, "-o/etc/x")])).is_err());
        assert!(parse_ota(&listing(&[(1, "../new.zip", &c, "https://x/3")])).is_err());
        assert!(parse_ota(&listing(&[(1, "new.img", &c, "https://x/3")])).is_err());
        assert!(parse_ota(&listing(&[(1, "new.zip", "cc", "https://x/3")])).is_err());
        assert!(ota_url("system", true).ends_with("/system/lineage/waydroid_x86_64/GAPPS.json"));
        assert!(ota_url("system", false).ends_with("/VANILLA.json"));
        assert!(ota_url("vendor", true).ends_with("/vendor/waydroid_x86_64/MAINLINE.json"));
    }

    #[test]
    fn every_channel_is_pinned_to_one_build_of_lineage_20() {
        for (part, gapps, channel) in
            [("system", true, "GAPPS"), ("system", false, "VANILLA"), ("vendor", true, "MAINLINE")]
        {
            let r = pinned(part, gapps);
            assert!(r.filename.starts_with("lineage-20.0-") && r.filename.ends_with(&format!("-{part}.zip")));
            assert!(r.filename.contains(&format!("-{channel}-")));
            assert!(r.url.starts_with("https://") && r.url.ends_with(&format!("/{}/download", r.filename)));
            assert!(r.url.contains(if part == "system" { "/system/lineage/" } else { "/vendor/" }));
            assert!(r.sha256.len() == 64 && r.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(r.size > 100 * MIB);
        }
        assert_eq!(pinned("vendor", true), pinned("vendor", false));
        let date = |r: Release| r.filename.split('-').nth(2).unwrap().to_string();
        assert_eq!(date(pinned("system", true)), date(pinned("system", false)));
    }

    #[test]
    fn debugfs_errors_are_whatever_is_not_the_banner() {
        assert!(debugfs_errors("debugfs 1.47.4 (6-Mar-2025)\n").is_empty());
        assert!(debugfs_errors("").is_empty());
        let full = "debugfs 1.47.4 (6-Mar-2025)\nrdump: No space left on device while writing file\n\n\
                    dump_file: Invalid argument while changing ownership of /x/system/big\n";
        assert_eq!(
            debugfs_errors(full),
            [
                "rdump: No space left on device while writing file",
                "dump_file: Invalid argument while changing ownership of /x/system/big"
            ]
        );
    }

    #[test]
    fn only_an_image_or_an_interrupted_extraction_is_removed() {
        let d = std::env::temp_dir().join(format!("sarab-removable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for t in ["system/system", "vendor", "home", "system.partial", "vendor.partial", "data.partial"] {
            std::fs::create_dir_all(d.join(t)).unwrap();
        }
        assert!(!removable(&d.join("system")));
        std::fs::write(d.join("system/system/build.prop"), "").unwrap();
        std::fs::write(d.join("vendor/build.prop"), "").unwrap();
        assert!(removable(&d.join("system")) && removable(&d.join("vendor")));
        assert!(removable(&d.join("system.partial")) && removable(&d.join("vendor.partial")));
        assert!(!removable(&d.join("home")) && !removable(&d.join("data.partial")));
        assert!(remove_inner(&d.join("home")).is_err() && d.join("home").is_dir());
        std::fs::remove_dir_all(&d).unwrap();
    }

    fn sparse(chunks: &[(u16, u32, Vec<u8>)], blk: u32, total: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(SPARSE_MAGIC.to_le_bytes());
        v.extend(1u16.to_le_bytes());
        v.extend(0u16.to_le_bytes());
        v.extend(28u16.to_le_bytes());
        v.extend(12u16.to_le_bytes());
        v.extend(blk.to_le_bytes());
        v.extend(total.to_le_bytes());
        v.extend((chunks.len() as u32).to_le_bytes());
        v.extend(0u32.to_le_bytes());
        for (kind, blocks, data) in chunks {
            v.extend(kind.to_le_bytes());
            v.extend(0u16.to_le_bytes());
            v.extend(blocks.to_le_bytes());
            v.extend((12 + data.len() as u32).to_le_bytes());
            v.extend(data);
        }
        v
    }

    #[test]
    fn unsparse_handles_raw_fill_hole_and_crc() {
        let d = std::env::temp_dir().join(format!("sarab-unsparse-{}", std::process::id()));
        let img = sparse(
            &[
                (0xCAC1, 1, vec![7u8; 8]),
                (0xCAC2, 2, vec![1, 2, 3, 4]),
                (0xCAC3, 1, vec![]),
                (0xCAC4, 0, vec![9, 9, 9, 9]),
            ],
            8,
            5,
        );
        assert!(unsparse(&img[..], &d).unwrap());
        let out = std::fs::read(&d).unwrap();
        let mut want = vec![7u8; 8];
        want.extend([1, 2, 3, 4].repeat(4));
        want.extend([0u8; 8]);
        want.extend([0u8; 8]);
        assert_eq!(out, want);
        assert!(!unsparse(&b"just some ext4 bytes, honestly"[..], &d).unwrap());
        assert_eq!(std::fs::read(&d).unwrap(), b"just some ext4 bytes, honestly");
        std::fs::remove_file(&d).unwrap();
    }

    #[test]
    fn unsparse_refuses_what_it_cannot_trust() {
        let d = std::env::temp_dir().join(format!("sarab-unsparse-bad-{}", std::process::id()));
        let good = sparse(&[(0xCAC1, 1, vec![7u8; 8])], 8, 1);
        let with = |at: usize, bytes: &[u8]| {
            let mut v = good.clone();
            v[at..at + bytes.len()].copy_from_slice(bytes);
            v
        };
        for (why, img) in [
            ("chunk header under 12", with(10, &8u16.to_le_bytes())),
            ("file header under 28", with(8, &20u16.to_le_bytes())),
            ("block size 0", with(12, &0u32.to_le_bytes())),
            ("block size not a multiple of 4", with(12, &6u32.to_le_bytes())),
            ("block size of 4 GiB", with(12, &0xFFFF_FFFCu32.to_le_bytes())),
            ("chunk smaller than its header", with(28 + 8, &4u32.to_le_bytes())),
            ("raw data shorter than its blocks", with(28 + 4, &2u32.to_le_bytes())),
            ("more blocks than declared", with(16, &0u32.to_le_bytes())),
            ("cut short", good[..good.len() - 3].to_vec()),
            ("fill without its pattern", sparse(&[(0xCAC2, 1, vec![])], 8, 1)),
        ] {
            assert!(unsparse(&img[..], &d).is_err(), "{why}");
        }
        assert!(unsparse(&good[..], &d).unwrap());
        std::fs::remove_file(&d).unwrap();
    }

    #[test]
    fn debugfs_output_parses() {
        assert_eq!(
            parse_ls_p("/1194/120755/0/2000/[/6/"),
            Some(Entry { mode: 0o120755, uid: 0, gid: 2000, name: "[".into() })
        );
        assert_eq!(parse_ls_p("/52/040755/0/0/..//").unwrap().name, "..");
        assert_eq!(parse_ls_p("debugfs 1.47"), None);
        assert_eq!(
            parse_capability("security.capability (20) = 01 00 00 02 c0 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 "),
            Some(vec![1, 0, 0, 2, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(parse_capability("ea_get: Extended attribute key not found"), None);
        assert_eq!(quoted("/system/bin/sh").as_deref(), Some("\"/system/bin/sh\""));
        assert_eq!(quoted("/a\"b"), None);
    }

    #[test]
    fn a_build_is_placed_against_the_pin_of_its_own_flavour() {
        assert_eq!(standing("system", "20.0-20260403-GAPPS-waydroid_x86_64"), Standing::Pinned);
        assert_eq!(standing("system", "20.0-20260403-VANILLA-waydroid_x86_64"), Standing::Pinned);
        assert_eq!(standing("system", "20.0-20260101-GAPPS-waydroid_x86_64"), Standing::Older);
        assert_eq!(standing("system", "20.0-20260101-VANILLA-waydroid_x86_64"), Standing::Older);
        assert_eq!(standing("system", "20.0-20261201-GAPPS-waydroid_x86_64"), Standing::Newer);
        assert_eq!(standing("system", "20.0-20260101-GAPPS-waydroid_arm64"), Standing::Other);
        assert_eq!(standing("system", "UNOFFICIAL"), Standing::Other);
        assert_eq!(standing("vendor", "1777409843"), Standing::Pinned);
        assert_eq!(standing("vendor", "1760000000"), Standing::Older);
        assert_eq!(standing("vendor", "1790000000"), Standing::Newer);
        assert_eq!(standing("vendor", "Tue Apr 28"), Standing::Other);
        assert!(gapps("20.0-20260403-GAPPS-waydroid_x86_64") && !gapps("20.0-20260403-VANILLA-waydroid_x86_64"));
    }

    #[test]
    fn only_the_tested_android_passes() {
        let d = std::env::temp_dir().join(format!("sarab-api-{}", std::process::id()));
        std::fs::create_dir_all(d.join("system")).unwrap();
        let write = |text: &str| std::fs::write(d.join("system/build.prop"), text).unwrap();
        write("ro.build.version.release=13\nro.build.version.sdk=33\nro.lineage.version=20.0-20260403-GAPPS-x\n");
        assert!(check_api(&d, "system").is_ok());
        assert_eq!(build_of(&d, "system").as_deref(), Some("20.0-20260403-GAPPS-x"));
        write("ro.build.version.release=14\nro.build.version.sdk=34\n");
        let e = check_api(&d, "system").unwrap_err().to_string();
        assert!(e.contains("is Android 14 (API 34)") && e.contains("--force"), "{e}");
        write("ro.build.version.sdk=\n");
        assert!(check_api(&d, "system").unwrap_err().to_string().contains("Android ? (API ?)"));
        std::fs::write(d.join("build.prop"), "ro.vendor.build.version.sdk=33\nro.vendor.build.date.utc=1\n").unwrap();
        assert!(check_api(&d, "vendor").is_ok());
        assert_eq!(build_of(&d, "vendor").as_deref(), Some("1"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
