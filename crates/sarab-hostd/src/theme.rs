//! Android's dark theme follows the desktop's. The desktop's choice is the
//! XDG desktop portal's `org.freedesktop.appearance` `color-scheme` (1 prefer
//! dark, 2 prefer light, 0 no preference), which every portal backend
//! publishes from its own settings: GNOME's `color-scheme` gsetting, which is
//! also what Omarchy's `omarchy-theme-set-gnome` writes from each theme's
//! mode, KDE's colour scheme, and so on. The portal emits `SettingChanged`
//! whenever it changes, so `follow` reads the value once and then waits on
//! that signal (matched on namespace and key, so the other settings the portal
//! announces never wake us); there is no polling on the host side.
//!
//! Android's side is `IUiModeManager` (service `uimode`) in system_server:
//! `setNightMode` (transaction 5 in Android 13's framework.jar) with
//! `MODE_NIGHT_YES` (2) or `MODE_NIGHT_NO` (1). It is the call behind
//! `cmd uimode night` and the quick-settings tile: it applies at once, recreates
//! the visible activities in the new theme and persists in Secure
//! `ui_night_mode`. The image locks night mode (`mNightModeLocked`), so the
//! caller needs `MODIFY_DAY_NIGHT_MODE`; we pass because the host user is
//! mapped to Android's root. `night_mode` sends dark only for an explicit
//! "prefer dark": GNOME's light setting is "default", which the portal reports
//! as no preference, and the desktop's apps render light for it, so Android
//! does too. `apply` reads `getNightMode` (6) first and leaves an equal mode
//! alone, since every set recreates activities.
//!
//! uimode registers early, but setting the mode works only once the window
//! manager is ready: called the moment uimode appeared, `setNightMode` threw
//! from `clearSnapshotCache` on a null WindowManagerInternal. So the mode is
//! applied when Android says its user is unlocked, which it tells
//! usermonitor.rs after every boot and after every system_server restart, and
//! which comes long after the window manager is up: `follow` takes that as a
//! channel (`unlocked`). `Wanted` holds the desktop's current choice and
//! whether Android has been ready yet; a desktop change made before that is
//! only recorded, and applied at the unlock, and one made after is applied at
//! once. `apply` looks uimode up each time, since a restarted system_server
//! registers a new one, and wakes a paused Android first
//! (`sarab_runtime::thaw`): hostd is outside the frozen cgroup, and a binder
//! call into a frozen system_server would hold this thread until something else
//! woke it; the idle watcher pauses it again later. A desktop without the
//! portal, or without the setting, makes `follow` fail at startup, and hostd
//! runs without this service. A change made inside Android stays until the
//! desktop changes again, or until Android's next unlock, when it is set to the
//! desktop's value. It needs usermonitor for that signal, so main.rs does not
//! start it without usermonitor.

use crate::wire::logln;
use anyhow::{Context, Result, anyhow, bail};
use rsbinder::*;
use sarab_runtime::check_service;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use zbus::zvariant::OwnedValue;

pub const SERVICE_NAME: &str = "theme";

const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";
const PREFER_DARK: u32 = 1;

const UIMODE: &str = "uimode";
const SET_NIGHT_MODE: TransactionCode = 5;
const GET_NIGHT_MODE: TransactionCode = 6;
const MODE_NIGHT_NO: i32 = 1;
const MODE_NIGHT_YES: i32 = 2;

fn night_mode(scheme: u32) -> i32 {
    if scheme == PREFER_DARK { MODE_NIGHT_YES } else { MODE_NIGHT_NO }
}

fn portal(conn: &zbus::blocking::Connection) -> zbus::Result<zbus::blocking::Proxy<'static>> {
    zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop".to_string(),
        "/org/freedesktop/portal/desktop".to_string(),
        "org.freedesktop.portal.Settings".to_string(),
    )
}

fn scheme(value: OwnedValue) -> Result<u32> {
    u32::try_from(value).map_err(|e| anyhow!("{KEY} is not a uint32: {e}"))
}

