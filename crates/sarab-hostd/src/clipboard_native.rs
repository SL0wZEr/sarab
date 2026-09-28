//! Native wlr-data-control clipboard backend — no `wl-copy`/`wl-paste`.
//!
//! `zwlr_data_control_manager_v1` is the "clipboard manager" protocol: a
//! client reads and sets the seat's selection without owning a surface or
//! having focus, which is exactly what a headless daemon needs. Hyprland
//! 0.56 exposes it and the renamed `ext_data_control_v1`; they are the same
//! protocol and we implement one.
//!
//! Shape: one thread owns the Wayland connection and event queue. Binder
//! threads hand it requests over an mpsc channel plus a wake pipe (poll()
//! watches the Wayland socket and the pipe) and wait on a reply channel with
//! a bounded timeout, so a stuck compositor can never wedge a binder thread.
//! The data pipes are read and written off the Wayland thread, with their
//! own bounded waits, so it keeps dispatching `send` events while a transfer
//! is in flight. `read_bounded` and `write_bounded` use a non-blocking fd plus
//! poll(), because a blocking read would sit forever on a source that never
//! closes its end; `wait_fd` rounds the remaining time *up* to poll()'s
//! millisecond so it never gives up before the deadline, and polls again
//! with what is left when a signal interrupts it. `write_bounded`
//! always closes the fd, which is what tells the reader the transfer is over,
//! and treats EPIPE (Rust ignores SIGPIPE) as a reader that simply left.
//!
//! When the selection is our own source, `get` answers from `last_set`
//! (`Got::Ours`) instead of asking the compositor: that would need the Wayland
//! thread to serve its own `send` while it waits on the pipe. An empty
//! clipboard, or one holding no text (an image copied on the desktop), is an
//! empty answer, never the text Android set earlier, which is no longer what
//! the desktop's clipboard holds. In
//! `State::get`, the pipe's fd only travels with the flush, and our copy of
//! the write end must be dropped after it or the reader never sees EOF.
//! `TEXT_MIMES` is in reading preference order: `text/plain;charset=utf-8` is
//! what wl-copy sends and GTK/Qt/Chromium prefer, the X11 atoms still come
//! from older toolkits through Xwayland, and `pick_mime` falls back to any
//! `text/plain` variant. Version 1 never sends a primary selection; one that
//! arrives anyway is destroyed so it does not leak.
//!
//! The ignored `native_get_probe` test binds the protocol on the running
//! compositor and only reads (`set` would clobber the user's clipboard); run it
//! with `cargo test -p sarab-hostd -- --ignored --nocapture native_get_probe`.

use crate::clipboard::Clipboard;
use crate::wire::logln;
use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use std::io::ErrorKind;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
    zwlr_data_control_offer_v1 as offer, zwlr_data_control_source_v1 as source,
};

pub const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BYTES: usize = 8 << 20;

const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

pub fn pick_mime(offered: &[String]) -> Option<&str> {
    TEXT_MIMES
        .iter()
        .find_map(|want| offered.iter().find(|m| m == want).map(String::as_str))
        .or_else(|| offered.iter().find(|m| m.starts_with("text/plain")).map(String::as_str))
}

