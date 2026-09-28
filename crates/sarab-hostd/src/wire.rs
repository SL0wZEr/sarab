//! Small helpers shared by the five services.
//!
//! The image's Java side speaks plain (non-AIDL-generated) binder: every
//! reply begins with an int32 exception code, which is what
//! `Status::from(ExceptionCode::None)` serialises to (`ok`). Arguments arrive
//! after the interface token, which rsbinder has already consumed by the
//! time `on_transact` runs. AIDL `boolean` is marshalled as int32.
//!
//! `Gate` admits Android's `system` uid (1000) and nothing else. That is
//! system_server, where the image's framework patches live and check
//! Android's own permissions before calling us, and every app that shares its
//! uid: the platform-signed system apps (Settings among them) and, because
//! the image is signed with AOSP's public test keys, any app signed with that
//! key (SECURITY.md). A check on the calling pid would not narrow it, since
//! those processes can name themselves anything. What the gate does keep out
//! is every ordinary app: without SELinux any app could look the services up
//! in the servicemanager and call them directly, skipping those checks (read
//! the host clipboard without focus, stop the runtime, post notifications
//! under another app's name). The binder driver reports the caller's euid as
//! the receiver's user namespace sees it, i.e. the host's, so `system` is
//! translated through the runtime's `/proc/<init>/uid_map` (100999 with a
//! subuid range starting at 100000). Refusals are logged: the legitimate
//! caller never trips the gate, so a refusal means an app is probing.
//! `Gate::for_uid` builds one without a runtime, for tests.

use anyhow::{Context, Result};
use rsbinder::*;

macro_rules! logln {
    ($($arg:tt)*) => {{
        eprintln!("[sarab-hostd] {}", format!($($arg)*));
    }};
}
pub(crate) use logln;

const ANDROID_SYSTEM: u32 = 1000;

#[derive(Clone, Copy)]
pub struct Gate {
    system: u32,
}

impl Gate {
    pub fn for_runtime(init_pid: u32) -> Result<Self> {
        let path = format!("/proc/{init_pid}/uid_map");
        let map = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
        let system =
            host_uid(&map, ANDROID_SYSTEM).with_context(|| format!("uid {ANDROID_SYSTEM} is not mapped in {path}"))?;
        Ok(Self { system })
    }

    #[cfg(test)]
    pub fn for_uid(system: u32) -> Self {
        Self { system }
    }

    pub fn check(&self, service: &str) -> rsbinder::Result<()> {
        let uid = get_calling_uid();
        if uid == self.system {
            return Ok(());
        }
        logln!("{service}: refused a call from host uid {uid} (only Android's system uid, {}, may call)", self.system);
        Err(StatusCode::PermissionDenied)
    }
}

fn host_uid(uid_map: &str, inside: u32) -> Option<u32> {
    uid_map.lines().find_map(|l| {
        let mut f = l.split_whitespace().map(|w| w.parse::<u32>().ok());
        let (Some(Some(first)), Some(Some(outside)), Some(Some(count))) = (f.next(), f.next(), f.next()) else {
            return None;
        };
        (inside >= first && inside - first < count).then(|| outside + (inside - first))
    })
}

pub fn str_arg(p: &mut Parcel) -> rsbinder::Result<String> {
    Ok(p.read::<Option<String>>()?.unwrap_or_default())
}

pub fn bool_arg(p: &mut Parcel) -> rsbinder::Result<bool> {
    Ok(p.read::<i32>()? != 0)
}

pub fn ok(reply: &mut Parcel) -> rsbinder::Result<()> {
    reply.write(&Status::from(ExceptionCode::None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_translates_through_the_runtime_uid_map() {
        let map = "         0       1000          1\n         1     100000      29999\n     90000     129999      10000\n\
                   2147483647     139999          1\n";
        assert_eq!(host_uid(map, 0), Some(1000));
        assert_eq!(host_uid(map, ANDROID_SYSTEM), Some(100_999));
        assert_eq!(host_uid(map, 10_113), Some(110_112));
        assert_eq!(host_uid(map, 99_000), Some(138_999));
        assert_eq!(host_uid(map, 50_000), None);
        assert_eq!(host_uid(map, 100_000), None);
        assert_eq!(host_uid("", ANDROID_SYSTEM), None);
    }
}
