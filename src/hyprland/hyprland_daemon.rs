use crate::hyprland;
use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::window_manager::WindowManagerEvent::WindowFocusChanged;
use anyhow::Result;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

pub fn run_hyprland_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    info!("Starting keymux-hyprland watcher");

    if !hyprland::is_hyprland_available() {
        error!("Hyprland socket not found - is Hyprland running?");
        error!("This daemon requires Hyprland window manager");
        return Ok(());
    }

    info!("Hyprland detected, starting window focus monitor");

    let (hyprland_tx, hyprland_rx) = mpsc::channel();
    hyprland::start_hyprland_monitor_sync(hyprland_tx);

    loop {
        match hyprland_rx.recv_timeout(Duration::from_millis(100)) {
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
                error!("Hyprland monitor died, exiting");
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("Hyprland watcher stopped");
    Ok(())
}
