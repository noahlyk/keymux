#![allow(clippy::cast_possible_truncation)]

use crate::gamemode_state::{GlobalOverride, WindowOverride};
use crate::steno::overlay::{OverlayHub, OverlaySnapshot};
use crate::window_manager::WindowInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// IPC message from client to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Check if daemon is alive
    Ping,
    /// List all detected keyboards with their status
    ListKeyboards,
    /// Toggle enabled status of keyboards (returns current list for interactive selection)
    ToggleKeyboards,
    /// Enable specific keyboard by hardware ID
    EnableKeyboard(String),
    /// Disable specific keyboard by hardware ID
    DisableKeyboard(String),
    /// Reload configuration from disk
    Reload,
    /// Force save adaptive timing stats immediately
    SaveAdaptiveStats,
    /// Shutdown daemon
    Shutdown,

    /// Reported by a per-compositor watcher process on every focus change.
    /// The daemon resolves the effective game mode itself (heuristics +
    /// overrides) and applies it - watchers no longer compute a boolean.
    WindowFocusChanged {
        app_id: Option<String>,
        pid: Option<u32>,
        title: Option<String>,
    },
    /// Ask the daemon for the last window focus event it received (used by
    /// `keymux gamemode window` to default to "the currently focused app"
    /// without any compositor-specific code in the CLI).
    GetFocusedWindow,
    /// Set (or clear, with `state: None`) a temporary per-app_id override.
    SetWindowOverride {
        app_id: String,
        state: Option<WindowOverride>,
    },
    /// Flip the currently-effective state for an app_id into an explicit
    /// override (On -> Off, Off/heuristic-Normal -> On).
    ToggleWindowOverride { app_id: String },
    /// List all active per-app_id overrides
    ListWindowOverrides,
    /// Set the global override (Auto/AlwaysOn/AlwaysOff)
    SetGlobalOverride(GlobalOverride),
    /// Get the current global override
    GetGlobalOverride,
    /// Keep the connection open and stream [`OverlaySnapshot`] frames on every change.
    /// Handled by the IPC thread itself, never reaches the daemon's main loop.
    SubscribeOverlay,
}

/// IPC response from daemon to client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Daemon is alive
    Pong,
    /// List of keyboards with their info
    KeyboardList(Vec<KeyboardInfo>),
    /// Operation succeeded
    Ok,
    /// Operation failed with error message
    Error(String),
    /// The last window the daemon saw focused (empty fields if none yet)
    FocusedWindow(WindowInfo),
    /// Active per-app_id overrides, sorted by app_id
    WindowOverrides(Vec<(String, WindowOverride)>),
    /// Current global override
    GlobalOverrideStatus(GlobalOverride),
}

/// Information about a detected keyboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardInfo {
    /// Persistent hardware ID including USB port (e.g. "362d:0210:0111:0003@3-4.2")
    pub hardware_id: String,
    /// Human-readable name
    pub name: String,
    /// Current device path (can change between boots)
    pub device_path: String,
    /// Whether this keyboard is currently enabled
    pub enabled: bool,
    /// Whether this keyboard is currently connected
    pub connected: bool,
    /// True when enabled via a portless config entry (e.g. "362d:0210:0111:0003")
    /// rather than an explicit port entry (e.g. "362d:0210:0111:0003@3-4.2").
    /// Used by the display layer to annotate the ID with a hint.
    pub enabled_by_portless: bool,
    /// The config rule pattern that matched (e.g., "*", "1234", "Keychron")
    /// None if implicitly enabled/disabled (no explicit rule matched)
    pub matched_rule: Option<String>,
}

/// Get the IPC socket path for root daemon
pub fn get_root_socket_path() -> PathBuf {
    Path::new("/run").join("keymux.sock")
}

/// Get the IPC socket path for user daemon (legacy, for compatibility)
pub fn get_user_socket_path() -> PathBuf {
    let (uid, _) = crate::get_actual_user_uid();
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", uid));
    Path::new(&runtime_dir).join("keymux.sock")
}

/// Get the IPC socket path (tries root first, falls back to user)
pub fn get_socket_path() -> PathBuf {
    let root_sock = get_root_socket_path();
    if root_sock.exists() {
        root_sock
    } else {
        get_user_socket_path()
    }
}

