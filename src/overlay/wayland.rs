//! The overlay's Wayland side: one layer-shell surface on the right screen edge.
//!
//! - `Layer::Overlay`, anchored `RIGHT` only, so the compositor centres it vertically.
//! - No keyboard interactivity, exclusive zone 0 (windows don't move for it), and an empty
//!   input region: clicks and focus go straight through to whatever is underneath.
//! - The surface exists only while a steno layer is on. Hiding destroys it and showing
//!   creates a fresh one, which is simpler than unmapping and costs nothing at this rate.
//! - Event-driven: the loop sleeps until a frame from the daemon or a Wayland event. At
//!   most one frame callback is pending, so a burst of strokes draws once.

use super::render::Renderer;
use super::{activity, client, Source};
use crate::config::OverlayConfig;
use crate::steno::overlay::OverlaySnapshot;
use anyhow::{anyhow, Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{channel, EventLoop},
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_output, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tiny_skia::PixmapMut;

/// The layer-shell namespace, so compositor rules can target the overlay
/// (niri: `layer-rule { match namespace="^keymux-steno$" }`).
const NAMESPACE: &str = "keymux-steno";

/// Surface height, in logical px, to lay out for before the compositor says how tall the
/// output is.
const FALLBACK_HEIGHT: u32 = 1080;

pub fn run(config: OverlayConfig, source: Source, always: bool) -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland session to draw on")?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("this compositor has no layer-shell, which the overlay needs (GNOME?)")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;

    // The real height arrives with the first configure; this is only until then
    let renderer = Renderer::new(&config, 1, FALLBACK_HEIGHT)?;
    let (width, height) = renderer.size();
    // Room for two buffers: one the compositor holds, one being drawn
    let pool = SlotPool::new((width * height * 4 * 2) as usize, &shm)?;

    let mut event_loop: EventLoop<Overlay> = EventLoop::try_new()?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow!("wayland event source: {e}"))?;

    let (frames, frames_source) = channel::channel::<OverlaySnapshot>();
    event_loop
        .handle()
        .insert_source(frames_source, |event, (), overlay| {
            if let channel::Event::Msg(snapshot) = event {
                overlay.on_snapshot(snapshot);
            }
        })
        .map_err(|e| anyhow!("overlay frame source: {e}"))?;
    match source {
        Source::Daemon => client::spawn(frames),
        Source::Demo => super::demo::spawn(frames),
    }

    // Edits to the config file arrive here, already parsed
    let (config_updates, config_source) = channel::channel::<OverlayConfig>();
    event_loop
        .handle()
        .insert_source(config_source, |event, (), overlay| {
            if let channel::Event::Msg(config) = event {
                overlay.on_config(config);
            }
        })
        .map_err(|e| anyhow!("overlay config source: {e}"))?;
    client::watch_config(config_updates);

    // A window switch or a click means you've stepped away from steno-typing: drop the
    // overlay until real steno activity (a stroke, a layer change) proves otherwise.
    let (interrupts, interrupt_source) = channel::channel::<()>();
    event_loop
        .handle()
        .insert_source(interrupt_source, |event, (), overlay| {
            if let channel::Event::Msg(()) = event {
                overlay.on_activity_interrupted();
            }
        })
        .map_err(|e| anyhow!("overlay activity source: {e}"))?;
    activity::watch_window_focus(interrupts.clone());
    activity::watch_mouse_clicks(&interrupts);

    let mut overlay = Overlay {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        qh,
        renderer,
        config,
        scale: 1,
        height: FALLBACK_HEIGHT,
        surface: None,
        configured: false,
        frame_pending: false,
        dirty: false,
        latest: OverlaySnapshot::default(),
        exit: false,
        always,
        suppressed: false,
    };
    // With `--always` it shows right away, without waiting for the daemon's first frame
    if always {
        overlay.on_snapshot(OverlaySnapshot::default());
    }
    while !overlay.exit {
        event_loop.dispatch(None, &mut overlay)?;
    }
    Ok(())
}

struct Overlay {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<Self>,
    renderer: Renderer,
    config: OverlayConfig,
    /// Integer scale of the output the panel is on. The renderer draws at this scale.
    scale: i32,
    /// Height of the surface in logical px, as the compositor last configured it
    height: u32,
    /// Present only while a steno layer is on
    surface: Option<LayerSurface>,
    /// The compositor has sent the first configure, so the surface may be drawn
    configured: bool,
    /// A frame callback is outstanding; draw when it fires, not before
    frame_pending: bool,
    /// `latest` hasn't been drawn yet
    dirty: bool,
    latest: OverlaySnapshot,
    exit: bool,
    /// `--always`: show on every layer, not only steno ones
    always: bool,
    /// A click or a window switch happened since the last real steno activity: stay hidden
    /// even if the steno layer is still on, until a genuine change (a stroke, a layer
    /// change) proves you're back to steno-typing. See [`Self::on_activity_interrupted`].
    suppressed: bool,
}

