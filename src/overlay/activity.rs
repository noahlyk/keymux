//! Hides the overlay the moment you're not actively steno-typing any more: a niri window
//! switch, or a mouse click, each forwarded to the Wayland loop as a plain "stop showing"
//! signal. Real steno activity (a stroke, a layer change) un-suppresses it again; see
//! `Overlay::on_snapshot` in `wayland.rs`.

use crate::window_manager::WindowManagerEvent;
use evdev::{InputEventKind, Key};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::sync::mpsc;
use std::thread;

/// Hide the overlay on a niri window-focus change. Switching to whatever you were about to
/// click or type into is as good a sign as any that you're done steno-typing for now. A
/// no-op off niri (no socket, or a different compositor): nothing watches, nothing hides on
/// this trigger, and the overlay falls back to its ordinary layer-based visibility.
pub fn watch_window_focus(interrupt: Sender<()>) {
    if !crate::niri::is_niri_available() {
        return;
    }
    let spawned = thread::Builder::new()
        .name("overlay-niri-focus".into())
        .spawn(move || {
            let (tx, rx) = mpsc::channel();
            crate::niri::start_niri_monitor_sync(tx);
            while let Ok(WindowManagerEvent::WindowFocusChanged(_)) = rx.recv() {
                if interrupt.send(()).is_err() {
                    return; // the Wayland loop is gone
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("overlay won't hide on window switch: {e}");
    }
}

/// Buttons that count as "you clicked on something" — the three standard ones, not every
/// side/extra button a gaming mouse might have.
const CLICK_KEYS: [Key; 3] = [Key::BTN_LEFT, Key::BTN_RIGHT, Key::BTN_MIDDLE];

/// Hide the overlay on a click, from any pointer device plugged in. One thread per device,
/// each just blocking on its own read; a motion event costs one enum match before it's
/// dropped, so nothing is accumulated or redrawn for it - only a button going down is ever
/// forwarded.
pub fn watch_mouse_clicks(interrupt: &Sender<()>) {
    for (path, device) in evdev::enumerate() {
        if !device
            .supported_keys()
            .is_some_and(|keys| keys.contains(Key::BTN_LEFT))
        {
            continue; // not a pointer device
        }
        let name = device.name().unwrap_or("mouse").to_string();
        let interrupt = interrupt.clone();
        let spawned = thread::Builder::new()
            .name("overlay-mouse".into())
            .spawn(move || {
                let mut device = device;
                loop {
                    let events = match device.fetch_events() {
                        Ok(events) => events,
                        Err(e) => {
                            tracing::debug!("overlay mouse {}: {e}", path.display());
                            return;
                        }
                    };
                    for event in events {
                        let InputEventKind::Key(key) = event.kind() else {
                            continue; // movement and everything else: never looked at further
                        };
                        if event.value() == 1
                            && CLICK_KEYS.contains(&key)
                            && interrupt.send(()).is_err()
                        {
                            return; // the Wayland loop is gone
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("overlay won't hide on a click from {name}: {e}");
        }
    }
}
