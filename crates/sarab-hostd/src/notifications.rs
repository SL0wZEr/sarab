//! The image's `INotifications` binder service: Android posts its
//! notifications to us and we forward them to the desktop notification daemon
//! over D-Bus (`org.freedesktop.Notifications`). Button presses travel back
//! into Android through a callback binder Android registers with us; a
//! listener whose process died is dropped, as the image's own death handler
//! does. `new` probes the daemon once so a missing one is an error at startup,
//! which the caller treats as "run without this service".
//!
//! Wire format: each action and the image are structured AIDL parcelables, a
//! flag (`NULL_PARCELABLE` when absent) then a size header; the AIDL `byte`
//! urgency arrives as an int32. `ID_NONE` (0) is what the Java side reads as
//! "posting failed". XDG activation tokens arrive in a separate D-Bus signal
//! just before `ActionInvoked` and are passed along, since Android needs them
//! to raise its own window. The relay is oneway, so its log line is the only
//! evidence a click reached Android at all.
//!
//! A click wakes Android first (`sarab_runtime::thaw`): hostd is outside the
//! runtime's cgroup, so it keeps running while Android is paused, and a click
//! on a notification left on the desktop would otherwise block this thread in
//! a binder call to a frozen system_server until something else woke it, with
//! every later click queued behind it. `focus_app_for` runs before the action
//! is relayed. The composer picks its
//! mode from the active-apps property on every frame: `none` (how Android
//! boots, and what closing the full-UI window leaves) is its closed mode,
//! which draws nothing, so an activity a notification opens with no app window
//! up rendered invisibly (measured 2026-09-22: GMS showed its activity in
//! 123 ms and nothing appeared). Any other value but the full-UI marker is
//! single-window mode, which gives the topmost task a window whatever its
//! package; naming the package is what the launcher does too, so we do the
//! same. It must be set before the activity starts. `should_focus` leaves it
//! alone in multi-window mode, when the property holds the full-UI marker
//! (everything composites already), and when it already names the package.
//!
//! `notify` drops re-posts whose `fingerprint` is unchanged: Android re-posts
//! ongoing notifications with nothing new (Play services' "not Play Protect
//! certified" did so 110 times in three minutes) and each Notify is a fresh
//! host popup, which Android itself would not alert for
//! (NotificationManagerService.isVisuallyInterruptive). `app_icon` is left
//! empty on purpose, since some daemons would then ignore `image-data`; the
//! `desktop-entry` hint supplies the icon and grouping instead. `image_fits`
//! guards the daemon, which indexes the pixel buffer with the app's own
//! width/height/rowstride. Bodies are escaped when the daemon renders
//! `body-markup`. `summary_or_name` covers RemoteViews notifications (the
//! Clock's running timer) whose title is null: the app name, then the
//! package, beats an empty bubble. The per-id maps are capped by `remember` at
//! `REMEMBERED`, dropping the lowest ids, because a daemon that expires a
//! notification on its own never tells us and its ids only increase.

use crate::wire::Gate;
use crate::wire::logln;
use crate::wire::{bool_arg, ok, str_arg};
use anyhow::{Context, Result};
use rsbinder::*;
use sarab_runtime::Platform;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zbus::zvariant::Value;

pub const DESCRIPTOR: &str = "lineageos.waydroid.INotifications";
pub const SERVICE_NAME: &str = "waydroidnotifications";

const REGISTER_LISTENER: TransactionCode = 1;
const NOTIFY: TransactionCode = 2;
const CLOSE_NOTIFICATION: TransactionCode = 3;
const ON_ACTION_INVOKED: TransactionCode = 1;

const NULL_PARCELABLE: i32 = 0;
const ID_NONE: i32 = 0;

struct Action {
    id: String,
    label: String,
}

struct Notification {
    replaces_id: i32,
    app_name: String,
    package_name: String,
    summary: String,
    body: String,
    actions: Vec<Action>,
    image: Option<ImageData>,
    category: String,
    suppress_sound: bool,
    expire_timeout: i32,
    resident: bool,
    transient: bool,
    urgency: u8,
}

