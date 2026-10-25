//! Keeps a subscription to the daemon open and forwards each frame to the Wayland loop.

use crate::steno::overlay::OverlaySnapshot;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::time::Duration;

const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Subscribe on a background thread. Reconnects with backoff when the daemon restarts, and
/// sends an empty snapshot meanwhile so the panel hides instead of freezing on stale keys.
pub fn spawn(frames: Sender<OverlaySnapshot>) {
    std::thread::Builder::new()
        .name("overlay-client".into())
        .spawn(move || {
            let mut retry = RETRY_MIN;
            loop {
                match crate::ipc::subscribe_overlay() {
                    Ok(stream) => {
                        retry = RETRY_MIN;
                        for frame in stream {
                            if frames.send(frame).is_err() {
                                return; // the Wayland loop is gone
                            }
                        }
                        tracing::warn!("daemon closed the overlay stream, reconnecting");
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
