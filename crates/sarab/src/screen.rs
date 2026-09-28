//! How large Android's display is: phone-shaped, and short enough to fit the
//! screen it opens on.
//!
//! The composer takes the size from `persist.waydroid.width` and `height` in
//! the prop file, in logical pixels, and multiplies by the output's scale for
//! the buffer; sarab-hostd's relay then fixes each app window at exactly that
//! size, and the compositor floats it. A fixed number cannot fit every screen:
//! 480x1000 ran past the top and bottom of a 2880x1920 laptop panel at scale
//! 2, which is 960 logical pixels tall. So `sarab start` asks the compositor,
//! once, before Android starts, and `display_for` makes the display `FILL` of the
//! shortest output's logical height (a window may open on any of them), never
//! taller than `MAX_HEIGHT`, with the width in the `ASPECT` of the 480x1000 it
//! replaces, both even. `FILL` is 92%: a client cannot see how much of the
//! screen a bar reserves, and 8% leaves room for a usual one (Hyprland centres
//! a floating window below its bar; on the panel above, a 26-pixel bar leaves
//! about 25 pixels over and under the window), where 85% looked lost. Changing screens takes a restart of Android, since the
//! composer reads the size once.
//!
//! One dp is one logical pixel of that screen: `ro.sf.lcd_density` is
//! `DP_PER_SCALE` (Android's own baseline, 160) times its scale, so Android's
//! text and controls are the size of the desktop's. Left to itself the composer
//! sets 180 times the scale, 12% larger, which on a scale-2 panel made the
//! display 346 dp wide and everything in it looked zoomed; `ro.` properties are
//! set once and the prop file loads before the composer starts, so ours wins.
//! `display_for` bundles the size and the density, and `from_props` reads
//! them back from a prop file, which is how session.rs knows what the running
//! Android was started with.
//!
//! `screens` connects to the compositor like any client, binds every
//! `wl_output`, and reads its logical size from `zxdg_output_v1` where the
//! compositor has it, which is exact under fractional scaling; otherwise the
//! current mode's height divided by the integer `wl_output.scale`; the scale
//! is the mode's height over the logical one (`Output::screen`, which swaps
//! the mode for a rotated output). Nothing here
//! is fatal: with no answer `display_for` falls back to `FALLBACK_HEIGHT`, which
//! fits a 1080p panel at 125%, and leaves the density to the composer.

use std::collections::HashMap;
use std::path::Path;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

const FILL: (u32, u32) = (92, 100);
const MAX_HEIGHT: u32 = 1000;
const ASPECT: (u32, u32) = (12, 25);
const FALLBACK_HEIGHT: u32 = 800;
const DP_PER_SCALE: f64 = 160.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, PartialEq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub density: Option<u32>,
}

pub fn display_for(screens: &[Screen]) -> Display {
    let shortest = screens.iter().filter(|s| s.height > 0).min_by_key(|s| s.height);
    let height = match shortest {
        Some(s) => (s.height * FILL.0 / FILL.1).min(MAX_HEIGHT),
        None => FALLBACK_HEIGHT,
    } & !1;
    let density = shortest.filter(|s| s.scale > 0.0).map(|s| (DP_PER_SCALE * s.scale).round() as u32);
    Display { width: (height * ASPECT.0 / ASPECT.1) & !1, height, density }
}

pub fn from_props(text: &str) -> Option<Display> {
    let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')?.trim().parse::<u32>().ok());
    Some(Display {
        width: get("persist.waydroid.width")?,
        height: get("persist.waydroid.height")?,
        density: get("ro.sf.lcd_density"),
    })
}

#[derive(Default)]
struct Output {
    mode_height: Option<i32>,
    mode_width: Option<i32>,
    scale: i32,
    logical: Option<(i32, i32)>,
}

#[derive(Default)]
struct Outputs(HashMap<usize, Output>);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Outputs {
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

impl Dispatch<wl_output::WlOutput, usize> for Outputs {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = state.0.entry(*i).or_insert(Output { scale: 1, ..Output::default() });
        match event {
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. }
                if f.contains(wl_output::Mode::Current) =>
            {
                o.mode_width = Some(width);
                o.mode_height = Some(height);
            }
            wl_output::Event::Scale { factor } => o.scale = factor.max(1),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for Outputs {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, height } = event {
            state.0.entry(*i).or_insert(Output { scale: 1, ..Output::default() }).logical = Some((width, height));
        }
    }
}

