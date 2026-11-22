//! Keeps a subscription to the daemon open and forwards each frame to the Wayland loop.

use crate::config::OverlayConfig;
use crate::steno::overlay::OverlaySnapshot;
use notify::{RecursiveMode, Watcher};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::time::Duration;

/// Send the overlay config again every time the config file is saved. Watches the directory,
/// since editors often replace the file instead of writing into it.
pub fn watch_config(updates: Sender<OverlayConfig>) {
    let Some(path) = super::config_path() else {
        return;
    };
    let Some(dir) = path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    std::thread::Builder::new()
        .name("overlay-config".into())
        .spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(watcher) => watcher,
                Err(e) => return tracing::warn!("overlay won't reload config: {e}"),
            };
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                return tracing::warn!("overlay won't reload config: {e}");
            }
            while let Ok(event) = rx.recv() {
                // Only writes count: reading the file raises access events of its own
                let Ok(event) = event else { continue };
                let wrote = event.kind.is_modify() || event.kind.is_create();
                if !wrote || !event.paths.iter().any(|changed| changed == &path) {
                    continue;
                }
                // An editor save is several events; wait for them to settle
                while rx.recv_timeout(Duration::from_millis(150)).is_ok() {}
                if updates.send(super::load_config()).is_err() {
                    return;
                }
            }
        })
        .expect("spawn overlay config watcher");
}

const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Subscribe on a background thread. Once the daemon that was feeding us goes away, hide the
/// panel and exit non-zero: the init system restarts the overlay when the daemon is back (the
/// unit waits for it first), so a daemon restart cycles the overlay and a daemon stop takes it
/// down instead of leaving it idle. Exit rather than reconnect because that is the only way
/// the init system can tell the daemon is down. Before the first connection (the overlay
/// started by hand ahead of the daemon) it retries with backoff, as it always did.
pub fn spawn(frames: Sender<OverlaySnapshot>) {
    std::thread::Builder::new()
        .name("overlay-client".into())
        .spawn(move || {
            let mut retry = RETRY_MIN;
            loop {
                match crate::ipc::subscribe_overlay() {
                    Ok(stream) => {
                        for (n, mut frame) in stream.enumerate() {
                            // View commands from before this connection are history: a restarted
                            // overlay starts as configured, not as an earlier one was left
                            if n == 0 {
                                frame.controls.clear();
                            }
                            if frames.send(frame).is_err() {
                                return; // the Wayland loop is gone
                            }
                        }
                        tracing::warn!(
                            "daemon closed the overlay stream, exiting to be restarted with it"
                        );
                        let _ = frames.send(OverlaySnapshot::default());
                        // Let the Wayland loop draw the hidden panel before the process ends
                        std::thread::sleep(Duration::from_millis(100));
                        std::process::exit(1);
                    }
                    Err(e) => tracing::debug!("overlay: {e:#}"),
                }
                if frames.send(OverlaySnapshot::default()).is_err() {
                    return;
                }
                std::thread::sleep(retry);
                retry = (retry * 2).min(RETRY_MAX);
            }
        })
        .expect("spawn overlay client thread");
}
