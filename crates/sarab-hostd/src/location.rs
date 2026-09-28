//! Android's GNSS HAL (`android.hardware.gnss.IGnss/default`, AIDL version 2,
//! the one Android 13's framework uses), with the desktop's location service,
//! GeoClue, where a phone has its GPS chip.
//!
//! Why this and not something else. Under Sarab Android has no location
//! source: there is no GPS, and Google's network location needs Wi-Fi and cell
//! scans Android cannot make here, so every provider stayed empty and apps fell
//! back to a default. The desktop does know where it is (GeoClue works it out
//! from Wi-Fi, the IP address, or a GPS the machine has), and the GNSS HAL is
//! the interface through which Android takes a location from its platform,
//! the same way the emulator feeds it one from its host. Apps then see an
//! ordinary `gps` provider, not a mock location: no app is special-cased, and
//! nothing is set up through Android's developer options. The fix is only as
//! good as GeoClue's, and its accuracy is reported as it is (often hundreds of
//! metres), not dressed up as satellite precision; `SYSTEM_NAME` tells Android
//! what the "GPS" really is.
//!
//! The binder side is hand-written, like the image's other services here, and
//! follows the frozen AIDL: `IGnss` and `IGnssCallback` transaction codes are
//! their methods' positions (the first is 1), and `getInterfaceVersion` and
//! `getInterfaceHash` are the two AIDL adds at the top of the range. The
//! framework asks for the version before anything else and uses the AIDL HAL
//! only when it is at least 2, so `VERSION` must stay 2 and `HASH` is the
//! frozen interface's. The binder is marked VINTF stable, as its interface is,
//! so servicemanager accepts it only because the vendor manifest declares it
//! (sarab's overlay.rs adds the entry). That declaration has a cost: the
//! framework waits for a declared HAL without a timeout while system_server
//! starts, so main.rs registers this service before any other, and `sarab
//! start` leaves the declaration out when hostd is not running.
//!
//! What is implemented is the position: `setCallback` answers with
//! `CAPABILITY_SCHEDULING` (the framework passes its interval to
//! `setPositionMode` and leaves the timing to us) and `SYSTEM_NAME`,
//! `setPositionMode` reads `minIntervalMs` (never below `MIN_INTERVAL`), and
//! `start`/`stop` begin and end a session with the framework's status
//! callbacks, as AOSP's reference HAL does, inside the transaction. Every
//! `getExtension*` answers `UnsupportedOperation` (`EXTENSIONS`), which the
//! framework treats as a HAL without that extension; an OK reply carrying null
//! would not do, since for the nullable ones it builds a wrapper around the
//! null and uses it. Satellite status, NMEA, injected time and locations and
//! aiding data have nothing to act on, so they are accepted and ignored.
//!
//! Privacy and security. Only Android's system uid may call (`Gate`), which is
//! system_server, and it hands a fix only to apps holding the location
//! permission, and only while Android's own location switch is on. GeoClue is
//! asked only between `start` and `stop`, that is while some app is asking:
//! the GeoClue client is started and stopped with the session, and a location
//! GeoClue reports outside one is dropped (`Shared::update`), as is the last
//! fix when a session ends. hostd's log gets the accuracy of each fix that
//! differs from the last one logged in the session (`accuracy_line`), never
//! coordinates. On the host GeoClue decides: an unsandboxed program
//! like hostd gets a location when a GeoClue agent is running and allows it
//! (GNOME's location switch is one), and none otherwise, and `hint` says so in
//! the log when GeoClue refuses or is missing. A `DesktopId` is required by
//! GeoClue before `Start`, hence `DESKTOP_ID`.
//!
//! Threads. A binder call never waits on GeoClue, whose `Start` can wait for
//! the agent: `start` and `stop` only post a `Cmd` to the `geoclue` thread,
//! which owns the system bus connection and the client and creates both on the
//! first `Start`. A second thread, spawned once a client exists, turns each
//! `LocationUpdated` into a `Fix`. The `report` thread sends the latest fix to
//! Android at once and then every interval, taking the lock only to copy the
//! callback and the fix, never across a call into Android, which blocks while
//! Android is frozen and must not hold up a `stop` meanwhile. It logs the first
//! failure of a run of failed reports, not each one. The `geoclue` and
//! `report` threads start on the first `start`, so a session nobody asks for
//! costs nothing. When `Start` is refused the client is kept, since GeoClue
//! keeps a client until our connection closes and a new one per attempt would
//! pile up; any other failure drops it, so a GeoClue that restarted gets a new
//! client on the next session.
//!
//! GeoClue's location is its current answer, re-announced only when it moves,
//! so each report is stamped with the time it is sent, and `fix` rejects
//! coordinates out of range and treats GeoClue's markers for an unknown
//! altitude (the most negative double), speed and heading (negative) as
//! absent. `write_location` lays out `GnssLocation` and its `ElapsedRealtime`
//! as AIDL does a non-null parcelable: a 1, then the size, then the fields in
//! order. The elapsed-realtime flags are left empty, so the framework stamps
//! the fix with its own clock when it arrives. `on_dump` shows the session and
//! the fix's accuracy, like the log, without coordinates.
//!
//! Accuracy decides whether apps use a fix. GeoClue's first answer is often
//! from the IP address, kilometres wide (25 km was seen), and Google Play
//! services dropped every such fix without a word: its fused location stayed
//! empty, so an app asking it for a precise location timed out, while the same
//! HAL's 50 m fixes (GeoClue's static source) reached the app. So from
//! `COARSE` metres up `accuracy_line` adds why apps may ignore the fix and
//! where the README says how to give the desktop a better one. It is a hint
//! for the log, not a threshold anything acts on.