/// Send an IPC request and receive response
pub fn send_request(request: &IpcRequest) -> Result<IpcResponse> {
    let socket_path = get_socket_path();
    let mut stream =
        UnixStream::connect(&socket_path).context("Failed to connect to daemon. Is it running?")?;

    // Serialize and send request
    let encoded = bincode::serialize(request)?;
    let len = (encoded.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&encoded)?;
    stream.flush()?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Read response
    let mut response_buf = vec![0u8; len];
    stream.read_exact(&mut response_buf)?;
    let response: IpcResponse = bincode::deserialize(&response_buf)?;

    Ok(response)
}

/// Write one length-prefixed bincode frame, the same framing as requests and responses.
fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let encoded = bincode::serialize(value)?;
    stream.write_all(&(encoded.len() as u32).to_le_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    Ok(())
}

/// Daemon side of [`IpcRequest::SubscribeOverlay`]: send the current state, then every newer one.
///
/// Runs on its own thread until the subscriber goes away, and never touches the key path except
/// through the hub's lock.
///
/// The subscriber never sends anything after subscribing, so a second thread blocks reading
/// the socket: it returns the moment the overlay closes its end, and wakes the writer. That
/// way a closed overlay is noticed at once, not at the next write (which can still succeed
/// once after the peer has gone) and with no idle wakeups.
pub fn serve_overlay(mut stream: UnixStream, hub: &OverlayHub) {
    let Ok(mut watcher) = stream.try_clone() else {
        return;
    };
    let gone = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut ignored = [0u8; 64];
            while matches!(watcher.read(&mut ignored), Ok(n) if n > 0) {}
            gone.store(true, Ordering::Release);
            hub.wake();
        });

        let mut state = hub.snapshot();
        'serving: loop {
            if write_frame(&mut stream, &state).is_err() {
                break;
            }
            // Wait for something newer; the long timeout only bounds a single wait
            loop {
                if gone.load(Ordering::Acquire) {
                    break 'serving;
                }
                if let Some(newer) =
                    hub.wait_newer(state.generation, Duration::from_secs(3600), &gone)
                {
                    state = newer;
                    break;
                }
            }
        }
        // Ends the watcher's blocking read too, if it is still running (a failed write)
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
}

/// Overlay side: connect, subscribe, and return the frames as they arrive. The iterator
/// ends when the daemon goes away; the caller reconnects.
pub fn subscribe_overlay() -> Result<impl Iterator<Item = OverlaySnapshot>> {
    let mut stream = UnixStream::connect(get_socket_path())
        .context("Failed to connect to daemon. Is it running?")?;
    write_frame(&mut stream, &IpcRequest::SubscribeOverlay)?;
    Ok(std::iter::from_fn(move || read_frame(&mut stream)))
}

/// The next length-prefixed frame, or `None` when the stream ends or breaks.
fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Option<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut frame).ok()?;
    bincode::deserialize(&frame).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_gets_the_current_state_then_each_change() {
        let hub = OverlayHub::default();
        hub.set_layer(Some("steno"));
        let (mut client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| serve_overlay(server, &hub));

            let first: OverlaySnapshot = read_frame(&mut client).unwrap();
            assert_eq!(first.layer.as_deref(), Some("steno"));

            hub.set_keys(0b11, 0b11);
            let next: OverlaySnapshot = read_frame(&mut client).unwrap();
            assert_eq!((next.chord, next.held), (0b11, 0b11));
            assert!(next.generation > first.generation);

            // Closing our end must stop the stream thread on its own, with no further change
            // to wake it, so the scope can end. If it didn't, this test would hang.
            drop(client);
        });
    }

    #[test]
    fn a_subscriber_that_stops_listening_is_noticed_without_any_change() {
        let hub = OverlayHub::default();
        let (client, server) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| serve_overlay(server, &hub));
            // Let it send its first frame and settle into waiting, then hang up
            let mut client = client;
            let _: OverlaySnapshot = read_frame(&mut client).unwrap();
            drop(client);
        });
        // A heartbeat design would take its whole interval here; this must be near instant
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_request_round_trips_in_the_same_framing() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_frame(&mut a, &IpcRequest::SubscribeOverlay).unwrap();
        assert!(matches!(
            read_frame::<IpcRequest>(&mut b),
            Some(IpcRequest::SubscribeOverlay)
        ));
    }
}
