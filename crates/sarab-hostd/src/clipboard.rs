//! The image's `IClipboard` service: Android asks the host for the desktop
//! clipboard, and pushes its own clipboard to us.
//!
//! Backend is the host Wayland clipboard, behind the `Clipboard` trait:
//! `clipboard_native::NativeClipboard` speaks wlr-data-control itself and is
//! tried first; `WlClipboard` shells out to `wl-copy`/`wl-paste` and stays as
//! the fallback for compositors without the protocol. `open` picks; with
//! `Backend::Auto` a native failure is logged and the wl-clipboard error is
//! the one reported as the reason the service is disabled.
//!
//! `wl-copy` daemonises to keep serving the selection, so the text outlives
//! `WlClipboard::set`. When `wl-paste` finds no text (an empty clipboard, or
//! an image), Android gets an empty answer rather than a failed transaction,
//! and never the text it pushed earlier, which is no longer the clipboard.

use crate::clipboard_native::NativeClipboard;
use crate::wire::Gate;
use crate::wire::logln;
use crate::wire::{ok, str_arg};
use rsbinder::*;
use std::io::Write;
use std::process::{Command, Stdio};

pub const DESCRIPTOR: &str = "lineageos.waydroid.IClipboard";
pub const SERVICE_NAME: &str = "waydroidclipboard";

const SEND_CLIPBOARD_DATA: TransactionCode = 1;
const GET_CLIPBOARD_DATA: TransactionCode = 2;

pub trait Clipboard: Send + Sync {
    fn set(&self, text: &str) -> anyhow::Result<()>;
    fn get(&self) -> anyhow::Result<String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Auto,
    WlClipboard,
    Native,
}

impl Backend {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "wl-clipboard" => Ok(Self::WlClipboard),
            "native" => Ok(Self::Native),
            other => anyhow::bail!("--clipboard: unknown backend {other:?} (wl-clipboard|native)"),
        }
    }
}

pub fn open(which: Backend) -> anyhow::Result<Box<dyn Clipboard>> {
    let native = || -> anyhow::Result<Box<dyn Clipboard>> { Ok(Box::new(NativeClipboard::new()?)) };
    let wl = || -> anyhow::Result<Box<dyn Clipboard>> { Ok(Box::new(WlClipboard::new()?)) };
    match which {
        Backend::Native => native(),
        Backend::WlClipboard => wl(),
        Backend::Auto => native().or_else(|e| {
            logln!("clipboard: native backend unavailable ({e}); trying wl-clipboard");
            wl()
        }),
    }
}

pub struct WlClipboard;

impl WlClipboard {
    pub fn new() -> anyhow::Result<Self> {
        for tool in ["wl-copy", "wl-paste"] {
            if Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
                anyhow::bail!("{tool} not found on PATH");
            }
        }
        Ok(Self)
    }
}

impl Clipboard for WlClipboard {
    fn set(&self, text: &str) -> anyhow::Result<()> {
        let mut child = Command::new("wl-copy")
            .arg("--type")
            .arg("text/plain;charset=utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child.stdin.take().unwrap().write_all(text.as_bytes())?;
        child.wait()?;
        Ok(())
    }

    fn get(&self) -> anyhow::Result<String> {
        let out = Command::new("wl-paste")
            .arg("--no-newline")
            .arg("--type")
            .arg("text/plain")
            .stderr(Stdio::null())
            .output()?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Ok(String::new())
    }
}

pub struct ClipboardService {
    backend: Box<dyn Clipboard>,
    gate: Gate,
}

impl ClipboardService {
    pub fn new(backend: Box<dyn Clipboard>, gate: Gate) -> Self {
        Self { backend, gate }
    }
}

impl Remotable for ClipboardService {
    fn descriptor() -> &'static str
    where
        Self: Sized,
    {
        DESCRIPTOR
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> rsbinder::Result<()> {
        self.gate.check(SERVICE_NAME)?;
        match code {
            SEND_CLIPBOARD_DATA => {
                let text = str_arg(data)?;
                if let Err(e) = self.backend.set(&text) {
                    logln!("clipboard: set failed: {e}");
                }
                ok(reply)
            }
            GET_CLIPBOARD_DATA => {
                let text = self.backend.get().unwrap_or_else(|e| {
                    logln!("clipboard: get failed: {e}");
                    String::new()
                });
                ok(reply)?;
                reply.write(&text)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
        let _ = writeln!(w, "sarab clipboard service");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Backend;

    #[test]
    fn backend_flag_parses() {
        assert_eq!(Backend::parse("native").unwrap(), Backend::Native);
        assert_eq!(Backend::parse("wl-clipboard").unwrap(), Backend::WlClipboard);
        assert_eq!(Backend::parse("auto").unwrap(), Backend::Auto);
        assert!(Backend::parse("xclip").is_err());
    }
}
