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
use super::{activity, anim, client, Source, ViewState};
use crate::config::OverlayConfig;
use crate::steno::layout::Layout;
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
use std::time::{Duration, Instant};
use tiny_skia::PixmapMut;

/// The layer-shell namespace, so compositor rules can target the overlay
/// (niri: `layer-rule { match namespace="^keymux-steno$" }`).
const NAMESPACE: &str = "keymux-steno";

/// Surface height, in logical px, to lay out for before the compositor says how tall the
/// output is.
const FALLBACK_HEIGHT: u32 = 1080;

/// The steno [`Layout`] `config.extra_keys` describes - the overlay process never sees a
/// steno layer's own config (it runs separately from the daemon; see `StenoConfig`'s
/// `extra_keys` and `OverlayConfig`'s own copy of the same field), so this is its only source
/// for which extra keys exist at all. A bad entry is dropped with a warning and the rest kept,
/// the same way a broken config elsewhere here falls back rather than refusing to draw anything.
fn layout_for(config: &OverlayConfig) -> Layout {
    Layout::plover_with(&config.extra_keys).unwrap_or_else(|errors| {
        tracing::warn!(
            "overlay extra_keys ignored, using the standard 23 only: {}",
            errors.join("; ")
        );
        Layout::plover()
    })
}

pub fn run(config: OverlayConfig, source: Source, always: bool) -> Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| super::CannotDraw::error("no Wayland session to draw on", &e))?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| {
        super::CannotDraw::error(
            "this compositor has no layer-shell, which the overlay needs (GNOME?)",
            &e,
        )
    })?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;

    // The real height arrives with the first configure; this is only until then
    let renderer = Renderer::with_layout(&config, 1, FALLBACK_HEIGHT, layout_for(&config))?;
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
            if matches!(event, channel::Event::Msg(())) {
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
        animation: None,
        exit: false,
        always,
        presence: Presence::default(),
        view: ViewState::default(),
        controls_seen: 0,
        notation: Notation::default(),
    };
    // With `--always` it shows right away, without waiting for the daemon's first frame
    if always {
        overlay.on_snapshot(OverlaySnapshot::default());
    }
    while !overlay.exit {
        // Asleep until something happens, except while a lookup is playing a multi-stroke outline
        // on the keyboard: then it also wakes when the next stroke is due
        event_loop.dispatch(overlay.next_wake(), &mut overlay)?;
        overlay.tick();
    }
    Ok(())
}

/// A multi-stroke lookup outline playing on the keyboard
struct Animation {
    outline: String,
    strokes: usize,
    started: Instant,
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
    /// The best lookup result's outline while it is being played stroke by stroke on the keyboard
    /// (it has more than one stroke), and when that started. `None` when nothing animates, which
    /// is what lets the loop sleep.
    animation: Option<Animation>,
    exit: bool,
    /// `--always`: show on every layer, not only steno ones
    always: bool,
    /// Whether a click or a window switch put the panel away, and when a hide that is waiting
    /// out `hide_delay_ms` is due
    presence: Presence,
    /// What `{:overlay:hide}`, `{:tape:hide}` and friends have done since this overlay started.
    /// Runtime only: never saved, not part of the config, and a config reload leaves it alone.
    view: ViewState,
    /// Number of the last view command applied (see [`super::take_controls`])
    controls_seen: u64,
    /// The steno layer's theory notation, kept across frames and renderer rebuilds
    notation: Notation,
}

/// The theory notation the daemon last sent (see `OverlaySnapshot::notation`): it comes only on
/// the frames where it changed, so the overlay keeps it.
#[derive(Default)]
struct Notation {
    seq: u64,
    text: Option<std::sync::Arc<str>>,
}

