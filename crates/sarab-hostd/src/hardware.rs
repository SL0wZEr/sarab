//! The image's `IHardware` service: power and radio requests from Android.
//!
//! Only the shutdown path is wired: when Android finishes its own shutdown
//! sequence it asks the host to tear the runtime down, and without an answer
//! it sits at a black screen. Honouring it is opt-in (`allow_shutdown`, set by
//! `--allow-shutdown`), because a stray request would kill a session. The
//! image encodes a reboot as a `SHUTDOWN_REQUEST` whose reason string starts
//! with "1". The radio toggles have no host counterpart yet and the image
//! treats a zero return as success.

use crate::wire::Gate;
use crate::wire::logln;
use crate::wire::{bool_arg, ok, str_arg};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use rsbinder::*;

pub const DESCRIPTOR: &str = "lineageos.waydroid.IHardware";
pub const SERVICE_NAME: &str = "waydroidhardware";

const ENABLE_NFC: TransactionCode = 1;
const ENABLE_BLUETOOTH: TransactionCode = 2;
const SUSPEND: TransactionCode = 3;
const REBOOT: TransactionCode = 4;
const UPGRADE: TransactionCode = 5;
const UPGRADE2: TransactionCode = 6;
const SHUTDOWN_REQUEST: TransactionCode = 7;

pub struct Hardware {
    init_pid: u32,
    gate: Gate,
    allow_shutdown: bool,
}

impl Hardware {
    pub fn new(init_pid: u32, allow_shutdown: bool, gate: Gate) -> Self {
        Self { init_pid, gate, allow_shutdown }
    }

    fn stop_runtime(&self) {
        if !self.allow_shutdown {
            logln!("hardware: shutdown requested; ignoring (pass --allow-shutdown to honour it)");
            return;
        }
        logln!("hardware: stopping runtime (pid {})", self.init_pid);
        if let Err(e) = kill(Pid::from_raw(self.init_pid as i32), Signal::SIGKILL) {
            logln!("hardware: could not stop pid {}: {e}", self.init_pid);
        }
    }
}

impl Remotable for Hardware {
    fn descriptor() -> &'static str
    where
        Self: Sized,
    {
        DESCRIPTOR
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> rsbinder::Result<()> {
        self.gate.check(SERVICE_NAME)?;
        match code {
            ENABLE_NFC | ENABLE_BLUETOOTH => {
                let enable = bool_arg(data)?;
                let what = if code == ENABLE_NFC { "NFC" } else { "Bluetooth" };
                logln!("hardware: {what} enable={enable} (no host radio; ignored)");
                ok(reply)?;
                reply.write(&0i32)
            }
            SUSPEND => {
                logln!("hardware: suspend requested (ignored)");
                ok(reply)
            }
            REBOOT => {
                logln!("hardware: reboot requested (not supported; start the runtime again by hand)");
                ok(reply)
            }
            UPGRADE | UPGRADE2 => {
                logln!("hardware: image upgrade requested (not supported)");
                ok(reply)
            }
            SHUTDOWN_REQUEST => {
                let reason = str_arg(data)?;
                if reason.starts_with('1') {
                    logln!("hardware: reboot request (not supported)");
                } else {
                    self.stop_runtime();
                }
                ok(reply)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
        let _ = writeln!(
            w,
            "sarab hardware: init pid {}, shutdown {}",
            self.init_pid,
            if self.allow_shutdown { "allowed" } else { "ignored" }
        );
        Ok(())
    }
}