pub fn follow(sm: SIBinder, unlocked: Receiver<()>) -> Result<()> {
    let conn = zbus::blocking::Connection::session().context("connect to session bus")?;
    let p = portal(&conn).context("open the desktop portal's Settings")?;
    let value: OwnedValue =
        p.call("ReadOne", &(NAMESPACE, KEY)).with_context(|| format!("the desktop portal has no {NAMESPACE} {KEY}"))?;
    let first = scheme(value)?;
    let changes = p
        .receive_signal_with_args("SettingChanged", &[(0, NAMESPACE), (1, KEY)])
        .context("subscribe to SettingChanged")?;
    logln!("theme: desktop color-scheme is {first}; Android follows it");
    let wanted = Arc::new(Mutex::new(Wanted { mode: night_mode(first), ready: false }));
    let on_unlock = (sm.clone(), wanted.clone());
    std::thread::spawn(move || {
        let (sm, wanted) = on_unlock;
        for () in unlocked {
            let mut w = wanted.lock().unwrap();
            w.ready = true;
            apply(&sm, w.mode);
        }
    });
    std::thread::spawn(move || {
        for msg in changes {
            match msg
                .body()
                .deserialize::<(String, String, OwnedValue)>()
                .map_err(anyhow::Error::from)
                .and_then(|b| scheme(b.2))
            {
                Ok(s) => {
                    let mut w = wanted.lock().unwrap();
                    w.mode = night_mode(s);
                    if w.ready {
                        apply(&sm, w.mode);
                    }
                }
                Err(e) => logln!("theme: ignoring a SettingChanged: {e:#}"),
            }
        }
        logln!("theme: the portal's signal stream ended; no longer following the desktop");
    });
    Ok(())
}

struct Wanted {
    mode: i32,
    ready: bool,
}

fn apply(sm: &SIBinder, mode: i32) {
    let name = if mode == MODE_NIGHT_YES { "dark" } else { "light" };
    if let Err(e) = sarab_runtime::thaw() {
        return logln!("theme: cannot wake Android to make it {name}: {e:#}");
    }
    let uimode = match check_service(sm, UIMODE) {
        Ok(Some(b)) => b,
        Ok(None) => return logln!("theme: Android has no {UIMODE} service; not made {name}"),
        Err(e) => return logln!("theme: cannot look up {UIMODE}: {e:#}"),
    };
    let result = call(&uimode, GET_NIGHT_MODE, None).and_then(|now| {
        if now == Some(mode) {
            return Ok(false);
        }
        call(&uimode, SET_NIGHT_MODE, Some(mode)).map(|_| true)
    });
    match result {
        Ok(true) => logln!("theme: Android is now {name}"),
        Ok(false) => {}
        Err(e) => logln!("theme: cannot make Android {name}: {e:#}"),
    }
}

fn call(uimode: &SIBinder, code: TransactionCode, arg: Option<i32>) -> Result<Option<i32>> {
    let proxy = uimode.as_proxy().context("uimode is not a proxy")?;
    let mut data = proxy.prepare_transact(true).map_err(|e| anyhow!("prepare: {e:?}"))?;
    if let Some(a) = arg {
        data.write(&a)?;
    }
    let mut reply = proxy
        .submit_transact(code, &data, 0)
        .map_err(|e| anyhow!("transact {code}: {e:?}"))?
        .context("no reply parcel")?;
    let status = reply.read::<Status>().map_err(|e| anyhow!("reply status: {e:?}"))?;
    if status.exception_code() != ExceptionCode::None {
        bail!("IUiModeManager transaction {code}: {status}");
    }
    Ok(if arg.is_none() { Some(reply.read::<i32>()?) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_dark_preference_makes_android_dark() {
        assert_eq!(night_mode(1), MODE_NIGHT_YES);
        assert_eq!(night_mode(2), MODE_NIGHT_NO);
        assert_eq!(night_mode(0), MODE_NIGHT_NO);
    }
}
