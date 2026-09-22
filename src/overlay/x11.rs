//! X11 backend for the overlay.
//!
//! The target deployment is a bare `startx` with no window manager at all, running
//! Attract-Mode and MAME/SDL2 fullscreen. There is nothing there to honour EWMH hints, so
//! the overlay does not ask a window manager for anything; it takes what it needs directly
//! from the server:
//!
//! * **override-redirect** — the window is never managed, never reparented, and cannot be
//!   pushed around by a window manager if one does happen to be running. When one is, the
//!   usual EWMH hints (`_NET_WM_WINDOW_TYPE_DOCK`, `_NET_WM_STATE_ABOVE`, `WM_CLASS`,
//!   `WM_HINTS` with `input = False`) are set as well, so a compositing desktop sees
//!   something sane in its window list even though it is not managing us.
//! * **SHAPE bounding mask** — set to exactly the rectangles the border paints, so outside
//!   the border the window does not exist: no pixels, no occlusion, nothing for a
//!   compositor to blend. During calibration the mask is reset to the whole window because
//!   the UI needs its black backdrop.
//! * **SHAPE input region set to empty** — every pointer and keyboard event falls straight
//!   through to whatever is underneath, everywhere, including on top of the border itself.
//!   We select no input events, never call `SetInputFocus`, and never grab.
//! * **re-raise on restack** — `SubstructureNotify` on the root tells us whenever anything
//!   else maps, is created, is reparented or is restacked; each one triggers a
//!   `ConfigureWindow` with `stack_mode = Above`, rate-limited to one per 50 ms, plus an
//!   unconditional raise every 2 s as a backstop for restacks we cannot observe.
//!
//! Together that is a window that only exists where the border is, takes no input, and
//! climbs back to the top whenever anything else appears. Honest caveat: the claim that
//! this wins against an SDL2 fullscreen window on a WM-less server comes from reading SDL's
//! X11 backend (with no WM it sets override-redirect on its own window and raises it at map
//! time and on focus, but does not re-raise continuously), not from a measurement. It still
//! needs a check on the real cabinet.

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape;
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gcontext, ImageFormat, PropMode,
    Rectangle, StackMode, VisualClass, Window, WindowClass,
};
use x11rb::protocol::{randr, Event as XEvent};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{Event, Rect};

/// How often at most a burst of restack notifications may cost us a raise.
const RAISE_MIN_GAP: Duration = Duration::from_millis(50);
/// How long we let pass without a raise before doing one anyway.
const RAISE_MAX_GAP: Duration = Duration::from_secs(2);

/// `poll(2)` on the X connection, so an idle pump costs nothing and still wakes instantly.
mod wait {
    #![allow(unsafe_code)]

    use std::io;
    use std::os::fd::RawFd;
    use std::time::Duration;

    /// Block until `fd` is readable or `timeout` elapses; `true` if it became readable.
    pub fn readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Round up so a sub-millisecond remainder still waits rather than spinning.
        let ms = timeout.as_micros().div_ceil(1000);
        let ms = i32::try_from(ms).unwrap_or(i32::MAX);
        // SAFETY: one correctly initialised pollfd is passed with a count of one, and the
        // borrow lasts only for the call.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            // A signal is not a failure; the caller's deadline loop handles the short wait.
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        Ok(r > 0)
    }
}

fn clamp_i16(v: i32) -> i16 {
    i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX })
}

fn clamp_u16(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

fn to_rectangle(r: &Rect) -> Rectangle {
    Rectangle {
        x: clamp_i16(r.x),
        y: clamp_i16(r.y),
        width: clamp_u16(r.w),
        height: clamp_u16(r.h),
    }
}

/// Where the overlay window goes: the whole screen, or the single enabled CRTC if RandR
/// reports exactly one.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    x: i16,
    y: i16,
    w: u16,
    h: u16,
}