use crate::wire::{Gate, logln, ok};
use anyhow::{Context, Result, anyhow, bail};
use rsbinder::*;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub const DESCRIPTOR: &str = "android.hardware.gnss.IGnss";
pub const SERVICE_NAME: &str = "android.hardware.gnss.IGnss/default";

const VERSION: i32 = 2;
const HASH: &str = "fc957f1d3d261d065ff5e5415f2d21caa79c310f";

const SET_CALLBACK: TransactionCode = 1;
const CLOSE: TransactionCode = 2;
const START: TransactionCode = 14;
const STOP: TransactionCode = 15;
const INJECT_TIME: TransactionCode = 16;
const INJECT_LOCATION: TransactionCode = 17;
const INJECT_BEST_LOCATION: TransactionCode = 18;
const DELETE_AIDING_DATA: TransactionCode = 19;
const SET_POSITION_MODE: TransactionCode = 20;
const START_SV_STATUS: TransactionCode = 23;
const STOP_SV_STATUS: TransactionCode = 24;
const START_NMEA: TransactionCode = 25;
const STOP_NMEA: TransactionCode = 26;
const EXTENSIONS: [TransactionCode; 13] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 21, 22];
const GET_INTERFACE_HASH: TransactionCode = FIRST_CALL_TRANSACTION + 16_777_213;
const GET_INTERFACE_VERSION: TransactionCode = FIRST_CALL_TRANSACTION + 16_777_214;

const CB_SET_CAPABILITIES: TransactionCode = 1;
const CB_STATUS: TransactionCode = 2;
const CB_LOCATION: TransactionCode = 4;
const CB_SET_SYSTEM_INFO: TransactionCode = 8;

const CAPABILITY_SCHEDULING: i32 = 1;
const SESSION_BEGIN: i32 = 1;
const SESSION_END: i32 = 2;
const SYSTEM_NAME: &str = "Sarab: the desktop's location, from GeoClue";

const HAS_LAT_LONG: i32 = 1;
const HAS_ALTITUDE: i32 = 2;
const HAS_SPEED: i32 = 4;
const HAS_BEARING: i32 = 8;
const HAS_HORIZONTAL_ACCURACY: i32 = 16;