impl Notation {
    /// Take a frame's notation if it is a new one. Returns whether it changed.
    fn take(&mut self, snapshot: &OverlaySnapshot) -> bool {
        if snapshot.notation_seq == self.seq {
            return false;
        }
        self.seq = snapshot.notation_seq;
        self.text = snapshot
            .notation
            .as_ref()
            .map(|text| std::sync::Arc::clone(&text.0));
        true
    }
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
// Four independent facts about one snapshot; a struct would only name the same four
#[allow(clippy::fn_params_excessive_bools)]
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

/// What one snapshot says about whether the panel should be up.
#[derive(Debug, Clone, Copy)]
struct Seen {
    /// The snapshot's layer is one the overlay shows on ([`super::is_visible`])
    on_layer: bool,
    /// The view commands and the config leave something to draw ([`super::has_content`])
    has_content: bool,
    /// Nothing on screen differs from the snapshot before
    unchanged: bool,
}

/// When the panel goes up and down, apart from the Wayland surface itself so it can be tested
/// without one: the caller says whether a surface exists and what time it is, and carries out the
/// [`Action`] it gets back.
///
/// A hide the user did not ask for (the steno layer was left, a click, a window switch) waits
/// `delay` first, so the panel is still in the frame when the key that took you off steno is a
/// screenshot key; steno activity inside that wait calls the hide off. A hide that was asked for
/// (`{:overlay:hide}`, nothing left to draw) is immediate, and so is every show.
#[derive(Debug, Default)]
struct Presence {
    /// A click or a window switch happened since the last real steno activity: stay hidden
    /// even if the steno layer is still on, until a genuine change (a stroke, a layer
    /// change) proves you're back to steno-typing.
    suppressed: bool,
    /// When the waiting hide is due. `None` when none waits, which lets the loop sleep.
    hide_at: Option<Instant>,
}

impl Presence {
    fn on_snapshot(
        &mut self,
        seen: Seen,
        has_surface: bool,
        delay: Duration,
        now: Instant,
    ) -> Action {
        let visible = seen.on_layer && seen.has_content;
        let (action, suppressed) =
            next_action(visible, has_surface, seen.unchanged, self.suppressed);
        self.suppressed = suppressed;
        if visible && !suppressed {
            // Back on steno before the wait ran out: the panel stays
            self.hide_at = None;
        }
        match action {
            // Only the layer was left: there is still something to draw, so wait
            Action::Hide if seen.has_content => self.hide_after(delay, now),
            action => action,
        }
    }

    /// A click or a window switch: put the panel away (after the wait) and keep it away until
    /// real steno activity.
    fn on_interrupt(&mut self, has_surface: bool, delay: Duration, now: Instant) -> Action {
        self.suppressed = true;
        if has_surface {
            self.hide_after(delay, now)
        } else {
            Action::Idle
        }
    }

    /// Hide now with no delay, else note when. A hide already waiting keeps its time: more
    /// reasons to hide (each key typed on the other keyboard) must not push it back.
    fn hide_after(&mut self, delay: Duration, now: Instant) -> Action {
        if delay.is_zero() {
            return Action::Hide;
        }
        self.hide_at.get_or_insert(now + delay);
        Action::Idle
    }

    /// The loop woke: is the waiting hide due?
    fn on_tick(&self, now: Instant) -> Action {
        match self.hide_at {
            Some(due) if now >= due => Action::Hide,
            _ => Action::Idle,
        }
    }

    /// How long until the waiting hide is due, for the loop's sleep
    fn until_hide(&self, now: Instant) -> Option<Duration> {
        self.hide_at.map(|due| due.saturating_duration_since(now))
    }
}

impl Overlay {
    const fn hide_delay(&self) -> Duration {
        Duration::from_millis(self.config.hide_delay_ms)
    }