fn single_crtc(conn: &RustConnection, root: Window) -> Result<Option<Geometry>> {
    if conn
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    randr::query_version(conn, 1, 2)?.reply()?;
    let res = randr::get_screen_resources_current(conn, root)?.reply()?;
    let mut found = None;
    for &crtc in &res.crtcs {
        let info = randr::get_crtc_info(conn, crtc, res.config_timestamp)?.reply()?;
        // mode 0 means the CRTC is disabled; a zero-sized one is no use either.
        if info.mode == 0 || info.width == 0 || info.height == 0 {
            continue;
        }
        if found.is_some() {
            return Ok(None); // More than one head: stick to the root geometry.
        }
        found = Some(Geometry {
            x: info.x,
            y: info.y,
            w: info.width,
            h: info.height,
        });
    }
    Ok(found)
}

fn atom(conn: &RustConnection, name: &str) -> Result<xproto::Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("interning atom {name}"))?
        .atom)
}

/// Is a window manager running? EWMH says a compliant one owns `_NET_SUPPORTING_WM_CHECK`
/// on the root; without one we are on a bare server and skip the hints entirely.
fn wm_present(conn: &RustConnection, root: Window) -> bool {
    let Ok(prop) = atom(conn, "_NET_SUPPORTING_WM_CHECK") else {
        return false;
    };
    conn.get_property(false, root, prop, AtomEnum::WINDOW, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|mut v| v.next().is_some()))
        .unwrap_or(false)
}

pub fn open(title: &str) -> Result<Box<dyn super::Backend>> {
    let (conn, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
    // Ask for BIG-REQUESTS now so `maximum_request_bytes` below is the real limit.
    conn.prefetch_maximum_request_bytes();

    let setup = conn.setup();
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or_else(|| anyhow!("X server reported no screen {screen_num}"))?;
    let root = screen.root;
    let depth = screen.root_depth;
    if depth != 24 && depth != 32 {
        bail!("root depth {depth} is not 24 or 32; the overlay needs a 32-bit-per-pixel visual");
    }
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == depth)
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == screen.root_visual)
        .ok_or_else(|| anyhow!("the root visual is not listed at the root depth"))?;
    if visual.class != VisualClass::TRUE_COLOR
        || (visual.red_mask, visual.green_mask, visual.blue_mask) != (0x00ff_0000, 0xff00, 0xff)
    {
        bail!(
            "the root visual is not TrueColor 0xRRGGBB (class {:?}, masks {:#x}/{:#x}/{:#x})",
            visual.class,
            visual.red_mask,
            visual.green_mask,
            visual.blue_mask
        );
    }
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .map(|f| f.bits_per_pixel)
        .ok_or_else(|| anyhow!("no pixmap format for depth {depth}"))?;
    if bpp != 32 {
        bail!("depth {depth} is {bpp} bits per pixel on this server; the overlay needs 32");
    }
    // ZPixmap pixels go out in the server's byte order, so a big-endian server gets the
    // same u32 written the other way round.
    let msb_first = setup.image_byte_order == xproto::ImageOrder::MSB_FIRST;
    // At depth 32 the top byte is alpha, and a fully transparent overlay would be useless.
    let alpha = if depth == 32 { 0xff00_0000 } else { 0 };

    if conn
        .extension_information(shape::X11_EXTENSION_NAME)?
        .is_none()
    {
        bail!("the X server has no SHAPE extension; the overlay cannot be made click-through");
    }
    let shape_version = shape::query_version(&conn)?
        .reply()
        .context("SHAPE version")?;
    if (shape_version.major_version, shape_version.minor_version) < (1, 1) {
        bail!(
            "SHAPE {}.{} is too old for input shapes (need 1.1)",
            shape_version.major_version,
            shape_version.minor_version
        );
    }

    let geom = single_crtc(&conn, root).unwrap_or_else(|e| {
        tracing::debug!("RandR geometry unavailable ({e:#}); using the root window");
        None
    });
    let geom = geom.unwrap_or(Geometry {
        x: 0,
        y: 0,
        w: screen.width_in_pixels,
        h: screen.height_in_pixels,
    });
    if geom.w == 0 || geom.h == 0 {
        bail!("the screen reports a zero size");
    }

    let win = conn.generate_id().context("allocating a window id")?;
    conn.create_window(
        depth,
        win,
        root,
        geom.x,
        geom.y,
        geom.w,
        geom.h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(0)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
    )
    .context("creating the overlay window")?;

    if wm_present(&conn, root) {
        tracing::debug!("a window manager is running; setting EWMH hints as well");
        set_wm_hints(&conn, win, title).context("setting window manager hints")?;
    }

    // Click-through: an empty input region means no pointer or keyboard event ever lands on
    // this window, wherever it is drawn.
    shape::rectangles(
        &conn,
        shape::SO::SET,
        shape::SK::INPUT,
        ClipOrdering::UNSORTED,
        win,
        0,
        0,
        &[],
    )
    .context("clearing the input shape")?;

    let gc = conn
        .generate_id()
        .context("allocating a graphics context")?;
    conn.create_gc(gc, win, &CreateGCAux::new())
        .context("creating the graphics context")?;

    // Every map, create, reparent and restack of another client's window arrives here, and
    // each one is a reason to check we are still on top.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )
    .context("selecting SubstructureNotify on the root")?;

    conn.map_window(win).context("mapping the overlay")?;
    conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
        .context("raising the overlay")?;
    conn.flush().context("flushing the X connection")?;

    Ok(Box::new(X11Overlay {
        fd: conn.stream().as_raw_fd(),
        conn,
        win,
        gc,
        depth,
        alpha,
        msb_first,
        w: geom.w,
        h: geom.h,
        buf: Vec::new(),
        last_raise: Instant::now(),
    }))
}

