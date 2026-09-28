//! sarab — a rootless Android runtime for the Linux desktop. One command, in
//! the shape of `docker`/`podman`: verbs for the runtime itself (start, stop,
//! status, pause), noun groups for what is inside it (app, prop, settings,
//! policy), and a few shortcuts for what people type every day (install).
//!
//! It is also the runtime's owner. `sarab start --foreground` (start.rs) is
//! what the systemd unit runs: it stays in the foreground for the life of
//! Android and takes its helpers with it when it ends. Android's system server
//! binds the host binder services (clipboard, notifications, user monitor,
//! hardware) **once**, during startup -- register them late and they are never
//! picked up, no error, just a runtime with no clipboard -- so `sarab-hostd
//! --wait` goes up before the namespace, every time.
//!
//! The helpers that remain separate binaries are the ones that must be:
//! `sarab-ns` (the namespace harness, which becomes Android's parent) and
//! `sarab-hostd` (the binder services). Nobody types either, so they are not
//! meant for PATH: `paths::helper` finds them in `lib/sarab/` or beside us.
//!
//! The clap attributes' `about`/`help` strings are the `--help` text.
//! `sarab app launch` and `sarab app intent` are also an interface: hostd
//! writes them into every .desktop entry. `main` restores the default SIGPIPE
//! action, because Rust starts with it ignored and `sarab app ls | head` would
//! otherwise end in an EPIPE panic instead of just ending. It also sets the
//! umask to 022: what sarab writes for Android (the generated overlay, the
//! prop file) is read by an init that skips group-writable .rc files, and a
//! desktop umask of 002, Ubuntu's default, made every generated one that
//! (overlay.rs). hostd, which sarab starts, inherits it.

