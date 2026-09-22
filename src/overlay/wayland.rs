//! Wayland overlay, on the wlr layer shell.
//!
//! A Wayland client cannot ask to be "always on top": the compositor decides stacking, and an
//! ordinary `xdg_toplevel` is always below a fullscreen window. The one portable escape is
//! `zwlr_layer_shell_v1`, which puts a surface on a fixed layer — and the `Overlay` layer sits
//! above fullscreen surfaces. So the overlay is a layer surface anchored to all four edges with
//! an exclusive zone of -1, which means "ignore panels, give me the physical screen edge": the
//! gun tracks the border against the real edge of the display, so a border inset by a taskbar
//! would be a lie.
//!
//! There is no shape extension here, and none is needed. The surface is ARGB8888, and every
//! pixel the scene leaves black is written as a fully transparent zero: the game shows through
//! the middle of the border with no mask at all. Input passthrough is separate from that — a
//! transparent pixel still takes clicks — so the surface gets an empty input region, and the
//! opaque region is set to just the border rectangles so the compositor can skip blending
//! under them.
//!
//! GNOME (Mutter) does not implement wlr-layer-shell and has said it will not, so there is no
//! way to draw over a fullscreen game in a GNOME Wayland session; [`open`] fails with that
//! explanation rather than falling back to a toplevel that the game would cover.

use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, QueueHandle};

use super::{Event, Rect};

/// The namespace the compositor sees; some show it in debug output and rules.
const NAMESPACE: &str = "sindenrs";
/// How many shm buffers to cycle through. Two is enough to draw the next frame while the
/// compositor still holds the last one.
const MAX_BUFFERS: usize = 2;

/// Open the overlay on the Wayland display named by the environment.
pub fn open(_title: &str) -> Result<Box<dyn super::Backend>> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).context("listing Wayland globals")?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow!("{e}"))
        .context("binding wl_compositor")?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| anyhow!("{e}"))
        .context("binding wl_shm")?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| {
        anyhow!(
            "this compositor has no zwlr_layer_shell_v1 ({e}), so nothing can be drawn over a \
             fullscreen game on it (GNOME/Mutter is the usual case). Run the session on X11, run \
             the game under XWayland with DISPLAY set, or draw the border with MAME artwork \
             instead."
        )
    })?;

    let surface = compositor.create_surface(&qh);
    // No pointer or keyboard input, ever: an empty input region means every click, and every
    // touch, lands on whatever is underneath. This is surface state, so it must be set before
    // the commit that maps the surface.
    let empty = Region::new(&compositor)
        .map_err(|e| anyhow!("{e}"))
        .context("creating the empty input region")?;
    surface.set_input_region(Some(empty.wl_region()));

    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some(NAMESPACE), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    // -1: do not let panels or docks push us in; we want the physical screen edge.
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // 0x0 with all four anchors means "the whole output"; the configure tells us how big.
    layer.set_size(0, 0);
    layer.commit();

    let pool = SlotPool::new(4096, &shm).context("creating the shm pool")?;
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        compositor,
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        size: (0, 0),
        configured: false,
        redraw: false,
        closed: false,
    };

    // The surface has no size until the compositor says so; block until the first configure.
    for _ in 0..10 {
        queue
            .blocking_dispatch(&mut state)
            .context("waiting for the layer surface configure")?;
        if state.configured || state.closed {
            break;
        }
    }
    if state.closed {
        bail!("the compositor closed the layer surface before it was configured");
    }
    if !state.configured {
        bail!("the compositor never configured the layer surface");
    }
    if state.size.0 == 0 || state.size.1 == 0 {
        bail!("the compositor configured the overlay with no size and no output mode is known");
    }

    Ok(Box::new(WaylandBackend {
        conn,
        queue,
        state,
        buffers: Vec::new(),
        buffers_size: (0, 0),
    }))
}

