//! `keymux steno-session`: everything steno needs from the user's graphical session, in one
//! process, so there is one service to enable (`keymux-steno.service`).
//!
//! The daemon runs as root and can't reach the Wayland socket, so the parts of steno that
//! need it run here instead: the on-screen overlay and the reader of the focused text box.
//! Each is optional. One that can't run (no layer-shell on GNOME, another input method on the
//! seat) says so and the other carries on. The session ends once neither is left, and fails
//! only if both did.

use anyhow::{anyhow, Result};

pub fn run() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_target(false).try_init();

    #[cfg(feature = "textbox")]
    let textbox = std::thread::spawn(|| crate::textbox::run(false));

    #[cfg(feature = "overlay")]
    let overlay = crate::overlay::run(false, false, false);
    #[cfg(not(feature = "overlay"))]
    let overlay: Result<()> = Err(anyhow!("built without the overlay feature"));

    #[cfg(feature = "textbox")]
    let reading = textbox
        .join()
        .unwrap_or_else(|_| Err(anyhow!("the text box reader crashed")));
    #[cfg(not(feature = "textbox"))]
    let reading: Result<()> = Err(anyhow!("built without the textbox feature"));

    if let Err(e) = &overlay {
        tracing::warn!("the overlay is off: {e:#}");
    }
    if let Err(e) = &reading {
        tracing::warn!("text box reading is off: {e:#}");
    }
    match (overlay, reading) {
        (Err(e), Err(_)) => Err(e),
        _ => Ok(()),
    }
}
