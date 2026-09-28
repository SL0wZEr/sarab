//! The built `sarab` binary, run from the outside the way a person or a script
//! runs it: its help, its completions, its error messages and its exit codes.
//! No Android is needed, so CI runs these.
//!
//! Each test gets a `Sandbox`: a throwaway directory that passes for a checkout
//! (a Cargo.toml next to overlay/, which is all `SARAB_ROOT` asks for), with
//! the XDG directories inside it too, so nothing reads or writes the real
//! image, data or config of the machine running the tests. `SARAB_NO_LAZY_START`
//! keeps any command from booting Android. The directory goes away on drop,
//! even when the test fails.
//!
//! The exit codes are an interface: 2 for a command line clap rejects, 1 for a
//! command that fails (with `sarab: ` and the reason on stderr), and 3 from
//! `status` when Android is not running, the LSB init-script code for a
//! stopped service. The tests that need Android to be stopped check `runtime_running`
//! first and skip, so they pass on a development machine with Android up.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("sarab-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("overlay")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "[workspace]\n").unwrap();
        Sandbox(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn run(&self, args: &[&str]) -> Output {
        let d = &self.0;
        Command::new(env!("CARGO_BIN_EXE_sarab"))
            .args(args)
            .current_dir(d)
            .env("SARAB_ROOT", d)
            .env("SARAB_NO_LAZY_START", "1")
            .env("XDG_CONFIG_HOME", d.join("config"))
            .env("XDG_DATA_HOME", d.join("share"))
            .env("XDG_STATE_HOME", d.join("state"))
            .env("XDG_RUNTIME_DIR", d.join("run"))
            .output()
            .expect("run sarab")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn runtime_running() -> bool {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "sarab-ns"))
}

#[test]
fn help_lists_every_everyday_command() {
    let s = Sandbox::new("help");
    let o = s.run(&["--help"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    for cmd in ["setup", "start", "stop", "status", "install", "app", "exec", "logs", "info", "completion"] {
        assert!(out.contains(&format!("\n  {cmd} ")), "--help lacks {cmd}:\n{out}");
    }
    for hidden in ["internal", "reclaim", "props", "overlay-binds"] {
        assert!(!out.contains(&format!("\n  {hidden} ")), "--help shows {hidden}");
    }
    let o = s.run(&["--version"]);
    assert_eq!((o.status.code(), stdout(&o)), (Some(0), format!("sarab {}\n", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn no_command_says_how_to_begin() {
    let s = Sandbox::new("bare");
    let o = s.run(&[]);
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).starts_with("Sarab is not set up yet. `sarab start` sets it up"), "{}", stdout(&o));
    assert!(stdout(&o).contains("Usage: sarab [COMMAND]"));
}

#[test]
fn setup_offers_the_tested_build_or_the_latest_but_not_both_with_a_zip() {
    let s = Sandbox::new("setup");
    let o = s.run(&["setup", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    for flag in ["--system", "--vendor", "--data-dir", "--vanilla", "--latest", "--force"] {
        assert!(stdout(&o).contains(flag), "setup --help lacks {flag}");
    }
    for zip in ["--system", "--vendor"] {
        let o = s.run(&["setup", "--latest", zip, "x.zip"]);
        assert_eq!(o.status.code(), Some(2));
        assert!(stderr(&o).contains("cannot be used with"), "{}", stderr(&o));
    }
}

#[test]
fn completions_are_generated_for_each_shell() {
    let s = Sandbox::new("completion");
    for shell in ["bash", "fish", "zsh"] {
        let o = s.run(&["completion", shell]);
        assert_eq!(o.status.code(), Some(0), "{shell}: {}", stderr(&o));
        let out = stdout(&o);
        assert!(out.contains("sarab") && out.contains("setup") && out.contains("latest"), "{shell}");
    }
    let o = s.run(&["completion", "tcsh"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("possible values"));
}

#[test]
fn a_command_line_clap_rejects_exits_2() {
    let s = Sandbox::new("usage");
    for args in [&["boot"][..], &["stop", "now"], &["app"], &["exec", "-u"], &["start", "--lifetime", "5"]] {
        let o = s.run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains("--help"), "{args:?}: {}", stderr(&o));
        assert!(stdout(&o).is_empty(), "{args:?}");
    }
}

#[test]
fn status_exits_3_when_android_is_not_running() {
    if runtime_running() {
        eprintln!("skipped: a sarab runtime is running on this machine");
        return;
    }
    let s = Sandbox::new("status");
    let o = s.run(&["status"]);
    assert_eq!((o.status.code(), stdout(&o).as_str()), (Some(3), "not running\n"));
    let o = s.run(&["status", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v, serde_json::json!({ "running": false }));
}

#[test]
fn commands_that_need_android_fail_with_a_reason_when_it_is_stopped() {
    if runtime_running() {
        eprintln!("skipped: a sarab runtime is running on this machine");
        return;
    }
    let s = Sandbox::new("stopped");
    for args in [&["pause"][..], &["unpause"]] {
        let o = s.run(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(stderr(&o).starts_with("sarab: ") && stderr(&o).contains("not running"), "{}", stderr(&o));
    }
}

#[test]
fn info_describes_a_checkout_that_is_not_set_up() {
    let s = Sandbox::new("info");
    let o = s.run(&["info"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with(&format!("Sarab {}\n", env!("CARGO_PKG_VERSION"))));
    assert!(out.contains("image          not set up"), "{out}");
    assert!(out.contains(&format!("mode           checkout ({})", s.path().display())), "{out}");
    assert!(out.contains(&format!("data           {}", s.path().join("images").display())), "{out}");
}

#[test]
fn internal_remove_deletes_nothing_but_an_image() {
    let s = Sandbox::new("remove");
    let keep = s.path().join("home");
    std::fs::create_dir_all(keep.join("docs")).unwrap();
    let o = s.run(&["internal", "remove", &keep.display().to_string()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("does not look like an extracted image; not deleting it"), "{}", stderr(&o));
    assert!(keep.join("docs").is_dir());
}