/// Everything the sctk handlers touch. Kept apart from the event queue so both can be borrowed
/// while dispatching.
struct State {
    registry_state: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    size: (u32, u32),
    configured: bool,
    /// A configure changed the size (or is the first one); the scene must be drawn again.
    redraw: bool,
    closed: bool,
}

impl State {
    /// The size of the output the overlay is on, for when a configure asks us to pick.
    fn output_size(&mut self) -> Option<(u32, u32)> {
        self.output_state.outputs().find_map(|o| {
            let info = self.output_state.info(&o)?;
            let mode = info.modes.iter().find(|m| m.current)?;
            let (w, h) = mode.dimensions;
            Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
        })
    }
}

struct WaylandBackend {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// Buffers cycled so the compositor can hold one while we fill the next.
    buffers: Vec<Buffer>,
    /// The size `buffers` were made for; a resize throws them away.
    buffers_size: (u32, u32),
}

impl WaylandBackend {
    /// Index in `buffers` of a buffer of the current size whose memory the compositor is not
    /// reading; a new one is made when every existing buffer is still in use.
    fn free_buffer(&mut self, w: u32, h: u32) -> Result<usize> {
        if self.buffers_size != (w, h) {
            self.buffers.clear();
            self.buffers_size = (w, h);
        }
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| !b.slot().has_active_buffers())
        {
            return Ok(i);
        }
        let width = i32::try_from(w).context("overlay width does not fit in i32")?;
        let height = i32::try_from(h).context("overlay height does not fit in i32")?;
        let stride = width
            .checked_mul(4)
            .context("overlay is too wide for an shm buffer")?;
        let (buffer, _) = self
            .state
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .context("allocating an shm buffer")?;
        if self.buffers.len() >= MAX_BUFFERS {
            // The oldest is still busy; drop our handle (sctk frees the slot once the compositor
            // releases it) so the set stays bounded.
            self.buffers.remove(0);
        }
        self.buffers.push(buffer);
        Ok(self.buffers.len() - 1)
    }

    /// Tell the compositor which rectangles are solid, so it can skip blending under them.
    fn set_opaque_region(&self, opaque: Option<&[Rect]>, w: u32, h: u32) -> Result<()> {
        let region = Region::new(&self.state.compositor)
            .map_err(|e| anyhow!("{e}"))
            .context("creating the opaque region")?;
        match opaque {
            None => region.add(
                0,
                0,
                i32::try_from(w).unwrap_or(i32::MAX),
                i32::try_from(h).unwrap_or(i32::MAX),
            ),
            Some(rects) => {
                for r in rects {
                    region.add(
                        r.x,
                        r.y,
                        i32::try_from(r.w).unwrap_or(i32::MAX),
                        i32::try_from(r.h).unwrap_or(i32::MAX),
                    );
                }
            }
        }
        self.state
            .layer
            .wl_surface()
            .set_opaque_region(Some(region.wl_region()));
        Ok(())
    }

    /// Wait up to `timeout` for the compositor to say something, and dispatch what it said.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        let Some(guard) = self.queue.prepare_read() else {
            // Events are already queued up; no point sleeping on the socket.
            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatching Wayland events")?;
            return Ok(());
        };
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let ready = poll_readable(guard.connection_fd().as_raw_fd(), ms)?;
        if ready {
            guard.read().context("reading from the Wayland socket")?;
        } else {
            drop(guard);
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatching Wayland events")?;
        Ok(())
    }
}

/// Wait for `fd` to become readable, or for `ms` milliseconds to pass.
#[allow(unsafe_code)]
fn poll_readable(fd: std::os::fd::RawFd, ms: i32) -> Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: pfd is a valid pollfd array of length 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r >= 0 {
            return Ok(r > 0);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e).context("polling the Wayland socket");
        }
    }
}

impl super::Backend for WaylandBackend {
    fn size(&self) -> (u32, u32) {
        self.state.size
    }

