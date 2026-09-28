//! The id maps sarab-ns writes for Android, and the check of the user's sub-id
//! ranges against them: the one place these numbers live. The `sarab` command
//! uses the same library for `sarab info` and `sarab setup`, so what they
//! report cannot drift from what sarab-ns does. It needs nothing but std.
//!
//! Android's ids for one user run to 99999, but sparsely, and the 65536 sub-ids
//! every distro hands a new user (`SUB_UID_COUNT`) are enough once only the
//! used ranges are mapped. Android 13's layout (android_filesystem_config.h):
//! system ids below 10000, apps 10000-19999, SDK-sandbox uids and per-app cache
//! gids 20000-29999, ext gids 30000-39999, ext cache gids 40000-49999, shared
//! gids 50000-59999, app-zygote and isolated ids (WebView renderers)
//! 90000-99999, nothing in 60000-89999. The uid and gid maps are separate
//! files, so each gets its own fixed layout, `UID_MAP` and `GID_MAP`, as
//! (Android id, offset into the sub-id range, count), plus 0 → the caller in
//! both. The gids decide the size: every app process gets its shared gid
//! (50000+appId) and cache gid (20000+appId) as supplementary groups, so an
//! unmapped one makes zygote's setgroups fail with EINVAL and no app starts.
//! Uids need `SUBUIDS_NEEDED` (40000) sub-ids and gids `SUBGIDS_NEEDED`
//! (60000), both computed from the maps by `needed`.
//!
//! The layout is one constant, never chosen per machine: the host owner of
//! every file under Android's /data follows from it, so a map that changed with
//! the size of /etc/subuid would change ownership when the range grew. The
//! kernel does not need host ids in order, so everything that owns files on
//! disk (ids below 30000, shared gids) sits at the offset the contiguous map
//! of earlier versions gave it, and only the isolated ids, which own nothing
//! persistent, moved into the gap. Ext gids go where ext cache gids would
//! have been; Android 13 with FUSE storage uses neither on /data, so nothing on
//! disk moves.
//!
//! The ends of ranges. The kernel resolves both ends of a range the namespace
//! writes through its maps, and refuses the write with EINVAL when an end is
//! unmapped, so `ALL_IDS` (INT_MAX) gets a one-id extent in both maps, above
//! every real Android id's host id. In the uid map it is for netd:
//! ConnectivityService hands it uid ranges ending at 99999, plus the default
//! network's 0..INT_MAX, and an `FRA_UID_RANGE` rule with an unmapped end
//! fails, the interface never joins its netId, and every socket gets
//! ENETUNREACH while `ip addr` looks fine; 99999 is mapped with the isolated
//! ids. In the gid map it is for init's `write
//! /proc/sys/net/ipv4/ping_group_range "0 2147483647"` (init.rc): without it
//! the write fails and every ping socket gets EPERM.
//!
//! `check` compares the user's first line in /etc/subuid and /etc/subgid, the
//! one range the maps are laid onto (`first_range`), with what they take, and
//! says how to fix a short or missing one in a single usermod line. It widens
//! a range in place only when the ids after it belong to no other entry of
//! that file; otherwise, and for a missing range, it proposes a fresh 65536
//! from `free_start`, above every entry in both files, so it never hands out
//! ids another user already has.
//!
//! `apparmor` is the other thing both sides must agree on: Ubuntu's
//! user-namespace restriction, the profile that lifts it for sarab-ns, and the
//! one fix line sarab-ns, `sarab setup`, `sarab info` and install.sh all give.

pub mod apparmor;

pub const ALL_IDS: u32 = i32::MAX as u32;
pub type Map = &'static [(u32, u32, u32)];
pub const UID_MAP: Map = &[(1, 0, 29_999), (90_000, 29_999, 10_000), (ALL_IDS, 39_999, 1)];
pub const GID_MAP: Map = &[
    (1, 0, 29_999),
    (90_000, 29_999, 10_000),
    (30_000, 39_999, 10_000),
    (50_000, 49_999, 10_000),
    (ALL_IDS, 59_999, 1),
];
pub const SUBUIDS_NEEDED: u32 = needed(UID_MAP);
pub const SUBGIDS_NEEDED: u32 = needed(GID_MAP);
const FRESH: u64 = 65_536;