const MIN_INTERVAL: Duration = Duration::from_secs(1);
const COARSE: f64 = 1000.0;

const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const DESKTOP_ID: &str = "sarab";
const ACCURACY_EXACT: u32 = 8;
const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";

#[derive(Clone, Debug, PartialEq)]
struct Fix {
    serial: u64,
    latitude: f64,
    longitude: f64,
    accuracy: f64,
    altitude: Option<f64>,
    speed: Option<f64>,
    heading: Option<f64>,
}

struct State {
    callback: Option<SIBinder>,
    interval: Duration,
    running: bool,
    fix: Option<Fix>,
    fixes: u64,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, mut fix: Fix) {
        let mut st = self.state.lock().unwrap();
        if !st.running {
            return;
        }
        st.fixes += 1;
        fix.serial = st.fixes;
        st.fix = Some(fix);
        self.changed.notify_all();
    }
}

enum Cmd {
    Start,
    Stop,
}

pub struct Gnss {
    gate: Gate,
    shared: Arc<Shared>,
    geoclue: Mutex<Option<Sender<Cmd>>>,
}

impl Gnss {
    pub fn new(gate: Gate) -> Self {
        let state = State { callback: None, interval: MIN_INTERVAL, running: false, fix: None, fixes: 0 };
        Self {
            gate,
            shared: Arc::new(Shared { state: Mutex::new(state), changed: Condvar::new() }),
            geoclue: Mutex::new(None),
        }
    }

    fn set_callback(&self, cb: SIBinder) {
        self.shared.state.lock().unwrap().callback = Some(cb.clone());
        if let Err(e) = send(&cb, CB_SET_CAPABILITIES, |p| p.write(&CAPABILITY_SCHEDULING)) {
            logln!("location: could not tell Android the capabilities: {e:#}");
        }
        let info = |p: &mut Parcel| {
            p.write(&1i32)?;
            p.sized_write(|p| {
                p.write(&0i32)?;
                p.write(SYSTEM_NAME)
            })
        };
        if let Err(e) = send(&cb, CB_SET_SYSTEM_INFO, info) {
            logln!("location: could not tell Android the system info: {e:#}");
        }
    }

    fn post(&self, cmd: Cmd) {
        let mut geoclue = self.geoclue.lock().unwrap();
        let tx = geoclue.get_or_insert_with(|| {
            let (tx, rx) = channel();
            let shared = self.shared.clone();
            std::thread::spawn(move || geoclue_thread(shared, rx));
            let shared = self.shared.clone();
            std::thread::spawn(move || report(shared));
            tx
        });
        let _ = tx.send(cmd);
    }

    fn session(&self, running: bool) {
        let callback = {
            let mut st = self.shared.state.lock().unwrap();
            if st.running == running {
                return;
            }
            st.running = running;
            st.fix = None;
            self.shared.changed.notify_all();
            st.callback.clone()
        };
        self.post(if running { Cmd::Start } else { Cmd::Stop });
        logln!("location: Android {} a session", if running { "started" } else { "ended" });
        let status = if running { SESSION_BEGIN } else { SESSION_END };
        if let Some(cb) = callback
            && let Err(e) = send(&cb, CB_STATUS, |p| p.write(&status))
        {
            logln!("location: could not report the session status: {e:#}");
        }
    }
}

