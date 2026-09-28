//! The display relay: Android's composer reaches the compositor through us, so
//! each Android window can be told its own size.
//!
//! The composer draws Android's display, whose size the prop file sets, and
//! scales it onto a toplevel with `wp_viewport.set_destination`. Nothing tells
//! the compositor that this is the only size the content has, so a tiling
//! compositor gives the window a tile, and the frame and the content disagree
//! (measured on Hyprland 0.56.2: a 701x910 tile around a 480x1000 display).
//! A toplevel whose `set_min_size` equals its `set_max_size` is a fixed-size
//! window, which Hyprland and sway float by themselves; that is a heuristic of
//! theirs, not protocol, so the README keeps a window rule as the fallback. The
//! relay adds those two requests for every toplevel whose app id starts with
//! `APP_PREFIX`, which the composer gives app windows, and takes the size from
//! the composer's own `set_destination` on that surface, never from a number of
//! ours. The composer computes the destination from the display size and the
//! output's scale, so the fixed size follows it through a scale change, and
//! the prop file stays the only place the size is set. The size is
//! double-buffered state, so `Tracker::request` puts the pair right before a
//! `wl_surface.commit`, whenever the destination about to be committed differs
//! from the size it last fixed; a destination the composer unsets, or a
//! viewport it destroys, releases the window again with 0x0.
//!
//! `sarab start` creates the listening socket, binds it into Android where the
//! compositor's would be, and hands it to us before the runtime starts, so it
//! exists before the composer looks for it and connections queue until
//! `serve`'s thread accepts them. `listener` refuses an fd that is not a
//! listening socket, so a wrong `--wayland-fd` fails at startup instead of as
//! accept errors in a loop, and makes it close-on-exec again (`sarab start` had
//! to clear that for us to inherit it), so the processes hostd starts, the icon
//! helper and wl-copy among them, never hold it open. Each connection gets its
//! own connection to the compositor (`upstream`, the path of the real socket,
//! looked up anew for every connection) and two threads, one per direction, in
//! `relay`, which share the connection's `Tracker` behind one mutex (`Conn`,
//! which also holds the upstream socket the request side writes to); the event
//! side takes it only to name objects in the trace. Both directions are cut
//! into whole messages (`frames`, which holds back a message until all of it
//! has arrived); requests pass through `Tracker`, which reads the arguments of
//! only the requests it tracks and adds nothing but whole messages between
//! whole messages, and events pass through untouched. When either side closes,
//! both sockets are shut down, so the composer sees what it would have seen on
//! the compositor's own socket.
//!
//! `SARAB_WAYLAND_TRACE=1` in hostd's environment (`sarab start` passes its
//! own on) logs every message in both directions (`trace`): `->` for requests,
//! `<-` for events, the object id and, when known, its interface, the opcode,
//! and the first eight argument words as integers. The interface is known for
//! every global Android binds and every object `Tracker` follows. It is loud,
//! about 800 lines a second while an app is open, since the composer commits
//! every frame.
//!
//! File descriptors travel beside the bytes (`SCM_RIGHTS`) and libwayland
//! takes them from a queue in the order the messages ask for them, so it is
//! enough that none arrives after its message: `send` attaches every pending fd
//! to the next bytes it sends, at most `MAX_FDS` per sendmsg (libwayland's
//! limit, above which the receiver drops the message), and `recv` accepts the
//! kernel's 253.
//!
//! `Tracker` knows objects only from the requests that create them: the
//! registry from `wl_display.get_registry`, the three globals it cares about
//! from `wl_registry.bind`, and surfaces, viewports, xdg surfaces and
//! toplevels from their constructors. Every one of these is destroyed by a
//! request of the client's own, and libwayland does not reuse an id before
//! the compositor confirms the destruction, so dropping an entry at its
//! destructor is enough to keep a reused id from being read as the old object.
//! Ids the client never tracked are copied through, opcodes and all.
//!
//! The cursor needs its own translation. The composer shows Android's
//! pointer icon as the host cursor: a bitmap drawn at Android's density,
//! cropped and halved with a viewport (`set_source` 12,0 38x50,
//! `set_destination` 19x25 at density 320). Hyprland ignores a viewport on a
//! cursor surface and draws the whole bitmap at one pixel per logical pixel,
//! so the arrow came out twice the size Android meant, and every change to the
//! viewport made no difference on screen. A buffer scale is what cursors
//! honour everywhere, so for a surface `wl_pointer.set_cursor` names, the
//! relay keeps the composer's viewport requests to itself and, right before
//! each commit, `cursor_commit` states the same scale as `set_buffer_scale`
//! of the composer's own ratio (source over destination, `cursor_scale`, with
//! the source read by `cursor_crop`, the whole bitmap when none is set) and
//! unsets the viewport. It needs the bitmap's size, so shm buffers are
//! tracked from `wl_shm_pool.create_buffer` to `wl_surface.attach`. A buffer
//! whose sides are not a multiple of the scale would be a protocol error that
//! ends the connection, and so would a fractional ratio; then the composer's
//! viewport goes through unchanged. The hotspot in `set_cursor` is moved from
//! the cropped, scaled picture into the whole bitmap (`cursor_hotspot`), so
//! the click lands on the tip. The first `set_cursor` for a surface comes after
//! its first commit, so that commit's viewport is replaced right away, before
//! the `set_cursor`, with a commit of our own: the surface's pending state is
//! empty there, and a surface with no role yet takes a commit at any time.
//! The first cursor a connection converts is logged once (`cursor_logged`).
//!
//! Whether the composer sends `set_app_id` and `set_destination` before the
//! commit that maps the window is not documented anywhere, so for the first
//! app window after hostd starts, `Tracker` logs the order of the requests
//! that concern its surface, up to that first commit with a buffer (`TRACED`,
//! `Window::trace`).

