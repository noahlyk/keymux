use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::window_manager::WindowManagerEvent::WindowFocusChanged;
use crate::x11;
use anyhow::Result;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

pub fn run_i3_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    info!("Starting keymux-i3 watcher");

    if !x11::is_i3_available() {
        error!("i3 socket not found - is i3 running?");
        error!("This daemon requires i3 window manager");
        return Ok(());
    }

    info!("i3 detected, starting window focus monitor");

    let (i3_tx, i3_rx) = mpsc::channel();
    x11::start_i3_monitor_sync(i3_tx);

    loop {
        match i3_rx.recv_timeout(Duration::from_millis(100)) {
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
                error!("i3 monitor died, exiting");
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    info!("i3 watcher stopped");
    Ok(())
}
