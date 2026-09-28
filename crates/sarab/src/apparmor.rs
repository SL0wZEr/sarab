//! The `sarab` side of Ubuntu's user-namespace restriction (the rules, the
//! profile text and the fix line live in `sarab_ns::apparmor`, so sarab-ns
//! says the same). `profile` fills in this install's sarab-ns, resolved
//! through symlinks since AppArmor attaches by the real path; `sarab
//! apparmor-profile` prints it, and the fix pipes that into `sudo tee`.
//!
//! `status` is the host check: fine when the kernel restricts nothing, fine
//! when `PROFILE_FILE` is exactly this install's profile, and otherwise the
//! reason sarab-ns will fail. `sarab setup` refuses on it before it downloads
//! anything (`check`), and `sarab info` shows it as a row. A file that is
//! there but not loaded yet passes; sarab-ns itself catches that case at its
//! unshare, with the same fix.

use crate::paths::Dirs;
use anyhow::{Result, anyhow};
use sarab_ns::apparmor::{self, PROFILE_FILE};

pub fn profile(dirs: &Dirs) -> Result<String> {
    let ns = dirs.helper("sarab-ns")?;
    let ns = std::fs::canonicalize(&ns).unwrap_or(ns);
    apparmor::profile(&ns).map_err(|e| anyhow!(e))
}

pub fn print(dirs: &Dirs) -> Result<()> {
    print!("{}", profile(dirs)?);
    Ok(())
}

pub fn status(dirs: &Dirs) -> Result<String, String> {
    if !apparmor::restricted() {
        return Ok("user namespaces not restricted".into());
    }
    let want = profile(dirs).map_err(|e| format!("{e:#}"))?;
    match std::fs::read_to_string(PROFILE_FILE) {
        Ok(have) if have == want => Ok(format!("user namespaces restricted; sarab-ns allowed by {PROFILE_FILE}")),
        Ok(_) => Err(format!("{PROFILE_FILE} is not this install's profile (another sarab-ns, or edited)")),
        Err(_) => Err(format!("no AppArmor profile allows sarab-ns ({PROFILE_FILE})")),
    }
}

pub fn check(dirs: &Dirs) -> Result<()> {
    status(dirs).map(drop).map_err(|why| anyhow!(apparmor::blocked(&why)))
}