delegate_noop!(Outputs: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

pub fn screens(socket: &Path) -> anyhow::Result<Vec<Screen>> {
    let conn = Connection::from_socket(std::os::unix::net::UnixStream::connect(socket)?)?;
    let (globals, mut queue) = registry_queue_init::<Outputs>(&conn)?;
    let qh = queue.handle();
    let manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> = globals.bind(&qh, 2..=3, ()).ok();
    let outputs: Vec<wl_output::WlOutput> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == "wl_output")
        .enumerate()
        .map(|(i, g)| globals.registry().bind(g.name, g.version.min(4), &qh, i))
        .collect();
    let mut state = Outputs::default();
    for (i, o) in outputs.iter().enumerate() {
        state.0.insert(i, Output { scale: 1, ..Output::default() });
        if let Some(m) = &manager {
            m.get_xdg_output(o, &qh, i);
        }
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    Ok(state.0.values().filter_map(Output::screen).collect())
}

impl Output {
    fn screen(&self) -> Option<Screen> {
        let (mw, mh) = (self.mode_width?, self.mode_height?);
        let (height, scale) = match self.logical {
            Some((lw, lh)) if lw > 0 && lh > 0 => {
                let rotated = (lw > lh) != (mw > mh);
                (lh, f64::from(if rotated { mw } else { mh }) / f64::from(lh))
            }
            _ => (mh / self.scale, f64::from(self.scale)),
        };
        Some(Screen { height: u32::try_from(height).ok()?, scale })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(height: u32, scale: f64) -> Screen {
        Screen { height, scale }
    }

    #[test]
    fn the_display_fits_the_shortest_screen_and_keeps_its_shape() {
        let d = display_for(&[at(960, 2.0)]);
        assert_eq!((d.width, d.height), (422, 882), "a 2880x1920 panel at scale 2");
        let d = display_for(&[at(1440, 1.0), at(960, 2.0)]);
        assert_eq!((d.width, d.height), (422, 882), "the shortest screen decides");
        let d = display_for(&[at(864, 1.25)]);
        assert_eq!((d.width, d.height), (380, 794), "1080p at 125%");
        let d = display_for(&[at(2160, 1.0)]);
        assert_eq!((d.width, d.height), (480, 1000), "a tall screen gets the old size, no more");
        assert_eq!(display_for(&[]), Display { width: 384, height: 800, density: None });
        assert_eq!(display_for(&[at(0, 2.0)]), display_for(&[]));
        for h in [600, 768, 900, 1080, 1200] {
            let d = display_for(&[at(h, 1.0)]);
            assert!(d.width.is_multiple_of(2) && d.height.is_multiple_of(2) && d.height < h, "{h}: {d:?}");
        }
    }

    #[test]
    fn one_dp_is_one_logical_pixel_of_the_screen_it_is_sized_for() {
        assert_eq!(display_for(&[at(960, 2.0)]).density, Some(320));
        assert_eq!(display_for(&[at(864, 1.25)]).density, Some(200));
        assert_eq!(display_for(&[at(1440, 1.0), at(960, 1.5)]).density, Some(240), "the shortest screen's scale");
    }

    #[test]
    fn the_prop_file_says_what_android_was_started_with() {
        let p = "# a comment\npersist.waydroid.width=422\npersist.waydroid.height=882\nro.sf.lcd_density=320\n";
        assert_eq!(from_props(p), Some(Display { width: 422, height: 882, density: Some(320) }));
        let old = "persist.waydroid.width=480\npersist.waydroid.height=1000\n";
        assert_eq!(from_props(old), Some(Display { width: 480, height: 1000, density: None }));
        assert_eq!(from_props("persist.waydroid.width=480\n"), None);
        assert_eq!(from_props("persist.waydroid.widthx=1\npersist.waydroid.height=2\n"), None);
    }

    #[test]
    fn a_screen_is_read_from_its_logical_size_or_its_integer_scale() {
        let o = |mode: (i32, i32), scale, logical| Output {
            mode_width: Some(mode.0),
            mode_height: Some(mode.1),
            scale,
            logical,
        };
        assert_eq!(o((2880, 1920), 2, Some((1440, 960))).screen(), Some(at(960, 2.0)));
        assert_eq!(o((1920, 1080), 2, Some((1536, 864))).screen(), Some(at(864, 1.25)), "fractional");
        assert_eq!(o((1920, 1080), 1, Some((1080, 1920))).screen(), Some(at(1920, 1.0)), "rotated");
        assert_eq!(o((2880, 1920), 2, None).screen(), Some(at(960, 2.0)), "no xdg_output");
        assert_eq!(Output::default().screen(), None);
    }
}