    fn present(&mut self, px: &[u32], opaque: Option<&[Rect]>) -> Result<()> {
        let (w, h) = self.state.size;
        let n = (w as usize) * (h as usize);
        if px.len() < n {
            bail!("frame has {} pixels, the surface wants {n}", px.len());
        }
        let idx = self.free_buffer(w, h)?;
        let canvas = self
            .state
            .pool
            .canvas(&self.buffers[idx])
            .context("the shm buffer is still held by the compositor")?;
        write_argb(canvas, px, w, h, opaque);

        self.set_opaque_region(opaque, w, h)?;
        let surface = self.state.layer.wl_surface();
        surface.damage_buffer(
            0,
            0,
            i32::try_from(w).unwrap_or(i32::MAX),
            i32::try_from(h).unwrap_or(i32::MAX),
        );
        self.buffers[idx]
            .attach_to(surface)
            .map_err(|e| anyhow!("{e}"))
            .context("attaching the shm buffer")?;
        self.state.layer.commit();
        self.queue.flush().context("flushing the Wayland queue")?;
        Ok(())
    }

    fn pump(&mut self, timeout: Duration) -> Result<Event> {
        self.state.redraw = false;
        if let Err(e) = self.queue.flush() {
            tracing::warn!("wayland connection lost: {e}");
            return Ok(Event::Closed);
        }
        if let Err(e) = self.queue.dispatch_pending(&mut self.state) {
            tracing::warn!("wayland connection lost: {e}");
            return Ok(Event::Closed);
        }
        if !self.state.closed && !self.state.redraw {
            if let Err(e) = self.wait(timeout) {
                tracing::warn!("wayland connection lost: {e:#}");
                return Ok(Event::Closed);
            }
        }
        if self.state.closed || self.conn.protocol_error().is_some() {
            return Ok(Event::Closed);
        }
        Ok(if self.state.redraw {
            Event::Redraw
        } else {
            Event::Idle
        })
    }
}

/// Convert `0x00RRGGBB` pixels into the surface's ARGB8888 buffer: black becomes fully
/// transparent, everything else fully opaque, and everything outside `opaque` (when given) is
/// transparent whatever its colour, so only the rectangles the caller named exist on screen.
fn write_argb(canvas: &mut [u8], px: &[u32], w: u32, h: u32, opaque: Option<&[Rect]>) {
    canvas.fill(0);
    let (w, h) = (w as usize, h as usize);
    let mut row = |y: usize, x0: usize, x1: usize| {
        let src = &px[y * w + x0..y * w + x1];
        let dst = &mut canvas[(y * w + x0) * 4..(y * w + x1) * 4];
        for (p, c) in src.iter().zip(dst.chunks_exact_mut(4)) {
            let argb = if *p == 0 { 0 } else { 0xFF00_0000 | *p };
            c.copy_from_slice(&argb.to_le_bytes());
        }
    };
    match opaque {
        None => {
            for y in 0..h {
                row(y, 0, w);
            }
        }
        Some(rects) => {
            for r in rects {
                let x0 = usize::try_from(r.x.max(0)).unwrap_or(0);
                let y0 = usize::try_from(r.y.max(0)).unwrap_or(0);
                if x0 >= w || y0 >= h {
                    continue;
                }
                let x1 = (x0 + r.w as usize).min(w);
                let y1 = (y0 + r.h as usize).min(h);
                for y in y0..y1 {
                    row(y, x0, x1);
                }
            }
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Buffer scale stays 1 on purpose: the border is a fraction of the screen, so it is the
        // right size at any scale, and a scaled buffer would only cost pixels.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // We never ask for frame callbacks; the scene is redrawn when it changes.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let mut size = configure.new_size;
        if size.0 == 0 || size.1 == 0 {
            // The compositor left the choice to us, so take the output's own mode.
            size = self.output_size().unwrap_or(size);
        }
        if size != self.size || !self.configured {
            self.size = size;
            self.redraw = true;
        }
        self.configured = true;
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);