struct ImageData {
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    data: Vec<u8>,
}

pub struct Notifications {
    state: Arc<State>,
    gate: Gate,
}

struct State {
    listeners: Mutex<Vec<SIBinder>>,
    pending_tokens: Mutex<HashMap<u32, String>>,
    packages: Mutex<HashMap<i32, String>>,
    shown: Mutex<HashMap<i32, u64>>,
    dbus: Mutex<zbus::blocking::Proxy<'static>>,
    markup: bool,
    binder: PathBuf,
}

const REMEMBERED: usize = 256;

fn proxy(conn: &zbus::blocking::Connection) -> zbus::Result<zbus::blocking::Proxy<'static>> {
    zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.Notifications".to_string(),
        "/org/freedesktop/Notifications".to_string(),
        "org.freedesktop.Notifications".to_string(),
    )
}

impl Notifications {
    pub fn new(binder: PathBuf, gate: Gate) -> Result<Self> {
        let conn = zbus::blocking::Connection::session().context("connect to session bus")?;
        let p = proxy(&conn).context("open org.freedesktop.Notifications")?;
        let info: (String, String, String, String) =
            p.call("GetServerInformation", &()).context("no notification daemon on the session bus")?;
        logln!("notifications: daemon is {} {}", info.0, info.2);
        let caps: Vec<String> = p.call("GetCapabilities", &()).unwrap_or_default();
        let markup = caps.iter().any(|c| c == "body-markup");
        let state = Arc::new(State {
            listeners: Mutex::new(Vec::new()),
            pending_tokens: Mutex::new(HashMap::new()),
            packages: Mutex::new(HashMap::new()),
            shown: Mutex::new(HashMap::new()),
            dbus: Mutex::new(p),
            markup,
            binder,
        });
        State::spawn_signal_threads(&state, conn);
        Ok(Self { state, gate })
    }
}

impl State {
    fn spawn_signal_threads(state: &Arc<Self>, conn: zbus::blocking::Connection) {
        if let Ok(p) = proxy(&conn) {
            let state = state.clone();
            std::thread::spawn(move || match p.receive_signal("ActivationToken") {
                Ok(stream) => {
                    for msg in stream {
                        if let Ok((id, token)) = msg.body().deserialize::<(u32, String)>() {
                            state.pending_tokens.lock().unwrap().insert(id, token);
                        }
                    }
                }
                Err(e) => logln!("notifications: ActivationToken signal: {e}"),
            });
        }

        if let Ok(p) = proxy(&conn) {
            let state = state.clone();
            std::thread::spawn(move || match p.receive_signal("ActionInvoked") {
                Ok(stream) => {
                    for msg in stream {
                        if let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() {
                            let token = state.pending_tokens.lock().unwrap().remove(&id).unwrap_or_default();
                            state.on_action_invoked(id as i32, &action, &token);
                        }
                    }
                }
                Err(e) => logln!("notifications: ActionInvoked signal: {e}"),
            });
        }
    }

    fn focus_app_for(&self, id: i32) {
        let Some(pkg) = self.packages.lock().unwrap().get(&id).cloned() else { return };
        let p = match Platform::connect(&self.binder) {
            Ok(p) => p,
            Err(e) => return logln!("notifications: no platform service to focus {pkg}: {e}"),
        };
        let multi = match p.getprop("persist.waydroid.multi_windows", "false") {
            Ok(v) => v,
            Err(e) => return logln!("notifications: cannot read multi_windows: {e}"),
        };
        let active = match p.getprop("waydroid.active_apps", "") {
            Ok(v) => v,
            Err(e) => return logln!("notifications: cannot read active_apps: {e}"),
        };
        if !should_focus(multi.trim(), active.trim(), &pkg) {
            return;
        }
        match p.setprop("waydroid.active_apps", &pkg) {
            Ok(()) => logln!("notifications: #{id} -> compositing {pkg}"),
            Err(e) => logln!("notifications: cannot focus {pkg}: {e}"),
        }
    }