mod android;
mod app;
mod apparmor;
mod freeze;
mod google;
mod host;
mod image;
mod info;
mod logs;
mod net;
mod overlay;
mod paths;
mod policy;
mod purge;
mod screen;
mod session;
mod settings_cmd;
mod setup;
mod start;
mod stop;
mod ui;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "sarab",
    version,
    about = "A rootless Android runtime for the Linux desktop",
    after_help = "`sarab start` boots Android, and sets it up the first time. Every command has --help.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    #[command(about = "Download and prepare the Android image (`start` does it the first time; safe to repeat)")]
    Setup {
        #[arg(long, value_name = "ZIP", help = "Use this system image zip instead of downloading one")]
        system: Option<PathBuf>,
        #[arg(long, value_name = "ZIP", help = "Use this vendor image zip instead of downloading one")]
        vendor: Option<PathBuf>,
        #[arg(
            long,
            value_name = "DIR",
            help = "Keep the image and Android's data here (installed sarab only; remembered)"
        )]
        data_dir: Option<PathBuf>,
        #[arg(long, help = "Download the image without Google apps (no Play Store)")]
        vanilla: bool,
        #[arg(
            long,
            conflicts_with_all = ["system", "vendor"],
            help = "Download the newest build instead of the tested one (untested; its checksum comes from the same server)"
        )]
        latest: bool,
        #[arg(long, help = "Extract again even if the image is already there")]
        force: bool,
    },
    #[command(about = "Move Android to the image this Sarab is tested with, keeping its apps and data")]
    Upgrade {
        #[arg(short, long, help = "Do not ask first (Android restarts if it is running)")]
        yes: bool,
        #[arg(long, help = "Only say whether an upgrade is due")]
        check: bool,
    },
    #[command(about = "Boot Android; returns once it is up (the first time, sets it up with you)")]
    Start(StartArgs),
    #[command(about = "Stop Android")]
    Stop {
        #[arg(long, hide = true, help = "Stop the runtime directly, never through systemd (the unit's own ExecStop)")]
        direct: bool,
    },
    #[command(about = "Stop Android, then start it again")]
    Restart,
    #[command(about = "Is Android running, paused, booting?")]
    Status {
        #[arg(long, help = "Machine-readable output")]
        json: bool,
    },
    #[command(about = "Freeze every Android process (no CPU, no wakeups; memory is kept)")]
    Pause,
    #[command(about = "Resume a paused Android")]
    Unpause,
    #[command(about = "Install an app (shortcut for `sarab app install`)")]
    Install {
        #[arg(help = "An .apk, or a split bundle (.apks, .xapk)")]
        file: PathBuf,
    },
    #[command(subcommand, about = "Manage apps")]
    App(AppCmd),
    #[command(about = "Run a command inside Android (a shell when none is given)")]
    Exec {
        #[arg(short, long, default_value = "root", help = "Run as this Android user: root, shell, system, or a uid")]
        user: String,
        #[arg(
            short,
            long,
            value_delimiter = ',',
            help = "Extra supplementary groups, as gids (e.g. 3003,3009); the user's own are always given"
        )]
        groups: Vec<u32>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, help = "The command and its arguments")]
        command: Vec<String>,
    },
    #[command(about = "Show Android's log (logcat), or the host-side logs")]
    Logs(LogsArgs),
    #[command(about = "Live memory, CPU and process count of the running Android")]
    Stats {
        #[arg(long, help = "Print one reading and exit")]
        no_stream: bool,
        #[arg(long, help = "Machine-readable output (one JSON object per reading)")]
        json: bool,
    },
    #[command(subcommand, about = "Read or set Android system properties")]
    Prop(PropCmd),
    #[command(subcommand, about = "Read or change Android settings")]
    Settings(settings_cmd::SettingsCmd),
    #[command(subcommand, about = "The desktop package policy: what of the image is switched off")]
    Policy(PolicyCmd),
    #[command(about = "Print this device's Google ID, to register it so the Play Store works")]
    GoogleId {
        #[arg(
            long,
            value_name = "SECS",
            default_value_t = 0,
            help = "Wait up to this many seconds for Google Services to check in"
        )]
        wait: u64,
        #[arg(short, long, help = "Print only the ID")]
        quiet: bool,
    },
    #[command(about = "Versions, paths and host checks (paste this into a bug report)")]
    Info,
    #[command(about = "Delete the Android image and Android's data (asks first; Android must be stopped)")]
    Purge {
        #[arg(long, help = "Delete only Android's data (apps, accounts, settings), keep the image")]
        data_only: bool,
        #[arg(long, short, help = "Do not ask")]
        yes: bool,
    },
    #[command(about = "Print a shell completion script")]
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(
        hide = true,
        about = "Print the AppArmor profile that lets sarab-ns create Android's namespace (Ubuntu 23.10 and later)"
    )]
    ApparmorProfile,
    #[command(hide = true, about = "Push a paused Android's memory out to swap")]
    Reclaim,
    #[command(hide = true, about = "The vendor property file the runtime boots with (plumbing)")]
    Props,
    #[command(hide = true, about = "The overlay `--bind` arguments, one word per line (plumbing)")]
    OverlayBinds,
    #[command(subcommand, hide = true, about = "Steps of other commands that must run inside the user namespace")]
    Internal(InternalCmd),
}

#[derive(Args, Debug)]
struct StartArgs {
    #[arg(short = 'F', long, help = "Stay in the foreground as the runtime's owner (what the systemd unit runs)")]
    foreground: bool,
    #[arg(
        long,
        value_name = "SECS",
        requires = "foreground",
        help = "Stop after SECS (with --foreground; default: run until `sarab stop`)"
    )]
    lifetime: Option<u64>,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 60,
        help = "Pause Android after SECS without a window; 0 never pauses"
    )]
    idle_freeze: u64,
    #[arg(
        long,
        requires = "foreground",
        help = "Do not start the host services (clipboard, notifications, launcher entries, location)"
    )]
    no_hostd: bool,
    #[arg(long, requires = "foreground", help = "Boot without networking")]
    no_network: bool,
}

