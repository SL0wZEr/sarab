//! Freeze the runtime when it has had no window for a while; anything that
//! talks to it through `sarab` thaws it again on the way in. It runs on a
//! thread of the daemon, one binder call per reading.
//!
//! The signal is the composer HAL's open-windows property, which it keeps equal
//! to the number of Android surfaces it has mapped on the Wayland side. Polling
//! it is one binder call every POLL seconds while awake, and nothing at all
//! while frozen -- a frozen runtime cannot answer, and must not be woken just
//! to be asked whether it is idle. While `is_frozen()` is true we therefore
//! read the cgroup only, never binder. A failed read (getprop, or an unreadable
//! socket table) is never evidence of quiet and never a reason to wake: it
//! resets the idle counter. Host sessions -- processes in Android's mount
//! namespace but outside the runtime's cgroup (`sarab exec`, `sarab logs -f`,
//! the `pm` behind `sarab app install`) -- count as windows, because freezing
//! under one would hang it at its next binder call. session.rs shares
//! `host_sessions`, since a restart would kill such a session just the same.
//! The binder handle is dropped on every freeze and on every failed read, since
//! it is useless while frozen and the runtime may be a different one by the
//! time we are awake again.
//!
//! Second stage: RECLAIM_AFTER seconds after freezing, push the tree to swap
//! (`reclaim()`; no-op without swap). Freeze is free to undo (13 ms); reclaim
//! costs a refault storm on the next use (~2.5 s on the first app launch,
//! measured), so it waits for a long idle rather than a short one.
//! `SARAB_RECLAIM=0` turns this stage off; `SARAB_RECLAIM_AFTER` moves it.
//!
//! **Waking for traffic.** A frozen tree still owns its TCP connections -- the
//! kernel holds them and keeps ACKing, so a server's data lands in the socket's
//! receive queue with nothing to read it (measured 2026-09-16: 11 of 12 sockets
//! survived 20 minutes frozen, and 4246 bytes queued within the first minute).
//! So the runtime is not unreachable while frozen, it is merely *not
//! listening*, and the queue depth says when that matters. We read it from the
//! host -- `/proc/<ns>/net/tcp` is the kernel's table, so asking costs the
//! frozen runtime nothing and keeps "0 Android wakeups" literally true -- and
//! thaw when it grows. Growth, not depth: a queue that was already full when we
//! froze may belong to something that will never drain it, and thawing on that
//! forever would be a loop. Only ESTABLISHED sockets (`st` 01) count in
//! `queued_rx`: a listening socket's queue is a backlog of connections, not
//! data. Unparseable lines are skipped, not guessed at. `FROZEN_POLL` re-reads
//! the queue within one frozen tick (with POLL alone the thaw landed 4.6 s after
//! a push); POLL must stay a whole multiple of it, and `wait_tick` returns early
//! only on the tick that thaws, which zeroes `frozen_for`, so the reclaim clock
//! never drifts. `SARAB_WAKE_ON_TRAFFIC=0` restores "frozen means frozen".
//!
//! The wake is useless on its own, which the same measurement showed: thawing
//! bought 60 s of wake, most of it spent refaulting out of zram, and every
//! connection was gone by the end of it. So a traffic wake also buys a longer
//! grace (`SARAB_WAKE_GRACE`, 180 s) before the next freeze -- enough to
//! finish a reconnect (the normal 60 s idle was measured too short). Opening a
//! window puts the ordinary idle rule back. Reclaim needs no special case: a
//! runtime being woken every few minutes never reaches RECLAIM_AFTER *frozen*,
//! so it never reclaims, and one that is genuinely idle still does.
//!
//! The watcher thread is detached and sleeps before its first reading, since
//! the runtime is still booting. Three consecutive "runtime gone" readings end
//! the watcher (not the daemon: `start` owns the lifecycle), so it cannot
//! outlive the runtime by more than 15 s. `State::step` is pure: the caller
//! performs the action, and whether it succeeds does not change the state.