    fn on_action_invoked(&self, id: i32, action: &str, token: &str) {
        if let Err(e) = sarab_runtime::thaw() {
            logln!("notifications: cannot wake Android for #{id}: {e:#}");
            return;
        }
        self.focus_app_for(id);
        let mut listeners = self.listeners.lock().unwrap();
        listeners.retain(|listener| {
            let Some(p) = listener.as_proxy() else { return false };
            let send = || -> rsbinder::Result<()> {
                let mut data = p.prepare_transact(true)?;
                data.write(&id)?;
                data.write(action)?;
                data.write(token)?;
                p.submit_transact(ON_ACTION_INVOKED, &data, FLAG_ONEWAY)?;
                Ok(())
            };
            match send() {
                Ok(()) => true,
                Err(e) => {
                    logln!("notifications: listener dropped ({e:?})");
                    false
                }
            }
        });
        logln!("notifications: action {action:?} on #{id} -> {} listener(s)", listeners.len());
    }

    fn notify(&self, n: &Notification) -> i32 {
        let Notification {
            replaces_id,
            app_name,
            package_name,
            summary,
            body,
            actions,
            image,
            category,
            suppress_sound,
            expire_timeout,
            resident,
            transient,
            urgency,
        } = n;
        let print = fingerprint(n);
        if *replaces_id != ID_NONE && !is_visually_interruptive(self.shown.lock().unwrap().get(replaces_id), print) {
            return *replaces_id;
        }
        let actions_flat: Vec<&str> = actions.iter().flat_map(|a| [a.id.as_str(), a.label.as_str()]).collect();

        let summary = summary_or_name(summary, app_name, package_name);

        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from(format!("sarab.{package_name}")));
        hints.insert("resident", Value::from(resident));
        hints.insert("transient", Value::from(transient));
        hints.insert("urgency", Value::from(urgency));
        hints.insert("suppress-sound", Value::from(suppress_sound));
        if !category.is_empty() {
            hints.insert("category", Value::from(category.to_string()));
        }
        if let Some(img) = image.as_ref().filter(|i| image_fits(i)) {
            let channels: i32 = if img.has_alpha { 4 } else { 3 };
            match zbus::zvariant::StructureBuilder::new()
                .add_field(img.width)
                .add_field(img.height)
                .add_field(img.rowstride)
                .add_field(img.has_alpha)
                .add_field(8i32)
                .add_field(channels)
                .add_field(img.data.clone())
                .build()
            {
                Ok(s) => {
                    hints.insert("image-data", Value::from(s));
                }
                Err(e) => logln!("notifications: image-data hint: {e}"),
            }
        }

        let body = if self.markup { escape_markup(body) } else { body.clone() };
        let args = (app_name, *replaces_id as u32, "", summary, body, actions_flat, hints, *expire_timeout);
        match self.dbus.lock().unwrap().call::<_, _, u32>("Notify", &args) {
            Ok(id) => {
                logln!("notifications: posted #{id} from {package_name} ({} action(s)): {summary}", actions.len());
                remember(&mut self.packages.lock().unwrap(), id as i32, package_name.clone());
                remember(&mut self.shown.lock().unwrap(), id as i32, print);
                id as i32
            }
            Err(e) => {
                logln!("notifications: Notify failed: {e}");
                ID_NONE
            }
        }
    }

    fn close(&self, id: i32) {
        self.packages.lock().unwrap().remove(&id);
        self.shown.lock().unwrap().remove(&id);
        if let Err(e) = self.dbus.lock().unwrap().call::<_, _, ()>("CloseNotification", &(id as u32)) {
            logln!("notifications: CloseNotification failed: {e}");
        }
    }
}

fn should_focus(multi_windows: &str, active_apps: &str, pkg: &str) -> bool {
    multi_windows != "true" && active_apps != "Waydroid" && active_apps != pkg
}