/// What a snapshot should do to the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Show,
    Hide,
    Redraw,
    Idle,
}

/// Work out what `on_snapshot` should do, and whether suppression should lift, as a pure
/// function so it's testable without a live Wayland connection (see [`super::is_visible`] for
/// the same idea applied to plain layer-based visibility).
///
/// `unchanged` says this snapshot carries nothing new on screen; `was_suppressed` is a click or
/// window switch with no real steno activity since. A real change clears suppression (you're
/// steno-typing again); a duplicate snapshot while suppressed leaves it hidden instead of
/// popping back up on its own.
const fn next_action(
    visible: bool,
    has_surface: bool,
    unchanged: bool,
    was_suppressed: bool,
) -> (Action, bool) {
    let suppressed = was_suppressed && unchanged;
    let action = match (visible, has_surface) {
        (true, false) if !suppressed => Action::Show,
        (false, true) => Action::Hide,
        (true, true) if !unchanged => Action::Redraw,
        _ => Action::Idle,
    };
    (action, suppressed)
}

impl Overlay {
    fn on_snapshot(&mut self, snapshot: OverlaySnapshot) {
        let visible = super::is_visible(&self.config, self.always, snapshot.layer.as_deref());
        // Skip frames that change nothing on screen, such as a repeat after a reconnect
        let unchanged = self.latest.layer == snapshot.layer
            && self.latest.chord == snapshot.chord
            && self.latest.held == snapshot.held
            && self.latest.lookup == snapshot.lookup
            && self.latest.tape.back().map(|e| (e.seq, e.repeats))
                == snapshot.tape.back().map(|e| (e.seq, e.repeats));
        self.latest = snapshot;
        let (action, suppressed) =
            next_action(visible, self.surface.is_some(), unchanged, self.suppressed);
        self.suppressed = suppressed;
        match action {
            Action::Show => self.show(),
            Action::Hide => self.hide(),
            Action::Redraw => {
                self.dirty = true;
                self.draw();
            }
            Action::Idle => {}
        }
    }

    /// A click or a window switch arrived from [`activity`]: you've stepped away from
    /// steno-typing, so hide the overlay (if it's up) until real steno activity says
    /// otherwise, even though the steno layer may still be on.
    fn on_activity_interrupted(&mut self) {
        self.suppressed = true;
        if self.surface.is_some() {
            self.hide();
        }
    }

    /// The config file changed. Layout, size and visibility rules may all differ, so rebuild
    /// the renderer and recreate the surface if it's showing.
    fn on_config(&mut self, config: OverlayConfig) {
        if config == self.config {
            return;
        }
        match Renderer::new(&config, self.scale as u32, self.height) {
            Ok(renderer) => self.renderer = renderer,
            Err(e) => {
                tracing::warn!("keeping the old overlay settings: {e:#}");
                return;
            }
        }
        self.config = config;
        let showing = self.surface.is_some();
        self.hide();
        if showing {
            // No surface now, so this shows a fresh one at the new size
            self.on_snapshot(self.latest.clone());
        }
    }