#[derive(Subcommand, Debug)]
enum AppCmd {
    #[command(visible_alias = "list", about = "List the installed apps that have a launcher entry")]
    Ls {
        #[arg(long, help = "Machine-readable output")]
        json: bool,
        #[arg(short, long, help = "Print package names only")]
        quiet: bool,
    },
    #[command(about = "Install an .apk, or a split bundle (.apks, .xapk)")]
    Install { file: PathBuf },
    #[command(visible_aliases = ["remove", "uninstall"], about = "Remove an app you installed")]
    Rm { package: String },
    #[command(about = "Open an app in its own window")]
    Launch { package: String },
    #[command(about = "Show the full Android interface instead of a single app")]
    Show,
    #[command(about = "Details about one app")]
    Inspect {
        package: String,
        #[arg(long, help = "Machine-readable output")]
        json: bool,
    },
    #[command(about = "Send an intent (e.g. android.intent.action.VIEW and a web address)")]
    Intent { action: String, uri: String },
}

#[derive(Subcommand, Debug)]
enum PropCmd {
    #[command(about = "Print a property")]
    Get {
        name: String,
        #[arg(help = "Printed when the property is unset")]
        default: Option<String>,
    },
    #[command(about = "Set a property")]
    Set { name: String, value: String },
    #[command(about = "List properties, optionally only those whose name contains PATTERN")]
    Ls { pattern: Option<String> },
}

#[derive(Subcommand, Debug)]
enum PolicyCmd {
    #[command(about = "Switch off what a desktop does not need (automatic on first boot)")]
    Apply,
    #[command(about = "Switch everything back on")]
    Revert,
    #[command(about = "Show what is on and off")]
    Status,
}