use crate::wire::logln;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, Shutdown, getsockopt, recvmsg, sendmsg, shutdown, sockopt,
};
use std::collections::HashMap;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

const APP_PREFIX: &str = "waydroid.";
const MAX_FDS: usize = 28;
const TRACE_LEN: usize = 32;
static TRACED: AtomicBool = AtomicBool::new(false);
static TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var("SARAB_WAYLAND_TRACE").is_ok_and(|v| !v.is_empty() && v != "0"));

const DISPLAY: u32 = 1;
const DISPLAY_GET_REGISTRY: u16 = 1;
const REGISTRY_BIND: u16 = 0;
const COMPOSITOR_CREATE_SURFACE: u16 = 0;
const SURFACE_DESTROY: u16 = 0;
const SURFACE_ATTACH: u16 = 1;
const SURFACE_COMMIT: u16 = 6;
const WM_BASE_DESTROY: u16 = 0;
const WM_BASE_GET_XDG_SURFACE: u16 = 2;
const XDG_SURFACE_DESTROY: u16 = 0;
const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
const TOPLEVEL_DESTROY: u16 = 0;
const TOPLEVEL_SET_APP_ID: u16 = 3;
const TOPLEVEL_SET_MAX_SIZE: u16 = 7;
const TOPLEVEL_SET_MIN_SIZE: u16 = 8;
const VIEWPORTER_DESTROY: u16 = 0;
const VIEWPORTER_GET_VIEWPORT: u16 = 1;
const VIEWPORT_DESTROY: u16 = 0;
const VIEWPORT_SET_DESTINATION: u16 = 2;
const VIEWPORT_SET_SOURCE: u16 = 1;
const SURFACE_SET_BUFFER_SCALE: u16 = 8;
const SHM_CREATE_POOL: u16 = 0;
const POOL_CREATE_BUFFER: u16 = 0;
const POOL_DESTROY: u16 = 1;
const BUFFER_DESTROY: u16 = 0;
const UNSET: u32 = -256i32 as u32;
const SEAT_GET_POINTER: u16 = 0;
const SEAT_RELEASE: u16 = 3;
const POINTER_SET_CURSOR: u16 = 0;
const POINTER_RELEASE: u16 = 1;

pub fn listener(fd: RawFd) -> anyhow::Result<UnixListener> {
    let l = unsafe { UnixListener::from_raw_fd(fd) };
    match getsockopt(&l, sockopt::AcceptConn) {
        Ok(true) => {}
        _ => anyhow::bail!("--wayland-fd {fd} is not a listening socket"),
    }
    fcntl(&l, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(l)
}

pub fn serve(listener: UnixListener, upstream: PathBuf) {
    logln!("wayland: relaying Android's display to {}", upstream.display());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let down = match conn {
                Ok(s) => s,
                Err(e) => {
                    logln!("wayland: accept failed: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
            };
            match UnixStream::connect(&upstream) {
                Ok(up) => {
                    if let Err(e) = relay(down, up) {
                        logln!("wayland: could not relay a connection: {e}");
                    }
                }
                Err(e) => logln!("wayland: cannot reach the compositor at {}: {e}", upstream.display()),
            }
        }
    });
}

struct Conn {
    up: UnixStream,
    tracker: Tracker,
}

fn relay(down: UnixStream, up: UnixStream) -> std::io::Result<()> {
    logln!("wayland: Android connected");
    let conn = Arc::new(Mutex::new(Conn { up: up.try_clone()?, tracker: Tracker::default() }));
    let done = Arc::new(AtomicBool::new(false));
    let (down2, up2, conn2, done2) = (down.try_clone()?, up.try_clone()?, conn.clone(), done.clone());
    std::thread::spawn(move || {
        let why = requests(&down, &conn);
        ended(&done, "Android", &why, &down, &up);
    });
    std::thread::spawn(move || {
        let why = events(&up2, &down2, &conn2);
        ended(&done2, "the compositor", &why, &down2, &up2);
    });
    Ok(())
}

fn ended(done: &AtomicBool, side: &str, why: &str, down: &UnixStream, up: &UnixStream) {
    if !done.swap(true, Ordering::Relaxed) {
        logln!("wayland: a connection ended ({side}: {why})");
    }
    let _ = shutdown(down.as_raw_fd(), Shutdown::Both);
    let _ = shutdown(up.as_raw_fd(), Shutdown::Both);
}