    fn show(&mut self) {
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        surface.set_buffer_scale(self.scale);
        // An empty input region: the panel never takes a click
        match Region::new(&self.compositor) {
            Ok(region) => surface.set_input_region(Some(region.wl_region())),
            Err(e) => tracing::warn!("overlay may catch clicks: {e}"),
        }
        let output = self.chosen_output();
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            output.as_ref(),
        );
        // Pinned to the right edge and stretched top to bottom: a height of 0 asks the
        // compositor for the full output height, which the renderer lays the tape out to fill
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT);
        layer.set_margin(0, self.config.margin as i32, 0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // -1: ignore other surfaces' exclusive zones (a bar), so it spans the whole screen
        layer.set_exclusive_zone(-1);
        layer.set_size(self.config.width, 0);
        // The first commit has no buffer; drawing waits for the compositor's configure
        layer.commit();
        self.surface = Some(layer);
        self.configured = false;
        self.frame_pending = false;
        self.dirty = true;
    }

    fn hide(&mut self) {
        // Dropping the last handle destroys the surface
        self.surface = None;
        self.configured = false;
    }

    /// The output named in config, if it's connected. `None` lets the compositor pick.
    fn chosen_output(&self) -> Option<wl_output::WlOutput> {
        let name = self.config.output.as_deref()?;
        let found = self.outputs.outputs().find(|output| {
            self.outputs
                .info(output)
                .is_some_and(|info| info.name.as_deref() == Some(name))
        });
        if found.is_none() {
            tracing::warn!("overlay output {name:?} not connected, letting the compositor pick");
        }
        found
    }

    fn draw(&mut self) {
        let Some(layer) = &self.surface else {
            return;
        };
        if !self.configured || self.frame_pending || !self.dirty {
            return;
        }
        let (width, height) = self.renderer.size();
        // tiny-skia writes RGBA. Abgr8888 is that byte order on little-endian, so it can
        // draw straight into the buffer; Argb8888 (always available) needs R and B swapped.
        let native = self.shm.formats().contains(&wl_shm::Format::Abgr8888);
        let format = if native {
            wl_shm::Format::Abgr8888
        } else {
            wl_shm::Format::Argb8888
        };
        // The pool hands back a slot the compositor has released, so this doesn't grow
        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(width as i32, height as i32, (width * 4) as i32, format)
            {
                Ok(created) => created,
                Err(e) => {
                    tracing::error!("overlay buffer: {e}");
                    return;
                }
            };
        let Some(mut pixmap) = PixmapMut::from_bytes(canvas, width, height) else {
            return;
        };
        self.renderer.draw(&self.latest, &mut pixmap);
        if !native {
            for pixel in canvas.as_chunks_mut::<4>().0.iter_mut() {
                pixel.swap(0, 2);
            }
        }

        let surface = layer.wl_surface();
        // TODO(overlay): damage only the keyboard or tape rect the renderer repainted
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(&self.qh, surface.clone());
        if let Err(e) = buffer.attach_to(surface) {
            tracing::error!("overlay attach: {e}");
            return;
        }
        layer.commit();
        self.frame_pending = true;
        self.dirty = false;
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // Integer scales only: draw at that many pixels per logical pixel so the panel is
        // sharp. A fractional scale gets the next integer up, which the compositor shrinks.
        // TODO(overlay): wp_fractional_scale + viewporter for exact fractional sizes.
        if new_factor == self.scale || new_factor < 1 {
            return;
        }
        self.scale = new_factor;
        match Renderer::new(&self.config, new_factor as u32, self.height) {
            Ok(renderer) => self.renderer = renderer,
            Err(e) => return tracing::warn!("overlay scale {new_factor}: {e:#}"),
        }
        if let Some(layer) = &self.surface {
            layer.wl_surface().set_buffer_scale(new_factor);
            self.dirty = true;
            self.draw();
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        self.draw();
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor took it away (output unplugged, say). Recreate on the next frame.
        if self.surface.as_ref() == Some(layer) {
            self.hide();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        // The compositor picks the height (the output's); lay the tape out to fill it
        let height = configure.new_size.1;
        if height > 0 && height != self.height {
            match Renderer::new(&self.config, self.scale as u32, height) {
                Ok(renderer) => {
                    self.renderer = renderer;
                    self.height = height;
                }
                Err(e) => tracing::warn!("overlay layout for height {height}: {e:#}"),
            }
        }
        self.configured = true;
        self.dirty = true;
        self.draw();
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_layer!(Overlay);
delegate_registry!(Overlay);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_shows_a_hidden_overlay_and_clears_suppression() {
        // Not suppressed: ordinary case, visible and nothing on screen yet
        assert_eq!(
            next_action(true, false, false, false),
            (Action::Show, false)
        );
        // Was suppressed, but this is a real change (a stroke, a layer toggle): shows again
        assert_eq!(
            next_action(true, false, false, true),
            (Action::Show, false)
        );
    }

    #[test]
    fn a_suppressed_duplicate_snapshot_stays_hidden() {
        // Suppressed, and this snapshot carries nothing new: stays down, stays suppressed
        assert_eq!(next_action(true, false, true, true), (Action::Idle, true));
    }

    #[test]
    fn leaving_the_steno_layer_hides_regardless_of_suppression() {
        assert_eq!(next_action(false, true, false, false), (Action::Hide, false));
        assert_eq!(next_action(false, true, true, true), (Action::Hide, true));
    }

    #[test]
    fn a_visible_change_redraws_an_already_showing_overlay() {
        assert_eq!(
            next_action(true, true, false, false),
            (Action::Redraw, false)
        );
    }

    #[test]
    fn nothing_happens_when_nothing_changed_and_nothing_is_suppressed() {
        assert_eq!(next_action(true, true, true, false), (Action::Idle, false));
        assert_eq!(next_action(false, false, true, false), (Action::Idle, false));
    }
}