#[derive(Args, Debug)]
struct LogsArgs {
    #[arg(short, long, help = "Keep printing new lines as they arrive")]
    follow: bool,
    #[arg(short = 'n', long, value_name = "N", help = "Only the last N lines")]
    tail: Option<u32>,
    #[arg(long, group = "source", help = "Android's kernel log (init's /dev/kmsg)")]
    kernel: bool,
    #[arg(long, group = "source", help = "The runtime owner's own output (sarab start)")]
    daemon: bool,
    #[arg(long, group = "source", help = "The host services: clipboard, notifications, launcher entries, location")]
    hostd: bool,
    #[arg(last = true, help = "Extra logcat arguments, after `--` (e.g. -- -s ActivityManager)")]
    logcat: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum InternalCmd {
    Extract { raw: PathBuf, tree: PathBuf },
    Remove { tree: PathBuf },
}

fn exit_with(code: i32) -> ! {
    std::process::exit(code)
}

fn run(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.cmd else {
        let mut c = Cli::command();
        if paths::dirs().is_ok_and(|d| !d.is_set_up()) {
            println!("Sarab is not set up yet. `sarab start` sets it up and boots Android.\n");
        }
        c.print_help()?;
        return Ok(());
    };
    match cmd {
        Cmd::Setup { system, vendor, data_dir, vanilla, latest, force } => setup::run(setup::Opts {
            system: system.map(|p| app::absolute(&p)).transpose()?,
            vendor: vendor.map(|p| app::absolute(&p)).transpose()?,
            data_dir: data_dir.map(|p| app::absolute(&p)).transpose()?,
            vanilla,
            latest,
            force,
        }),
        Cmd::Upgrade { yes, check } => setup::upgrade(yes, check),
        Cmd::Start(a) if a.foreground => start::start(start::Opts {
            lifetime: a.lifetime,
            hostd: !a.no_hostd,
            idle_freeze: a.idle_freeze,
            network: !a.no_network,
        }),
        Cmd::Start(_) => {
            if let Ok((pid, _)) = sarab_runtime::find_runtime() {
                println!("Android is already running (pid {pid})");
                return Ok(());
            }
            let dirs = paths::dirs()?;
            if !dirs.is_set_up() {
                return setup::first_run(&dirs);
            }
            for note in image::behind(&dirs.data) {
                println!("{note}");
            }
            session::ensure_running(&paths::dirs()?, &|s| println!("{s}")).map(drop)
        }
        Cmd::Stop { direct } => session::stop(direct),
        Cmd::Restart => session::restart(&paths::dirs()?),
        Cmd::Status { json } => stop::status(json),
        Cmd::Pause => {
            android::init_pid()?;
            sarab_runtime::set_frozen(true)?;
            println!("paused");
            Ok(())
        }
        Cmd::Unpause => {
            android::init_pid()?;
            sarab_runtime::set_frozen(false)?;
            println!("running");
            Ok(())
        }
        Cmd::Install { file } | Cmd::App(AppCmd::Install { file }) => {
            app::install(&paths::dirs()?, &app::absolute(&file)?)
        }
        Cmd::App(a) => match a {
            AppCmd::Ls { json, quiet } => app::ls(json, quiet),
            AppCmd::Install { .. } => unreachable!("matched above"),
            AppCmd::Rm { package } => app::rm(&package),
            AppCmd::Launch { package } => app::launch(&paths::dirs()?, &package),
            AppCmd::Show => app::show(&paths::dirs()?),
            AppCmd::Inspect { package, json } => app::inspect(&package, json),
            AppCmd::Intent { action, uri } => app::intent(&paths::dirs()?, &action, &uri),
        },
        Cmd::Exec { user, groups, command } => {
            exit_with(android::exec(android::User::parse(&user)?, &groups, &command)?)
        }
        Cmd::Logs(l) => {
            let source = if l.kernel {
                logs::Source::Kernel
            } else if l.daemon {
                logs::Source::Daemon
            } else if l.hostd {
                logs::Source::Hostd
            } else {
                logs::Source::Android
            };
            let code =
                logs::run(&paths::dirs()?, logs::Opts { source, follow: l.follow, tail: l.tail, extra: l.logcat })?;
            exit_with(code)
        }
        Cmd::Stats { no_stream, json } => info::stats(!no_stream, json),
        Cmd::Prop(p) => match p {
            PropCmd::Get { name, default } => {
                println!("{}", app::connect()?.getprop(&name, default.as_deref().unwrap_or(""))?);
                Ok(())
            }
            PropCmd::Set { name, value } => app::connect()?.setprop(&name, &value),
            PropCmd::Ls { pattern } => {
                let out = android::run(android::User::Root, &["getprop"])?;
                let name = |l: &str| l.split(']').next().unwrap_or("").trim_start_matches('[').to_string();
                for l in out.lines().filter(|l| pattern.as_deref().is_none_or(|p| name(l).contains(p))) {
                    println!("{l}");
                }
                Ok(())
            }
        },
        Cmd::Settings(s) => settings_cmd::run(s),
        Cmd::Policy(p) => match p {
            PolicyCmd::Apply => policy::apply(),
            PolicyCmd::Revert => policy::revert(),
            PolicyCmd::Status => policy::status(),
        },
        Cmd::GoogleId { wait, quiet } => google::run(wait, quiet),
        Cmd::Info => info::info(&paths::dirs()?),
        Cmd::Purge { data_only, yes } => purge::run(data_only, yes),
        Cmd::Completion { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "sarab", &mut std::io::stdout());
            Ok(())
        }
        Cmd::Reclaim => {
            match sarab_runtime::reclaim()? {
                Some(left) => println!("reclaimed; {} MB still resident", left / 1_048_576),
                None => println!("no swap on the host; nothing reclaimed"),
            }
            Ok(())
        }
        Cmd::ApparmorProfile => apparmor::print(&paths::dirs()?),
        Cmd::Props => start::print_props(),
        Cmd::OverlayBinds => start::print_overlay_binds(),
        Cmd::Internal(InternalCmd::Extract { raw, tree }) => image::extract_inner(&raw, &tree),
        Cmd::Internal(InternalCmd::Remove { tree }) => image::remove_inner(&tree),
    }
}