/// The EWMH / ICCCM hints that matter when a window manager is running. We stay
/// override-redirect regardless, so these only tell a desktop what we are.
fn set_wm_hints(conn: &RustConnection, win: Window, title: &str) -> Result<()> {
    let utf8 = atom(conn, "UTF8_STRING")?;
    let net_name = atom(conn, "_NET_WM_NAME")?;
    let net_type = atom(conn, "_NET_WM_WINDOW_TYPE")?;
    let net_type_dock = atom(conn, "_NET_WM_WINDOW_TYPE_DOCK")?;
    let net_state = atom(conn, "_NET_WM_STATE")?;
    let net_state_above = atom(conn, "_NET_WM_STATE_ABOVE")?;

    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    conn.change_property8(PropMode::REPLACE, win, net_name, utf8, title.as_bytes())?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"sindenrs\0sindenrs\0",
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        net_type,
        AtomEnum::ATOM,
        &[net_type_dock],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        net_state,
        AtomEnum::ATOM,
        &[net_state_above],
    )?;
    // WM_HINTS: flags = InputHint only, input = False. We never want the focus.
    conn.change_property32(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_HINTS,
        AtomEnum::WM_HINTS,
        &[1, 0, 0, 0, 0, 0, 0, 0, 0],
    )?;
    Ok(())
}

struct X11Overlay {
    conn: RustConnection,
    fd: RawFd,
    win: Window,
    gc: Gcontext,
    depth: u8,
    /// ORed into every pixel: the opaque alpha byte on a depth-32 visual, else zero.
    alpha: u32,
    msb_first: bool,
    w: u16,
    h: u16,
    /// Scratch for the ZPixmap upload, kept across frames.
    buf: Vec<u8>,
    last_raise: Instant,
}

