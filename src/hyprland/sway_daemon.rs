use crate::hyprland;
use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::window_manager::WindowManagerEvent::WindowFocusChanged;
use anyhow::Result;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

pub fn run_sway_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    info!("Starting keymux-sway watcher");

    if !hyprland::is_sway_available() {
        error!("Sway socket not found - is Sway running?");
        error!("This daemon requires Sway window manager");
        return Ok(());
    }

    info!("Sway detected, starting window focus monitor");

    let (sway_tx, sway_rx) = mpsc::channel();
    hyprland::start_sway_monitor_sync(sway_tx);

    loop {
        match sway_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(WindowFocusChanged(window_info)) => {
                // The root daemon resolves game mode itself (heuristics +
                // overrides) - this watcher just reports the raw focus event.
                match send_request(&IpcRequest::WindowFocusChanged {
                    app_id: window_info.app_id,
                    pid: window_info.pid,
                    title: window_info.title,
                }) {
                    Ok(IpcResponse::Ok) => {}
                    Ok(other) => {
                        warn!("Unexpected response from daemon: {:?}", other);
                    }
                    Err(e) => {
                        error!("Failed to send window focus update to daemon: {}", e);
                        error!("Is keymux daemon running?");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                error!("Sway monitor died, exiting");
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("Sway watcher stopped");
    Ok(())
}