pub const fn needed(map: Map) -> u32 {
    let (mut i, mut top) = (0, 0);
    while i < map.len() {
        let end = map[i].1 + map[i].2;
        if end > top {
            top = end;
        }
        i += 1;
    }
    top
}

fn entries(text: &str) -> impl Iterator<Item = (&str, u64, u64)> {
    text.lines().filter_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        Some((name, f.next()?.trim().parse().ok()?, f.next()?.trim().parse().ok()?))
    })
}

pub fn first_range(text: &str, user: &str) -> Option<(u64, u64)> {
    entries(text).find(|e| e.0 == user).map(|(_, s, c)| (s, c))
}

pub fn free_start(files: &[&str]) -> u64 {
    files.iter().flat_map(|t| entries(t)).map(|(_, s, c)| s + c).fold(100_000, u64::max)
}

pub fn check(user: &str, subuid: &str, subgid: &str) -> Result<(), String> {
    let fresh = free_start(&[subuid, subgid]);
    let (mut problems, mut flags) = (Vec::new(), Vec::new());
    for (file, text, need, kind) in [
        ("/etc/subuid", subuid, SUBUIDS_NEEDED as u64, "subuids"),
        ("/etc/subgid", subgid, SUBGIDS_NEEDED as u64, "subgids"),
    ] {
        match first_range(text, user) {
            Some((_, have)) if have >= need => continue,
            Some((start, have)) => {
                problems.push(format!("{file} gives {user} {have} sub-ids and Android needs {need}"));
                let own = entries(text).position(|e| e.0 == user);
                let taken = entries(text)
                    .enumerate()
                    .any(|(i, (_, s, c))| Some(i) != own && s < start + need && start + have < s + c);
                flags.push(format!("--del-{kind} {start}-{}", start + have - 1));
                if taken {
                    flags.push(format!("--add-{kind} {fresh}-{}", fresh + FRESH - 1));
                } else {
                    flags.push(format!("--add-{kind} {start}-{}", start + need - 1));
                }
            }
            None => {
                problems.push(format!("{file} has no range for {user}"));
                flags.push(format!("--add-{kind} {fresh}-{}", fresh + FRESH - 1));
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{}.\nFix (once; usermod refuses while you are logged in, so from another account or a TTY):\n  \
         sudo usermod {} {user}",
        problems.join("; "),
        flags.join(" ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap(a: (u64, u64), b: (u64, u64)) -> bool {
        a.0 < b.0 + b.1 && b.0 < a.0 + a.1
    }

    fn host(map: Map, id: u32) -> Option<u32> {
        map.iter().find(|&&(i, _, c)| id >= i && id - i < c).map(|&(i, o, _)| o + (id - i))
    }

    #[test]
    fn both_maps_fit_the_default_65536_and_never_overlap() {
        assert_eq!((SUBUIDS_NEEDED, SUBGIDS_NEEDED), (40_000, 60_000));
        for map in [UID_MAP, GID_MAP] {
            assert!(needed(map) as u64 <= FRESH);
            assert!(map.len() < 340, "the kernel takes at most 340 extents, the caller's included");
            for (i, &(ia, oa, ca)) in map.iter().enumerate() {
                assert!(ia >= 1, "0 is the caller's");
                for &(ib, ob, cb) in &map[i + 1..] {
                    assert!(!overlap((ia as u64, ca as u64), (ib as u64, cb as u64)), "inside ids overlap");
                    assert!(!overlap((oa as u64, ca as u64), (ob as u64, cb as u64)), "host ids overlap");
                }
            }
        }
    }

    #[test]
    fn every_id_android_13_uses_is_mapped() {
        for uid in [1000, 1099, 2000, 9999, 10000, 10250, 19999, 20000, 29999, 90000, 99000, 99999] {
            assert!(host(UID_MAP, uid).is_some(), "uid {uid}");
        }
        for gid in [1000, 3003, 9999, 10250, 20250, 29999, 30250, 39999, 50000, 50250, 59999, 99999] {
            assert!(host(GID_MAP, gid).is_some(), "gid {gid}");
        }
        assert!(host(UID_MAP, 60_000).is_none() && host(UID_MAP, 50_250).is_none());
    }

    #[test]
    fn both_ends_of_the_ranges_android_writes_are_mapped() {
        for (map, what) in [(UID_MAP, "netd's uid ranges"), (GID_MAP, "init's ping_group_range")] {
            let ends: Vec<Option<u32>> = [0, 99_999, ALL_IDS].iter().map(|&id| host(map, id)).collect();
            assert!(ends[1..].iter().all(Option::is_some), "{what}: an end is unmapped");
            let top = ends[2].unwrap();
            assert!(map.iter().filter(|e| e.0 != ALL_IDS).all(|&(_, o, c)| o + c - 1 < top), "{what}");
        }
    }

    #[test]
    fn what_owns_files_keeps_the_host_id_the_contiguous_map_gave_it() {
        let contiguous = |id: u32| id - 1;
        for id in [1, 1000, 2000, 10000, 10250, 19999, 20250, 29999] {
            assert_eq!(host(UID_MAP, id), Some(contiguous(id)), "uid {id}");
            assert_eq!(host(GID_MAP, id), Some(contiguous(id)), "gid {id}");
        }
        for gid in [50000, 50250, 59999] {
            assert_eq!(host(GID_MAP, gid), Some(contiguous(gid)), "shared gid {gid}");
        }
    }

    #[test]
    fn the_first_range_is_the_one_that_counts() {
        let t = "root:100000:65536\nme:165536:65536\nme:500000:10\nbad:x:y\n";
        assert_eq!(first_range(t, "me"), Some((165_536, 65_536)));
        assert_eq!(first_range(t, "nobody"), None);
        assert_eq!(first_range(t, "bad"), None);
        assert_eq!(free_start(&[t, "other:900000:5\n"]), 900_005);
        assert_eq!(free_start(&["", ""]), 100_000);
    }

    #[test]
    fn the_default_range_needs_no_fix() {
        let t = "me:100000:65536\nyou:165536:65536\n";
        assert_eq!(check("me", t, t), Ok(()));
    }

    #[test]
    fn a_short_range_with_room_after_it_is_widened_in_place() {
        let t = "me:100000:50000\n";
        let m = check("me", t, t).unwrap_err();
        assert!(!m.contains("/etc/subuid"), "the uid range is long enough: {m}");
        assert!(m.contains("/etc/subgid gives me 50000 sub-ids and Android needs 60000"), "{m}");
        assert!(m.ends_with("sudo usermod --del-subgids 100000-149999 --add-subgids 100000-159999 me"), "{m}");
    }

    #[test]
    fn a_short_range_up_against_another_user_gets_a_fresh_one_above_everything() {
        let t = "me:100000:50000\nyou:150000:65536\n";
        let m = check("me", t, t).unwrap_err();
        assert!(m.ends_with("sudo usermod --del-subgids 100000-149999 --add-subgids 215536-281071 me"), "{m}");
    }

    #[test]
    fn a_missing_range_gets_a_fresh_one_nobody_has() {
        let m = check("me", "you:100000:65536\n", "").unwrap_err();
        assert!(m.starts_with("/etc/subuid has no range for me; /etc/subgid has no range for me."), "{m}");
        assert!(m.ends_with("sudo usermod --add-subuids 165536-231071 --add-subgids 165536-231071 me"), "{m}");
    }
}