use sarab_runtime::Platform;
use std::time::Duration;

const POLL: Duration = Duration::from_secs(5);

const FROZEN_POLL: Duration = Duration::from_secs(1);

const WAKE_GRACE: Duration = Duration::from_secs(180);

#[derive(Debug, PartialEq, Clone, Copy)]
enum Sample {
    Gone,
    Frozen(Option<u64>),
    Running(Option<u32>),
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Action {
    Wait,
    Freeze(u64),
    Thaw(u64),
    Reclaim,
    Stop,
}

#[derive(Default)]
struct State {
    quiet: Duration,
    frozen_for: Duration,
    reclaimed: bool,
    gone: u32,
    rx_at_freeze: Option<u64>,
    woke_for_traffic: bool,
}

impl State {
    fn step(&mut self, s: Sample, idle: Duration, reclaim_after: Option<Duration>, grace: Duration) -> Action {
        match s {
            Sample::Gone => {
                self.gone += 1;
                if self.gone >= 3 { Action::Stop } else { Action::Wait }
            }
            Sample::Frozen(rx) => {
                self.gone = 0;
                self.quiet = Duration::ZERO;
                self.frozen_for += POLL;
                if let Some(rx) = rx {
                    match self.rx_at_freeze {
                        None => self.rx_at_freeze = Some(rx),
                        Some(base) if rx > base => {
                            self.rx_at_freeze = None;
                            self.woke_for_traffic = true;
                            self.frozen_for = Duration::ZERO;
                            self.reclaimed = false;
                            return Action::Thaw(rx - base);
                        }
                        Some(_) => {}
                    }
                }
                match reclaim_after {
                    Some(after) if !self.reclaimed && self.frozen_for >= after => {
                        self.reclaimed = true;
                        Action::Reclaim
                    }
                    _ => Action::Wait,
                }
            }
            Sample::Running(open) => {
                self.gone = 0;
                self.frozen_for = Duration::ZERO;
                self.reclaimed = false;
                self.rx_at_freeze = None;
                if matches!(open, Some(n) if n > 0) {
                    self.woke_for_traffic = false;
                }
                let idle = if self.woke_for_traffic { grace } else { idle };
                match open {
                    None => self.quiet = Duration::ZERO,
                    Some(0) => self.quiet += POLL,
                    Some(_) => self.quiet = Duration::ZERO,
                }
                if self.quiet >= idle {
                    let quiet = self.quiet;
                    self.quiet = Duration::ZERO;
                    self.woke_for_traffic = false;
                    Action::Freeze(quiet.as_secs())
                } else {
                    Action::Wait
                }
            }
        }
    }
}

fn queued_rx(table: &str) -> u64 {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 5 || f[3] != "01" {
                return None;
            }
            u64::from_str_radix(f[4].split(':').nth(1)?, 16).ok()
        })
        .sum()
}

fn queued_rx_of(pid: u32) -> Option<u64> {
    let mut any = false;
    let mut total = 0;
    for f in ["tcp", "tcp6"] {
        if let Ok(t) = std::fs::read_to_string(format!("/proc/{pid}/net/{f}")) {
            any = true;
            total += queued_rx(&t);
        }
    }
    any.then_some(total)
}

fn sample(p: &mut Option<Platform>, wake_on_traffic: bool, ns: &mut Option<u32>) -> Sample {
    let Ok((pid, dev)) = sarab_runtime::find_runtime() else {
        *ns = None;
        return Sample::Gone;
    };
    *ns = Some(pid);
    if sarab_runtime::is_frozen() {
        return Sample::Frozen(wake_on_traffic.then(|| queued_rx_of(pid)).flatten());
    }
    if p.is_none() {
        *p = Platform::connect(&dev).ok();
    }
    let open: Option<u32> =
        p.as_ref().and_then(|p| p.getprop("waydroid.open_windows", "0").ok()).and_then(|v| v.trim().parse().ok());
    if open.is_none() {
        *p = None;
    }
    Sample::Running(open.map(|n| n + host_sessions(pid) as u32))
}