    fn on_snapshot(&mut self, snapshot: OverlaySnapshot) {
        let notation_changed = self.notation.take(&snapshot);
        if notation_changed {
            self.renderer.set_notation(self.notation.text.as_deref());
        }
        let before = self.controls_seen;
        let view_changed = super::take_controls(&mut self.view, &mut self.controls_seen, &snapshot);
        let commanded = snapshot.controls.iter().any(|(seq, _)| *seq > before);
        if commanded && !self.view.hidden {
            // A show (even of what already shows) brings it back after a click or a window
            // switch put it away: the user asked for it
            self.presence.suppressed = false;
        }
        let on_layer = super::is_visible(&self.config, self.always, snapshot.layer.as_deref());
        let has_content = super::has_content(&self.config, self.view, &snapshot);
        // Skip frames that change nothing on screen, such as a repeat after a reconnect
        let unchanged = !view_changed
            && !notation_changed
            && self.latest.layer == snapshot.layer
            && self.latest.chord == snapshot.chord
            && self.latest.held == snapshot.held
            && self.latest.lookup == snapshot.lookup
            && self
                .latest
                .tape
                .back()
                .map(|e| (e.seq, e.repeats, &e.split))
                == snapshot.tape.back().map(|e| (e.seq, e.repeats, &e.split));
        self.latest = snapshot;
        self.follow_lookup();
        let action = self.presence.on_snapshot(
            Seen {
                on_layer,
                has_content,
                unchanged,
            },
            self.surface.is_some(),
            self.hide_delay(),
            Instant::now(),
        );
        self.act(action);
    }