fn requests(down: &UnixStream, conn: &Mutex<Conn>) -> String {
    let mut buf = vec![0u8; 16 * 1024];
    let (mut held, mut fds) = (Vec::new(), Vec::new());
    loop {
        match recv(down.as_raw_fd(), &mut buf, &mut fds) {
            Ok(0) => return "closed".into(),
            Ok(n) => held.extend_from_slice(&buf[..n]),
            Err(e) => return format!("read failed: {e}"),
        }
        let mut c = conn.lock().unwrap_or_else(|p| p.into_inner());
        let out = match c.tracker.requests(&mut held) {
            Ok(out) => out,
            Err(e) => return e,
        };
        if let Err(e) = send(c.up.as_raw_fd(), &out, &mut fds) {
            return format!("write failed: {e}");
        }
    }
}

fn events(up: &UnixStream, down: &UnixStream, conn: &Mutex<Conn>) -> String {
    let mut buf = vec![0u8; 16 * 1024];
    let (mut held, mut fds) = (Vec::new(), Vec::new());
    loop {
        match recv(up.as_raw_fd(), &mut buf, &mut fds) {
            Ok(0) => return "closed".into(),
            Ok(n) => held.extend_from_slice(&buf[..n]),
            Err(e) => return format!("read failed: {e}"),
        }
        let out = match conn.lock().unwrap_or_else(|p| p.into_inner()).tracker.events(&mut held) {
            Ok(out) => out,
            Err(e) => return e,
        };
        if let Err(e) = send(down.as_raw_fd(), &out, &mut fds) {
            return format!("write failed: {e}");
        }
    }
}

fn recv(fd: RawFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> nix::Result<usize> {
    let mut space = nix::cmsg_space!([RawFd; 253]);
    loop {
        let mut iov = [IoSliceMut::new(buf)];
        let m = match recvmsg::<()>(fd, &mut iov, Some(&mut space), MsgFlags::MSG_CMSG_CLOEXEC) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };
        for c in m.cmsgs()? {
            if let ControlMessageOwned::ScmRights(v) = c {
                fds.extend(v.into_iter().map(|f| unsafe { OwnedFd::from_raw_fd(f) }));
            }
        }
        if m.flags.contains(MsgFlags::MSG_CTRUNC) {
            return Err(Errno::EMSGSIZE);
        }
        return Ok(m.bytes);
    }
}

fn send(fd: RawFd, data: &[u8], fds: &mut Vec<OwnedFd>) -> nix::Result<()> {
    let mut off = 0;
    while off < data.len() {
        let take = fds.len().min(MAX_FDS);
        let raw: Vec<RawFd> = fds[..take].iter().map(AsRawFd::as_raw_fd).collect();
        let rights = [ControlMessage::ScmRights(&raw)];
        let cmsgs: &[ControlMessage] = if take > 0 { &rights } else { &[] };
        let end = if fds.len() > take { off + 1 } else { data.len() };
        let n = match sendmsg::<()>(fd, &[IoSlice::new(&data[off..end])], cmsgs, MsgFlags::MSG_NOSIGNAL, None) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };
        fds.drain(..take);
        off += n;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Obj {
    Registry,
    Compositor,
    WmBase,
    Viewporter,
    Surface,
    XdgSurface(u32),
    Toplevel(u32),
    Viewport(u32),
    Seat,
    Pointer,
    Shm,
    ShmPool,
    Buffer,
}

#[derive(Default)]
struct Window {
    toplevel: Option<u32>,
    app_id: Option<String>,
    destination: Option<(i32, i32)>,
    fixed: Option<(i32, i32)>,
    attached: bool,
    trace: Option<Vec<String>>,
    viewport: Option<u32>,
    cursor: bool,
    source: Option<[u32; 4]>,
    buffer: Option<(i32, i32)>,
}

impl Window {
    fn is_app(&self) -> bool {
        self.toplevel.is_some() && self.app_id.as_deref().is_some_and(|a| a.starts_with(APP_PREFIX))
    }

    fn note(&mut self, what: impl FnOnce() -> String) {
        if let Some(t) = &mut self.trace
            && t.len() < TRACE_LEN
        {
            t.push(what());
        }
    }
}

#[derive(Default)]
struct Tracker {
    objs: HashMap<u32, Obj>,
    windows: HashMap<u32, Window>,
    names: HashMap<u32, String>,
    buffers: HashMap<u32, (i32, i32)>,
    cursor_logged: bool,
}

fn word(msg: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(msg.get(at..at + 4)?.try_into().ok()?))
}

fn string(msg: &[u8], at: usize) -> Option<(String, usize)> {
    let len = word(msg, at)? as usize;
    let bytes = msg.get(at + 4..at + 4 + len)?;
    let text = std::str::from_utf8(bytes.strip_suffix(&[0])?).ok()?.to_string();
    Some((text, at + 4 + len.div_ceil(4) * 4))
}

fn message(id: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let size = 8 + 4 * args.len() as u32;
    let mut m = Vec::with_capacity(size as usize);
    for w in [id, (size << 16) | opcode as u32].iter().chain(args) {
        m.extend_from_slice(&w.to_ne_bytes());
    }
    m
}