impl X11Overlay {
    fn raise(&mut self) {
        self.last_raise = Instant::now();
        if let Err(e) = self.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        ) {
            tracing::debug!("raise failed: {e}");
        }
    }

    /// A raise triggered by someone else's window, rate-limited so a burst of maps (a game
    /// starting up) costs one request, not fifty.
    fn raise_rate_limited(&mut self) {
        if self.last_raise.elapsed() >= RAISE_MIN_GAP {
            self.raise();
        }
    }

    /// Fill `buf` with the frame in the server's byte order.
    fn encode(&mut self, px: &[u32]) {
        let (alpha, msb) = (self.alpha, self.msb_first);
        self.buf.resize(px.len() * 4, 0);
        for (dst, &p) in self.buf.chunks_exact_mut(4).zip(px) {
            let v = p | alpha;
            dst.copy_from_slice(&if msb {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            });
        }
    }

    /// Upload the encoded frame, split so every PutImage fits the server's request limit.
    fn upload(&self) -> Result<()> {
        let stride = usize::from(self.w) * 4;
        // The PutImage header is 24 bytes; leave generous room for it and any padding.
        let budget = self.conn.maximum_request_bytes().saturating_sub(64);
        let rows_per_band = u16::try_from((budget / stride.max(1)).max(1)).unwrap_or(u16::MAX);
        let mut y = 0u16;
        while y < self.h {
            let rows = rows_per_band.min(self.h - y);
            let start = usize::from(y) * stride;
            let end = start + usize::from(rows) * stride;
            let data = self
                .buf
                .get(start..end)
                .ok_or_else(|| anyhow!("short pixel buffer"))?;
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.win,
                    self.gc,
                    self.w,
                    rows,
                    0,
                    i16::try_from(y).context("window too tall for a PutImage offset")?,
                    0,
                    self.depth,
                    data,
                )
                .context("uploading pixels")?;
            y += rows;
        }
        Ok(())
    }

    /// One event; `None` means it says nothing about what the caller should do.
    fn handle(&mut self, ev: &XEvent) -> Option<Event> {
        match ev {
            XEvent::Expose(_) => Some(Event::Redraw),
            XEvent::DestroyNotify(e) if e.window == self.win => Some(Event::Closed),
            XEvent::ConfigureNotify(e) if e.window == self.win => {
                if (e.width, e.height) != (self.w, self.h) && e.width > 0 && e.height > 0 {
                    self.w = e.width;
                    self.h = e.height;
                    return Some(Event::Redraw);
                }
                None
            }
            XEvent::MapNotify(_)
            | XEvent::ConfigureNotify(_)
            | XEvent::CreateNotify(_)
            | XEvent::ReparentNotify(_)
            | XEvent::CirculateNotify(_) => {
                // Someone else appeared or moved in the stack; get back on top.
                self.raise_rate_limited();
                None
            }
            XEvent::Error(e) => {
                tracing::debug!("X error: {e:?}");
                None
            }
            _ => None,
        }
    }
}

impl super::Backend for X11Overlay {
    fn size(&self) -> (u32, u32) {
        (u32::from(self.w), u32::from(self.h))
    }

    fn present(&mut self, px: &[u32], opaque: Option<&[Rect]>) -> Result<()> {
        let want = usize::from(self.w) * usize::from(self.h);
        if px.len() != want {
            bail!("present got {} pixels, window holds {want}", px.len());
        }
        self.encode(px);
        self.upload()?;

        // Outside the bounding shape the window does not exist at all, so the game shows
        // through the middle of the border without any compositing.
        let rects: Vec<Rectangle> = match opaque {
            Some(rs) => rs.iter().map(to_rectangle).collect(),
            None => vec![Rectangle {
                x: 0,
                y: 0,
                width: self.w,
                height: self.h,
            }],
        };
        shape::rectangles(
            &self.conn,
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::UNSORTED,
            self.win,
            0,
            0,
            &rects,
        )
        .context("setting the bounding shape")?;

        self.raise();
        self.conn.flush().context("flushing the X connection")?;
        Ok(())
    }

    fn pump(&mut self, timeout: Duration) -> Result<Event> {
        // Anything queued from the last present has to reach the server before we sleep.
        if self.conn.flush().is_err() {
            return Ok(Event::Closed);
        }
        let deadline = Instant::now() + timeout;
        let mut out = Event::Idle;
        loop {
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(ev)) => match self.handle(&ev) {
                        Some(Event::Closed) => return Ok(Event::Closed),
                        Some(e) => out = e,
                        None => {}
                    },
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("X connection lost: {e}");
                        return Ok(Event::Closed);
                    }
                }
            }
            if out != Event::Idle {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if !wait::readable(self.fd, deadline - now).context("waiting on the X connection")? {
                break;
            }
        }
        // A backstop for restacks we never hear about (an override-redirect client raising
        // itself sends no SubstructureNotify we can rely on seeing in time).
        if self.last_raise.elapsed() >= RAISE_MAX_GAP {
            self.raise();
            let _ = self.conn.flush();
        }
        Ok(out)
    }
}

impl Drop for X11Overlay {
    fn drop(&mut self) {
        let _ = self.conn.free_gc(self.gc);
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}
