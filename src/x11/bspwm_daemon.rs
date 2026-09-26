use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::window_manager::WindowManagerEvent::WindowFocusChanged;
use crate::x11;
use anyhow::Result;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

pub fn run_bspwm_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    info!("Starting keymux-bspwm watcher");

    if !x11::is_bspwm_available() {
        error!("bspwm not available - is bspwm running?");
        error!("This daemon requires bspwm window manager");
        return Ok(());
    }

    info!("bspwm detected, starting window focus monitor");

    let (bspwm_tx, bspwm_rx) = mpsc::channel();
    x11::start_bspwm_monitor_sync(bspwm_tx);

    loop {
        match bspwm_rx.recv_timeout(Duration::from_millis(100)) {
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
                error!("bspwm monitor died, exiting");
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("bspwm watcher stopped");
    Ok(())
}