fn frames(held: &mut Vec<u8>, mut each: impl FnMut(u32, u16, &[u8], &mut Vec<u8>) -> bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(held.len());
    let mut at = 0;
    while let (Some(id), Some(head)) = (word(held, at), word(held, at + 4)) {
        let size = (head >> 16) as usize;
        if size < 8 || !size.is_multiple_of(4) {
            return Err(format!("a malformed message of {size} bytes"));
        }
        let Some(msg) = held.get(at..at + size) else { break };
        if each(id, head as u16, msg, &mut out) {
            out.extend_from_slice(msg);
        }
        at += size;
    }
    held.drain(..at);
    Ok(out)
}

fn trace(dir: &str, id: u32, name: Option<&str>, opcode: u16, msg: &[u8]) {
    let args: Vec<String> =
        msg[8..].chunks(4).take(8).map(|w| i32::from_ne_bytes(w.try_into().unwrap()).to_string()).collect();
    logln!("wayland: {dir} {id}@{}.{opcode} [{}]", name.unwrap_or("?"), args.join(" "));
}

impl Tracker {
    fn requests(&mut self, held: &mut Vec<u8>) -> Result<Vec<u8>, String> {
        frames(held, |id, opcode, msg, out| {
            if *TRACE {
                trace("->", id, self.name(id), opcode, msg);
            }
            self.request(id, opcode, msg, out)
        })
    }

    fn events(&mut self, held: &mut Vec<u8>) -> Result<Vec<u8>, String> {
        frames(held, |id, opcode, msg, _| {
            if *TRACE {
                trace("<-", id, self.name(id), opcode, msg);
            }
            true
        })
    }

    fn name(&self, id: u32) -> Option<&str> {
        if id == DISPLAY {
            return Some("wl_display");
        }
        self.names.get(&id).map(String::as_str).or(match self.objs.get(&id)? {
            Obj::Registry => Some("wl_registry"),
            Obj::Surface => Some("wl_surface"),
            Obj::XdgSurface(_) => Some("xdg_surface"),
            Obj::Toplevel(_) => Some("xdg_toplevel"),
            Obj::Viewport(_) => Some("wp_viewport"),
            Obj::Pointer => Some("wl_pointer"),
            _ => None,
        })
    }

    #[cfg(test)]
    fn feed(&mut self, held: &mut Vec<u8>) -> Result<Vec<u8>, String> {
        self.requests(held)
    }

    fn window(&mut self, surface: u32) -> Option<&mut Window> {
        self.windows.get_mut(&surface)
    }

