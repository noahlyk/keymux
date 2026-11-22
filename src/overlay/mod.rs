//! `keymux overlay`: the on-screen steno keyboard and tape.
//!
//! Runs as the session user, because the daemon runs as root and can't reach the Wayland
//! socket. It subscribes to the daemon's [`OverlayHub`](crate::steno::overlay::OverlayHub)
//! and draws a layer-shell panel on the right edge of the screen while a steno layer is
//! on. The panel takes no input and no focus. See `docs/overlay-plan.md`.

mod activity;
pub mod anim;
mod client;
mod demo;
pub mod geometry;
pub mod glyphs;
pub mod merge;
pub mod render;
mod wayland;

use crate::config::{Config, OverlayConfig};
use crate::steno::layout::render_stroke;
use crate::steno::overlay::OverlaySnapshot;
pub use crate::steno::overlay_control::ViewState;
use anyhow::Result;

/// Where the overlay gets its frames from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The daemon
    Daemon,
    /// Made-up strokes, to check the drawing alone
    Demo,
}

/// Exit status of `keymux overlay` when this session cannot show it at all.
///
/// That is no Wayland, or no layer-shell as on GNOME. It is `EX_CONFIG`; the systemd unit
/// lists it in `RestartPreventExitStatus` so that deliberate exit is not retried, while every
/// other failure, including the daemon going away, is.
pub const EXIT_CANNOT_DRAW: i32 = 78;

/// The session cannot show the overlay; retrying will not help.
#[derive(Debug)]
pub struct CannotDraw(String);

impl CannotDraw {
    fn error(what: &str, cause: &dyn std::fmt::Display) -> anyhow::Error {
        anyhow::Error::new(Self(format!("{what}: {cause}")))
    }
}

impl std::fmt::Display for CannotDraw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CannotDraw {}

pub fn run(dump: bool, demo: bool, always: bool) -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    if dump {
        return dump_frames();
    }
    let config = load_config();
    if !config.enabled {
        tracing::info!("overlay is disabled in config (overlay: (enabled: false))");
        return Ok(());
    }
    let source = if demo { Source::Demo } else { Source::Daemon };
    wayland::run(config, source, always)
}

/// Whether the overlay should be on screen for a snapshot on `layer`. `always` (the
/// `--always` flag) shows it on every layer; otherwise it follows the steno layers allowed by
/// the config's `layers` list, where an empty list allows them all.
fn is_visible(config: &OverlayConfig, always: bool, layer: Option<&str>) -> bool {
    config.enabled
        && (always
            || layer.is_some_and(|layer| {
                config.layers.is_empty() || config.layers.iter().any(|l| l.0 == layer)
            }))
}

/// Apply the view commands (`{:overlay:hide}`, ...) a snapshot carries that were not seen yet.
/// `seen` is the number of the last one applied; it only moves forward, so the placeholder
/// snapshot between connections (number 0) changes nothing. Returns whether the view changed.
fn take_controls(view: &mut ViewState, seen: &mut u64, snapshot: &OverlaySnapshot) -> bool {
    let mut changed = false;
    for (seq, command) in &snapshot.controls {
        if *seq > *seen {
            changed |= view.apply(*command);
        }
    }
    *seen = (*seen).max(snapshot.control_seq);
    changed
}

/// Whether the panel has anything to draw under `view`: not hidden whole, and either a part of it
/// left showing or a lookup open (which shows even with the tape hidden). When it has nothing,
/// the surface is released rather than drawn empty, the same as when the layer is left: that
/// costs no memory or compositor work, and showing it again is a single commit.
const fn has_content(config: &OverlayConfig, view: ViewState, snapshot: &OverlaySnapshot) -> bool {
    !view.hidden
        && ((config.keyboard && !view.keyboard_hidden)
            || (config.tape && !view.tape_hidden)
            || snapshot.lookup.is_some())
}

fn config_path() -> Option<std::path::PathBuf> {
    Config::default_path().ok()
}

/// The overlay section of the user's config, or the defaults if it can't be read. A broken
/// config shouldn't keep the overlay from showing; the daemon reports config errors.
fn load_config() -> OverlayConfig {
    config_path()
        .ok_or_else(|| anyhow::anyhow!("no config path"))
        .and_then(|path| Config::load_or_hint(&path))
        .map(|config| config.overlay)
        .unwrap_or_else(|e| {
            tracing::warn!("using default overlay settings: {e:#}");
            OverlayConfig::default()
        })
}