fn is_host_session(mnt: &str, android_mnt: &str, cgroup: &str, runtime_cgroup: &str) -> bool {
    mnt == android_mnt && !cgroup.starts_with(runtime_cgroup)
}

pub fn host_sessions(rt: u32) -> usize {
    let Ok(android_mnt) = std::fs::read_link(format!("/proc/{rt}/ns/mnt")) else { return 0 };
    let android_mnt = android_mnt.to_string_lossy().into_owned();
    let Some(runtime_cgroup) = sarab_runtime::runtime_cgroup() else { return 0 };
    let Ok(rel) = runtime_cgroup.strip_prefix("/sys/fs/cgroup") else { return 0 };
    let runtime_cgroup = format!("0::/{}", rel.display());
    let Ok(rd) = std::fs::read_dir("/proc") else { return 0 };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(mnt) = std::fs::read_link(format!("/proc/{pid}/ns/mnt")) else { return false };
            let mnt = mnt.to_string_lossy();
            if mnt != android_mnt {
                return false;
            }
            let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
            is_host_session(&mnt, &android_mnt, cg.trim(), &runtime_cgroup)
        })
        .count()
}

fn wait_tick(watch: Option<(u32, u64)>) {
    let Some((pid, base)) = watch else {
        std::thread::sleep(POLL);
        return;
    };
    let mut slept = Duration::ZERO;
    while slept < POLL {
        std::thread::sleep(FROZEN_POLL);
        slept += FROZEN_POLL;
        if matches!(queued_rx_of(pid), Some(rx) if rx > base) {
            return;
        }
    }
}