    fn request(&mut self, id: u32, opcode: u16, msg: &[u8], out: &mut Vec<u8>) -> bool {
        let arg = |i: usize| word(msg, 8 + 4 * i);
        let obj = if id == DISPLAY { None } else { self.objs.get(&id).copied() };
        match (id, obj, opcode) {
            (DISPLAY, _, DISPLAY_GET_REGISTRY) => {
                if let Some(new) = arg(0) {
                    self.objs.insert(new, Obj::Registry);
                }
            }
            (_, Some(Obj::Registry), REGISTRY_BIND) => {
                let Some((interface, next)) = string(msg, 12) else { return true };
                if let Some(new) = word(msg, next + 4) {
                    self.names.insert(new, interface.clone());
                }
                let kind = match interface.as_str() {
                    "wl_compositor" => Obj::Compositor,
                    "xdg_wm_base" => Obj::WmBase,
                    "wp_viewporter" => Obj::Viewporter,
                    "wl_seat" => Obj::Seat,
                    "wl_shm" => Obj::Shm,
                    _ => return true,
                };
                if let Some(new) = word(msg, next + 4) {
                    self.objs.insert(new, kind);
                }
            }
            (_, Some(Obj::Compositor), COMPOSITOR_CREATE_SURFACE) => {
                if let Some(new) = arg(0) {
                    self.objs.insert(new, Obj::Surface);
                    let trace = (!TRACED.load(Ordering::Relaxed)).then(Vec::new);
                    self.windows.insert(new, Window { trace, ..Window::default() });
                }
            }
            (_, Some(Obj::Viewporter), VIEWPORTER_GET_VIEWPORT) => {
                if let (Some(new), Some(surface)) = (arg(0), arg(1)) {
                    self.objs.insert(new, Obj::Viewport(surface));
                    if let Some(w) = self.window(surface) {
                        w.viewport = Some(new);
                        w.note(|| "get_viewport".into());
                    }
                }
            }
            (_, Some(Obj::WmBase), WM_BASE_GET_XDG_SURFACE) => {
                if let (Some(new), Some(surface)) = (arg(0), arg(1)) {
                    self.objs.insert(new, Obj::XdgSurface(surface));
                    if let Some(w) = self.window(surface) {
                        w.note(|| "get_xdg_surface".into());
                    }
                }
            }
            (_, Some(Obj::XdgSurface(surface)), XDG_SURFACE_GET_TOPLEVEL) => {
                if let Some(new) = arg(0) {
                    self.objs.insert(new, Obj::Toplevel(surface));
                    if let Some(w) = self.window(surface) {
                        w.toplevel = Some(new);
                        w.note(|| "get_toplevel".into());
                    }
                }
            }
            (_, Some(Obj::Toplevel(surface)), TOPLEVEL_SET_APP_ID) => {
                if let (Some((app_id, _)), Some(w)) = (string(msg, 8), self.windows.get_mut(&surface)) {
                    w.note(|| format!("set_app_id {app_id}"));
                    w.app_id = Some(app_id);
                }
            }
            (_, Some(Obj::Viewport(surface)), VIEWPORT_SET_DESTINATION) => {
                if let (Some(width), Some(height), Some(w)) = (arg(0), arg(1), self.windows.get_mut(&surface)) {
                    let (width, height) = (width as i32, height as i32);
                    w.destination = (width > 0 && height > 0).then_some((width, height));
                    w.note(|| format!("set_destination {width}x{height}"));
                    if w.cursor {
                        return false;
                    }
                }
            }
            (_, Some(Obj::Viewport(surface)), VIEWPORT_SET_SOURCE) => {
                if let (Some(x), Some(y), Some(width), Some(height), Some(w)) =
                    (arg(0), arg(1), arg(2), arg(3), self.windows.get_mut(&surface))
                {
                    w.source = (x != UNSET).then_some([x, y, width, height]);
                    if w.cursor {
                        return false;
                    }
                }
            }
            (_, Some(Obj::Surface), SURFACE_ATTACH) => {
                let attached = arg(0).is_some_and(|b| b != 0);
                let buffer = arg(0).and_then(|b| self.buffers.get(&b).copied());
                if let Some(w) = self.window(id) {
                    w.attached = attached;
                    w.buffer = buffer;
                    w.note(|| if attached { "attach" } else { "attach null" }.into());
                }
            }
            (_, Some(Obj::Surface), SURFACE_COMMIT) => {
                if let Some(w) = self.windows.get_mut(&id) {
                    if w.cursor {
                        cursor_commit(id, w, out, &mut self.cursor_logged);
                    }
                    commit(w, out);
                }
            }
            (_, Some(Obj::Shm), SHM_CREATE_POOL) => {
                if let Some(new) = arg(0) {
                    self.objs.insert(new, Obj::ShmPool);
                }
            }
            (_, Some(Obj::ShmPool), POOL_CREATE_BUFFER) => {
                if let (Some(new), Some(width), Some(height)) = (arg(0), arg(2), arg(3)) {
                    self.objs.insert(new, Obj::Buffer);
                    self.buffers.insert(new, (width as i32, height as i32));
                }
            }
            (_, Some(Obj::Buffer), BUFFER_DESTROY) => {
                self.objs.remove(&id);
                self.buffers.remove(&id);
            }
            (_, Some(Obj::ShmPool), POOL_DESTROY) => {
                self.objs.remove(&id);
            }
            (_, Some(Obj::Seat), SEAT_GET_POINTER) => {
                if let Some(new) = arg(0) {
                    self.objs.insert(new, Obj::Pointer);
                }
            }
            (_, Some(Obj::Pointer), POINTER_SET_CURSOR) => {
                let (Some(serial), Some(surface), Some(hx), Some(hy)) = (arg(0), arg(1), arg(2), arg(3)) else {
                    return true;
                };
                let Some(w) = self.windows.get_mut(&surface) else { return true };
                if !std::mem::replace(&mut w.cursor, true) {
                    cursor_commit(surface, w, out, &mut self.cursor_logged);
                    out.extend(message(surface, SURFACE_COMMIT, &[]));
                }
                let (hx, hy) = cursor_hotspot(w, (hx as i32, hy as i32));
                out.extend(message(id, POINTER_SET_CURSOR, &[serial, surface, hx as u32, hy as u32]));
                return false;
            }
            (_, Some(Obj::Pointer), POINTER_RELEASE) | (_, Some(Obj::Seat), SEAT_RELEASE) => {
                self.objs.remove(&id);
            }
            (_, Some(Obj::Surface), SURFACE_DESTROY) => {
                self.objs.remove(&id);
                self.windows.remove(&id);
            }
            (_, Some(Obj::Toplevel(surface)), TOPLEVEL_DESTROY) => {
                self.objs.remove(&id);
                if let Some(w) = self.window(surface) {
                    w.toplevel = None;
                    w.app_id = None;
                    w.fixed = None;
                }
            }
            (_, Some(Obj::Viewport(surface)), VIEWPORT_DESTROY) => {
                self.objs.remove(&id);
                if let Some(w) = self.window(surface) {
                    w.destination = None;
                    w.viewport = None;
                    w.source = None;
                }
            }
            (_, Some(Obj::XdgSurface(_)), XDG_SURFACE_DESTROY)
            | (_, Some(Obj::WmBase), WM_BASE_DESTROY)
            | (_, Some(Obj::Viewporter), VIEWPORTER_DESTROY) => {
                self.objs.remove(&id);
            }
            _ => {}
        }
        true
    }
}