/// `keymux overlay --dump`: print each frame, to check the daemon side without Wayland.
fn dump_frames() -> Result<()> {
    for frame in crate::ipc::subscribe_overlay()? {
        let last = frame.tape.back();
        println!(
            "gen {:>5}  layer {:<12} chord {:<14} held {:<14} last {:<12} {}",
            frame.generation,
            frame.layer.as_deref().unwrap_or("-"),
            render_stroke(frame.chord),
            render_stroke(frame.held),
            last.map_or(String::new(), |entry| render_stroke(entry.stroke)),
            last.map_or("", |entry| entry.text.as_str()),
        );
    }
    println!("daemon closed the connection");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Layer;
    use crate::steno::overlay_control::{Part, Verb, ViewCommand};

    #[test]
    fn it_shows_only_on_steno_layers_unless_always_is_set() {
        let config = OverlayConfig::default();
        assert!(is_visible(&config, false, Some("steno")));
        assert!(!is_visible(&config, false, None));
        // `--always` shows it off steno too
        assert!(is_visible(&config, true, None));
        assert!(is_visible(&config, true, Some("steno")));
    }

    #[test]
    fn the_layers_list_limits_which_steno_layers_show_it() {
        let config = OverlayConfig {
            layers: vec![Layer::new("steno")],
            ..OverlayConfig::default()
        };
        assert!(is_visible(&config, false, Some("steno")));
        assert!(!is_visible(&config, false, Some("practice")));
        // `--always` overrides the list as well
        assert!(is_visible(&config, true, Some("practice")));
    }

    fn command(part: Part, verb: Verb) -> ViewCommand {
        ViewCommand { part, verb }
    }

    fn with_controls(entries: &[(u64, ViewCommand)]) -> OverlaySnapshot {
        OverlaySnapshot {
            control_seq: entries.last().map_or(0, |entry| entry.0),
            controls: entries.iter().copied().collect(),
            ..OverlaySnapshot::default()
        }
    }

    #[test]
    fn a_view_command_is_applied_once_however_often_its_frame_comes() {
        let (mut view, mut seen) = (ViewState::default(), 0);
        let toggle = command(Part::Overlay, Verb::Toggle);
        let frame = with_controls(&[(1, toggle)]);
        assert!(take_controls(&mut view, &mut seen, &frame));
        assert!(view.hidden);
        // The same frame again (a redraw, a reconnect) must not toggle it back
        assert!(!take_controls(&mut view, &mut seen, &frame));
        assert!(view.hidden);
        let frame = with_controls(&[(1, toggle), (2, toggle)]);
        assert!(take_controls(&mut view, &mut seen, &frame));
        assert!(!view.hidden);
    }

    #[test]
    fn a_new_overlay_starts_shown_whatever_was_toggled_before_it_connected() {
        // The client strips the history from the first frame of a connection; the number stays
        let mut first = with_controls(&[(1, command(Part::Overlay, Verb::Hide))]);
        first.controls.clear();
        let (mut view, mut seen) = (ViewState::default(), 0);
        assert!(!take_controls(&mut view, &mut seen, &first));
        assert_eq!((view, seen), (ViewState::default(), 1));
        // The placeholder frame sent while the daemon is away does not rewind or change it
        assert!(!take_controls(
            &mut view,
            &mut seen,
            &OverlaySnapshot::default()
        ));
        assert_eq!(seen, 1);
    }

    #[test]
    fn hiding_every_part_releases_the_surface_but_an_open_lookup_still_shows() {
        let config = OverlayConfig::default();
        let shown = ViewState::default();
        let snapshot = OverlaySnapshot::default();
        assert!(has_content(&config, shown, &snapshot));
        let no_tape = ViewState {
            tape_hidden: true,
            ..shown
        };
        assert!(
            has_content(&config, no_tape, &snapshot),
            "the keyboard stays"
        );
        let nothing = ViewState {
            tape_hidden: true,
            keyboard_hidden: true,
            ..shown
        };
        assert!(!has_content(&config, nothing, &snapshot));
        let looking = OverlaySnapshot {
            lookup: Some(crate::steno::overlay::LookupView::default()),
            ..snapshot
        };
        assert!(has_content(&config, nothing, &looking));
        let hidden = ViewState {
            hidden: true,
            ..shown
        };
        assert!(
            !has_content(&config, hidden, &looking),
            "hide wins over a lookup"
        );
    }

    #[test]
    fn hiding_the_overlay_never_touches_the_configured_start() {
        // The view is its own state: `is_visible` reads only the config, which a command cannot reach
        let config = OverlayConfig::default();
        let before = config.clone();
        let mut view = ViewState::default();
        view.apply(command(Part::Overlay, Verb::Hide));
        assert!(is_visible(&config, false, Some("steno")));
        assert_eq!(config, before);
        // Disabled in config: a "show" cannot start it (no overlay process runs at all, and
        // `is_visible` would still say no)
        let off = OverlayConfig {
            enabled: false,
            ..OverlayConfig::default()
        };
        view.apply(command(Part::Overlay, Verb::Show));
        assert!(!is_visible(&off, false, Some("steno")));
    }

    #[test]
    fn disabled_in_config_wins_over_always() {
        let config = OverlayConfig {
            enabled: false,
            ..OverlayConfig::default()
        };
        assert!(!is_visible(&config, true, Some("steno")));
    }
}