impl Remotable for Gnss {
    fn descriptor() -> &'static str
    where
        Self: Sized,
    {
        DESCRIPTOR
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> rsbinder::Result<()> {
        self.gate.check(SERVICE_NAME)?;
        match code {
            SET_CALLBACK => {
                let cb: SIBinder = data.read()?;
                self.set_callback(cb);
                ok(reply)
            }
            CLOSE => {
                self.session(false);
                self.shared.state.lock().unwrap().callback = None;
                ok(reply)
            }
            START => {
                self.session(true);
                ok(reply)
            }
            STOP => {
                self.session(false);
                ok(reply)
            }
            SET_POSITION_MODE => {
                let interval = interval(data)?;
                self.shared.state.lock().unwrap().interval = interval;
                self.shared.changed.notify_all();
                ok(reply)
            }
            INJECT_TIME | INJECT_LOCATION | INJECT_BEST_LOCATION | DELETE_AIDING_DATA | START_SV_STATUS
            | STOP_SV_STATUS | START_NMEA | STOP_NMEA => ok(reply),
            GET_INTERFACE_VERSION => {
                ok(reply)?;
                reply.write(&VERSION)
            }
            GET_INTERFACE_HASH => {
                ok(reply)?;
                reply.write(HASH)
            }
            c if EXTENSIONS.contains(&c) => reply.write(&Status::from(ExceptionCode::UnsupportedOperation)),
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
        let st = self.shared.state.lock().unwrap();
        let fix = st.fix.as_ref().map_or("none".to_string(), |f| format!("accurate to {:.0} m", f.accuracy));
        let _ = writeln!(w, "sarab location: session {}, every {:?}, fix {fix}", st.running, st.interval);
        Ok(())
    }
}

fn interval(data: &mut Parcel) -> rsbinder::Result<Duration> {
    if data.read::<i32>()? == 0 {
        return Err(StatusCode::UnexpectedNull);
    }
    let mut ms = 0i32;
    data.sized_read(|p| {
        let _mode: i32 = p.read()?;
        let _recurrence: i32 = p.read()?;
        ms = p.read()?;
        Ok(())
    })?;
    Ok(Duration::from_millis(ms.max(0) as u64).max(MIN_INTERVAL))
}

fn send(cb: &SIBinder, code: TransactionCode, args: impl FnOnce(&mut Parcel) -> rsbinder::Result<()>) -> Result<()> {
    let proxy = cb.as_proxy().context("the callback is not a remote binder")?;
    let mut data = proxy.prepare_transact(true).map_err(|e| anyhow!("prepare: {e:?}"))?;
    args(&mut data).map_err(|e| anyhow!("write: {e:?}"))?;
    let mut reply = proxy
        .submit_transact(code, &data, 0)
        .map_err(|e| anyhow!("transact {code}: {e:?}"))?
        .context("no reply parcel")?;
    let status = reply.read::<Status>().map_err(|e| anyhow!("reply status: {e:?}"))?;
    if status.exception_code() != ExceptionCode::None {
        bail!("IGnssCallback transaction {code}: {status}");
    }
    Ok(())
}

fn write_location(p: &mut Parcel, fix: &Fix, time_ms: i64) -> rsbinder::Result<()> {
    let mut flags = HAS_LAT_LONG | HAS_HORIZONTAL_ACCURACY;
    for (value, flag) in [(fix.altitude, HAS_ALTITUDE), (fix.speed, HAS_SPEED), (fix.heading, HAS_BEARING)] {
        if value.is_some() {
            flags |= flag;
        }
    }
    p.write(&1i32)?;
    p.sized_write(|p| {
        p.write(&flags)?;
        for v in [
            fix.latitude,
            fix.longitude,
            fix.altitude.unwrap_or(0.0),
            fix.speed.unwrap_or(0.0),
            fix.heading.unwrap_or(0.0),
            fix.accuracy,
            0.0,
            0.0,
            0.0,
        ] {
            p.write(&v)?;
        }
        p.write(&time_ms)?;
        p.write(&1i32)?;
        p.sized_write(|p| {
            p.write(&0i32)?;
            p.write(&0i64)?;
            p.write(&0f64)
        })
    })
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn accuracy_line(metres: f64) -> String {
    let mut line = format!("location: GeoClue's fix is accurate to about {metres:.0} m");
    if metres >= COARSE {
        line.push_str(
            "; apps that ask Google Play services for a precise location ignore a fix this coarse \
             (README, Location, says how to give the desktop a better one)",
        );
    }
    line
}

fn report(shared: Arc<Shared>) {
    let mut sent: Option<(u64, Instant)> = None;
    let mut logged: Option<f64> = None;
    let mut failing = false;
    let mut st = shared.state.lock().unwrap();
    loop {
        let (Some(cb), Some(fix), true) = (st.callback.clone(), st.fix.clone(), st.running) else {
            sent = None;
            logged = None;
            st = shared.changed.wait(st).unwrap();
            continue;
        };
        if let Some((serial, at)) = sent
            && serial == fix.serial
        {
            let left = st.interval.saturating_sub(at.elapsed());
            if !left.is_zero() {
                st = shared.changed.wait_timeout(st, left).unwrap().0;
                continue;
            }
        }
        drop(st);
        if logged != Some(fix.accuracy) {
            logged = Some(fix.accuracy);
            logln!("{}", accuracy_line(fix.accuracy));
        }
        match send(&cb, CB_LOCATION, |p| write_location(p, &fix, now_ms())) {
            Ok(()) => failing = false,
            Err(e) if !failing => {
                failing = true;
                logln!("location: could not report a fix to Android: {e:#}");
            }
            Err(_) => {}
        }
        sent = Some((fix.serial, Instant::now()));
        st = shared.state.lock().unwrap();
    }
}

fn fix(get: impl Fn(&str) -> Option<f64>) -> Option<Fix> {
    let (latitude, longitude, accuracy) = (get("Latitude")?, get("Longitude")?, get("Accuracy")?);
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    if !accuracy.is_finite() || accuracy < 0.0 {
        return None;
    }
    Some(Fix {
        serial: 0,
        latitude,
        longitude,
        accuracy,
        altitude: get("Altitude").filter(|a| a.is_finite() && *a > f64::MIN),
        speed: get("Speed").filter(|s| s.is_finite() && *s >= 0.0),
        heading: get("Heading").filter(|h| (0.0..360.0).contains(h)),
    })
}

fn read_fix(conn: &zbus::blocking::Connection, path: OwnedObjectPath) -> Result<Fix> {
    let props = zbus::blocking::Proxy::new(conn, GEOCLUE, path, "org.freedesktop.DBus.Properties")?;
    let all: HashMap<String, OwnedValue> = props.call("GetAll", &("org.freedesktop.GeoClue2.Location",))?;
    fix(|k| all.get(k).and_then(|v| f64::try_from(v).ok()))
        .context("GeoClue sent a location without usable coordinates")
}

fn open(conn: &zbus::blocking::Connection, shared: &Arc<Shared>) -> Result<zbus::blocking::Proxy<'static>> {
    let manager =
        zbus::blocking::Proxy::new(conn, GEOCLUE, MANAGER_PATH, "org.freedesktop.GeoClue2.Manager".to_string())?;
    let path: OwnedObjectPath = manager.call("CreateClient", &())?;
    let client = zbus::blocking::Proxy::new(conn, GEOCLUE, path, "org.freedesktop.GeoClue2.Client".to_string())?;
    client.set_property("DesktopId", DESKTOP_ID)?;
    client.set_property("RequestedAccuracyLevel", ACCURACY_EXACT)?;
    let updates = client.receive_signal("LocationUpdated")?;
    let (conn, shared) = (conn.clone(), shared.clone());
    std::thread::spawn(move || {
        for msg in updates {
            let fix = msg
                .body()
                .deserialize::<(OwnedObjectPath, OwnedObjectPath)>()
                .map_err(anyhow::Error::from)
                .and_then(|(_, new)| read_fix(&conn, new));
            match fix {
                Ok(f) => shared.update(f),
                Err(e) => logln!("location: ignoring a GeoClue update: {e:#}"),
            }
        }
        logln!("location: GeoClue's signal stream ended");
    });
    Ok(client)
}

fn denied(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _) if name.as_str() == ACCESS_DENIED)
}