fn set_nonblock(fd: &OwnedFd) -> Result<()> {
    use nix::fcntl::{FcntlArg, fcntl};
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

fn wait_fd(fd: &impl AsFd, events: i16, deadline: Instant) -> bool {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ms = left.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd { fd: fd.as_fd().as_raw_fd(), events, revents: 0 };
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            n if n > 0 => return true,
            0 => return false,
            _ if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

pub fn read_bounded(fd: OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    set_nonblock(&fd)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match nix::unistd::read(&fd, &mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_BYTES {
                    bail!("selection larger than {MAX_BYTES} bytes");
                }
            }
            Err(Errno::EAGAIN) => {
                if !wait_fd(&fd, libc::POLLIN, deadline) {
                    bail!("source did not finish writing within {timeout:?}");
                }
            }
            Err(Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn write_bounded(fd: OwnedFd, data: &[u8], timeout: Duration) -> Result<()> {
    set_nonblock(&fd)?;
    let deadline = Instant::now() + timeout;
    let mut off = 0;
    while off < data.len() {
        match nix::unistd::write(&fd, &data[off..]) {
            Ok(n) => off += n,
            Err(Errno::EAGAIN) => {
                if !wait_fd(&fd, libc::POLLOUT, deadline) {
                    bail!("sink did not drain within {timeout:?}");
                }
            }
            Err(Errno::EINTR) => {}
            Err(Errno::EPIPE) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

enum Request {
    Set(String, Sender<Result<()>>),
    Get(Sender<Result<Got>>),
}

enum Got {
    Ours,
    Empty,
    Pipe(OwnedFd),
}

struct State {
    manager: manager::ZwlrDataControlManagerV1,
    device: device::ZwlrDataControlDeviceV1,
    offer: Option<offer::ZwlrDataControlOfferV1>,
    own: Option<source::ZwlrDataControlSourceV1>,
    finished: bool,
}

pub struct NativeClipboard {
    tx: Sender<Request>,
    wake: OwnedFd,
    last_set: Mutex<Option<String>>,
}

impl NativeClipboard {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let (globals, mut queue): (_, EventQueue<State>) = registry_queue_init(&conn)?;
        let qh = queue.handle();
        let manager: manager::ZwlrDataControlManagerV1 =
            globals.bind(&qh, 1..=1, ()).context("compositor does not expose zwlr_data_control_manager_v1")?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
        let device = manager.get_data_device(&seat, &qh, ());
        let mut state = State { manager, device, offer: None, own: None, finished: false };
        queue.roundtrip(&mut state)?;
        if state.finished {
            bail!("compositor finished the data device at once");
        }

        let (tx, rx) = channel();
        let (wake_r, wake_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
        std::thread::Builder::new().name("clipboard-wl".into()).spawn(move || serve(conn, queue, state, rx, wake_r))?;
        Ok(Self { tx, wake: wake_w, last_set: Mutex::new(None) })
    }

    fn call<T>(&self, make: impl FnOnce(Sender<Result<T>>) -> Request) -> Result<T> {
        let (rtx, rrx) = channel();
        self.tx.send(make(rtx)).map_err(|_| anyhow::anyhow!("clipboard thread is gone"))?;
        let _ = nix::unistd::write(&self.wake, &[1]);
        rrx.recv_timeout(TIMEOUT).context("clipboard thread did not answer within 2s")?
    }
}

impl Clipboard for NativeClipboard {
    fn set(&self, text: &str) -> Result<()> {
        self.call(|r| Request::Set(text.to_string(), r))?;
        *self.last_set.lock().unwrap() = Some(text.to_string());
        Ok(())
    }

    fn get(&self) -> Result<String> {
        match self.call(Request::Get)? {
            Got::Ours => Ok(self.last_set.lock().unwrap().clone().unwrap_or_default()),
            Got::Empty => Ok(String::new()),
            Got::Pipe(fd) => Ok(String::from_utf8_lossy(&read_bounded(fd, TIMEOUT)?).into_owned()),
        }
    }
}

fn serve(conn: Connection, mut queue: EventQueue<State>, mut state: State, rx: Receiver<Request>, wake: OwnedFd) {
    let qh = queue.handle();
    loop {
        if let Err(e) = queue.dispatch_pending(&mut state) {
            logln!("clipboard: wayland dispatch failed: {e}");
            return;
        }
        if state.finished {
            logln!("clipboard: compositor finished the data device");
            return;
        }
        if let Err(e) = conn.flush() {
            logln!("clipboard: wayland flush failed: {e}");
            return;
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let mut pfds = [
            libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } < 0
            && std::io::Error::last_os_error().kind() != ErrorKind::Interrupted
        {
            return;
        }
        if pfds[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => {
                    logln!("clipboard: wayland read failed: {e}");
                    return;
                }
            }
        } else {
            drop(guard);
        }
        if pfds[1].revents != 0 {
            let mut sink = [0u8; 64];
            while nix::unistd::read(&wake, &mut sink).is_ok_and(|n| n == sink.len()) {}
        }
        loop {
            match rx.try_recv() {
                Ok(Request::Set(text, reply)) => {
                    let _ = reply.send(state.set(&qh, text));
                }
                Ok(Request::Get(reply)) => {
                    let _ = reply.send(state.get(&conn));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

impl State {
    fn set(&mut self, qh: &QueueHandle<State>, text: String) -> Result<()> {
        let source = self.manager.create_data_source(qh, Arc::new(text));
        for mime in TEXT_MIMES {
            source.offer(mime.to_string());
        }
        self.device.set_selection(Some(&source));
        if let Some(old) = self.own.replace(source) {
            old.destroy();
        }
        Ok(())
    }

    fn get(&mut self, conn: &Connection) -> Result<Got> {
        if self.own.is_some() {
            return Ok(Got::Ours);
        }
        let Some(offer) = &self.offer else { return Ok(Got::Empty) };
        let mimes = offer.data::<Mutex<Vec<String>>>().unwrap().lock().unwrap().clone();
        let Some(mime) = pick_mime(&mimes) else { return Ok(Got::Empty) };
        let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        offer.receive(mime.to_string(), w.as_fd());
        conn.flush()?;
        drop(w);
        Ok(Got::Pipe(r))
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore manager::ZwlrDataControlManagerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<device::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &device::ZwlrDataControlDeviceV1,
        ev: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            device::Event::DataOffer { .. } => {}
            device::Event::Selection { id } => {
                if let Some(old) = st.offer.take() {
                    old.destroy();
                }
                st.offer = id;
            }
            device::Event::Finished => st.finished = true,
            device::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, device::ZwlrDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (offer::ZwlrDataControlOfferV1, Mutex::new(Vec::<String>::new())),
    ]);
}

impl Dispatch<offer::ZwlrDataControlOfferV1, Mutex<Vec<String>>> for State {
    fn event(
        _: &mut Self,
        _: &offer::ZwlrDataControlOfferV1,
        ev: offer::Event,
        mimes: &Mutex<Vec<String>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = ev {
            mimes.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<source::ZwlrDataControlSourceV1, Arc<String>> for State {
    fn event(
        st: &mut Self,
        src: &source::ZwlrDataControlSourceV1,
        ev: source::Event,
        text: &Arc<String>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            source::Event::Send { fd, .. } => {
                let text = text.clone();
                std::thread::spawn(move || {
                    if let Err(e) = write_bounded(fd, text.as_bytes(), TIMEOUT) {
                        logln!("clipboard: send failed: {e}");
                    }
                });
            }
            source::Event::Cancelled => {
                src.destroy();
                if st.own.as_ref().is_some_and(|o| o.id() == src.id()) {
                    st.own = None;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn pipe() -> (OwnedFd, OwnedFd) {
        nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn mime_preference() {
        assert_eq!(pick_mime(&[]), None);
        assert_eq!(pick_mime(&s(&["image/png", "text/html"])), None);
        assert_eq!(
            pick_mime(&s(&["text/plain", "STRING", "text/plain;charset=utf-8"])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(pick_mime(&s(&["STRING", "UTF8_STRING"])), Some("UTF8_STRING"));
        assert_eq!(pick_mime(&s(&["text/html", "text/plain"])), Some("text/plain"));
        assert_eq!(pick_mime(&s(&["text/html", "text/plain;charset=us-ascii"])), Some("text/plain;charset=us-ascii"));
    }

    #[test]
    fn read_to_eof() {
        let (r, w) = pipe();
        let t = std::thread::spawn(move || {
            let mut w = std::fs::File::from(w);
            w.write_all(b"hello ").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            w.write_all(b"world").unwrap();
        });
        assert_eq!(read_bounded(r, TIMEOUT).unwrap(), b"hello world");
        t.join().unwrap();
    }

    #[test]
    fn read_gives_up_on_stuck_writer() {
        let (r, w) = pipe();
        nix::unistd::write(&w, b"partial").unwrap();
        let t0 = Instant::now();
        let err = read_bounded(r, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
        assert!(t0.elapsed() >= Duration::from_millis(200));
        assert!(t0.elapsed() < Duration::from_secs(2));
        drop(w);
    }

    #[test]
    fn write_all_then_close() {
        let (r, w) = pipe();
        let data = vec![b'x'; 300_000];
        let d2 = data.clone();
        let t = std::thread::spawn(move || write_bounded(w, &d2, TIMEOUT));
        let mut got = Vec::new();
        std::fs::File::from(r).read_to_end(&mut got).unwrap();
        t.join().unwrap().unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn write_gives_up_on_stuck_reader() {
        let (r, w) = pipe();
        let data = vec![b'x'; 1 << 20];
        let t0 = Instant::now();
        let err = write_bounded(w, &data, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("did not drain"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(2));
        drop(r);
    }

    #[test]
    fn write_to_closed_reader_is_not_an_error() {
        let (r, w) = pipe();
        drop(r);
        write_bounded(w, b"gone", TIMEOUT).unwrap();
    }

    #[test]
    #[ignore]
    fn native_get_probe() {
        let c = NativeClipboard::new().expect("wlr-data-control unavailable");
        let n = c.get().expect("get failed").len();
        eprintln!("probe: bound zwlr_data_control_manager_v1, selection is {n} bytes");
    }
}