fn fixed(v: u32) -> f64 {
    f64::from(v as i32) / 256.0
}

fn cursor_crop(w: &Window) -> Option<(f64, f64, f64, f64)> {
    let (bw, bh) = w.buffer?;
    Some(match w.source {
        Some([x, y, sw, sh]) => (fixed(x), fixed(y), fixed(sw), fixed(sh)),
        None => (0.0, 0.0, f64::from(bw), f64::from(bh)),
    })
}

fn cursor_scale(w: &Window) -> i32 {
    let (Some((bw, bh)), Some((dw, dh)), Some((_, _, sw, sh))) = (w.buffer, w.destination, cursor_crop(w)) else {
        return 1;
    };
    let (rx, ry) = (sw / f64::from(dw), sh / f64::from(dh));
    let n = ry.round() as i32;
    let whole = (rx - f64::from(n)).abs() < 0.05 && (ry - f64::from(n)).abs() < 0.05;
    if n > 1 && whole && bw % n == 0 && bh % n == 0 { n } else { 1 }
}

fn cursor_hotspot(w: &Window, (hx, hy): (i32, i32)) -> (i32, i32) {
    let n = cursor_scale(w);
    let (Some((dw, dh)), Some((x, y, sw, sh))) = (w.destination, cursor_crop(w)) else { return (hx, hy) };
    if n == 1 {
        return (hx, hy);
    }
    let at = |o: f64, h: i32, s: f64, d: i32| ((o + f64::from(h) * s / f64::from(d)) / f64::from(n)).round() as i32;
    (at(x, hx, sw, dw), at(y, hy, sh, dh))
}

fn cursor_commit(surface: u32, w: &Window, out: &mut Vec<u8>, logged: &mut bool) {
    let n = cursor_scale(w);
    if let Some(viewport) = w.viewport {
        if n > 1 {
            out.extend(message(viewport, VIEWPORT_SET_SOURCE, &[UNSET; 4]));
            out.extend(message(viewport, VIEWPORT_SET_DESTINATION, &[-1i32 as u32, -1i32 as u32]));
        } else {
            out.extend(message(viewport, VIEWPORT_SET_SOURCE, &w.source.unwrap_or([UNSET; 4])));
            let (dw, dh) = w.destination.unwrap_or((-1, -1));
            out.extend(message(viewport, VIEWPORT_SET_DESTINATION, &[dw as u32, dh as u32]));
        }
    }
    out.extend(message(surface, SURFACE_SET_BUFFER_SCALE, &[n as u32]));
    if n > 1
        && let Some((bw, bh)) = w.buffer
        && !std::mem::replace(logged, true)
    {
        logln!(
            "wayland: Android's cursor, a {bw}x{bh} bitmap, shown at buffer scale {n} instead of through its viewport"
        );
    }
}

