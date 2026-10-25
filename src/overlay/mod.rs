//! `keymux overlay`: the on-screen steno keyboard and tape.
//!
//! Runs as the session user, because the daemon runs as root and can't reach the Wayland
//! socket. It subscribes to the daemon's [`OverlayHub`](crate::steno::overlay::OverlayHub)
//! and draws a layer-shell panel on the right edge of the screen while a steno layer is
//! on. The panel takes no input and no focus. See `docs/overlay-plan.md`.

mod client;
mod demo;
pub mod geometry;
pub mod glyphs;
pub mod render;
mod wayland;

use crate::config::{Config, OverlayConfig};
use crate::steno::layout::render_stroke;
use anyhow::Result;

/// Where the overlay gets its frames from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The daemon
    Daemon,
    /// Made-up strokes, to check the drawing alone
    Demo,
}

pub fn run(dump: bool, demo: bool) -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    if dump {
        return dump_frames();
    }
    let config = load_config();
    if !config.enabled {
        tracing::info!("overlay is disabled in config (overlay: (enabled: false))");
        return Ok(());
    }
    wayland::run(config, if demo { Source::Demo } else { Source::Daemon })
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
