//! `sarab google-id`: the one step between a fresh install and a working Play
//! Store.
//!
//! Google refuses to sign in on a device that is not Play Protect certified,
//! and an emulator image never is. It does let a person register a device ID
//! by hand -- the Google Services Framework's `android_id`, a decimal number --
//! at `REGISTER_URL`, and from then on the account works. Reading that number
//! means `sqlite3` inside Android as root; that is this command. The only
//! subtlety is ANDROID_RUNTIME_ROOT, which android.rs sets and without which
//! sqlite3 aborts on its first query.
//!
//! The ID exists only after GSF has checked in with Google once, which needs a
//! network and takes up to a minute after the first boot. Hence `--wait`. A
//! missing database, or one without its table yet, is "not checked in yet", not
//! an error: GSF creates it on its first run. The database is opened read-only,
//! and only once it exists, since sqlite3 would otherwise create an empty one,
//! owned by root, where GSF is about to make its own. Any other sqlite3 failure
//! is reported as it is, not as "no ID yet". `wait_for_id` is that wait,
//! returning nothing when the time runs out, and `steps` the text that goes
//! with the ID; the first-run wizard (setup.rs) uses both, and shows them
//! without a command to know about.

use crate::android::{self, User};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

const DB: &str = "/data/data/com.google.android.gsf/databases/gservices.db";
const QUERY: &str = "select value from main where name = 'android_id'";
pub const REGISTER_URL: &str = "https://www.google.com/android/uncertified";

fn parse_id(out: &str) -> Option<String> {
    let v = out.lines().next()?.trim();
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit())).then(|| v.to_string())
}

fn read_id() -> Result<Option<String>> {
    if !android::output(User::Root, &["test", "-f", DB])?.status.success() {
        return Ok(None);
    }
    let o = android::output(User::Root, &["sqlite3", "-readonly", DB, QUERY])?;
    let err = String::from_utf8_lossy(&o.stderr);
    if !o.status.success() {
        if err.contains("no such table") {
            return Ok(None);
        }
        bail!("could not read the Google ID: sqlite3 {}: {}", o.status, err.trim());
    }
    Ok(parse_id(&String::from_utf8_lossy(&o.stdout)))
}

pub fn wait_for_id(wait: Duration) -> Result<Option<String>> {
    crate::app::connect()?;
    let t0 = Instant::now();
    loop {
        if let Some(id) = read_id()? {
            return Ok(Some(id));
        }
        if t0.elapsed() >= wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn steps(id: &str) -> String {
    format!(
        "Google lets only certified devices sign in, so register this one, once:\n\n\
         Google ID: {id}\n\n\
         1. Open {REGISTER_URL}\n\
         2. Sign in, paste the ID above, and press Register\n\
         3. Wait a few minutes, then run `sarab restart`\n\n\
         Until then, Google Play services keeps posting a reminder with a sound.\n\
         The ID changes if Android's data is wiped; register the new one then.\n\
         `sarab google-id` shows it again."
    )
}

pub fn run(wait: u64, quiet: bool) -> Result<()> {
    let Some(id) = wait_for_id(Duration::from_secs(wait))? else {
        bail!(
            "no Google ID yet: Google Services Framework has not checked in.\n\
             It needs a network connection and up to a minute after the first boot;\n\
             `sarab google-id --wait 120` waits for it."
        );
    };
    if quiet {
        println!("{id}");
        return Ok(());
    }
    println!("{}", steps(&id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_one_decimal_number_or_nothing() {
        assert_eq!(parse_id("1234567890123456789\n").as_deref(), Some("1234567890123456789"));
        assert_eq!(parse_id(""), None);
        assert_eq!(parse_id("\n"), None);
        assert_eq!(parse_id("Error: no such table: main\n"), None);
    }
}