fn remember<V>(map: &mut HashMap<i32, V>, id: i32, value: V) {
    map.insert(id, value);
    if map.len() > REMEMBERED {
        let mut ids: Vec<i32> = map.keys().copied().collect();
        ids.sort_unstable();
        for old in &ids[..ids.len() - REMEMBERED / 2] {
            map.remove(old);
        }
    }
}

fn fingerprint(n: &Notification) -> u64 {
    let mut h = DefaultHasher::new();
    (&n.app_name, &n.summary, &n.body, &n.category, n.urgency, n.resident, n.transient).hash(&mut h);
    for a in &n.actions {
        (&a.id, &a.label).hash(&mut h);
    }
    if let Some(i) = &n.image {
        (i.width, i.height, &i.data).hash(&mut h);
    }
    h.finish()
}

fn is_visually_interruptive(previous: Option<&u64>, now: u64) -> bool {
    previous != Some(&now)
}

fn image_fits(i: &ImageData) -> bool {
    const MAX_SIDE: i32 = 4096;
    let channels = if i.has_alpha { 4 } else { 3 };
    if !(1..=MAX_SIDE).contains(&i.width) || !(1..=MAX_SIDE).contains(&i.height) {
        return false;
    }
    let (w, h, stride) = (i.width as usize, i.height as usize, i.rowstride as usize);
    i.rowstride >= i.width * channels && i.data.len() >= stride * (h - 1) + w * channels as usize
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn summary_or_name<'a>(summary: &'a str, app_name: &'a str, package_name: &'a str) -> &'a str {
    for candidate in [summary, app_name, package_name] {
        if !candidate.trim().is_empty() {
            return candidate;
        }
    }
    ""
}