pub fn spawn(idle: Duration) {
    let wake_on_traffic = std::env::var("SARAB_WAKE_ON_TRAFFIC").as_deref() != Ok("0");
    let grace =
        std::env::var("SARAB_WAKE_GRACE").ok().and_then(|s| s.parse().ok()).map_or(WAKE_GRACE, Duration::from_secs);
    let reclaim_after = match std::env::var("SARAB_RECLAIM").as_deref() {
        Ok("0") => None,
        _ => Some(Duration::from_secs(
            std::env::var("SARAB_RECLAIM_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(600),
        )),
    };
    std::thread::spawn(move || {
        let mut st = State::default();
        let mut p = None;
        let mut ns = None;
        let mut watch = None;
        loop {
            wait_tick(watch);
            let s = sample(&mut p, wake_on_traffic, &mut ns);
            let action = st.step(s, idle, reclaim_after, grace);
            watch = match (matches!(s, Sample::Frozen(Some(_))), st.rx_at_freeze, ns) {
                (true, Some(base), Some(pid)) => Some((pid, base)),
                _ => None,
            };
            match action {
                Action::Wait => {}
                Action::Freeze(quiet) => {
                    match sarab_runtime::set_frozen(true) {
                        Ok(()) => println!("idle: frozen after {quiet}s without a window"),
                        Err(e) => eprintln!("idle: freeze failed: {e}"),
                    }
                    p = None;
                }
                Action::Thaw(bytes) => match sarab_runtime::set_frozen(false) {
                    Ok(()) => println!("idle: thawed; {bytes} B arrived while frozen (awake for {}s)", grace.as_secs()),
                    Err(e) => eprintln!("idle: thaw failed: {e}"),
                },
                Action::Reclaim => match sarab_runtime::reclaim() {
                    Ok(Some(left)) => println!("idle: reclaimed; {} MB still resident", left / 1048576),
                    Ok(None) => println!("idle: no swap on the host; nothing reclaimed"),
                    Err(e) => eprintln!("idle: reclaim failed: {e}"),
                },
                Action::Stop => {
                    println!("idle: runtime gone; watcher exiting");
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(60);
    const AFTER: Option<Duration> = Some(Duration::from_secs(600));
    const GRACE: Duration = Duration::from_secs(180);
    #[test]
    fn a_frozen_tick_is_a_whole_number_of_fast_polls() {
        assert_eq!(POLL.as_secs() % FROZEN_POLL.as_secs(), 0);
        assert!(FROZEN_POLL < POLL, "a fast poll that is not faster buys nothing");
    }

    const QUIET: Sample = Sample::Frozen(Some(0));

    fn run(st: &mut State, s: Sample, n: usize) -> Vec<Action> {
        (0..n).map(|_| st.step(s, IDLE, AFTER, GRACE)).collect()
    }

    #[test]
    fn quiet_accumulates_and_a_window_resets_it() {
        let mut st = State::default();
        assert!(run(&mut st, Sample::Running(Some(0)), 11).iter().all(|a| *a == Action::Wait));
        assert_eq!(st.quiet, Duration::from_secs(55));
        assert_eq!(st.step(Sample::Running(Some(1)), IDLE, AFTER, GRACE), Action::Wait);
        assert_eq!(st.quiet, Duration::ZERO);
        run(&mut st, Sample::Running(Some(0)), 3);
        assert_eq!(st.step(Sample::Running(None), IDLE, AFTER, GRACE), Action::Wait);
        assert_eq!(st.quiet, Duration::ZERO);
    }

    #[test]
    fn freezes_at_the_threshold_and_resets_quiet() {
        let mut st = State::default();
        let a = run(&mut st, Sample::Running(Some(0)), 12);
        assert_eq!(a[11], Action::Freeze(60), "60 s of quiet is 12 polls");
        assert!(a[..11].iter().all(|a| *a == Action::Wait));
        assert_eq!(st.quiet, Duration::ZERO);
    }

    #[test]
    fn reclaims_once_per_freeze_episode() {
        let mut st = State::default();
        let a = run(&mut st, QUIET, 200);
        assert_eq!(a.iter().filter(|a| **a == Action::Reclaim).count(), 1);
        assert_eq!(a[119], Action::Reclaim, "600 s frozen is 120 polls");
        st.step(Sample::Running(Some(1)), IDLE, AFTER, GRACE);
        assert!(!st.reclaimed && st.frozen_for == Duration::ZERO);
        assert_eq!(run(&mut st, QUIET, 120).iter().filter(|a| **a == Action::Reclaim).count(), 1);
    }

    #[test]
    fn sarab_reclaim_zero_only_freezes() {
        let mut st = State::default();
        assert!((0..500).all(|_| st.step(QUIET, IDLE, None, GRACE) == Action::Wait));
    }

    #[test]
    fn three_gone_reads_stop_the_watcher() {
        let mut st = State::default();
        assert_eq!(run(&mut st, Sample::Gone, 3), vec![Action::Wait, Action::Wait, Action::Stop]);
        let mut st = State::default();
        st.step(Sample::Gone, IDLE, AFTER, GRACE);
        st.step(Sample::Gone, IDLE, AFTER, GRACE);
        st.step(QUIET, IDLE, AFTER, GRACE);
        assert_eq!(st.step(Sample::Gone, IDLE, AFTER, GRACE), Action::Wait);
    }

    #[test]
    fn a_growing_receive_queue_thaws_and_a_static_one_does_not() {
        let mut st = State::default();
        assert_eq!(st.step(Sample::Frozen(Some(4246)), IDLE, AFTER, GRACE), Action::Wait);
        let a = run(&mut st, Sample::Frozen(Some(4246)), 240);
        assert!(!a.iter().any(|x| matches!(x, Action::Thaw(_))));
        assert_eq!(a.iter().filter(|x| **x == Action::Reclaim).count(), 1);
        assert_eq!(st.step(Sample::Frozen(Some(9000)), IDLE, AFTER, GRACE), Action::Thaw(4754));
    }

    #[test]
    fn an_unreadable_table_never_wakes_anything() {
        let mut st = State::default();
        assert!(!run(&mut st, Sample::Frozen(None), 300).iter().any(|a| matches!(a, Action::Thaw(_))));
        assert_eq!(st.rx_at_freeze, None);
    }

    #[test]
    fn a_traffic_wake_buys_the_longer_grace_before_refreezing() {
        let mut st = State::default();
        st.step(Sample::Frozen(Some(0)), IDLE, AFTER, GRACE);
        assert_eq!(st.step(Sample::Frozen(Some(10)), IDLE, AFTER, GRACE), Action::Thaw(10));
        let a = run(&mut st, Sample::Running(Some(0)), 12);
        assert!(a.iter().all(|x| *x == Action::Wait), "60 s must not refreeze after a traffic wake");
        let a = run(&mut st, Sample::Running(Some(0)), 24);
        assert_eq!(a[23], Action::Freeze(180));
        assert!(!st.woke_for_traffic);
    }

    #[test]
    fn a_window_puts_the_ordinary_idle_rule_back() {
        let mut st = State::default();
        st.step(Sample::Frozen(Some(0)), IDLE, AFTER, GRACE);
        st.step(Sample::Frozen(Some(10)), IDLE, AFTER, GRACE);
        assert!(st.woke_for_traffic);
        st.step(Sample::Running(Some(1)), IDLE, AFTER, GRACE);
        assert!(!st.woke_for_traffic);
        assert_eq!(run(&mut st, Sample::Running(Some(0)), 12)[11], Action::Freeze(60));
    }

    #[test]
    fn traffic_keeps_a_runtime_out_of_reclaim_without_a_special_case() {
        let mut st = State::default();
        let mut rx = 0;
        let mut actions = vec![];
        for _ in 0..6 {
            actions.extend(run(&mut st, Sample::Frozen(Some(rx)), 24));
            rx += 4246;
            actions.push(st.step(Sample::Frozen(Some(rx)), IDLE, AFTER, GRACE));
            actions.extend(run(&mut st, Sample::Running(Some(0)), 36));
        }
        assert!(!actions.contains(&Action::Reclaim), "a woken runtime must not reclaim");
        assert_eq!(actions.iter().filter(|a| matches!(a, Action::Thaw(_))).count(), 6);
        let mut st = State::default();
        assert_eq!(run(&mut st, QUIET, 130).iter().filter(|a| **a == Action::Reclaim).count(), 1);
    }

    #[test]
    fn a_host_session_is_in_androids_namespace_but_not_its_cgroup() {
        let android = "mnt:[4026532871]";
        let rt = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/sarab-4242.scope";
        assert!(is_host_session(
            android,
            android,
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foot.scope",
            rt
        ));
        assert!(!is_host_session(android, android, &format!("{rt}/android"), rt));
        assert!(!is_host_session(android, android, rt, rt));
        assert!(!is_host_session("mnt:[4026531841]", android, "0::/user.slice", rt));
    }

    #[test]
    fn queued_rx_reads_established_sockets_only() {
        let t = "\
  sl  local_address rem_address   st tx_queue:rx_queue tr:tm->when retrnsmt   uid  timeout inode
   0: 0102FEA9:80F6 AED1FB8E:01BB 01 00000000:000010A6 02:00000B98 00000000 10061 0 12345 1 0 10
   1: 0102FEA9:80F7 AED1FB8E:01BB 01 00000000:00000001 02:00000B98 00000000 10061 0 12346 1 0 10
   2: 00000000:1F90 00000000:0000 0A 00000000:00000FFF 00:00000000 00000000  1000 0 12347 1 0 10
";
        assert_eq!(queued_rx(t), 4263);
        assert_eq!(queued_rx("sl local_address\n"), 0, "header only");
        assert_eq!(queued_rx(""), 0);
        assert_eq!(queued_rx("sl ...\n  0: junk\n"), 0, "a short line is skipped, not guessed");
    }
}