fn main() {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    unsafe { libc::umask(0o022) };
    if let Err(e) = run(Cli::parse()) {
        ui::fail(&e);
        exit_with(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("sarab").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn upgrade_asks_unless_told_yes() {
        let Some(Cmd::Upgrade { yes, check }) = p(&["upgrade"]).unwrap().cmd else { panic!() };
        assert!(!yes && !check);
        let Some(Cmd::Upgrade { yes, check }) = p(&["upgrade", "-y", "--check"]).unwrap().cmd else { panic!() };
        assert!(yes && check);
    }

    #[test]
    fn start_detaches_unless_asked_to_stay() {
        let Some(Cmd::Start(a)) = p(&["start"]).unwrap().cmd else { panic!() };
        assert!(!a.foreground);
        assert_eq!(a.idle_freeze, 60);
        let Some(Cmd::Start(a)) =
            p(&["start", "-F", "--lifetime", "45", "--no-hostd", "--idle-freeze", "0"]).unwrap().cmd
        else {
            panic!()
        };
        assert!(a.foreground && a.no_hostd && !a.no_network);
        assert_eq!((a.lifetime, a.idle_freeze), (Some(45), 0));
        assert!(p(&["start", "--lifetime", "45"]).is_err());
        assert!(p(&["start", "--no-network"]).is_err());
        assert!(p(&["start", "--lifetime", "soon", "-F"]).is_err());
    }

    #[test]
    fn exec_passes_everything_after_the_user_through() {
        let Some(Cmd::Exec { user, command, .. }) =
            p(&["exec", "-u", "shell", "pm", "list", "packages", "-3"]).unwrap().cmd
        else {
            panic!()
        };
        assert_eq!(user, "shell");
        assert_eq!(command, ["pm", "list", "packages", "-3"]);
        let Some(Cmd::Exec { user, command, .. }) = p(&["exec"]).unwrap().cmd else { panic!() };
        assert_eq!((user.as_str(), command.len()), ("root", 0));
        let Some(Cmd::Exec { groups, command, .. }) = p(&["exec", "-g", "3003,3009", "id", "-G"]).unwrap().cmd else {
            panic!()
        };
        assert_eq!((groups, command), (vec![3003, 3009], vec!["id".to_string(), "-G".to_string()]));
    }

    #[test]
    fn app_aliases_and_the_install_shortcut() {
        assert!(matches!(p(&["app", "list"]).unwrap().cmd, Some(Cmd::App(AppCmd::Ls { .. }))));
        assert!(matches!(p(&["app", "uninstall", "x"]).unwrap().cmd, Some(Cmd::App(AppCmd::Rm { .. }))));
        assert!(matches!(p(&["install", "a.apk"]).unwrap().cmd, Some(Cmd::Install { .. })));
        assert!(matches!(p(&["app", "launch", "com.x"]).unwrap().cmd, Some(Cmd::App(AppCmd::Launch { .. }))));
        assert!(matches!(
            p(&["app", "intent", "android.settings.APPLICATION_DETAILS_SETTINGS", "package:com.x"]).unwrap().cmd,
            Some(Cmd::App(AppCmd::Intent { .. }))
        ));
    }

    #[test]
    fn logs_sources_are_exclusive_and_logcat_args_come_after_dashes() {
        let Some(Cmd::Logs(l)) = p(&["logs", "-f", "-n", "20", "--", "-s", "ActivityManager"]).unwrap().cmd else {
            panic!()
        };
        assert!(l.follow && !l.kernel);
        assert_eq!(l.tail, Some(20));
        assert_eq!(l.logcat, ["-s", "ActivityManager"]);
        assert!(p(&["logs", "--kernel", "--hostd"]).is_err());
    }

    #[test]
    fn nonsense_is_rejected() {
        assert!(p(&["boot"]).is_err());
        assert!(p(&["stop", "now"]).is_err());
        assert!(p(&["app"]).is_err());
        assert!(p(&["exec", "-u"]).is_err());
    }
}