    fn act(&mut self, action: Action) {
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

    /// Start, restart or stop the stroke-by-stroke animation to match the lookup on screen: a new
    /// best outline starts again from its first stroke, the same one carries on, and one stroke
    /// (or no lookup) stops it.
    fn follow_lookup(&mut self) {
        let playing = self.latest.lookup.as_ref().and_then(|view| {
            let strokes = Renderer::lookup_strokes(view).len();
            let row = view.rows.first()?;
            (strokes > 1).then(|| (row.outline.clone(), strokes))
        });
        match (playing, &self.animation) {
            (Some((outline, _)), Some(now)) if now.outline == outline => {}
            (Some((outline, strokes)), _) => {
                self.animation = Some(Animation {
                    outline,
                    strokes,
                    started: Instant::now(),
                });
                self.renderer.set_step(0);
            }
            (None, _) => {
                self.animation = None;
                self.renderer.set_step(0);
            }
        }
    }

    /// How long the loop may sleep: `None` (until an event) unless a hide is waiting out its
    /// delay, or an outline is playing on a panel that is showing; then until the sooner of the
    /// two is due.
    fn next_wake(&self) -> Option<Duration> {
        let hide = self.presence.until_hide(Instant::now());
        let step = self.animation.as_ref().and_then(|animation| {
            self.surface.as_ref()?;
            anim::until_next_step(animation.strokes, animation.started.elapsed())
        });
        match (hide, step) {
            (Some(hide), Some(step)) => Some(hide.min(step)),
            (hide, step) => hide.or(step),
        }
    }

    /// Carry out a hide whose delay ran out, and light the stroke whose turn it is, redrawing
    /// only when that changed.
    fn tick(&mut self) {
        let due = self.presence.on_tick(Instant::now());
        self.act(due);
        let Some(animation) = &self.animation else {
            return;
        };
        if self.surface.is_none() {
            return;
        }
        let step = anim::step_at(animation.strokes, animation.started.elapsed());
        if step != self.renderer.step() {
            self.renderer.set_step(step);
            self.dirty = true;
            self.draw();
        }
    }

    /// A click or a window switch arrived from [`activity`]: you've stepped away from
    /// steno-typing, so hide the overlay (if it's up, and after `hide_delay_ms`) until real
    /// steno activity says otherwise, even though the steno layer may still be on.
    fn on_activity_interrupted(&mut self) {
        let action =
            self.presence
                .on_interrupt(self.surface.is_some(), self.hide_delay(), Instant::now());
        self.act(action);
    }

    /// The config file changed. Layout, size and visibility rules may all differ, so rebuild
    /// the renderer and recreate the surface if it's showing.
    fn on_config(&mut self, config: OverlayConfig) {
        if config == self.config {
            return;
        }
        match Renderer::with_layout(&config, self.scale as u32, self.height, layout_for(&config)) {
            Ok(renderer) => {
                self.renderer = renderer;
                self.renderer.set_notation(self.notation.text.as_deref());
            }
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
        // Whatever hid it, no hide is waiting any more
        self.presence.hide_at = None;
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
        self.renderer
            .draw_view(&self.latest, self.view, &mut pixmap);
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
        match Renderer::with_layout(
            &self.config,
            new_factor as u32,
            self.height,
            layout_for(&self.config),
        ) {
            Ok(renderer) => {
                self.renderer = renderer;
                self.renderer.set_notation(self.notation.text.as_deref());
            }
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
            match Renderer::with_layout(
                &self.config,
                self.scale as u32,
                height,
                layout_for(&self.config),
            ) {
                Ok(renderer) => {
                    self.renderer = renderer;
                    self.renderer.set_notation(self.notation.text.as_deref());
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
        assert_eq!(next_action(true, false, false, true), (Action::Show, false));
    }

    #[test]
    fn a_suppressed_duplicate_snapshot_stays_hidden() {
        // Suppressed, and this snapshot carries nothing new: stays down, stays suppressed
        assert_eq!(next_action(true, false, true, true), (Action::Idle, true));
    }

    #[test]
    fn leaving_the_steno_layer_hides_regardless_of_suppression() {
        assert_eq!(
            next_action(false, true, false, false),
            (Action::Hide, false)
        );
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
        assert_eq!(
            next_action(false, false, true, false),
            (Action::Idle, false)
        );
    }

    const DELAY: Duration = Duration::from_millis(40);
    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    /// A stroke on a steno layer
    const STROKE: Seen = Seen {
        on_layer: true,
        has_content: true,
        unchanged: false,
    };
    /// A key on another keyboard: the steno layer is no longer the active one
    const LEFT_STENO: Seen = Seen {
        on_layer: false,
        has_content: true,
        unchanged: false,
    };
    /// `{:overlay:hide}` struck on the steno layer
    const COMMANDED_HIDE: Seen = Seen {
        on_layer: true,
        has_content: false,
        unchanged: false,
    };

    /// The panel as `Overlay` drives it, without the surface: `up` stands in for it.
    struct Panel {
        presence: Presence,
        up: bool,
        delay: Duration,
        start: Instant,
    }

    impl Panel {
        /// Up after a first stroke at time 0
        fn showing(delay: Duration) -> Self {
            let mut panel = Self {
                presence: Presence::default(),
                up: false,
                delay,
                start: Instant::now(),
            };
            assert_eq!(panel.snapshot(STROKE, 0), Action::Show);
            panel
        }
        fn act(&mut self, action: Action) -> Action {
            match action {
                Action::Show => self.up = true,
                Action::Hide => {
                    self.up = false;
                    self.presence.hide_at = None; // as `Overlay::hide` does
                }
                Action::Redraw | Action::Idle => {}
            }
            action
        }
        fn snapshot(&mut self, seen: Seen, at_ms: u64) -> Action {
            let now = self.start + ms(at_ms);
            let action = self.presence.on_snapshot(seen, self.up, self.delay, now);
            self.act(action)
        }
        fn interrupt(&mut self, at_ms: u64) -> Action {
            let now = self.start + ms(at_ms);
            let action = self.presence.on_interrupt(self.up, self.delay, now);
            self.act(action)
        }
        fn tick(&mut self, at_ms: u64) -> Action {
            let action = self.presence.on_tick(self.start + ms(at_ms));
            self.act(action)
        }
        fn sleep(&self, at_ms: u64) -> Option<Duration> {
            self.presence.until_hide(self.start + ms(at_ms))
        }
    }

    #[test]
    fn leaving_steno_keeps_the_panel_up_until_the_delay_runs_out() {
        let mut panel = Panel::showing(DELAY);
        assert_eq!(panel.sleep(50), None, "nothing waits: the loop sleeps");
        assert_eq!(panel.snapshot(LEFT_STENO, 100), Action::Idle);
        assert!(panel.up);
        // The loop sleeps exactly until the hide is due, and waking early changes nothing
        assert_eq!(panel.sleep(100), Some(DELAY));
        assert_eq!(panel.tick(139), Action::Idle);
        assert!(panel.up);
        assert_eq!(panel.sleep(139), Some(ms(1)));
        assert_eq!(panel.tick(140), Action::Hide);
        assert!(!panel.up);
        assert_eq!(panel.sleep(140), None, "hidden: the loop sleeps again");
        assert_eq!(panel.tick(500), Action::Idle);
    }

    #[test]
    fn more_keys_off_steno_do_not_push_the_hide_back() {
        let mut panel = Panel::showing(DELAY);
        panel.snapshot(LEFT_STENO, 100);
        assert_eq!(panel.snapshot(LEFT_STENO, 130), Action::Idle);
        assert_eq!(panel.interrupt(135), Action::Idle);
        assert_eq!(panel.sleep(135), Some(ms(5)));
        assert_eq!(panel.tick(140), Action::Hide);
    }

    #[test]
    fn a_stroke_inside_the_delay_calls_the_hide_off() {
        let mut panel = Panel::showing(DELAY);
        panel.snapshot(LEFT_STENO, 100);
        assert_eq!(panel.snapshot(STROKE, 120), Action::Redraw);
        assert_eq!(panel.sleep(120), None);
        assert_eq!(panel.tick(1000), Action::Idle);
        assert!(panel.up);
    }

    #[test]
    fn a_click_hides_after_the_delay_and_a_stroke_inside_it_calls_that_off() {
        let mut panel = Panel::showing(DELAY);
        assert_eq!(panel.interrupt(100), Action::Idle);
        assert!(panel.up);
        // A repeat of the same snapshot is not steno activity: the hide still waits
        let repeat = Seen {
            unchanged: true,
            ..STROKE
        };
        assert_eq!(panel.snapshot(repeat, 110), Action::Idle);
        assert_eq!(panel.sleep(110), Some(ms(30)));
        assert_eq!(panel.snapshot(STROKE, 120), Action::Redraw);
        assert_eq!(panel.tick(1000), Action::Idle);
        assert!(panel.up);

        // Left alone, the click does hide it, and it stays away until a real change
        panel.interrupt(2000);
        assert_eq!(panel.tick(2040), Action::Hide);
        assert_eq!(panel.snapshot(repeat, 2050), Action::Idle);
        assert!(!panel.up);
        assert_eq!(panel.snapshot(STROKE, 2060), Action::Show);
    }

    #[test]
    fn a_commanded_hide_is_never_delayed() {
        let mut panel = Panel::showing(DELAY);
        assert_eq!(panel.snapshot(COMMANDED_HIDE, 100), Action::Hide);
        assert!(!panel.up);

        // Nor while another hide is waiting, and nothing is left waiting after it
        let mut panel = Panel::showing(DELAY);
        panel.snapshot(LEFT_STENO, 100);
        let hidden_off_layer = Seen {
            on_layer: false,
            ..COMMANDED_HIDE
        };
        assert_eq!(panel.snapshot(hidden_off_layer, 110), Action::Hide);
        assert_eq!(panel.sleep(110), None);
    }

    #[test]
    fn no_delay_hides_at_once_as_before() {
        let mut panel = Panel::showing(Duration::ZERO);
        assert_eq!(panel.snapshot(LEFT_STENO, 100), Action::Hide);
        assert!(!panel.up);
        assert_eq!(panel.sleep(100), None);

        let mut panel = Panel::showing(Duration::ZERO);
        assert_eq!(panel.interrupt(100), Action::Hide);
        assert!(!panel.up);
        assert_eq!(panel.sleep(100), None);
    }

    #[test]
    fn showing_is_never_delayed() {
        let mut panel = Panel::showing(DELAY);
        panel.snapshot(LEFT_STENO, 100);
        panel.tick(140);
        assert_eq!(panel.snapshot(STROKE, 141), Action::Show);
        assert!(panel.up);
    }
}