fn hint(e: &zbus::Error) -> &'static str {
    if denied(e) {
        "; GeoClue gives a location only while an agent allows it: GNOME has one built in, \
         elsewhere run GeoClue's demo agent (/usr/lib/geoclue-2.0/demos/agent)"
    } else if matches!(e, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown")
    {
        "; is GeoClue installed? (package geoclue)"
    } else {
        ""
    }
}

fn geoclue_thread(shared: Arc<Shared>, cmds: Receiver<Cmd>) {
    let mut conn: Option<zbus::blocking::Connection> = None;
    let mut client: Option<zbus::blocking::Proxy<'static>> = None;
    for cmd in cmds {
        match cmd {
            Cmd::Start => {
                if conn.is_none() {
                    match zbus::blocking::Connection::system() {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            logln!("location: cannot reach the system bus: {e}");
                            continue;
                        }
                    }
                }
                if client.is_none() {
                    match open(conn.as_ref().unwrap(), &shared) {
                        Ok(c) => client = Some(c),
                        Err(e) => {
                            let hint = e.downcast_ref::<zbus::Error>().map(hint).unwrap_or("");
                            logln!("location: cannot open a GeoClue client: {e:#}{hint}");
                            continue;
                        }
                    }
                }
                if let Err(e) = client.as_ref().unwrap().call::<_, _, ()>("Start", &()) {
                    logln!("location: GeoClue did not start: {e}{}", hint(&e));
                    if !denied(&e) {
                        client = None;
                    }
                }
            }
            Cmd::Stop => {
                if let Some(c) = &client
                    && let Err(e) = c.call::<_, _, ()>("Stop", &())
                {
                    logln!("location: GeoClue did not stop: {e}");
                    client = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geoclue(props: &[(&str, f64)]) -> Option<Fix> {
        let m: HashMap<&str, f64> = props.iter().copied().collect();
        fix(|k| m.get(k).copied())
    }

    #[test]
    fn a_fix_needs_coordinates_in_range_and_an_accuracy() {
        let f = geoclue(&[("Latitude", 32.9), ("Longitude", 13.18), ("Accuracy", 250.0)]).unwrap();
        assert_eq!((f.latitude, f.longitude, f.accuracy), (32.9, 13.18, 250.0));
        assert!(geoclue(&[("Latitude", 32.9), ("Longitude", 13.18)]).is_none());
        assert!(geoclue(&[("Latitude", 91.0), ("Longitude", 13.18), ("Accuracy", 1.0)]).is_none());
        assert!(geoclue(&[("Latitude", f64::NAN), ("Longitude", 13.18), ("Accuracy", 1.0)]).is_none());
        assert!(geoclue(&[("Latitude", 1.0), ("Longitude", 1.0), ("Accuracy", -1.0)]).is_none());
    }

    #[test]
    fn geoclues_unknown_markers_are_absent_values() {
        let base = [("Latitude", 1.0), ("Longitude", 2.0), ("Accuracy", 3.0)];
        let unknown = geoclue(&[&base[..], &[("Altitude", f64::MIN), ("Speed", -1.0), ("Heading", -1.0)]].concat());
        let unknown = unknown.unwrap();
        assert_eq!((unknown.altitude, unknown.speed, unknown.heading), (None, None, None));
        let known = geoclue(&[&base[..], &[("Altitude", 12.5), ("Speed", 0.0), ("Heading", 90.0)]].concat()).unwrap();
        assert_eq!((known.altitude, known.speed, known.heading), (Some(12.5), Some(0.0), Some(90.0)));
    }

    #[test]
    fn the_location_is_laid_out_as_aidls_gnss_location() {
        let fix = Fix {
            serial: 1,
            latitude: 32.9,
            longitude: 13.18,
            accuracy: 250.0,
            altitude: Some(40.0),
            speed: None,
            heading: None,
        };
        let mut p = Parcel::new();
        write_location(&mut p, &fix, 1_700_000_000_000).unwrap();
        p.set_data_position(0);
        assert_eq!(p.read::<i32>().unwrap(), 1);
        let size = p.read::<i32>().unwrap();
        assert_eq!(p.read::<i32>().unwrap(), HAS_LAT_LONG | HAS_HORIZONTAL_ACCURACY | HAS_ALTITUDE);
        let doubles: Vec<f64> = (0..9).map(|_| p.read::<f64>().unwrap()).collect();
        assert_eq!(doubles, [32.9, 13.18, 40.0, 0.0, 0.0, 250.0, 0.0, 0.0, 0.0]);
        assert_eq!(p.read::<i64>().unwrap(), 1_700_000_000_000);
        assert_eq!(p.read::<i32>().unwrap(), 1);
        assert_eq!(p.read::<i32>().unwrap(), 4 + 4 + 8 + 8);
        assert_eq!((p.read::<i32>().unwrap(), p.read::<i64>().unwrap(), p.read::<f64>().unwrap()), (0, 0, 0.0));
        assert_eq!(size as usize, p.data_position() - 4);
        assert_eq!(p.data_avail(), 0);
    }

    fn options(min_interval_ms: i32) -> Parcel {
        let mut p = Parcel::new();
        p.write(&1i32).unwrap();
        p.sized_write(|p| {
            for v in [0i32, 0, min_interval_ms, 0, 0, 0] {
                p.write(&v)?;
            }
            Ok(())
        })
        .unwrap();
        p.set_data_position(0);
        p
    }

    #[test]
    fn the_interval_comes_from_position_mode_options_and_has_a_floor() {
        assert_eq!(interval(&mut options(5000)).unwrap(), Duration::from_secs(5));
        assert_eq!(interval(&mut options(100)).unwrap(), MIN_INTERVAL);
        assert_eq!(interval(&mut options(-1)).unwrap(), MIN_INTERVAL);
        let mut null = Parcel::new();
        null.write(&0i32).unwrap();
        null.set_data_position(0);
        assert!(interval(&mut null).is_err());
    }

    #[test]
    fn transaction_codes_follow_the_frozen_interface() {
        assert_eq!(GET_INTERFACE_VERSION, 0x00ff_ffff);
        assert_eq!(GET_INTERFACE_HASH, 0x00ff_fffe);
        let named = [
            SET_CALLBACK,
            CLOSE,
            START,
            STOP,
            INJECT_TIME,
            INJECT_LOCATION,
            INJECT_BEST_LOCATION,
            DELETE_AIDING_DATA,
            SET_POSITION_MODE,
            START_SV_STATUS,
            STOP_SV_STATUS,
            START_NMEA,
            STOP_NMEA,
        ];
        let mut all: Vec<TransactionCode> = named.iter().chain(EXTENSIONS.iter()).copied().collect();
        all.sort();
        assert_eq!(all, (1..=26).collect::<Vec<_>>());
    }

    #[test]
    fn a_coarse_fix_says_why_apps_may_ignore_it() {
        assert_eq!(accuracy_line(50.0), "location: GeoClue's fix is accurate to about 50 m");
        assert!(accuracy_line(25_000.0).contains("about 25000 m; apps that ask Google Play services"));
    }

    #[test]
    fn a_location_outside_a_session_is_dropped() {
        let g = Gnss::new(Gate::for_uid(0));
        let f = geoclue(&[("Latitude", 1.0), ("Longitude", 2.0), ("Accuracy", 3.0)]).unwrap();
        g.shared.update(f.clone());
        assert!(g.shared.state.lock().unwrap().fix.is_none());
        g.shared.state.lock().unwrap().running = true;
        g.shared.update(f);
        assert_eq!(g.shared.state.lock().unwrap().fix.as_ref().map(|f| f.serial), Some(1));
    }
}