fn commit(w: &mut Window, out: &mut Vec<u8>) {
    let want = if w.is_app() { w.destination } else { None };
    if want != w.fixed
        && let Some(toplevel) = w.toplevel
    {
        let (width, height) = want.unwrap_or((0, 0));
        out.extend(message(toplevel, TOPLEVEL_SET_MIN_SIZE, &[width as u32, height as u32]));
        out.extend(message(toplevel, TOPLEVEL_SET_MAX_SIZE, &[width as u32, height as u32]));
        let app = w.app_id.clone().unwrap_or_default();
        match want {
            Some(_) => logln!("wayland: {app} is {width}x{height}, fixed"),
            None => logln!("wayland: {app} released"),
        }
        w.note(|| format!("(relay: min and max {width}x{height})"));
        w.fixed = want;
    }
    w.note(|| "commit".into());
    if w.attached {
        if let Some(trace) = w.trace.take()
            && w.is_app()
            && !TRACED.swap(true, Ordering::Relaxed)
        {
            logln!("wayland: the first app window's requests, in order: {}", trace.join(", "));
        }
        w.attached = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_arg(s: &str) -> Vec<u32> {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        let len = bytes.len() as u32;
        bytes.resize(bytes.len().div_ceil(4) * 4, 0);
        std::iter::once(len).chain(bytes.chunks(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap()))).collect()
    }

    fn bind(name: u32, interface: &str, version: u32, new: u32) -> Vec<u8> {
        let mut args = vec![name];
        args.extend(string_arg(interface));
        args.extend([version, new]);
        message(2, REGISTRY_BIND, &args)
    }

    fn setup(app_id: &str) -> Vec<u8> {
        [
            message(DISPLAY, DISPLAY_GET_REGISTRY, &[2]),
            bind(1, "wl_compositor", 4, 3),
            bind(2, "xdg_wm_base", 2, 4),
            bind(3, "wp_viewporter", 1, 5),
            bind(4, "wl_seat", 5, 6),
            message(3, COMPOSITOR_CREATE_SURFACE, &[10]),
            message(5, VIEWPORTER_GET_VIEWPORT, &[11, 10]),
            message(4, WM_BASE_GET_XDG_SURFACE, &[12, 10]),
            message(12, XDG_SURFACE_GET_TOPLEVEL, &[13]),
            message(13, TOPLEVEL_SET_APP_ID, &string_arg(app_id)),
        ]
        .concat()
    }

    fn fixed(w: i32, h: i32) -> Vec<u8> {
        [
            message(13, TOPLEVEL_SET_MIN_SIZE, &[w as u32, h as u32]),
            message(13, TOPLEVEL_SET_MAX_SIZE, &[w as u32, h as u32]),
        ]
        .concat()
    }

    fn run(t: &mut Tracker, input: &[u8]) -> Vec<u8> {
        let mut held = input.to_vec();
        let out = t.feed(&mut held).unwrap();
        assert!(held.is_empty());
        out
    }

    fn commit_msg() -> Vec<u8> {
        message(10, SURFACE_COMMIT, &[])
    }

    fn destination(w: i32, h: i32) -> Vec<u8> {
        message(11, VIEWPORT_SET_DESTINATION, &[w as u32, h as u32])
    }

    #[test]
    fn an_app_window_is_fixed_at_the_destination_before_the_commit_that_carries_it() {
        let mut t = Tracker::default();
        let s = setup("waydroid.com.example.app");
        assert_eq!(run(&mut t, &s), s, "requests pass through unchanged");
        assert_eq!(run(&mut t, &commit_msg()), commit_msg(), "no size is known yet");
        let d = [destination(480, 1000), commit_msg()].concat();
        assert_eq!(run(&mut t, &d), [destination(480, 1000), fixed(480, 1000), commit_msg()].concat());
        assert_eq!(run(&mut t, &commit_msg()), commit_msg(), "the same size is not sent twice");
        let d = [destination(960, 2000), commit_msg()].concat();
        assert_eq!(run(&mut t, &d), [destination(960, 2000), fixed(960, 2000), commit_msg()].concat());
        let d = [destination(-1, -1), commit_msg()].concat();
        assert_eq!(run(&mut t, &d), [destination(-1, -1), fixed(0, 0), commit_msg()].concat(), "unset releases it");
    }

    #[test]
    fn other_toplevels_are_left_alone() {
        for app_id in ["Waydroid", "org.gnome.Nautilus", "waydroid"] {
            let mut t = Tracker::default();
            let s = [setup(app_id), destination(480, 1000), commit_msg()].concat();
            assert_eq!(run(&mut t, &s), s, "{app_id}");
        }
    }

    #[test]
    fn a_message_split_across_reads_is_held_until_it_is_whole() {
        let mut t = Tracker::default();
        let all = [setup("waydroid.x"), destination(480, 1000), commit_msg()].concat();
        let mut held = Vec::new();
        let mut out = Vec::new();
        for chunk in all.chunks(5) {
            held.extend_from_slice(chunk);
            out.extend(t.feed(&mut held).unwrap());
        }
        assert!(held.is_empty());
        assert_eq!(out, [setup("waydroid.x"), destination(480, 1000), fixed(480, 1000), commit_msg()].concat());
    }

    #[test]
    fn a_destroyed_toplevel_or_viewport_is_forgotten() {
        let mut t = Tracker::default();
        run(&mut t, &[setup("waydroid.x"), destination(480, 1000), commit_msg()].concat());
        let d = [message(11, VIEWPORT_DESTROY, &[]), commit_msg()].concat();
        assert_eq!(run(&mut t, &d), [message(11, VIEWPORT_DESTROY, &[]), fixed(0, 0), commit_msg()].concat());
        let reuse = [
            message(13, TOPLEVEL_DESTROY, &[]),
            message(12, XDG_SURFACE_DESTROY, &[]),
            message(10, SURFACE_DESTROY, &[]),
            message(13, TOPLEVEL_SET_APP_ID, &[7, 8]),
            message(11, VIEWPORT_SET_DESTINATION, &[1, 1]),
            message(10, SURFACE_COMMIT, &[]),
        ]
        .concat();
        assert_eq!(run(&mut t, &reuse), reuse, "ids reused by untracked objects are only copied");
        assert!(t.objs.values().all(|o| matches!(
            o,
            Obj::Registry | Obj::Compositor | Obj::WmBase | Obj::Viewporter | Obj::Seat | Obj::Shm
        )));
    }

    fn fx(v: f64) -> u32 {
        ((v * 256.0) as i32) as u32
    }

    #[test]
    fn androids_cursor_is_scaled_by_buffer_scale_not_its_viewport() {
        let mut t = Tracker::default();
        run(&mut t, &[setup("waydroid.x"), bind(5, "wl_shm", 1, 30)].concat());
        let make = [
            message(3, COMPOSITOR_CREATE_SURFACE, &[21]),
            message(5, VIEWPORTER_GET_VIEWPORT, &[22, 21]),
            message(6, SEAT_GET_POINTER, &[20]),
            message(30, SHM_CREATE_POOL, &[31, 4096]),
            message(31, POOL_CREATE_BUFFER, &[32, 0, 64, 64, 256, 0]),
        ]
        .concat();
        assert_eq!(run(&mut t, &make), make);
        let source = message(22, VIEWPORT_SET_SOURCE, &[fx(12.0), 0, fx(38.0), fx(50.0)]);
        let dest = message(22, VIEWPORT_SET_DESTINATION, &[19, 25]);
        let unset = [
            message(22, VIEWPORT_SET_SOURCE, &[UNSET; 4]),
            message(22, VIEWPORT_SET_DESTINATION, &[-1i32 as u32, -1i32 as u32]),
            message(21, SURFACE_SET_BUFFER_SCALE, &[2]),
        ]
        .concat();
        let commit = || message(21, SURFACE_COMMIT, &[]);
        let frame = [message(21, SURFACE_ATTACH, &[32, 0, 0]), source.clone(), dest.clone(), commit()].concat();
        assert_eq!(run(&mut t, &frame), frame, "not a cursor until set_cursor names it");
        let set = message(20, POINTER_SET_CURSOR, &[5, 21, 9, 4]);
        let tip = message(20, POINTER_SET_CURSOR, &[5, 21, 15, 4]);
        assert_eq!(run(&mut t, &set), [unset.clone(), commit(), tip.clone()].concat(), "the tip moves into the bitmap");
        assert_eq!(
            run(&mut t, &[frame.clone(), set.clone()].concat()),
            [message(21, SURFACE_ATTACH, &[32, 0, 0]), unset, commit(), tip].concat(),
            "later frames keep their viewport to themselves"
        );
        let hide = message(20, POINTER_SET_CURSOR, &[6, 0, 0, 0]);
        assert_eq!(run(&mut t, &hide), hide);
    }

    #[test]
    fn a_cursor_bitmap_the_scale_does_not_divide_keeps_its_viewport() {
        let mut t = Tracker::default();
        run(&mut t, &[setup("waydroid.x"), bind(5, "wl_shm", 1, 30)].concat());
        run(
            &mut t,
            &[
                message(3, COMPOSITOR_CREATE_SURFACE, &[21]),
                message(5, VIEWPORTER_GET_VIEWPORT, &[22, 21]),
                message(6, SEAT_GET_POINTER, &[20]),
                message(30, SHM_CREATE_POOL, &[31, 4096]),
                message(31, POOL_CREATE_BUFFER, &[32, 0, 50, 63, 200, 0]),
                message(21, SURFACE_ATTACH, &[32, 0, 0]),
                message(22, VIEWPORT_SET_SOURCE, &[0, 0, fx(50.0), fx(63.0)]),
                message(22, VIEWPORT_SET_DESTINATION, &[25, 32]),
                message(21, SURFACE_COMMIT, &[]),
            ]
            .concat(),
        );
        let out = run(&mut t, &message(20, POINTER_SET_CURSOR, &[5, 21, 6, 6]));
        assert_eq!(
            out,
            [
                message(22, VIEWPORT_SET_SOURCE, &[0, 0, fx(50.0), fx(63.0)]),
                message(22, VIEWPORT_SET_DESTINATION, &[25, 32]),
                message(21, SURFACE_SET_BUFFER_SCALE, &[1]),
                message(21, SURFACE_COMMIT, &[]),
                message(20, POINTER_SET_CURSOR, &[5, 21, 6, 6]),
            ]
            .concat(),
            "63 is odd: scale 2 would be a protocol error"
        );
    }

    #[test]
    fn a_malformed_request_ends_the_connection() {
        let mut t = Tracker::default();
        let mut held = [1u32.to_ne_bytes(), (4u32 << 16).to_ne_bytes()].concat();
        assert!(t.feed(&mut held).is_err());
    }

    #[test]
    fn file_descriptors_cross_with_the_bytes() {
        let (client, down) = UnixStream::pair().unwrap();
        let (up, server) = UnixStream::pair().unwrap();
        relay(down, up).unwrap();
        let files: Vec<OwnedFd> = (0..MAX_FDS + 5).map(|_| std::fs::File::open("/dev/null").unwrap().into()).collect();
        let mut fds = files;
        let request = [setup("waydroid.x"), destination(480, 1000), commit_msg()].concat();
        send(client.as_raw_fd(), &request, &mut fds).unwrap();
        assert!(fds.is_empty());
        let mut got = Vec::new();
        let mut received = Vec::new();
        let want = request.len() + fixed(0, 0).len();
        let mut buf = [0u8; 4096];
        while got.len() < want {
            let n = recv(server.as_raw_fd(), &mut buf, &mut received).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, [setup("waydroid.x"), destination(480, 1000), fixed(480, 1000), commit_msg()].concat());
        assert_eq!(received.len(), MAX_FDS + 5);
        let mut back = vec![std::fs::File::open("/dev/null").unwrap().into()];
        let event = message(20, 1, &[7]);
        send(server.as_raw_fd(), &event, &mut back).unwrap();
        let mut fds = Vec::new();
        let n = recv(client.as_raw_fd(), &mut buf, &mut fds).unwrap();
        assert_eq!((&buf[..n], fds.len()), (&event[..], 1));
        drop(server);
        assert_eq!(recv(client.as_raw_fd(), &mut buf, &mut fds).unwrap(), 0, "the compositor's close reaches Android");
    }
}