impl Remotable for Notifications {
    fn descriptor() -> &'static str
    where
        Self: Sized,
    {
        DESCRIPTOR
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> rsbinder::Result<()> {
        self.gate.check(SERVICE_NAME)?;
        match code {
            REGISTER_LISTENER => {
                let listener: SIBinder = data.read()?;
                self.state.listeners.lock().unwrap().push(listener);
                logln!("notifications: listener registered");
                ok(reply)
            }
            NOTIFY => {
                let replaces_id: i32 = data.read()?;
                let app_name = str_arg(data)?;
                let package_name = str_arg(data)?;
                let summary = str_arg(data)?;
                let body = str_arg(data)?;

                let n_actions: i32 = data.read()?;
                let mut actions = Vec::new();
                for _ in 0..n_actions.max(0) {
                    if data.read::<i32>()? != NULL_PARCELABLE {
                        let _size: i32 = data.read()?;
                        actions.push(Action { id: str_arg(data)?, label: str_arg(data)? });
                    }
                }

                let mut image = None;
                if data.read::<i32>()? != NULL_PARCELABLE {
                    let _size: i32 = data.read()?;
                    image = Some(ImageData {
                        width: data.read()?,
                        height: data.read()?,
                        rowstride: data.read()?,
                        has_alpha: bool_arg(data)?,
                        data: data.read::<Vec<u8>>()?,
                    });
                }

                let category = str_arg(data)?;
                let suppress_sound = bool_arg(data)?;
                let expire_timeout: i32 = data.read()?;
                let resident = bool_arg(data)?;
                let transient = bool_arg(data)?;
                let urgency = data.read::<i32>().unwrap_or(1) as u8;

                let id = self.state.notify(&Notification {
                    replaces_id,
                    app_name,
                    package_name,
                    summary,
                    body,
                    actions,
                    image,
                    category,
                    suppress_sound,
                    expire_timeout,
                    resident,
                    transient,
                    urgency,
                });
                ok(reply)?;
                reply.write(&id)
            }
            CLOSE_NOTIFICATION => {
                let id: i32 = data.read()?;
                self.state.close(id);
                ok(reply)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
        let _ = writeln!(w, "sarab notifications: {} listener(s)", self.state.listeners.lock().unwrap().len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_must_fit_its_own_numbers() {
        let img =
            |width, height, rowstride, len| ImageData { width, height, rowstride, has_alpha: true, data: vec![0; len] };
        assert!(image_fits(&img(2, 2, 8, 16)));
        assert!(image_fits(&img(2, 2, 12, 20)));
        assert!(!image_fits(&img(2, 2, 8, 15)), "data one byte short");
        assert!(!image_fits(&img(2, 2, 4, 16)), "stride narrower than a row");
        assert!(!image_fits(&img(0, 2, 8, 16)));
        assert!(!image_fits(&img(-1, 2, 8, 16)));
        assert!(!image_fits(&img(2, 100_000, 8, 16)));
    }

    #[test]
    fn body_markup_is_escaped() {
        assert_eq!(escape_markup("<b>1 & 2</b>"), "&lt;b&gt;1 &amp; 2&lt;/b&gt;");
        assert_eq!(escape_markup("plain"), "plain");
    }

    #[test]
    fn a_notification_with_a_title_keeps_it() {
        assert_eq!(summary_or_name("Timer done", "Clock", "com.android.deskclock"), "Timer done");
    }

    #[test]
    fn a_remoteviews_notification_falls_back_to_the_app_name() {
        assert_eq!(summary_or_name("", "Clock", "com.android.deskclock"), "Clock");
        assert_eq!(summary_or_name("   ", "Clock", "com.android.deskclock"), "Clock");
    }

    #[test]
    fn with_no_app_name_either_the_package_is_the_last_resort() {
        assert_eq!(summary_or_name("", "", "com.android.deskclock"), "com.android.deskclock");
        assert_eq!(summary_or_name("", "", ""), "");
    }

    #[test]
    fn a_click_from_another_app_takes_the_screen() {
        assert!(should_focus("false", "com.example.app", "com.google.android.gms"));
        assert!(should_focus("false", "none", "com.google.android.gms"));
        assert!(should_focus("", "", "com.android.deskclock"));
    }

    #[test]
    fn nothing_to_switch_to_is_left_alone() {
        assert!(!should_focus("false", "com.example.app", "com.example.app"));
        assert!(!should_focus("true", "com.example.app", "com.google.android.gms"));
        assert!(!should_focus("false", "Waydroid", "com.google.android.gms"));
    }

    #[test]
    fn a_remembered_notification_knows_its_app() {
        let mut m = HashMap::new();
        remember(&mut m, 7, "com.android.deskclock".to_string());
        assert_eq!(m.get(&7).map(String::as_str), Some("com.android.deskclock"));
        remember(&mut m, 7, "com.google.android.gms".to_string());
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&7).map(String::as_str), Some("com.google.android.gms"));
    }

    #[test]
    fn an_unchanged_update_is_not_shown_again() {
        let n = |body: &str| Notification {
            replaces_id: 131,
            app_name: "Google Play services".into(),
            package_name: "com.google.android.gms".into(),
            summary: "This device isn't Play Protect certified".into(),
            body: body.into(),
            actions: vec![],
            image: None,
            category: String::new(),
            suppress_sound: false,
            expire_timeout: -1,
            resident: false,
            transient: false,
            urgency: 1,
        };
        let first = fingerprint(&n("Google apps and services can't run on this device"));
        assert!(is_visually_interruptive(None, first), "a notification we never showed is shown");
        let again = fingerprint(&n("Google apps and services can't run on this device"));
        assert!(!is_visually_interruptive(Some(&first), again), "the same content again is not a popup");
        let changed = fingerprint(&n("something else"));
        assert!(is_visually_interruptive(Some(&first), changed), "new content is");
    }

    #[test]
    fn remembering_is_bounded_and_keeps_the_newest() {
        let mut m = HashMap::new();
        for id in 0..=(REMEMBERED as i32 + 10) {
            remember(&mut m, id, "com.example".to_string());
        }
        assert!(m.len() <= REMEMBERED, "grew to {}", m.len());
        let newest = REMEMBERED as i32 + 10;
        assert!(m.contains_key(&newest), "dropped the newest notification");
        assert!(!m.contains_key(&0), "kept the oldest notification");
    }
}
