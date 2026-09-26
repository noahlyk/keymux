use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::niri;
use crate::window_manager::WindowManagerEvent::WindowFocusChanged;
use anyhow::Result;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

pub fn run_niri_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    info!("Starting keymux-niri watcher");

    if !niri::is_niri_available() {
        error!("Niri socket not found - is Niri running?");
        error!("This daemon requires Niri window manager");
        return Ok(());
    }

    info!("Niri detected, starting window focus monitor");

    let (niri_tx, niri_rx) = mpsc::channel();
    niri::start_niri_monitor_sync(niri_tx);

    loop {
        match niri_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(WindowFocusChanged(window_info)) => {
                info!(
                    "Window focus: app_id={:?}, pid={:?}",
                    window_info.app_id.as_deref().unwrap_or("(none)"),
                    window_info.pid
                );

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
                error!("Niri monitor died, exiting");
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("Niri watcher stopped");
    Ok(())
}
