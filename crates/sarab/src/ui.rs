//! How sarab talks to a person during the first run, and to everything else
//! the rest of the time.
//!
//! The first `sarab start` at a terminal is a wizard (setup.rs, `first_run`),
//! drawn with cliclack: a frame opened by `begin`, a line per step, spinners,
//! a download bar, and a closing line. `begin` switches the whole process into
//! that mode (`WIZARD`), so the setup code underneath, which also serves
//! `sarab setup`, `sarab upgrade` and scripts, asks `wizard()` rather than
//! taking a flag through every call. Out of it, every function prints the plain
//! line it always has: `info` to stdout, `warn` with a `WARNING:` prefix and
//! `fail` as `sarab: ...` to stderr, `detail` as is; in it, `detail` is
//! dropped, being for logs rather than people, and `fail` closes the frame
//! with the error, which is how main.rs prints every error. `alert` is for
//! the one thing a person must not miss while skimming: a short headline in
//! bold yellow and one plain line under it (out of the wizard, one `WARNING:`
//! line).
//!
//! `Task` is one step that takes a while. In the wizard it is a spinner
//! (`spin`) or a byte counter with the speed and the time left (`bytes`); out
//! of it, `spin` prints its line and `bytes` prints nothing, since curl draws
//! its own meter then (image.rs). `done` finishes it with a line saying what
//! happened, `failed` with the reason, and a `Task` dropped by an error on the
//! way is marked failed with the line it started with, so no spinner is left
//! turning above the error.

use cliclack::ProgressBar;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

static WIZARD: AtomicBool = AtomicBool::new(false);

pub fn wizard() -> bool {
    WIZARD.load(Ordering::Relaxed)
}

pub fn begin(title: &str) {
    WIZARD.store(true, Ordering::Relaxed);
    let _ = cliclack::intro(title);
}

pub fn end(message: impl Display) {
    if wizard() {
        let _ = cliclack::outro(message);
    } else {
        println!("{message}");
    }
}

pub fn info(message: impl Display) {
    if wizard() {
        let _ = cliclack::log::info(message);
    } else {
        println!("{message}");
    }
}

pub fn warn(message: impl Display) {
    if wizard() {
        let _ = cliclack::log::warning(message);
    } else {
        eprintln!("WARNING: {message}");
    }
}

pub fn alert(headline: &str, line: &str) {
    if wizard() {
        let _ = cliclack::log::warning(format!("{}\n{line}", console::style(headline).yellow().bold()));
    } else {
        eprintln!("WARNING: {headline}. {line}");
    }
}

pub fn detail(message: impl Display) {
    if !wizard() {
        println!("{message}");
    }
}

pub fn fail(error: &anyhow::Error) {
    if wizard() {
        let _ = cliclack::outro_cancel(format!("{error:#}"));
    } else {
        eprintln!("sarab: {error:#}");
    }
}

pub struct Task {
    bar: Option<ProgressBar>,
    started: String,
}

impl Task {
    pub fn spin(message: impl Display) -> Self {
        let started = message.to_string();
        if !wizard() {
            println!("{started}");
            return Self { bar: None, started };
        }
        let bar = cliclack::spinner();
        bar.start(&started);
        Self { bar: Some(bar), started }
    }

    pub fn bytes(total: u64, message: impl Display) -> Self {
        let started = message.to_string();
        if !wizard() {
            return Self { bar: None, started };
        }
        let bar = cliclack::progress_bar(total).with_download_template();
        bar.start(&started);
        Self { bar: Some(bar), started }
    }

    pub fn set(&self, position: u64) {
        if let Some(b) = &self.bar {
            b.set_position(position);
        }
    }

    pub fn message(&self, message: impl Display) {
        match &self.bar {
            Some(b) => b.set_message(message),
            None => println!("{message}"),
        }
    }

    pub fn done(mut self, message: impl Display) {
        if let Some(b) = self.bar.take() {
            b.stop(message);
        }
    }

    pub fn failed(mut self, message: impl Display) {
        match self.bar.take() {
            Some(b) => b.error(message),
            None => eprintln!("{message}"),
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(b) = self.bar.take() {
            b.error(&self.started);
        }
    }
}
